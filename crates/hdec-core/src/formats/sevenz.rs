//! 7z via sevenz-rust2.

use crate::fs::{self as hfs, Extractor, OutFile, Source};
use crate::sfx::{self, Skip};
use crate::volume::Volume;
use crate::{CreateOptions, Ctx, Entry, Error, ExtractOptions, Level, Meter, Result};
use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
use sevenz_rust2::{
    Archive, ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod,
    NtTime, Password, SourceReader,
};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// p7zip / 7-Zip convention: this attribute bit means the high 16 bits hold `st_mode`.
const UNIX_EXTENSION: u32 = 0x8000;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_ARCHIVE: u32 = 0x20;
const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
/// Smallest solid block: big enough for a good ratio, small enough that one
/// file doesn't require decoding gigabytes.
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

type Reader = ArchiveReader<Skip<BufReader<Volume>>>;

fn open(path: &Path, ctx: &Ctx) -> Result<Reader> {
    let password = ctx.password.map_or_else(Password::empty, Password::new);
    let mut inner = BufReader::new(Volume::open(path)?);
    let mut sig = [0; 2];
    inner.read_exact(&mut sig)?;
    let base = match sig {
        [b'M', b'Z'] => sfx::find(&mut inner)?.map_or(0, |(_, offset)| offset),
        _ => 0,
    };
    let mut src = Skip { inner, base };
    src.seek(SeekFrom::Start(0))?;
    ArchiveReader::new(src, password).map_err(|e| password_hint(e.into(), ctx))
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
            link: unix_mode(f)
                .filter(|m| m & S_IFMT == S_IFLNK)
                .map(|_| String::new()),
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
    let mut x = dest
        .map(|d| Extractor::new(d, opts.overwrite, ctx))
        .transpose()?;
    let mut failure = None;
    reader
        .for_each_entries(|f, mut r| {
            let name = name(f);
            let result = if f.is_anti_item || !crate::is_selected(&name, only) {
                Ok(())
            } else if let Some(x) = &mut x {
                match extract_entry(f, &name, &mut r, x, &mut meter) {
                    Err(Error::UnsafePath(p)) => {
                        x.warnings.push(format!("skipped unsafe path: {p}"));
                        Ok(())
                    }
                    r => r,
                }
            } else if f.is_directory {
                Ok(())
            } else {
                meter.copy_sized(&name, &mut r, &mut io::sink(), f.size)
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
    if let Some(e) = failure {
        return Err(password_hint(e, ctx));
    }
    x.map_or(Ok(Vec::new()), Extractor::finish)
}

fn extract_entry(
    f: &ArchiveEntry,
    name: &str,
    r: &mut impl Read,
    x: &mut Extractor,
    meter: &mut Meter,
) -> Result<()> {
    if f.is_directory {
        return x.dir(name);
    }
    let mode = unix_mode(f);
    if mode.is_some_and(|m| m & S_IFMT == S_IFLNK) {
        if f.size > 4096 {
            return Err(Error::Corrupt(format!("{name}: symlink target too long")));
        }
        let mut target = Vec::new();
        meter.copy_sized(name, r, &mut target, f.size)?;
        return x.symlink(name, &String::from_utf8_lossy(&target));
    }
    let Some(mut out) = x.file(name, f.size)? else {
        return Ok(());
    };
    meter.copy_sized(name, r, &mut out, f.size)?;
    let modified = f
        .has_last_modified_date
        .then(|| f.last_modified_date.into());
    x.done(out, modified, mode)
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
    // LZMA2 in independent chunks of 3x the dictionary, compressed on all
    // cores (memory permitting) and decodable in parallel too.
    let (method, solid_block): (EncoderConfiguration, u64) = match opts.level {
        Level::Store => (EncoderMethod::COPY.into(), SOLID_BLOCK),
        level => {
            let preset = match level {
                Level::Fast => 1,
                Level::Normal => 5,
                _ => 9,
            };
            let dict = lzma_rust2::Lzma2Options::with_preset(preset)
                .lzma_options
                .dict_size;
            let workers = super::stream::lzma_workers(dict);
            let chunk = 3 * u64::from(dict);
            let block = SOLID_BLOCK.max(chunk * u64::from(workers));
            (
                Lzma2Options::from_level_mt(preset, workers, chunk).into(),
                block,
            )
        }
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
            if block_size + item.meta.len() > solid_block && !entries.is_empty() {
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
