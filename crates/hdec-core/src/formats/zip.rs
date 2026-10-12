use crate::fs::{self as hfs, Extractor, Out, OutFile, Target};
use crate::volume::Volume;
use crate::{CreateOptions, Ctx, Edit, Entry, Error, ExtractOptions, Level, Meter, Result};
use jiff::tz::TimeZone;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use zip::result::ZipError;
use zip::unstable::write::FileOptionsExt;
use zip::write::{FileOptions, SimpleFileOptions};
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
    let mut zip = open(path)?;
    let mut out = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let f = zip.by_index_data(i)?;
        let mut entry = Entry {
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
            link: None,
        };
        if f.is_symlink() {
            // The target is the entry's (tiny) data; encrypted ones stay unknown.
            let (encrypted, small) = (f.encrypted(), f.size() <= 4096);
            let mut target = String::new();
            if !encrypted && small {
                io::Read::read_to_string(&mut io::Read::take(zip.by_index(i)?, 4096), &mut target)?;
            }
            entry.link = Some(target);
        }
        out.push(entry);
    }
    Ok(out)
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
    let Some(dest) = dest else {
        let jobs = items.iter().filter(|i| !i.is_dir).map(|i| (i, None));
        decode_parallel(path, jobs.collect(), all_cores(), ctx, &meter)?;
        return Ok(Vec::new());
    };
    let mut x = Extractor::new(dest, opts.overwrite, ctx)?;
    let mut seen = HashSet::new();
    let distinct = items
        .iter()
        .all(|i| seen.insert(i.name.trim_end_matches('/')));
    let mut jobs = Vec::new();
    for item in &items {
        let result = if !distinct {
            // Same name twice: keep archive order, the parallel path can't.
            extract_entry(&mut zip, item, &mut x, ctx, &mut meter, None)
        } else {
            extract_entry(&mut zip, item, &mut x, ctx, &mut meter, Some(&mut jobs))
        };
        match result {
            Err(Error::UnsafePath(p)) => x.warnings.push(format!("skipped unsafe path: {p}")),
            r => r?,
        }
    }
    decode_parallel(path, jobs, hfs::writer_threads(), ctx, &meter)?;
    x.finish()
}

fn all_cores() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Folders and links are made here; files are written now, or with `defer`
/// only resolved and queued for `decode_parallel`.
fn extract_entry<'i>(
    zip: &mut Archive,
    item: &'i Item,
    x: &mut Extractor,
    ctx: &Ctx,
    meter: &mut Meter,
    defer: Option<&mut Vec<(&'i Item, Option<Target>)>>,
) -> Result<()> {
    if item.is_dir {
        return x.dir(&item.name);
    }
    if item.is_symlink {
        if item.size > 4096 {
            return Err(Error::Corrupt(format!(
                "{}: symlink target too long",
                item.name
            )));
        }
        let mut target = Vec::new();
        read_entry(zip, item, ctx, meter, &mut target)?;
        return x.symlink(&item.name, &String::from_utf8_lossy(&target));
    }
    match defer {
        Some(jobs) => {
            if let Some(t) = x.target(&item.name)? {
                jobs.push((item, Some(t)));
            }
            Ok(())
        }
        None => {
            let Some(mut out) = x.file(&item.name, item.size)? else {
                return Ok(());
            };
            read_entry(zip, item, ctx, meter, &mut out)?;
            x.done(out, item.modified, item.mode)
        }
    }
}

/// Decodes `jobs` on `threads` threads, each with its own handle on the
/// archive, writing each file straight to its target (`None` = test only).
fn decode_parallel(
    path: &Path,
    jobs: Vec<(&Item, Option<Target>)>,
    threads: usize,
    ctx: &Ctx,
    meter: &Meter,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let error = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len()) {
            s.spawn(|| {
                let run = || -> Result<()> {
                    let mut zip = open(path)?;
                    let mut meter = meter.fork();
                    while let Some((item, target)) = jobs.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        if error.lock().unwrap().is_some() {
                            break;
                        }
                        ctx.check()?;
                        match target {
                            None => read_entry(&mut zip, item, ctx, &mut meter, &mut io::sink())?,
                            Some(t) => {
                                let mut out = Out::new(t, item.size)?;
                                read_entry(&mut zip, item, ctx, &mut meter, &mut out)?;
                                out.finish(item.modified, item.mode)?;
                            }
                        }
                    }
                    Ok(())
                };
                if let Err(e) = run() {
                    error.lock().unwrap().get_or_insert(e);
                }
            });
        }
    });
    error.into_inner().unwrap().map_or(Ok(()), Err)
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

/// Files up to this size are compressed whole on worker threads (each into a
/// one-entry archive in memory, then raw-copied in order); bigger ones are
/// split across all cores by `deflate_big`. Bounds memory: 16 per thread wait.
const PARALLEL_MAX: u64 = 1 << 20;

fn add_items(
    w: &mut Writer,
    items: &[hfs::Source],
    prefix: &str,
    opts: &CreateOptions,
    ctx: &Ctx,
) -> Result<()> {
    let meter = Meter::new(
        ctx,
        items
            .iter()
            .filter(|s| s.meta.is_file())
            .map(|s| s.meta.len())
            .sum(),
    );
    let deflate_level = match opts.level {
        Level::Store => None,
        Level::Fast => Some(1),
        Level::Normal => Some(6),
        Level::Max => Some(9),
    };
    let base = match deflate_level {
        None => SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        Some(n) => SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(n.into())),
    };
    let entry_options =
        |item: &hfs::Source| entry_options(base, item, ctx.password, opts.zip_crypto);
    // ZipCrypto entries don't survive `raw_copy_file` (zip 9.0.1): legacy, so serial.
    let parallel = |item: &hfs::Source| {
        !opts.zip_crypto && item.meta.is_file() && item.meta.len() <= PARALLEL_MAX
    };
    let compress = |i: usize| -> Result<Option<Vec<u8>>> {
        let item = &items[i];
        if !parallel(item) {
            return Ok(None);
        }
        ctx.check()?;
        let name = format!("{prefix}{}", item.name);
        let mut one = ZipWriter::new(io::Cursor::new(Vec::with_capacity(
            item.meta.len() as usize / 2 + 256,
        )));
        one.start_file(&name, entry_options(item)?)?;
        let mut file = File::open(&item.path)?;
        meter.fork().copy(&name, &mut file, &mut one)?;
        Ok(Some(one.finish()?.into_inner()))
    };
    let threads = all_cores();
    let mut own = meter.fork();
    let write = |i: usize, packed: Option<Vec<u8>>| -> Result<()> {
        let item = &items[i];
        let name = format!("{prefix}{}", item.name);
        if let Some(bytes) = packed {
            let mut one = ZipArchive::new(io::Cursor::new(bytes))?;
            return Ok(w.raw_copy_file(one.by_index_raw(0)?)?);
        }
        let o = entry_options(item)?;
        if item.meta.is_dir() {
            w.add_directory(&name, o)?;
        } else if item.meta.is_symlink() {
            let target = std::fs::read_link(&item.path)?;
            w.add_symlink(&name, target.to_string_lossy(), o)?;
        } else if let (Some(level), None) = (deflate_level, ctx.password) {
            let one = deflate_big(item, &name, level, &mut own)?;
            w.raw_copy_file(ZipArchive::new(one)?.by_index_raw(0)?)?;
        } else {
            w.start_file(&name, o)?;
            own.copy(&name, &mut File::open(&item.path)?, w)?;
        }
        Ok(())
    };
    crate::ordered_parallel(items.len(), threads, 16 * threads, compress, write)
}

/// Deflates a big file on all cores into a one-entry ZIP64 archive in a temp
/// file, ready for `raw_copy_file` (the zip crate can't take pre-compressed data).
fn deflate_big(item: &hfs::Source, name: &str, level: u32, meter: &mut Meter) -> Result<File> {
    let dos = item
        .meta
        .modified()
        .ok()
        .and_then(to_dos)
        .unwrap_or_default();
    let (made_by, external): (u16, u32) = match hfs::mode(&item.meta) {
        Some(mode) => ((3 << 8) | 45, (0o100000 | mode) << 16),
        None => (45, 0),
    };
    // Version 4.5 (ZIP64), UTF-8 name, deflate.
    let common = |crc: u32| {
        let mut h = Vec::new();
        for v in [45u16, 0x0800, 8, dos.timepart(), dos.datepart()] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(&crc.to_le_bytes());
        h.extend_from_slice(&[0xFF; 8]); // sizes: see the ZIP64 field
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&20u16.to_le_bytes());
        h
    };
    let zip64 = |size: u64, packed: u64| {
        let mut e = vec![1, 0, 16, 0];
        e.extend_from_slice(&size.to_le_bytes());
        e.extend_from_slice(&packed.to_le_bytes());
        e
    };
    let local = |crc, size, packed| {
        let mut h = 0x04034b50u32.to_le_bytes().to_vec();
        h.extend(common(crc));
        h.extend_from_slice(name.as_bytes());
        h.extend(zip64(size, packed));
        h
    };
    let mut tmp = tempfile::tempfile()?;
    let header_len = local(0, 0, 0).len() as u64;
    tmp.write_all(&local(0, 0, 0))?;
    let mut enc = super::stream::ParEncoder::deflate(BufWriter::new(&tmp), level);
    meter.copy(name, &mut File::open(&item.path)?, &mut enc)?;
    let (out, crc, size) = enc.finish()?;
    out.into_inner().map_err(|e| e.into_error())?;
    let packed = tmp.seek(SeekFrom::End(0))? - header_len;
    tmp.seek(SeekFrom::Start(0))?;
    tmp.write_all(&local(crc, size, packed))?;
    tmp.seek(SeekFrom::End(0))?;

    let mut tail = 0x02014b50u32.to_le_bytes().to_vec();
    tail.extend_from_slice(&made_by.to_le_bytes());
    tail.extend(common(crc));
    tail.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
    tail.extend_from_slice(&external.to_le_bytes());
    tail.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    tail.extend_from_slice(name.as_bytes());
    tail.extend(zip64(size, packed));
    let (cd_offset, cd_size) = (header_len + packed, tail.len() as u64);
    let eocd64 = cd_offset + cd_size;
    tail.extend_from_slice(&0x06064b50u32.to_le_bytes());
    tail.extend_from_slice(&44u64.to_le_bytes());
    for v in [made_by, 45] {
        tail.extend_from_slice(&v.to_le_bytes());
    }
    tail.extend_from_slice(&[0; 8]); // disk numbers
    for v in [1, 1, cd_size, cd_offset] {
        tail.extend_from_slice(&u64::to_le_bytes(v));
    }
    tail.extend_from_slice(&0x07064b50u32.to_le_bytes());
    tail.extend_from_slice(&0u32.to_le_bytes());
    tail.extend_from_slice(&eocd64.to_le_bytes());
    tail.extend_from_slice(&1u32.to_le_bytes());
    tail.extend_from_slice(&0x06054b50u32.to_le_bytes());
    tail.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
    tail.extend_from_slice(&[0xFF; 8]); // CD size and offset: see ZIP64
    tail.extend_from_slice(&[0, 0]);
    tmp.write_all(&tail)?;
    tmp.seek(SeekFrom::Start(0))?;
    Ok(tmp)
}

fn entry_options<'k>(
    base: SimpleFileOptions,
    item: &hfs::Source,
    password: Option<&'k str>,
    zip_crypto: bool,
) -> Result<FileOptions<'k, 'static, ()>> {
    let mut o: FileOptions<'k, 'static, ()> = base.last_modified_time(
        item.meta
            .modified()
            .ok()
            .and_then(to_dos)
            .unwrap_or_default(),
    );
    if let Some(mode) = hfs::mode(&item.meta) {
        o = o.unix_permissions(mode);
    }
    if let Some(password) = password {
        o = if zip_crypto {
            o.with_deprecated_encryption(password.as_bytes())?
        } else {
            o.with_aes_encryption(AesMode::Aes256, password)
        };
    }
    Ok(o.large_file(item.meta.len() >= u32::MAX as u64))
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
            Edit::Rename(pairs) => pairs.iter().find_map(|(from, to)| renamed(&name, from, to)),
            _ => None,
        };
        let f = src.by_index_raw(i)?;
        match new_name {
            Some(n) => w.raw_copy_file_rename(f, n)?,
            None => w.raw_copy_file(f)?,
        }
    }
    add_items(&mut w, &added, &prefix, &CreateOptions::default(), ctx)?;
    if let Edit::CreateDir(dir) = edit {
        w.add_directory(dir.as_str(), SimpleFileOptions::default())?;
    }
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
