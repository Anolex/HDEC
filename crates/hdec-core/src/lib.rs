mod formats {
    pub mod rar;
    pub mod sevenz;
    pub mod stream;
    pub mod tar;
    pub mod zip;
}
mod fs;
pub mod safe_path;
mod volume;

use std::cell::Cell;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

#[derive(Debug)]
pub enum Error {
    PasswordRequired,
    WrongPassword,
    Unsupported(String),
    Corrupt(String),
    UnsafePath(String),
    Cancelled,
    Io(io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::PasswordRequired => f.write_str("password required"),
            Error::WrongPassword => f.write_str("wrong password"),
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Corrupt(s) => write!(f, "corrupt archive: {s}"),
            Error::UnsafePath(s) => write!(f, "unsafe path in archive: {s:?}"),
            Error::Cancelled => f.write_str("cancelled"),
            Error::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Zip,
    SevenZ,
    Rar,
    Tar,
    TarGz,
    TarXz,
    TarBz2,
    TarZst,
    Gz,
    Xz,
    Bz2,
    Zst,
    Lzma,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "ZIP",
            Format::SevenZ => "7Z",
            Format::Rar => "RAR",
            Format::Tar => "TAR",
            Format::TarGz => "TAR.GZ",
            Format::TarXz => "TAR.XZ",
            Format::TarBz2 => "TAR.BZ2",
            Format::TarZst => "TAR.ZST",
            Format::Gz => "GZ",
            Format::Xz => "XZ",
            Format::Bz2 => "BZ2",
            Format::Zst => "ZST",
            Format::Lzma => "LZMA",
        }
    }

    pub fn encryptable(self) -> bool {
        matches!(self, Format::Zip | Format::SevenZ)
    }

    /// File extension without the leading dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::Rar => "rar",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarXz => "tar.xz",
            Format::TarBz2 => "tar.bz2",
            Format::TarZst => "tar.zst",
            Format::Gz => "gz",
            Format::Xz => "xz",
            Format::Bz2 => "bz2",
            Format::Zst => "zst",
            Format::Lzma => "lzma",
        }
    }
}

/// One archive member.
#[derive(Debug, Clone)]
pub struct Entry {
    /// '/'-separated, no trailing slash.
    pub path: String,
    pub size: u64,
    pub packed: Option<u64>,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    pub encrypted: bool,
}

/// `progress(done_bytes, total_bytes, current_entry)`.
pub type ProgressFn = dyn Fn(u64, u64, &str) + Send + Sync;

/// Per-operation context. Cancel and progress are shared (`Arc`) because some
/// decoders take `'static` writers that must report progress themselves.
pub struct Ctx<'a> {
    pub password: Option<&'a str>,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<ProgressFn>,
}

impl Ctx<'_> {
    fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Errors raised through our cancelling readers are reported as `Cancelled`.
    fn or_cancelled(&self, e: impl Into<Error>) -> Error {
        if self.cancel.load(Ordering::Relaxed) {
            Error::Cancelled
        } else {
            e.into()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overwrite {
    #[default]
    Skip,
    Replace,
    Rename,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// Entry paths to extract (a directory selects everything below it); `None` = all.
    pub only: Option<Vec<String>>,
    pub overwrite: Overwrite,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Level {
    Store,
    Fast,
    #[default]
    Normal,
    Max,
}

#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub format: Format,
    pub level: Level,
    /// ZIP only: legacy ZipCrypto instead of AES-256. Weak, but the only
    /// encryption Windows Explorer and macOS Archive Utility can open.
    pub zip_crypto: bool,
    /// Split the finished archive into `dest.001`, `dest.002`, … of this many bytes.
    pub split: Option<u64>,
}

/// One change to an existing archive.
#[derive(Debug, Clone)]
pub enum Edit {
    /// Add files/folders into archive folder `into` ("" = root), replacing same-named entries.
    Add { sources: Vec<PathBuf>, into: String },
    /// Remove entries; a folder removes everything below it.
    Delete(Vec<String>),
    /// Rename or move one entry or folder; `to` is the full new path.
    Rename { from: String, to: String },
}

pub fn list(path: &Path, ctx: &Ctx) -> Result<Vec<Entry>> {
    match detect(path)? {
        Format::Zip => formats::zip::list(path, ctx),
        Format::SevenZ => formats::sevenz::list(path, ctx),
        Format::Rar => formats::rar::list(path, ctx),
        f @ (Format::Tar | Format::TarGz | Format::TarXz | Format::TarBz2 | Format::TarZst) => {
            formats::tar::list(path, f)
        }
        f @ (Format::Gz | Format::Xz | Format::Bz2 | Format::Zst | Format::Lzma) => {
            formats::stream::list(path, f)
        }
    }
}

/// Extracts into `dest`. Unsafe entries are skipped, not fatal; they come back as warnings.
pub fn extract(path: &Path, dest: &Path, opts: &ExtractOptions, ctx: &Ctx) -> Result<Vec<String>> {
    run_extract(path, Some(dest), opts, ctx)
}

/// Decodes everything and verifies checksums without writing anything.
pub fn test(path: &Path, ctx: &Ctx) -> Result<()> {
    run_extract(path, None, &ExtractOptions::default(), ctx).map(drop)
}

fn run_extract(
    path: &Path,
    dest: Option<&Path>,
    opts: &ExtractOptions,
    ctx: &Ctx,
) -> Result<Vec<String>> {
    match detect(path)? {
        Format::Zip => formats::zip::extract(path, dest, opts, ctx),
        Format::SevenZ => formats::sevenz::extract(path, dest, opts, ctx),
        Format::Rar => formats::rar::extract(path, dest, opts, ctx),
        f @ (Format::Tar | Format::TarGz | Format::TarXz | Format::TarBz2 | Format::TarZst) => {
            formats::tar::extract(path, f, dest, opts, ctx)
        }
        f @ (Format::Gz | Format::Xz | Format::Bz2 | Format::Zst | Format::Lzma) => {
            formats::stream::extract(path, f, dest, opts, ctx)
        }
    }
}

pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()> {
    if ctx.password.is_some() && !opts.format.encryptable() {
        return Err(Error::Unsupported(format!(
            "{} cannot be encrypted",
            opts.format.name()
        )));
    }
    match opts.format {
        Format::Zip => formats::zip::create(sources, dest, opts, ctx),
        Format::SevenZ => formats::sevenz::create(sources, dest, opts, ctx),
        Format::Tar | Format::TarGz | Format::TarXz | Format::TarBz2 => {
            formats::tar::create(sources, dest, opts, ctx)
        }
        Format::Gz | Format::Xz | Format::Bz2 => formats::stream::create(sources, dest, opts, ctx),
        f => Err(Error::Unsupported(format!(
            "creating {} is not supported",
            f.name()
        ))),
    }?;
    match opts.split {
        Some(size) => volume::split(dest, size, ctx),
        None => Ok(()),
    }
}

/// True if `path` can be changed with [`modify`].
pub fn modifiable(path: &Path) -> bool {
    volume::base_path(path).is_none()
        && matches!(
            detect(path),
            Ok(Format::Zip
                | Format::SevenZ
                | Format::Tar
                | Format::TarGz
                | Format::TarXz
                | Format::TarBz2)
        )
}

/// Applies `edit`, replacing the archive only once the new one is complete.
pub fn modify(path: &Path, edit: &Edit, ctx: &Ctx) -> Result<()> {
    if !modifiable(path) {
        return Err(Error::Unsupported(format!(
            "{} can't be modified",
            path.display()
        )));
    }
    match detect(path)? {
        Format::Zip => formats::zip::modify(path, edit, ctx),
        format => rewrite(path, format, edit, ctx),
    }
}

/// Generic modify: extract next to the archive, edit the files, re-create.
/// ponytail: recompresses everything at Normal level; ZIP has a raw-copy path instead.
fn rewrite(path: &Path, format: Format, edit: &Edit, ctx: &Ctx) -> Result<()> {
    let scratch = tempfile::Builder::new()
        .prefix(".hdec-edit")
        .tempdir_in(path.parent().unwrap_or(Path::new(".")))?;
    let root = scratch.path();
    let warnings = extract(path, root, &ExtractOptions::default(), ctx)?;
    if let Some(w) = warnings.first() {
        return Err(Error::Unsupported(format!(
            "refusing to modify, an entry can't be extracted safely: {w}"
        )));
    }
    match edit {
        Edit::Add { sources, into } => {
            let dir = if into.is_empty() {
                root.to_path_buf()
            } else {
                safe_path::join(root, into)?
            };
            fs::make_dirs(root, &dir)?;
            for src in sources {
                let name = src
                    .file_name()
                    .ok_or_else(|| Error::Unsupported(format!("bad source: {}", src.display())))?;
                fs::copy_tree(src, &dir.join(name))?;
            }
        }
        Edit::Delete(names) => {
            for name in names {
                let p = safe_path::join(root, name)?;
                match std::fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p)?,
                    Ok(_) => std::fs::remove_file(&p)?,
                    Err(_) => {}
                }
            }
        }
        Edit::Rename { from, to } => {
            let (from, to) = (safe_path::join(root, from)?, safe_path::join(root, to)?);
            fs::make_dirs(root, to.parent().unwrap_or(root))?;
            std::fs::rename(from, to)?;
        }
    }
    let mut sources = std::fs::read_dir(root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<Vec<_>>>()?;
    sources.sort();
    let opts = CreateOptions {
        format,
        ..Default::default()
    };
    create(&sources, path, &opts, ctx)
}

fn is_selected(name: &str, only: Option<&[String]>) -> bool {
    let name = name.trim_end_matches('/');
    only.is_none_or(|sel| {
        sel.iter().any(|s| {
            name.strip_prefix(s.as_str())
                .is_some_and(|r| r.is_empty() || r.starts_with('/'))
        })
    })
}

/// Copies entry data while reporting progress and honouring cancellation.
struct Meter<'a> {
    ctx: &'a Ctx<'a>,
    done: Cell<u64>,
    total: u64,
    /// Streamed formats report compressed bytes consumed instead of bytes written.
    consumed: Option<&'a Cell<u64>>,
    buf: Vec<u8>,
}

impl<'a> Meter<'a> {
    fn new(ctx: &'a Ctx<'a>, total: u64) -> Self {
        Self {
            ctx,
            done: Cell::new(0),
            total,
            consumed: None,
            buf: vec![0; 256 << 10],
        }
    }

    /// Progress = `consumed` (fed by a `CountingRead` on the archive file) out of the archive size.
    fn streamed(ctx: &'a Ctx<'a>, archive_size: u64, consumed: &'a Cell<u64>) -> Self {
        Self {
            consumed: Some(consumed),
            ..Self::new(ctx, archive_size)
        }
    }

    fn advance(&self, n: usize, name: &str) {
        self.done.set(self.done.get() + n as u64);
        let shown = self.consumed.map_or(self.done.get(), Cell::get);
        (*self.ctx.progress)(shown, self.total, name);
    }

    /// For APIs that pull from a reader (e.g. tar::Builder). Cancellation surfaces
    /// as an I/O error; map it back with `Ctx::or_cancelled`.
    fn reader<'m, R: Read>(&'m self, name: &'m str, inner: R) -> MeterRead<'m, 'a, R> {
        MeterRead {
            meter: self,
            name,
            inner,
        }
    }

    fn copy(&mut self, name: &str, r: &mut impl Read, w: &mut impl Write) -> Result<u64> {
        let mut copied = 0;
        loop {
            self.ctx.check()?;
            let n = match r.read(&mut self.buf) {
                Ok(0) => return Ok(copied),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            w.write_all(&self.buf[..n])?;
            copied += n as u64;
            self.advance(n, name);
        }
    }

    /// Copies exactly `size` bytes, then reads to EOF so the decoder verifies its
    /// checksum. More data than declared is an error, never written (bomb guard).
    fn copy_sized(
        &mut self,
        name: &str,
        r: &mut impl Read,
        w: &mut impl Write,
        size: u64,
    ) -> Result<()> {
        let n = self.copy(name, &mut r.by_ref().take(size), w)?;
        if n != size || r.read(&mut [0])? != 0 {
            return Err(Error::Corrupt(format!("{name}: size differs from header")));
        }
        Ok(())
    }
}

struct MeterRead<'m, 'a, R> {
    meter: &'m Meter<'a>,
    name: &'m str,
    inner: R,
}

impl<R: Read> Read for MeterRead<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.meter.ctx.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.meter.advance(n, self.name);
        Ok(n)
    }
}

/// Counts bytes read from the archive file, for `Meter::streamed`.
struct CountingRead<'a, R> {
    inner: R,
    count: &'a Cell<u64>,
}

impl<R: Read> Read for CountingRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

/// Detects the format by magic bytes; the file name only decides tar-vs-plain
/// for compressed streams and covers formats without magic (v7 tar, .lzma).
pub fn detect(path: &Path) -> Result<Format> {
    let mut head = Vec::with_capacity(512);
    volume::Volume::open(path)?
        .take(512)
        .read_to_end(&mut head)?;
    let base = volume::base_path(path);
    let name = base
        .as_deref()
        .unwrap_or(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    detect_bytes(&head, &name)
        .ok_or_else(|| Error::Unsupported(format!("unknown format: {}", path.display())))
}

fn detect_bytes(head: &[u8], name: &str) -> Option<Format> {
    let tarred = |exts: &[&str]| name.contains(".tar.") || exts.iter().any(|e| name.ends_with(e));
    let pick = |exts: &[&str], tar, plain| if tarred(exts) { tar } else { plain };
    Some(match head {
        [b'P', b'K', 3, 4, ..] | [b'P', b'K', 5, 6, ..] | [b'P', b'K', 7, 8, ..] => Format::Zip,
        [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, ..] => Format::SevenZ,
        [b'R', b'a', b'r', b'!', 0x1A, 0x07, ..] => Format::Rar,
        [0x1F, 0x8B, ..] => pick(&[".tgz"], Format::TarGz, Format::Gz),
        [0xFD, b'7', b'z', b'X', b'Z', 0, ..] => pick(&[".txz"], Format::TarXz, Format::Xz),
        [b'B', b'Z', b'h', ..] => pick(&[".tbz", ".tbz2"], Format::TarBz2, Format::Bz2),
        [0x28, 0xB5, 0x2F, 0xFD, ..] => pick(&[".tzst"], Format::TarZst, Format::Zst),
        _ if head.get(257..262) == Some(b"ustar") || name.ends_with(".tar") => Format::Tar,
        _ if name.ends_with(".lzma") => Format::Lzma,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_entries_and_subtrees() {
        let sel = vec!["a".to_string(), "b/c.txt".to_string()];
        let only = Some(sel.as_slice());
        assert!(is_selected("a", only));
        assert!(is_selected("a/", only));
        assert!(is_selected("a/x/y", only));
        assert!(is_selected("b/c.txt", only));
        assert!(!is_selected("ab", only));
        assert!(!is_selected("b/c.txt2", only));
        assert!(!is_selected("b", only));
        assert!(is_selected("anything", None));
    }

    #[test]
    fn detects_by_magic_and_name() {
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        let cases: &[(&[u8], &str, Option<Format>)] = &[
            (b"PK\x03\x04", "a.zip", Some(Format::Zip)),
            (b"PK\x05\x06", "empty.jar", Some(Format::Zip)),
            (b"PK\x03\x04", "no_ext", Some(Format::Zip)),
            (b"7z\xBC\xAF\x27\x1C", "a.7z", Some(Format::SevenZ)),
            (b"Rar!\x1A\x07\x01\x00", "a.rar", Some(Format::Rar)),
            (b"Rar!\x1A\x07\x00", "a.part1.rar", Some(Format::Rar)),
            (b"\x1F\x8B\x08", "a.gz", Some(Format::Gz)),
            (b"\x1F\x8B\x08", "a.tar.gz", Some(Format::TarGz)),
            (b"\x1F\x8B\x08", "a.tgz", Some(Format::TarGz)),
            (b"\xFD7zXZ\x00", "a.txz", Some(Format::TarXz)),
            (b"BZh9", "a.bz2", Some(Format::Bz2)),
            (b"BZh9", "a.tar.bz2", Some(Format::TarBz2)),
            (b"\x28\xB5\x2F\xFD", "a.zst", Some(Format::Zst)),
            (&tar, "no_ext", Some(Format::Tar)),
            (b"\x00\x00", "old.tar", Some(Format::Tar)),
            (b"\x5D\x00\x00", "a.lzma", Some(Format::Lzma)),
            (b"hello", "a.txt", None),
            (b"", "empty", None),
        ];
        for (head, name, want) in cases {
            assert_eq!(detect_bytes(head, name), *want, "{name}");
        }
    }
}
