use crate::fs::{self as hfs, Extractor, Out, OutFile, Target};
use crate::volume::Volume;
use crate::{CreateOptions, Ctx, Edit, Entry, Error, ExtractOptions, Level, Meter, Result};
use jiff::tz::TimeZone;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
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
    let mut x = Extractor::new(dest, opts, ctx)?;
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
                                let Some(mut out) = Out::new(t, item.size)? else {
                                    continue;
                                };
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

/// One unit of work for `add_items`.
enum Job {
    /// A whole item: compressed by a worker if small, else written by the consumer.
    Item(usize),
    /// 1 MiB of a big file, deflated by a worker (pigz-style, see `deflate_chunk`).
    Chunk {
        item: usize,
        offset: u64,
        last: bool,
    },
}

enum Packed {
    Whole(Vec<u8>),
    Chunk(Vec<u8>, crc32fast::Hasher, u64),
    Later,
}

const CHUNK: u64 = 1 << 20;

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
    let whole = |item: &hfs::Source| {
        !opts.zip_crypto && item.meta.is_file() && item.meta.len() <= PARALLEL_MAX
    };
    // Big unencrypted files are cut into chunks that join into one deflate stream.
    let chunked = |item: &hfs::Source| {
        deflate_level.is_some()
            && ctx.password.is_none()
            && item.meta.is_file()
            && item.meta.len() > PARALLEL_MAX
    };
    let mut jobs = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        if chunked(item) {
            let len = item.meta.len();
            jobs.extend((0..len.div_ceil(CHUNK)).map(|k| Job::Chunk {
                item: i,
                offset: k * CHUNK,
                last: (k + 1) * CHUNK >= len,
            }));
        } else {
            jobs.push(Job::Item(i));
        }
    }
    let work = |j: usize| -> Result<Packed> {
        ctx.check()?;
        match jobs[j] {
            Job::Item(i) if whole(&items[i]) => {
                let item = &items[i];
                let name = format!("{prefix}{}", item.name);
                let mut one = ZipWriter::new(io::Cursor::new(Vec::with_capacity(
                    item.meta.len() as usize / 2 + 256,
                )));
                one.start_file(&name, entry_options(item)?)?;
                // Small: one read, no copy buffer.
                let data = std::fs::read(&item.path)?;
                one.write_all(&data)?;
                meter.advance(data.len(), &name);
                Ok(Packed::Whole(one.finish()?.into_inner()))
            }
            Job::Item(_) => Ok(Packed::Later),
            Job::Chunk { item, offset, last } => {
                let item = &items[item];
                let dict_start = offset.saturating_sub(32 << 10);
                let mut file = File::open(&item.path)?;
                file.seek(SeekFrom::Start(dict_start))?;
                let mut data = Vec::new();
                file.take(offset - dict_start + CHUNK)
                    .read_to_end(&mut data)?;
                let split = ((offset - dict_start) as usize).min(data.len());
                let (dict, data) = (data[..split].to_vec(), data[split..].to_vec());
                let mut crc = crc32fast::Hasher::new();
                crc.update(&data);
                let len = data.len() as u64;
                let job = super::stream::ChunkJob {
                    id: 0,
                    data,
                    dict,
                    last,
                };
                let packed = super::stream::deflate_chunk(&job, deflate_level.unwrap_or(6))?;
                meter.advance(len as usize, &item.name);
                Ok(Packed::Chunk(packed, crc, len))
            }
        }
    };
    let threads = all_cores();
    let mut own = meter.fork();
    let mut big: Option<BigEntry> = None;
    let write = |j: usize, packed: Packed| -> Result<()> {
        let (i, last) = match jobs[j] {
            Job::Item(i) => (i, true),
            Job::Chunk { item, last, .. } => (item, last),
        };
        let item = &items[i];
        let name = format!("{prefix}{}", item.name);
        match packed {
            Packed::Whole(bytes) => {
                let mut one = ZipArchive::new(io::Cursor::new(bytes))?;
                w.raw_copy_file(one.by_index_raw(0)?)?;
            }
            Packed::Chunk(data, crc, len) => {
                let entry = match &mut big {
                    Some(entry) => entry,
                    None => big.insert(BigEntry::start(item, &name)?),
                };
                entry.push(&data, &crc, len)?;
                if last {
                    let one = big.take().expect("started above").finish()?;
                    w.raw_copy_file(ZipArchive::new(one)?.by_index_raw(0)?)?;
                }
            }
            Packed::Later => {
                let o = entry_options(item)?;
                if item.meta.is_dir() {
                    w.add_directory(&name, o)?;
                } else if item.meta.is_symlink() {
                    let target = std::fs::read_link(&item.path)?;
                    w.add_symlink(&name, target.to_string_lossy(), o)?;
                } else {
                    w.start_file(&name, o)?;
                    own.copy(&name, &mut File::open(&item.path)?, w)?;
                }
            }
        }
        Ok(())
    };
    crate::ordered_parallel(jobs.len(), threads, 16 * threads, work, write)
}

/// A big file's deflate stream, wrapped as a one-entry ZIP64 archive in a temp
/// file for `raw_copy_file` (the zip crate can't take pre-compressed data).
struct BigEntry {
    tmp: File,
    name: Vec<u8>,
    dos: zip::DateTime,
    made_by: u16,
    external: u32,
    header_len: u64,
    crc: crc32fast::Hasher,
    size: u64,
    packed: u64,
}

impl BigEntry {
    fn start(item: &hfs::Source, name: &str) -> Result<Self> {
        let (made_by, external) = match hfs::mode(&item.meta) {
            Some(mode) => ((3 << 8) | 45, (0o100000 | mode) << 16),
            None => (45, 0),
        };
        let mut entry = Self {
            tmp: tempfile::tempfile()?,
            name: name.as_bytes().to_vec(),
            dos: item
                .meta
                .modified()
                .ok()
                .and_then(to_dos)
                .unwrap_or_default(),
            made_by,
            external,
            header_len: 0,
            crc: crc32fast::Hasher::new(),
            size: 0,
            packed: 0,
        };
        let header = entry.local_header(0);
        entry.header_len = header.len() as u64;
        entry.tmp.write_all(&header)?;
        Ok(entry)
    }

    fn push(&mut self, packed: &[u8], crc: &crc32fast::Hasher, len: u64) -> Result<()> {
        self.tmp.write_all(packed)?;
        self.crc.combine(crc);
        self.size += len;
        self.packed += packed.len() as u64;
        Ok(())
    }

    /// Version 4.5 (ZIP64), UTF-8 name, deflate; sizes live in the ZIP64 field.
    fn common(&self, crc: u32) -> Vec<u8> {
        let mut h = Vec::new();
        for v in [45u16, 0x0800, 8, self.dos.timepart(), self.dos.datepart()] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.extend_from_slice(&crc.to_le_bytes());
        h.extend_from_slice(&[0xFF; 8]);
        h.extend_from_slice(&(self.name.len() as u16).to_le_bytes());
        h.extend_from_slice(&20u16.to_le_bytes());
        h
    }

    fn zip64(&self) -> Vec<u8> {
        let mut e = vec![1, 0, 16, 0];
        e.extend_from_slice(&self.size.to_le_bytes());
        e.extend_from_slice(&self.packed.to_le_bytes());
        e
    }

    fn local_header(&self, crc: u32) -> Vec<u8> {
        let mut h = 0x04034b50u32.to_le_bytes().to_vec();
        h.extend(self.common(crc));
        h.extend_from_slice(&self.name);
        h.extend(self.zip64());
        h
    }

    fn finish(mut self) -> Result<File> {
        let crc = self.crc.clone().finalize();
        self.tmp.seek(SeekFrom::Start(0))?;
        self.tmp.write_all(&self.local_header(crc))?;
        self.tmp.seek(SeekFrom::End(0))?;
        let mut tail = 0x02014b50u32.to_le_bytes().to_vec();
        tail.extend_from_slice(&self.made_by.to_le_bytes());
        tail.extend(self.common(crc));
        tail.extend_from_slice(&[0; 6]); // comment length, disk, internal attributes
        tail.extend_from_slice(&self.external.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        tail.extend_from_slice(&self.name);
        tail.extend(self.zip64());
        let (cd_offset, cd_size) = (self.header_len + self.packed, tail.len() as u64);
        tail.extend_from_slice(&0x06064b50u32.to_le_bytes());
        tail.extend_from_slice(&44u64.to_le_bytes());
        for v in [self.made_by, 45] {
            tail.extend_from_slice(&v.to_le_bytes());
        }
        tail.extend_from_slice(&[0; 8]); // disk numbers
        for v in [1, 1, cd_size, cd_offset] {
            tail.extend_from_slice(&u64::to_le_bytes(v));
        }
        tail.extend_from_slice(&0x07064b50u32.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        tail.extend_from_slice(&(cd_offset + cd_size).to_le_bytes());
        tail.extend_from_slice(&1u32.to_le_bytes());
        tail.extend_from_slice(&0x06054b50u32.to_le_bytes());
        tail.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        tail.extend_from_slice(&[0xFF; 8]); // CD size and offset: see ZIP64
        tail.extend_from_slice(&[0, 0]);
        self.tmp.write_all(&tail)?;
        self.tmp.seek(SeekFrom::Start(0))?;
        Ok(self.tmp)
    }
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

/// Any ZipCrypto (not AES) entry? `raw_copy_file` drops their encryption flag
/// (zip 9.0.1), so `modify` can't copy them.
pub fn has_zip_crypto(path: &Path) -> Result<bool> {
    use zip::read::HasZipMetadata;
    let mut zip = open(path)?;
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i)?;
        if f.encrypted() && f.get_metadata().aes_settings().is_none() {
            return Ok(true);
        }
    }
    Ok(false)
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
