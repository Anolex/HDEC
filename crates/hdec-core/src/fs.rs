//! Filesystem helpers shared by all formats: crash-safe output files, the
//! extraction writer (cached folder checks, a pool of writer threads), and
//! walking source trees.

use crate::{Ctx, Error, Overwrite, Result, safe_path};
use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::SystemTime;

/// Writes to `<path>.hdec-tmp`; `commit` renames it over `path`.
/// Dropped without `commit` (error, cancel) → the temp file is removed.
pub(crate) struct OutFile {
    path: PathBuf,
    tmp: PathBuf,
    file: File,
    committed: bool,
}

impl OutFile {
    pub fn create(path: PathBuf) -> Result<Self> {
        let mut tmp = path.clone().into_os_string();
        tmp.push(".hdec-tmp");
        let tmp = PathBuf::from(tmp);
        let file = File::create(&tmp)?;
        Ok(Self {
            path,
            tmp,
            file,
            committed: false,
        })
    }

    /// Writes `path` directly; fails if anything (even a dangling symlink) is
    /// there. Nothing is overwritten, so no temp file and no rename are needed;
    /// an uncommitted file is still removed.
    pub fn create_new(path: PathBuf) -> Result<Self> {
        let file = File::options().write(true).create_new(true).open(&path)?;
        Ok(Self {
            tmp: path.clone(),
            path,
            file,
            committed: false,
        })
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn commit(mut self) -> Result<()> {
        if self.tmp != self.path {
            fs::rename(&self.tmp, &self.path)?;
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for OutFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

impl Write for OutFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for OutFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

/// Entries up to this size are decoded into memory and written by the pool;
/// bigger ones are written by the decoder and only finished by the pool.
const BUFFERED: u64 = 1 << 20;

/// Writer threads. File creation, not bandwidth, is the bottleneck. On APFS
/// 4 is best wherever we measured (8 is up to 2x slower in watched folders).
/// Windows: Defender scans each file on close, which more threads hide.
/// ponytail: Windows count unmeasured; run the bench example on Windows 10.
pub(crate) fn writer_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    if cfg!(windows) {
        cores.clamp(4, 16)
    } else {
        cores.min(4)
    }
}

/// One extracted file being filled. `finish` sets its time and mode and
/// commits it; dropped unfinished, nothing is left behind.
pub(crate) struct Out {
    target: Target,
    data: Data,
}

enum Data {
    Buf(Vec<u8>),
    File(OutFile),
}

impl Out {
    /// `size` is the declared size (a capacity hint only; `u64::MAX` = unknown).
    /// `None` = skipped by the overwrite policy.
    pub fn new(target: &Target, size: u64) -> Result<Option<Self>> {
        let data = if size <= BUFFERED {
            Data::Buf(Vec::with_capacity(size as usize))
        } else {
            match target.open()? {
                Some(f) => Data::File(f),
                None => return Ok(None),
            }
        };
        Ok(Some(Self {
            target: target.clone(),
            data,
        }))
    }

    pub fn finish(self, modified: Option<SystemTime>, mode: Option<u32>) -> Result<()> {
        let out = match self.data {
            Data::File(f) => f,
            Data::Buf(buf) => {
                let Some(mut f) = self.target.open()? else {
                    return Ok(());
                };
                f.write_all(&buf)?;
                f
            }
        };
        if let Some(t) = modified {
            out.file().set_modified(t)?;
        }
        set_mode(out.file(), mode)?;
        out.commit()
    }
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.data {
            Data::Buf(b) => {
                b.extend_from_slice(buf);
                Ok(buf.len())
            }
            Data::File(f) => f.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.data {
            Data::Buf(_) => Ok(()),
            Data::File(f) => f.flush(),
        }
    }
}

/// Where an entry goes. `replace`: something is there and the policy is Replace.
#[derive(Clone)]
pub(crate) struct Target {
    pub path: PathBuf,
    replace: bool,
    overwrite: Overwrite,
}

impl Target {
    /// Opens the output; `None` = skipped. A name that turns up taken anyway
    /// (case-insensitive file systems: `Makefile` vs `makefile`; a folder of
    /// the same name) gets the overwrite policy instead of failing.
    fn open(&self) -> Result<Option<OutFile>> {
        if self.replace {
            return OutFile::create(self.path.clone()).map(Some);
        }
        let taken = |r: &Result<OutFile>| matches!(r, Err(Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists);
        let first = OutFile::create_new(self.path.clone());
        if !taken(&first) {
            return first.map(Some);
        }
        match self.overwrite {
            Overwrite::Skip => Ok(None),
            Overwrite::Replace => OutFile::create(self.path.clone()).map(Some),
            Overwrite::Rename => {
                for n in 1.. {
                    let next = OutFile::create_new(numbered(&self.path, n));
                    if !taken(&next) {
                        return next.map(Some);
                    }
                }
                unreachable!("unbounded range")
            }
        }
    }
}

/// Extracts into `dest`: checks each folder once and hands finished files
/// and symlinks to writer threads; hard links come last.
pub(crate) struct Extractor {
    dest: PathBuf,
    overwrite: Overwrite,
    /// Folders known to be real directories (created or checked).
    dirs: HashSet<PathBuf>,
    /// Folders this run created: nothing in them predates us.
    created: HashSet<PathBuf>,
    /// Outputs promised to entries so far (some may still be queued).
    claimed: HashSet<PathBuf>,
    /// Hard links `(name, path, target)`, made once their targets are written.
    links: Vec<(String, PathBuf, PathBuf)>,
    pub warnings: Vec<String>,
    pool: Pool,
}

impl Extractor {
    pub fn new(dest: &Path, overwrite: Overwrite, ctx: &Ctx) -> Result<Self> {
        fs::create_dir_all(dest)?;
        Ok(Self {
            dest: dest.to_path_buf(),
            overwrite,
            dirs: HashSet::from([dest.to_path_buf()]),
            created: HashSet::new(),
            claimed: HashSet::new(),
            links: Vec::new(),
            warnings: Vec::new(),
            pool: Pool::new(writer_threads(), ctx.cancel.clone()),
        })
    }

    pub fn dir(&mut self, name: &str) -> Result<()> {
        let dir = safe_path::join(&self.dest, name)?;
        self.make_dirs(&dir)
    }

    /// Creates `dir` one component at a time, refusing to pass through
    /// symlinks or files. `create_dir` never follows a symlink, so only an
    /// existing name costs a second look.
    fn make_dirs(&mut self, dir: &Path) -> Result<()> {
        if self.dirs.contains(dir) {
            return Ok(());
        }
        let parent = match dir.parent() {
            Some(p) if dir.starts_with(&self.dest) => p,
            _ => return Err(Error::UnsafePath(dir.display().to_string())),
        };
        self.make_dirs(parent)?;
        if self.claimed.contains(dir) {
            // A queued link or file has this name: let it land, then look.
            self.pool.wait()?;
        }
        match fs::create_dir(dir) {
            Ok(()) => {
                self.created.insert(dir.to_path_buf());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(dir)?.is_dir() {
                    return Err(Error::UnsafePath(dir.display().to_string()));
                }
            }
            Err(e) => return Err(e.into()),
        }
        self.dirs.insert(dir.to_path_buf());
        Ok(())
    }

    /// Resolves where entry `name` goes. `None` = skip (exists, policy Skip).
    pub fn target(&mut self, name: &str) -> Result<Option<Target>> {
        self.pool.check()?;
        let path = safe_path::join(&self.dest, name)?;
        let parent = path.parent().unwrap_or(&self.dest).to_path_buf();
        self.make_dirs(&parent)?;
        let claimed = self.claimed.contains(&path);
        if claimed {
            // Same name twice: let the earlier one land first.
            self.pool.wait()?;
        }
        // In a folder we created, only our own entries can exist.
        let ours = claimed || self.dirs.contains(&path);
        let existing = (ours || !self.created.contains(&parent))
            .then(|| fs::symlink_metadata(&path).ok())
            .flatten();
        let (path, replace) = match (existing, self.overwrite) {
            (None, _) => (path, false),
            (Some(_), Overwrite::Skip) => return Ok(None),
            (Some(m), Overwrite::Replace) if m.is_dir() => {
                self.warnings
                    .push(format!("skipped {name}: a folder with that name exists"));
                return Ok(None);
            }
            (Some(_), Overwrite::Replace) => (path, true),
            (Some(_), Overwrite::Rename) => (unique(&path, &self.claimed), false),
        };
        let target = Target {
            path,
            replace,
            overwrite: self.overwrite,
        };
        self.claimed.insert(target.path.clone());
        Ok(Some(target))
    }

    /// Opens entry `name` for writing; `None` = skip.
    pub fn file(&mut self, name: &str, size: u64) -> Result<Option<Out>> {
        Ok(self
            .target(name)?
            .map(|t| Out::new(&t, size))
            .transpose()?
            .flatten())
    }

    /// Queues a filled entry for the writer threads.
    pub fn done(
        &mut self,
        out: Out,
        modified: Option<SystemTime>,
        mode: Option<u32>,
    ) -> Result<()> {
        self.pool
            .submit(Box::new(move || out.finish(modified, mode).map(|()| None)))
    }

    /// Queued right away: `make_dirs` and `create_new` never follow links, so
    /// a link can't redirect later entries.
    pub fn symlink(&mut self, name: &str, target: &str) -> Result<()> {
        let Some(t) = self.target(name)? else {
            return Ok(());
        };
        let (dest, name, target) = (self.dest.clone(), name.to_owned(), target.to_owned());
        self.pool.submit(Box::new(move || {
            Ok((!symlink(&dest, &t.path, &target)?)
                .then(|| format!("skipped symlink {name} -> {target}")))
        }))
    }

    /// Hard link to `target`, an earlier entry's name.
    pub fn hard_link(&mut self, name: &str, target: &str) -> Result<()> {
        let target = safe_path::join(&self.dest, target)?;
        if let Some(t) = self.target(name)? {
            self.links.push((name.to_owned(), t.path, target));
        }
        Ok(())
    }

    /// Once every file is written, creates the hard links.
    pub fn finish(mut self) -> Result<Vec<String>> {
        self.pool.wait()?;
        for (name, path, target) in std::mem::take(&mut self.links) {
            if fs::symlink_metadata(&path).is_ok() {
                fs::remove_file(&path)?;
            }
            if fs::hard_link(&target, &path).is_err() {
                let target = target.display();
                self.warnings
                    .push(format!("skipped hard link {name} -> {target}"));
            }
        }
        self.pool.finish()?;
        let mut warnings = self.warnings;
        warnings.append(&mut self.pool.state.warnings.lock().unwrap());
        Ok(warnings)
    }
}

/// Returns a warning, if any.
type Job = Box<dyn FnOnce() -> Result<Option<String>> + Send>;

/// Writer threads fed through a bounded queue. The first error stops the
/// rest; once cancelled, queued jobs are dropped (and leave nothing behind).
struct Pool {
    tx: Option<SyncSender<Job>>,
    threads: Vec<JoinHandle<()>>,
    state: Arc<PoolState>,
}

struct PoolState {
    pending: Mutex<usize>,
    idle: Condvar,
    error: Mutex<Option<Error>>,
    warnings: Mutex<Vec<String>>,
    cancel: Arc<AtomicBool>,
}

impl Pool {
    fn new(threads: usize, cancel: Arc<AtomicBool>) -> Self {
        // Bounded: at most this many buffered entries (≤ 1 MiB each) wait.
        let (tx, rx) = mpsc::sync_channel::<Job>(64);
        let rx = Arc::new(Mutex::new(rx));
        let state = Arc::new(PoolState {
            pending: Mutex::new(0),
            idle: Condvar::new(),
            error: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
            cancel,
        });
        let threads = (0..threads)
            .map(|_| {
                let (rx, state) = (rx.clone(), state.clone());
                std::thread::spawn(move || work(&rx, &state))
            })
            .collect();
        Self {
            tx: Some(tx),
            threads,
            state,
        }
    }

    fn submit(&mut self, job: Job) -> Result<()> {
        self.check()?;
        *self.state.pending.lock().unwrap() += 1;
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(job).is_ok());
        if !sent {
            *self.state.pending.lock().unwrap() -= 1;
            return Err(io::Error::other("writer threads stopped").into());
        }
        Ok(())
    }

    /// Returns the first writer error, if any.
    fn check(&self) -> Result<()> {
        match self.state.error.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Blocks until every queued file is written.
    fn wait(&self) -> Result<()> {
        let mut pending = self.state.pending.lock().unwrap();
        while *pending > 0 {
            pending = self.state.idle.wait(pending).unwrap();
        }
        drop(pending);
        self.check()
    }

    fn finish(&mut self) -> Result<()> {
        self.tx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.check()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn work(rx: &Mutex<Receiver<Job>>, state: &PoolState) {
    loop {
        let job = rx.lock().unwrap().recv();
        let Ok(job) = job else { return };
        let stopped = state.cancel.load(Ordering::Relaxed) || state.error.lock().unwrap().is_some();
        if !stopped {
            match crate::no_panic(job) {
                Ok(None) => {}
                Ok(Some(w)) => state.warnings.lock().unwrap().push(w),
                Err(e) => {
                    state.error.lock().unwrap().get_or_insert(e);
                }
            }
        }
        let mut pending = state.pending.lock().unwrap();
        *pending -= 1;
        if *pending == 0 {
            state.idle.notify_all();
        }
    }
}

/// Creates `dir` (which must lie under `dest`) one component at a time,
/// refusing to pass through symlinks or files planted by earlier entries.
pub(crate) fn make_dirs(dest: &Path, dir: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let rel = dir
        .strip_prefix(dest)
        .map_err(|_| Error::UnsafePath(dir.display().to_string()))?;
    let mut cur = dest.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(Error::UnsafePath(cur.display().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&cur)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// `name.ext` → `name (1).ext`, `name (2).ext`, … first one that is free.
fn unique(path: &Path, claimed: &HashSet<PathBuf>) -> PathBuf {
    (1..)
        .map(|n| numbered(path, n))
        .find(|p| !claimed.contains(p) && fs::symlink_metadata(p).is_err())
        .expect("unbounded range")
}

/// `name.ext` → `name (n).ext`.
fn numbered(path: &Path, n: u32) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem} ({n}){ext}"))
}

/// Creates `link → target` if the target stays inside `dest`. Returns false if skipped.
/// Windows: always skipped (creating symlinks needs special privileges there).
pub(crate) fn symlink(dest: &Path, link: &Path, target: &str) -> Result<bool> {
    let rel = link
        .strip_prefix(dest)
        .map_err(|_| Error::UnsafePath(link.display().to_string()))?;
    if !cfg!(unix) || !safe_path::link_stays_inside(rel, target) {
        return Ok(false);
    }
    if fs::symlink_metadata(link).is_ok() {
        fs::remove_file(link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)?;
    Ok(true)
}

/// A file, directory or symlink to put into an archive.
pub(crate) struct Source {
    pub path: PathBuf,
    /// Archive name: '/'-separated, relative to the source's parent directory.
    pub name: String,
    pub meta: Metadata,
}

/// Expands sources depth-first (parents before children, sorted). Symlinks are not followed.
pub(crate) fn walk(sources: &[PathBuf]) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for src in sources {
        let name = src
            .file_name()
            .ok_or_else(|| {
                Error::Io(io::Error::other(format!(
                    "bad source path: {}",
                    src.display()
                )))
            })?
            .to_string_lossy()
            .into_owned();
        walk_into(src.clone(), name, &mut out)?;
    }
    Ok(out)
}

fn walk_into(path: PathBuf, name: String, out: &mut Vec<Source>) -> Result<()> {
    let meta = fs::symlink_metadata(&path)?;
    let is_dir = meta.is_dir();
    out.push(Source {
        path: path.clone(),
        name: name.clone(),
        meta,
    });
    if is_dir {
        let mut kids = fs::read_dir(&path)?.collect::<io::Result<Vec<_>>>()?;
        kids.sort_by_key(|k| k.file_name());
        for k in kids {
            walk_into(
                k.path(),
                format!("{name}/{}", k.file_name().to_string_lossy()),
                out,
            )?;
        }
    }
    Ok(())
}

/// Copies a file or directory tree, keeping symlinks as symlinks (skipped on
/// Windows) and modification times.
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else if meta.is_symlink() {
        if fs::symlink_metadata(dst).is_ok() {
            fs::remove_file(dst)?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else {
        fs::copy(src, dst)?;
        File::options()
            .write(true)
            .open(dst)?
            .set_modified(meta.modified()?)?;
    }
    Ok(())
}

/// Unix permission bits of a source file, if the platform has them.
pub(crate) fn mode(meta: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Applies archived permission bits (without setuid/setgid/sticky) on Unix.
pub(crate) fn set_mode(file: &File, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(m & 0o777))?;
    }
    let _ = (file, mode);
    Ok(())
}
