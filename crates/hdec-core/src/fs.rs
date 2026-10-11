//! Filesystem helpers shared by all formats: crash-safe output files, safe
//! directory creation, symlinks, and walking source trees.

use crate::{Error, Overwrite, Result, safe_path};
use std::fs::{self, File, Metadata};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn commit(mut self) -> Result<()> {
        fs::rename(&self.tmp, &self.path)?;
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

/// Resolves where entry `name` goes. `None` = skip (exists and policy is Skip).
pub(crate) fn output_path(dest: &Path, name: &str, ow: Overwrite) -> Result<Option<PathBuf>> {
    let path = safe_path::join(dest, name)?;
    make_dirs(dest, path.parent().unwrap_or(dest))?;
    if fs::symlink_metadata(&path).is_err() {
        return Ok(Some(path));
    }
    Ok(match ow {
        Overwrite::Skip => None,
        Overwrite::Replace => Some(path),
        Overwrite::Rename => Some(unique(&path)),
    })
}

/// `name.ext` → `name (1).ext`, `name (2).ext`, … first one that doesn't exist.
fn unique(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("unbounded range")
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
