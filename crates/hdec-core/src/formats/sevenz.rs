//! 7z via sevenz-rust2.

use crate::fs::{self as hfs, OutFile, Source};
use crate::{CreateOptions, Ctx, Entry, Error, ExtractOptions, Level, Meter, Result, safe_path};
use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{
    Archive, ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod,
    NtTime, Password, SourceReader,
};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};

/// p7zip / 7-Zip convention: this attribute bit means the high 16 bits hold `st_mode`.
const UNIX_EXTENSION: u32 = 0x8000;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_ARCHIVE: u32 = 0x20;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
/// Solid block size: big enough for a good ratio, small enough that one file
/// doesn't require decoding gigabytes.
const SOLID_BLOCK: u64 = 256 << 20;

impl From<sevenz_rust2::Error> for Error {
    fn from(e: sevenz_rust2::Error) -> Self {
        use sevenz_rust2::Error as E;
        match e {
            E::PasswordRequired => Error::PasswordRequired,
            E::MaybeBadPassword(_) => Error::WrongPassword,
            E::Io(e, _) | E::FileOpen(e, _) => Error::Io(e),
            E::Unsupported(s) => Error::Unsupported(s.into_owned()),
            E::UnsupportedCompressionMethod(m) => Error::Unsupported(format!("7z method {m}")),
            E::ExternalUnsupported => Error::Unsupported("external 7z coder".into()),
            e => Error::Corrupt(e.to_string()),
        }
    }
}

/// With a password, garbage after decryption shows up as corruption.
/// ponytail: a genuinely corrupt encrypted archive is also reported as a wrong password.
fn password_hint(e: Error, ctx: &Ctx) -> Error {
    match e {
        Error::Corrupt(_) if ctx.password.is_some() => Error::WrongPassword,
        Error::Io(e) if ctx.password.is_some() && e.kind() == io::ErrorKind::InvalidData => {
            Error::WrongPassword
        }
        e => e,
    }
}

fn open(path: &Path, ctx: &Ctx) -> Result<ArchiveReader<File>> {
    let password = ctx.password.map_or_else(Password::empty, Password::new);
    ArchiveReader::open(path, password).map_err(|e| password_hint(e.into(), ctx))
}

fn name(f: &ArchiveEntry) -> String {
    f.name.replace('\\', "/").trim_end_matches('/').to_owned()
}

fn encrypted(archive: &Archive, index: usize) -> bool {
    archive.stream_map.file_block_index[index].is_some_and(|b| {
        archive.blocks[b]
            .coders
            .iter()
            .any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
    })
}

fn unix_mode(f: &ArchiveEntry) -> Option<u32> {
    (f.has_windows_attributes && f.windows_attributes & UNIX_EXTENSION != 0)
        .then_some(f.windows_attributes >> 16)
}

pub fn list(path: &Path, ctx: &Ctx) -> Result<Vec<Entry>> {
    let reader = open(path, ctx)?;
    let archive = reader.archive();
    Ok(archive
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.is_anti_item)
        .map(|(i, f)| Entry {
            path: name(f),
            size: f.size,
            packed: Some(f.compressed_size).filter(|&s| s > 0),
            modified: f
                .has_last_modified_date
                .then(|| f.last_modified_date.into()),
            is_dir: f.is_directory,
            encrypted: encrypted(archive, i),
        })
        .collect())
}

/// `dest = None` → test only.
pub fn extract(
    path: &Path,
    dest: Option<&Path>,
    opts: &ExtractOptions,
    ctx: &Ctx,
) -> Result<Vec<String>> {
    let mut reader = open(path, ctx)?;
    let only = opts.only.as_deref();
    let total = reader
        .archive()
        .files
        .iter()
        .filter(|f| crate::is_selected(&name(f), only))
        .map(|f| f.size)
        .sum();
    let mut meter = Meter::new(ctx, total);
    let mut warnings = Vec::new();
    let mut failure = None;
    reader
        .for_each_entries(|f, mut r| {
            let name = name(f);
            let result = if f.is_anti_item || !crate::is_selected(&name, only) {
                Ok(())
            } else if let Some(dest) = dest {
                extract_entry(f, &name, &mut r, dest, opts, &mut meter, &mut warnings)
            } else if f.is_directory {
                Ok(())
            } else {
                meter.copy_sized(&name, &mut r, &mut io::sink(), f.size)
            };
            let result = match result {
                Err(Error::UnsafePath(p)) => {
                    warnings.push(format!("skipped unsafe path: {p}"));
                    Ok(())
                }
                r => r,
            };
            // Entries share one decoder stream: whatever wasn't read must be skipped over.
            match result.and_then(|()| Ok(io::copy(&mut r, &mut io::sink())?)) {
                Ok(_) => Ok(true),
                Err(e) => {
                    failure = Some(e);
                    Ok(false)
                }
            }
        })
        .map_err(|e| password_hint(e.into(), ctx))?;
    match failure {
        Some(e) => Err(password_hint(e, ctx)),
        None => Ok(warnings),
    }
}

fn extract_entry(
    f: &ArchiveEntry,
    name: &str,
    r: &mut impl Read,
    dest: &Path,
    opts: &ExtractOptions,
    meter: &mut Meter,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if f.is_directory {
        return hfs::make_dirs(dest, &safe_path::join(dest, name)?);
    }
    let Some(out_path) = hfs::output_path(dest, name, opts.overwrite)? else {
        return Ok(());
    };
    let mode = unix_mode(f);
    if mode.is_some_and(|m| m & S_IFMT == S_IFLNK) {
        if f.size > 4096 {
            return Err(Error::Corrupt(format!("{name}: symlink target too long")));
        }
        let mut target = Vec::new();
        meter.copy_sized(name, r, &mut target, f.size)?;
        let target = String::from_utf8_lossy(&target);
        if !hfs::symlink(dest, &out_path, &target)? {
            warnings.push(format!("skipped symlink {name} -> {target}"));
        }
        return Ok(());
    }
    let mut out = OutFile::create(out_path)?;
    meter.copy_sized(name, r, &mut out, f.size)?;
    if f.has_last_modified_date {
        out.file().set_modified(f.last_modified_date.into())?;
    }
    hfs::set_mode(out.file(), mode)?;
    out.commit()
}

fn archive_entry(item: &Source) -> ArchiveEntry {
    let (mut entry, attrs, kind) = if item.meta.is_dir() {
        (
            ArchiveEntry::new_directory(&item.name),
            ATTR_DIRECTORY,
            S_IFDIR,
        )
    } else if item.meta.is_symlink() {
        (ArchiveEntry::new_file(&item.name), ATTR_ARCHIVE, S_IFLNK)
    } else {
        (ArchiveEntry::new_file(&item.name), ATTR_ARCHIVE, S_IFREG)
    };
    if let Some(t) = item
        .meta
        .modified()
        .ok()
        .and_then(|t| NtTime::try_from(t).ok())
    {
        entry.has_last_modified_date = true;
        entry.last_modified_date = t;
    }
    entry.has_windows_attributes = true;
    entry.windows_attributes = match hfs::mode(&item.meta) {
        Some(mode) => attrs | UNIX_EXTENSION | ((kind | mode) << 16),
        None => attrs,
    };
    entry
}

/// Opens the file on first read and closes it at EOF, so a solid block of
/// thousands of files doesn't hold thousands of descriptors.
struct LazyFile<'a> {
    path: &'a Path,
    file: Option<File>,
    finished: bool,
}

impl Read for LazyFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finished {
            return Ok(0);
        }
        let file = match &mut self.file {
            Some(f) => f,
            None => self.file.insert(File::open(self.path)?),
        };
        let n = file.read(buf)?;
        if n == 0 {
            self.finished = true;
            self.file = None;
        }
        Ok(n)
    }
}

pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()> {
    let items = hfs::walk(sources)?;
    let meter = Meter::new(
        ctx,
        items
            .iter()
            .filter(|s| s.meta.is_file())
            .map(|s| s.meta.len())
            .sum(),
    );
    let mut w = ArchiveWriter::new(BufWriter::new(OutFile::create(dest.to_path_buf())?))?;
    let method: EncoderConfiguration = match opts.level {
        Level::Store => EncoderMethod::COPY.into(),
        Level::Fast => Lzma2Options::from_level(1).into(),
        Level::Normal => Lzma2Options::from_level(5).into(),
        Level::Max => Lzma2Options::from_level(9).into(),
    };
    match ctx.password {
        Some(pw) => {
            w.set_content_methods(vec![
                AesEncoderOptions::new(Password::new(pw)).into(),
                method,
            ]);
            w.set_encrypt_header(true);
        }
        None => {
            w.set_content_methods(vec![method]);
        }
    }

    let (mut entries, mut readers, mut block_size) = (Vec::new(), Vec::new(), 0);
    for item in &items {
        ctx.check()?;
        let entry = archive_entry(item);
        if item.meta.is_file() {
            if block_size + item.meta.len() > SOLID_BLOCK && !entries.is_empty() {
                let (e, r) = (std::mem::take(&mut entries), std::mem::take(&mut readers));
                w.push_archive_entries(e, r)
                    .map_err(|e| ctx.or_cancelled(e))?;
                block_size = 0;
            }
            block_size += item.meta.len();
            entries.push(entry);
            let file = LazyFile {
                path: &item.path,
                file: None,
                finished: false,
            };
            readers.push(SourceReader::new(meter.reader(&item.name, file)));
        } else if item.meta.is_symlink() {
            let target = fs::read_link(&item.path)?.to_string_lossy().into_owned();
            w.push_archive_entry(entry, Some(target.as_bytes()))?;
        } else {
            w.push_archive_entry::<&[u8]>(entry, None)?;
        }
    }
    if !entries.is_empty() {
        w.push_archive_entries(entries, readers)
            .map_err(|e| ctx.or_cancelled(e))?;
    }
    let out = w.finish()?.into_inner().map_err(|e| e.into_error())?;
    out.file().sync_all()?;
    out.commit()
}
