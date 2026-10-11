use crate::fs::{self as hfs, OutFile};
use crate::volume::Volume;
use crate::{
    CreateOptions, Ctx, Edit, Entry, Error, ExtractOptions, Level, Meter, Result, safe_path,
};
use jiff::tz::TimeZone;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use zip::result::ZipError;
use zip::unstable::write::FileOptionsExt;
use zip::write::SimpleFileOptions;
use zip::{AesMode, CompressionMethod, ZipArchive, ZipWriter};

type Archive = ZipArchive<BufReader<Volume>>;

impl From<ZipError> for Error {
    fn from(e: ZipError) -> Self {
        match e {
            ZipError::Io(e) => Error::Io(e),
            ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED) => Error::PasswordRequired,
            ZipError::InvalidPassword => Error::WrongPassword,
            ZipError::UnsupportedArchive(s) => Error::Unsupported(s.to_owned()),
            ZipError::CompressionMethodNotSupported(m) => {
                Error::Unsupported(format!("ZIP compression method {m}"))
            }
            e => Error::Corrupt(e.to_string()),
        }
    }
}

fn open(path: &Path) -> Result<Archive> {
    Ok(ZipArchive::new(BufReader::new(Volume::open(path)?))?)
}

pub fn list(path: &Path, _ctx: &Ctx) -> Result<Vec<Entry>> {
    let zip = open(path)?;
    (0..zip.len())
        .map(|i| {
            let f = zip.by_index_data(i)?;
            Ok(Entry {
                path: f
                    .name()?
                    .replace('\\', "/")
                    .trim_end_matches('/')
                    .to_owned(),
                size: f.size(),
                packed: Some(f.compressed_size()),
                modified: f.last_modified().and_then(from_dos),
                is_dir: f.is_dir(),
                encrypted: f.encrypted(),
            })
        })
        .collect()
}

struct Item {
    index: usize,
    name: String,
    size: u64,
    is_dir: bool,
    is_symlink: bool,
    encrypted: bool,
    modified: Option<SystemTime>,
    mode: Option<u32>,
}

/// `dest = None` → test only: decode and verify CRCs, write nothing.
pub fn extract(
    path: &Path,
    dest: Option<&Path>,
    opts: &ExtractOptions,
    ctx: &Ctx,
) -> Result<Vec<String>> {
    let mut zip = open(path)?;
    let mut items = Vec::new();
    for index in 0..zip.len() {
        let f = zip.by_index_data(index)?;
        let name = f.name()?.replace('\\', "/");
        if crate::is_selected(&name, opts.only.as_deref()) {
            items.push(Item {
                index,
                name,
                size: f.size(),
                is_dir: f.is_dir(),
                is_symlink: f.is_symlink(),
                encrypted: f.encrypted(),
                modified: f.last_modified().and_then(from_dos),
                mode: f.unix_mode(),
            });
        }
    }
    let mut meter = Meter::new(ctx, items.iter().map(|i| i.size).sum());
    let mut warnings = Vec::new();
    for item in &items {
        let result = match dest {
            None if item.is_dir => Ok(()),
            None => read_entry(&mut zip, item, ctx, &mut meter, &mut io::sink()),
            Some(dest) => extract_entry(&mut zip, item, dest, opts, ctx, &mut meter, &mut warnings),
        };
        match result {
            Err(Error::UnsafePath(p)) => warnings.push(format!("skipped unsafe path: {p}")),
            r => r?,
        }
    }
    Ok(warnings)
}

fn extract_entry(
    zip: &mut Archive,
    item: &Item,
    dest: &Path,
    opts: &ExtractOptions,
    ctx: &Ctx,
    meter: &mut Meter,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if item.is_dir {
        return hfs::make_dirs(dest, &safe_path::join(dest, &item.name)?);
    }
    let Some(out_path) = hfs::output_path(dest, &item.name, opts.overwrite)? else {
        return Ok(());
    };
    if item.is_symlink {
        if item.size > 4096 {
            return Err(Error::Corrupt(format!(
                "{}: symlink target too long",
                item.name
            )));
        }
        let mut target = Vec::new();
        read_entry(zip, item, ctx, meter, &mut target)?;
        let target = String::from_utf8_lossy(&target);
        if !hfs::symlink(dest, &out_path, &target)? {
            warnings.push(format!("skipped symlink {} -> {target}", item.name));
        }
        return Ok(());
    }
    let mut out = OutFile::create(out_path)?;
    read_entry(zip, item, ctx, meter, &mut out)?;
    if let Some(t) = item.modified {
        out.file().set_modified(t)?;
    }
    hfs::set_mode(out.file(), item.mode)?;
    out.commit()
}

fn read_entry(
    zip: &mut Archive,
    item: &Item,
    ctx: &Ctx,
    meter: &mut Meter,
    w: &mut impl Write,
) -> Result<()> {
    let mut f = if item.encrypted {
        let password = ctx.password.ok_or(Error::PasswordRequired)?;
        zip.by_index_decrypt(item.index, password.as_bytes())?
    } else {
        zip.by_index(item.index)?
    };
    match meter.copy_sized(&item.name, &mut f, w, item.size) {
        // ZipCrypto's header check passes 1 in 256 wrong passwords; the CRC catches the rest.
        // ponytail: a corrupt encrypted entry is also reported as a wrong password.
        Err(Error::Corrupt(_)) if item.encrypted => Err(Error::WrongPassword),
        Err(Error::Io(e)) if item.encrypted && e.kind() == io::ErrorKind::InvalidData => {
            Err(Error::WrongPassword)
        }
        r => r,
    }
}

type Writer = ZipWriter<BufWriter<OutFile>>;

pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()> {
    let items = hfs::walk(sources)?;
    let mut w = ZipWriter::new(BufWriter::new(OutFile::create(dest.to_path_buf())?));
    add_items(&mut w, &items, "", opts, ctx)?;
    finish(w)
}

fn finish(w: Writer) -> Result<()> {
    let out = w.finish()?.into_inner().map_err(|e| e.into_error())?;
    out.file().sync_all()?;
    out.commit()
}

fn add_items(
    w: &mut Writer,
    items: &[hfs::Source],
    prefix: &str,
    opts: &CreateOptions,
    ctx: &Ctx,
) -> Result<()> {
    let mut meter = Meter::new(
        ctx,
        items
            .iter()
            .filter(|s| s.meta.is_file())
            .map(|s| s.meta.len())
            .sum(),
    );
    let base = match opts.level {
        Level::Store => SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        level => SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(match level {
                Level::Fast => 1,
                Level::Normal => 6,
                _ => 9,
            })),
    };
    for item in items {
        ctx.check()?;
        let name = format!("{prefix}{}", item.name);
        let mut o = base.last_modified_time(
            item.meta
                .modified()
                .ok()
                .and_then(to_dos)
                .unwrap_or_default(),
        );
        if let Some(mode) = hfs::mode(&item.meta) {
            o = o.unix_permissions(mode);
        }
        if let Some(password) = ctx.password {
            o = if opts.zip_crypto {
                o.with_deprecated_encryption(password.as_bytes())?
            } else {
                o.with_aes_encryption(AesMode::Aes256, password)
            };
        }
        if item.meta.is_dir() {
            w.add_directory(&name, o)?;
        } else if item.meta.is_symlink() {
            let target = std::fs::read_link(&item.path)?;
            w.add_symlink(&name, target.to_string_lossy(), o)?;
        } else {
            w.start_file(&name, o.large_file(item.meta.len() >= u32::MAX as u64))?;
            meter.copy(&name, &mut File::open(&item.path)?, w)?;
        }
    }
    Ok(())
}

/// Copies unchanged entries raw (no recompression, encryption kept), then adds new ones.
pub fn modify(path: &Path, edit: &Edit, ctx: &Ctx) -> Result<()> {
    let mut src = open(path)?;
    let mut w = ZipWriter::new(BufWriter::new(OutFile::create(path.to_path_buf())?));
    let (added, prefix) = match edit {
        Edit::Add { sources, into } if into.is_empty() => (hfs::walk(sources)?, String::new()),
        Edit::Add { sources, into } => (hfs::walk(sources)?, format!("{into}/")),
        _ => (Vec::new(), String::new()),
    };
    let replaced: HashSet<String> = added
        .iter()
        .map(|s| format!("{prefix}{}", s.name))
        .collect();
    for i in 0..src.len() {
        ctx.check()?;
        let name = src.by_index_data(i)?.name()?.replace('\\', "/");
        let bare = name.trim_end_matches('/');
        let new_name = match edit {
            Edit::Delete(names) if crate::is_selected(bare, Some(names)) => continue,
            Edit::Add { .. } if replaced.contains(bare) => continue,
            Edit::Rename { from, to } => renamed(&name, from, to),
            _ => None,
        };
        let f = src.by_index_raw(i)?;
        match new_name {
            Some(n) => w.raw_copy_file_rename(f, n)?,
            None => w.raw_copy_file(f)?,
        }
    }
    add_items(&mut w, &added, &prefix, &CreateOptions::default(), ctx)?;
    drop(src); // Windows can't replace a file that is still open
    finish(w)
}

/// `renamed("a/b/c", "a/b", "x")` → `Some("x/c")`.
fn renamed(name: &str, from: &str, to: &str) -> Option<String> {
    let rest = name.strip_prefix(from)?;
    (rest.is_empty() || rest.starts_with('/')).then(|| format!("{to}{rest}"))
}

/// ZIP stores local wall-clock time without a zone.
fn to_dos(t: SystemTime) -> Option<zip::DateTime> {
    let local = jiff::Timestamp::try_from(t)
        .ok()?
        .to_zoned(TimeZone::system())
        .datetime();
    zip::DateTime::try_from(local).ok()
}

fn from_dos(d: zip::DateTime) -> Option<SystemTime> {
    let local = jiff::civil::DateTime::try_from(d).ok()?;
    Some(local.to_zoned(TimeZone::system()).ok()?.timestamp().into())
}
