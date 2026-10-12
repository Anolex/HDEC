//! Compression filters (gz, bz2, xz, zst, lzma) and single-file compressed streams.

use crate::fs::{Extractor, OutFile};
use crate::volume::Volume;
use crate::{
    CountingRead, CreateOptions, Ctx, Entry, Error, ExtractOptions, Format, Level, Meter, Result,
};
use bzip2::read::MultiBzDecoder;
use bzip2::write::BzEncoder;
use flate2::read::MultiGzDecoder;
use lzma_rust2::{LzipReader, LzmaReader, XzOptions, XzReader, XzReaderMt, XzWriter, XzWriterMt};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, mpsc};

/// Refuse LZMA/XZ streams that need more than this much dictionary memory.
const MEM_LIMIT_KB: u32 = 2 << 20; // 2 GiB

pub(crate) fn decoder<'a>(
    format: Format,
    mut r: impl BufRead + Seek + Send + 'a,
) -> Result<Box<dyn Read + Send + 'a>> {
    Ok(match format {
        Format::Tar => Box::new(r),
        Format::Gz | Format::TarGz => Box::new(MultiGzDecoder::new(r)),
        Format::Bz2 | Format::TarBz2 => Box::new(MultiBzDecoder::new(r)),
        Format::Xz | Format::TarXz => match xz_workers(&mut r)? {
            Some(n) => Box::new(XzReaderMt::new(r, true, n)?),
            None => Box::new(XzReader::new_mem_limit(r, true, MEM_LIMIT_KB)),
        },
        Format::Zst | Format::TarZst => Box::new(ZstReader::new(r)?),
        Format::Lzma => Box::new(LzmaReader::new_mem_limit(r, MEM_LIMIT_KB, None)?),
        Format::Lz | Format::TarLz => Box::new(LzipReader::new(r)),
        f => unreachable!("{f:?} is not a stream format"),
    })
}

/// Decodes `input` on other threads while `f` reads the data.
pub(crate) fn decode_ahead<T>(
    format: Format,
    mut input: impl BufRead + Seek + Send,
    f: impl FnOnce(&mut crate::ChunkReader) -> Result<T>,
) -> Result<T> {
    if matches!(format, Format::Bz2 | Format::TarBz2)
        && let Some(segments) = bzip2_streams(&mut input)?
    {
        return crate::feed(move |tx| bzip2_parallel(input, &segments, &tx), f);
    }
    crate::read_ahead(decoder(format, input)?, f)
}

/// A bzip2 stream bigger than this (packed) keeps the file on one thread.
const BZ_STREAM_MAX: u64 = 4 << 20;
/// Decoded output kept per stream; beyond it the stream is decoded again,
/// in order and streaming, so memory stays bounded.
const BZ_OUT_MAX: u64 = if cfg!(test) { 1 << 20 } else { 64 << 20 };

/// Start and end of each stream in a file of several concatenated bzip2
/// streams (ours, pbzip2's), found by their signature: "BZh", the level, and
/// the block magic. `None` = one stream, or streams too big to split.
fn bzip2_streams(r: &mut (impl Read + Seek)) -> io::Result<Option<Vec<(u64, u64)>>> {
    const BLOCK: [u8; 6] = [0x31, 0x41, 0x59, 0x26, 0x53, 0x59];
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let (mut starts, mut data, mut base) = (Vec::new(), Vec::new(), 0u64);
    loop {
        let chunk = crate::read_chunk(r, 1 << 20)?;
        let end = chunk.is_empty();
        data.extend_from_slice(&chunk);
        let scan = if end {
            data.len()
        } else {
            data.len().saturating_sub(9)
        };
        for p in 0..scan {
            let sig = &data[p..data.len().min(p + 10)];
            if sig.len() == 10
                && sig[..3] == *b"BZh"
                && (b'1'..=b'9').contains(&sig[3])
                && sig[4..] == BLOCK
            {
                starts.push(base + p as u64);
            }
        }
        if end {
            break;
        }
        data.drain(..scan);
        base += scan as u64;
    }
    r.seek(SeekFrom::Start(0))?;
    let ends = starts.iter().skip(1).copied().chain([len]);
    let segments: Vec<_> = starts.iter().copied().zip(ends).collect();
    let ok = starts.first() == Some(&0)
        && segments.len() >= 2
        && segments.iter().all(|(a, b)| b - a <= BZ_STREAM_MAX);
    Ok(ok.then_some(segments))
}

fn bzip2_parallel(
    input: impl Read + Seek + Send,
    segments: &[(u64, u64)],
    tx: &crate::ChunkSender,
) {
    let input = Mutex::new(input);
    let packed = |i: usize| -> io::Result<Vec<u8>> {
        let (start, end) = segments[i];
        let mut r = input.lock().unwrap();
        r.seek(SeekFrom::Start(start))?;
        let mut data = vec![0; (end - start) as usize];
        r.read_exact(&mut data)?;
        Ok(data)
    };
    let send = |chunk: Vec<u8>| -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        tx.send(Ok(chunk))
            .map_err(|_| Error::Io(io::Error::other("reader stopped")))
    };
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let result = crate::ordered_parallel(
        segments.len(),
        threads,
        threads,
        |i| {
            let data = packed(i)?;
            let mut out = Vec::new();
            MultiBzDecoder::new(&data[..])
                .take(BZ_OUT_MAX + 1)
                .read_to_end(&mut out)?;
            Ok(if out.len() as u64 > BZ_OUT_MAX {
                Err(data)
            } else {
                Ok(out)
            })
        },
        |_, decoded| match decoded {
            Ok(out) => send(out),
            Err(data) => {
                let mut d = MultiBzDecoder::new(&data[..]);
                loop {
                    let chunk = crate::read_chunk(&mut d, 1 << 20)?;
                    if chunk.is_empty() {
                        return Ok(());
                    }
                    send(chunk)?;
                }
            }
        },
    );
    if let Err(e) = result {
        let _ = tx.send(Err(match e {
            Error::Io(e) => e,
            e => io::Error::other(e.to_string()),
        }));
    }
}

/// Memory a parallel LZMA encoder or decoder may use across all its threads.
pub(crate) const MT_MEM_BUDGET: u64 = 2 << 30;

/// Threads for a parallel LZMA2 encoder with this dictionary: each needs
/// roughly 12x the dictionary for its match finder plus in/out blocks.
pub(crate) fn lzma_workers(dict: u32) -> u32 {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let per_thread = 18 * u64::from(dict);
    cores.min(MT_MEM_BUDGET / per_thread).max(1) as u32
}

/// Threads for decoding an XZ file in parallel, or `None` to decode it on
/// one thread. Parallel decoding buffers whole blocks, sized by the file's own
/// index, so the index and every block header are checked here first: one
/// stream whose blocks exactly fill the file, and whose blocks and
/// dictionaries fit `MT_MEM_BUDGET` for at least two threads.
fn xz_workers(r: &mut (impl Read + Seek)) -> Result<Option<u32>> {
    let plan = xz_block_memory(r);
    r.seek(SeekFrom::Start(0))?;
    let Ok(Some((blocks, per_block))) = plan else {
        return Ok(None);
    };
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u64);
    let n = cores.min(blocks).min(MT_MEM_BUDGET / per_block.max(1));
    Ok((n >= 2).then_some(n as u32))
}

/// `(block count, worst-case memory per block)`; `None` = not a plain multi-block file.
fn xz_block_memory(r: &mut (impl Read + Seek)) -> io::Result<Option<(u64, u64)>> {
    let len = r.seek(SeekFrom::End(0))?;
    if len < 32 {
        return Ok(None);
    }
    let mut footer = [0; 12];
    r.seek(SeekFrom::Start(len - 12))?;
    r.read_exact(&mut footer)?;
    if footer[10..] != *b"YZ" {
        return Ok(None);
    }
    let index_size = (u64::from(u32::from_le_bytes(footer[4..8].try_into().unwrap())) + 1) * 4;
    let Some(index_start) = len.checked_sub(12 + index_size).filter(|&s| s >= 12) else {
        return Ok(None);
    };
    r.seek(SeekFrom::Start(index_start))?;
    let mut index = vec![0; index_size as usize];
    r.read_exact(&mut index)?;
    let mut at = 1; // after the 0x00 indicator
    let count = varint(&index, &mut at);
    let Some(count) = count.filter(|&c| index[0] == 0 && c >= 2 && c <= index_size) else {
        return Ok(None);
    };
    let (mut offset, mut worst) = (12u64, 0u64);
    let mut starts = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (Some(unpadded), Some(size)) = (varint(&index, &mut at), varint(&index, &mut at))
        else {
            return Ok(None);
        };
        starts.push(offset);
        let padded = unpadded.div_ceil(4) * 4;
        offset = offset.saturating_add(padded);
        worst = worst.max(padded.saturating_add(size));
    }
    // Blocks must exactly fill the space between the header and the index.
    if offset != index_start {
        return Ok(None);
    }
    let mut dict = 0;
    for start in starts {
        r.seek(SeekFrom::Start(start))?;
        let mut head = [0; 1024];
        r.read_exact(&mut head[..1])?;
        let size = (usize::from(head[0]) + 1) * 4;
        if head[0] == 0 {
            return Ok(None); // the index indicator, not a block
        }
        r.read_exact(&mut head[1..size])?;
        match lzma2_dict(&head[..size]) {
            Some(d) => dict = dict.max(d),
            None => return Ok(None),
        }
    }
    Ok(Some((count, worst.saturating_add(dict))))
}

/// The LZMA2 dictionary size a block header asks for.
fn lzma2_dict(head: &[u8]) -> Option<u64> {
    let flags = *head.get(1)?;
    let mut at = 2;
    if flags & 0x40 != 0 {
        varint(head, &mut at)?;
    }
    if flags & 0x80 != 0 {
        varint(head, &mut at)?;
    }
    for _ in 0..=(flags & 3) {
        let id = varint(head, &mut at)?;
        let props = varint(head, &mut at)? as usize;
        if id == 0x21 && props == 1 {
            let bits = u32::from(*head.get(at)? & 0x3F);
            return Some(match bits {
                40 => u64::from(u32::MAX),
                0..40 => u64::from(2 | (bits & 1)) << (bits / 2 + 11),
                _ => return None,
            });
        }
        at += props;
    }
    None
}

/// XZ multibyte integer (7 bits per byte, at most 9 bytes).
fn varint(buf: &[u8], at: &mut usize) -> Option<u64> {
    let mut n = 0u64;
    for i in 0..9 {
        let b = *buf.get(*at)?;
        *at += 1;
        n |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some(n);
        }
    }
    None
}

pub(crate) enum Encoder<W: Write> {
    Plain(W),
    Gz(ParEncoder<W>),
    Bz2(ParEncoder<W>),
    Xz(XzWriter<W>),
    XzMt(XzWriterMt<W>),
}

impl<W: Write> Encoder<W> {
    pub fn new(format: Format, w: W, level: Level) -> Result<Self> {
        let n = match level {
            Level::Store => 0,
            Level::Fast => 1,
            Level::Normal => 6,
            Level::Max => 9,
        };
        Ok(match format {
            Format::Tar => Encoder::Plain(w),
            Format::Gz | Format::TarGz => {
                let mut w = w;
                // gzip header: deflate, no name, no mtime, OS unknown (as flate2 writes it).
                w.write_all(&[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 255])?;
                Encoder::Gz(ParEncoder::deflate(w, n))
            }
            Format::Bz2 | Format::TarBz2 => Encoder::Bz2(ParEncoder::bzip2(w, n.max(1))),
            Format::Xz | Format::TarXz => {
                let mut opts = XzOptions::with_preset(n);
                let dict = opts.lzma_options.dict_size;
                match lzma_workers(dict) {
                    1 => Encoder::Xz(XzWriter::new(w, opts)?),
                    workers => {
                        // Independent blocks, 3x the dictionary like `xz -T0`: the
                        // reader can decode them in parallel too.
                        opts.set_block_size(NonZeroU64::new(3 * u64::from(dict)));
                        Encoder::XzMt(XzWriterMt::new(w, opts, workers)?)
                    }
                }
            }
            f => {
                return Err(Error::Unsupported(format!(
                    "creating {} is not supported",
                    f.name()
                )));
            }
        })
    }

    pub fn finish(self) -> Result<W> {
        Ok(match self {
            Encoder::Plain(w) => w,
            Encoder::Gz(e) => {
                let (mut w, crc, len) = e.finish()?;
                w.write_all(&crc.to_le_bytes())?;
                w.write_all(&(len as u32).to_le_bytes())?;
                w
            }
            Encoder::Bz2(e) => e.finish()?.0,
            Encoder::Xz(e) => e.finish()?,
            Encoder::XzMt(e) => e.finish()?,
        })
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Encoder::Plain(w) => w.write(buf),
            Encoder::Gz(w) => w.write(buf),
            Encoder::Bz2(w) => w.write(buf),
            Encoder::Xz(w) => w.write(buf),
            Encoder::XzMt(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Encoder::Plain(w) => w.flush(),
            Encoder::Gz(w) => w.flush(),
            Encoder::Bz2(w) => w.flush(),
            Encoder::Xz(w) => w.flush(),
            Encoder::XzMt(w) => w.flush(),
        }
    }
}

/// Compresses on all cores by cutting the input into chunks compressed
/// independently and written back in order.
///
/// - deflate (pigz-style): each chunk is primed with the previous 32 KiB as
///   dictionary and ends with a sync flush, so the pieces join into one
///   ordinary deflate stream (a few bytes larger per MiB).
/// - bzip2 (pbzip2-style): each chunk is a complete stream; bzip2 blocks are
///   independent anyway, and every reader takes concatenated streams.
pub(crate) struct ParEncoder<W: Write> {
    inner: W,
    chunk_size: usize,
    /// Dictionary carried into the next chunk (0 = none).
    window: usize,
    chunk: Vec<u8>,
    /// The last `window` bytes before `chunk`.
    dict: Vec<u8>,
    crc: crc32fast::Hasher,
    total_in: u64,
    sent: u64,
    written: u64,
    done: std::collections::BTreeMap<u64, Vec<u8>>,
    /// Dropping it (or the whole encoder) stops the threads.
    jobs: Option<mpsc::SyncSender<ChunkJob>>,
    results: mpsc::Receiver<(u64, io::Result<Vec<u8>>)>,
    threads: Vec<std::thread::JoinHandle<()>>,
    max_in_flight: u64,
}

pub(crate) struct ChunkJob {
    pub id: u64,
    pub data: Vec<u8>,
    /// Deflate only: the input just before `data`.
    pub dict: Vec<u8>,
    pub last: bool,
}

type ChunkFn = fn(&ChunkJob, u32) -> io::Result<Vec<u8>>;

impl<W: Write> ParEncoder<W> {
    pub fn deflate(inner: W, level: u32) -> Self {
        Self::new(inner, level, 1 << 20, 32 << 10, deflate_chunk)
    }

    pub fn bzip2(inner: W, level: u32) -> Self {
        Self::new(inner, level, level as usize * 100_000, 0, bzip2_chunk)
    }

    fn new(inner: W, level: u32, chunk_size: usize, window: usize, compress: ChunkFn) -> Self {
        let n = std::thread::available_parallelism().map_or(1, |n| n.get());
        let (jobs, rx) = mpsc::sync_channel::<ChunkJob>(n);
        let (tx, results) = mpsc::channel();
        let rx = Arc::new(Mutex::new(rx));
        let threads = (0..n)
            .map(|_| {
                let (rx, tx) = (rx.clone(), tx.clone());
                std::thread::spawn(move || {
                    loop {
                        // Own statement: the lock must not be held while compressing.
                        let Ok(job) = rx.lock().unwrap().recv() else {
                            return;
                        };
                        let out = compress(&job, level);
                        if tx.send((job.id, out)).is_err() {
                            return;
                        }
                    }
                })
            })
            .collect();
        Self {
            inner,
            chunk_size,
            window,
            chunk: Vec::with_capacity(chunk_size),
            dict: Vec::new(),
            crc: crc32fast::Hasher::new(),
            total_in: 0,
            sent: 0,
            written: 0,
            done: Default::default(),
            jobs: Some(jobs),
            results,
            threads,
            max_in_flight: 2 * n as u64,
        }
    }

    fn dispatch(&mut self, last: bool) -> io::Result<()> {
        let data = std::mem::replace(&mut self.chunk, Vec::with_capacity(self.chunk_size));
        let dict = std::mem::take(&mut self.dict);
        if self.window > 0 {
            let mut next = if data.len() >= self.window {
                Vec::new()
            } else {
                dict.clone()
            };
            next.extend_from_slice(&data[data.len().saturating_sub(self.window)..]);
            self.dict = next[next.len().saturating_sub(self.window)..].to_vec();
        }
        let job = ChunkJob {
            id: self.sent,
            data,
            dict,
            last,
        };
        self.sent += 1;
        let sent = self.jobs.as_ref().is_some_and(|j| j.send(job).is_ok());
        if !sent {
            return Err(io::Error::other("compression threads stopped"));
        }
        self.collect(self.max_in_flight)
    }

    /// Writes finished chunks in order until at most `outstanding` are left.
    fn collect(&mut self, outstanding: u64) -> io::Result<()> {
        loop {
            while let Some(out) = self.done.remove(&self.written) {
                self.inner.write_all(&out)?;
                self.written += 1;
            }
            if self.sent - self.written <= outstanding {
                return Ok(());
            }
            let (id, out) = self
                .results
                .recv()
                .map_err(|_| io::Error::other("compression threads stopped"))?;
            self.done.insert(id, out?);
        }
    }

    /// `(inner, crc32 of the input, input length)`.
    pub fn finish(mut self) -> io::Result<(W, u32, u64)> {
        // Deflate needs a final block; bzip2 needs at least one stream.
        if self.window > 0 || !self.chunk.is_empty() || self.sent == 0 {
            self.dispatch(true)?;
        }
        self.collect(0)?;
        self.jobs = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let crc = std::mem::take(&mut self.crc).finalize();
        Ok((self.inner, crc, self.total_in))
    }
}

fn bzip2_chunk(job: &ChunkJob, level: u32) -> io::Result<Vec<u8>> {
    let mut e = BzEncoder::new(Vec::new(), bzip2::Compression::new(level));
    e.write_all(&job.data)?;
    e.finish()
}

pub(crate) fn deflate_chunk(job: &ChunkJob, level: u32) -> io::Result<Vec<u8>> {
    use flate2::{Compress, Compression, FlushCompress, Status};
    let mut c = Compress::new(Compression::new(level), false);
    if !job.dict.is_empty() {
        c.set_dictionary(&job.dict).map_err(io::Error::other)?;
    }
    let flush = if job.last {
        FlushCompress::Finish
    } else {
        FlushCompress::Sync
    };
    let mut out = Vec::with_capacity(job.data.len() / 2 + 4096);
    loop {
        if out.capacity() - out.len() < 1024 {
            out.reserve(out.capacity());
        }
        let pos = c.total_in() as usize;
        let status = c
            .compress_vec(&job.data[pos..], &mut out, flush)
            .map_err(io::Error::other)?;
        let all_in = c.total_in() as usize == job.data.len();
        match status {
            Status::StreamEnd => return Ok(out),
            _ if !job.last && all_in && out.len() < out.capacity() => return Ok(out),
            _ => {}
        }
    }
}

impl<W: Write> Write for ParEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(self.chunk_size - self.chunk.len());
        self.chunk.extend_from_slice(&buf[..n]);
        self.crc.update(&buf[..n]);
        self.total_in += n as u64;
        if self.chunk.len() == self.chunk_size {
            self.dispatch(false)?;
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Decodes concatenated zstd frames, verifying each frame's checksum if present.
struct ZstReader<R: BufRead> {
    dec: Option<StreamingDecoder<R, FrameDecoder>>,
}

impl<R: BufRead> ZstReader<R> {
    fn new(r: R) -> Result<Self> {
        Ok(Self {
            dec: Some(StreamingDecoder::new(r).map_err(|e| Error::Corrupt(e.to_string()))?),
        })
    }
}

impl<R: BufRead> Read for ZstReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let Some(dec) = &mut self.dec else {
                return Ok(0);
            };
            let n = dec.read(buf)?;
            if n > 0 || buf.is_empty() {
                return Ok(n);
            }
            let (mut src, frame) = self.dec.take().unwrap().into_parts();
            if let (Some(want), Some(got)) = (
                frame.get_checksum_from_data(),
                frame.get_calculated_checksum(),
            ) && want != got
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd checksum mismatch",
                ));
            }
            if src.fill_buf()?.is_empty() {
                return Ok(0);
            }
            self.dec = Some(StreamingDecoder::new(src).map_err(io::Error::other)?);
        }
    }
}

/// `notes.txt.gz` → `notes.txt`; no matching extension → `<name>.out`.
fn inner_name(path: &Path, format: Format) -> String {
    let base = crate::volume::base_path(path);
    let name = base
        .as_deref()
        .unwrap_or(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = format!(".{}", format.extension());
    match name.len().checked_sub(ext.len()) {
        Some(cut) if cut > 0 && name[cut..].eq_ignore_ascii_case(&ext) => name[..cut].to_owned(),
        _ => format!("{name}.out"),
    }
}

pub fn list(path: &Path, format: Format) -> Result<Vec<Entry>> {
    let meta = fs::metadata(path)?;
    let packed = Volume::open(path)?.len();
    Ok(vec![Entry {
        path: inner_name(path, format),
        size: 0, // ponytail: unknown without decoding; gz/xz trailers could provide it
        packed: Some(packed),
        modified: meta.modified().ok(),
        is_dir: false,
        encrypted: false,
        link: None,
    }])
}

pub fn extract(
    path: &Path,
    format: Format,
    dest: Option<&Path>,
    opts: &ExtractOptions,
    ctx: &Ctx,
) -> Result<Vec<String>> {
    let name = inner_name(path, format);
    let file = Volume::open(path)?;
    let consumed = AtomicU64::new(0);
    let mut meter = Meter::streamed(ctx, file.len(), &consumed);
    let input = BufReader::new(CountingRead {
        inner: file,
        count: &consumed,
    });
    decode_ahead(format, input, |r| match dest {
        None => {
            meter.copy(&name, r, &mut io::sink())?;
            Ok(Vec::new())
        }
        Some(dest) => {
            let mut x = Extractor::new(dest, opts.overwrite, ctx)?;
            if let Some(mut out) = x.file(&name, u64::MAX)? {
                meter.copy(&name, r, &mut out)?;
                out.finish(None, None)?;
            }
            Ok(Vec::new())
        }
    })
}

pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()> {
    let single = match sources {
        [src] if fs::metadata(src)?.is_file() => src,
        _ => {
            return Err(Error::Unsupported(format!(
                "{} compresses a single file; use TAR.{} for folders or several files",
                opts.format.name(),
                opts.format.name()
            )));
        }
    };
    let mut src = File::open(single)?;
    let mut meter = Meter::new(ctx, src.metadata()?.len());
    let mut enc = Encoder::new(
        opts.format,
        BufWriter::new(OutFile::create(dest.to_path_buf())?),
        opts.level,
    )?;
    meter.copy(&single.to_string_lossy(), &mut src, &mut enc)?;
    let out = enc.finish()?.into_inner().map_err(|e| e.into_error())?;
    out.file().sync_all()?;
    out.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bzip2_streams_bigger_than_the_cap_are_decoded_in_order() {
        let bz = |data: &[u8]| {
            let mut e = BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
            e.write_all(data).unwrap();
            e.finish().unwrap()
        };
        let (big, small) = (vec![0u8; 3 << 20], b"tail".to_vec());
        let mut file = bz(&big);
        file.extend(bz(&small));
        let mut input = io::Cursor::new(&file);
        assert_eq!(bzip2_streams(&mut input).unwrap().unwrap().len(), 2);
        let out = decode_ahead(Format::Bz2, input, |r| {
            let mut out = Vec::new();
            r.read_to_end(&mut out)?;
            Ok(out)
        })
        .unwrap();
        assert!(out[..big.len()] == big[..] && out[big.len()..] == small[..]);
    }

    #[test]
    fn decodes_multi_block_xz_in_parallel_only_when_the_index_checks_out() {
        let data: Vec<u8> = (0..12_000_000u32)
            .map(|i| ((i % 251) ^ (i >> 12)) as u8)
            .collect();
        let mut opts = XzOptions::with_preset(1);
        opts.set_block_size(NonZeroU64::new(3 << 20));
        let mut w = XzWriterMt::new(Vec::new(), opts, 4).unwrap();
        w.write_all(&data).unwrap();
        let xz = w.finish().unwrap();
        let mut r = io::Cursor::new(&xz);
        let (blocks, per_block) = xz_block_memory(&mut r).unwrap().unwrap();
        assert!(blocks >= 4 && per_block > 3 << 20, "{blocks} {per_block}");
        let mut out = Vec::new();
        decoder(Format::Xz, io::Cursor::new(&xz))
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert!(out == data);

        // An index claiming more data than the file holds: decode on one thread, and fail.
        let mut bad = xz.clone();
        let footer = bad.len() - 12;
        let index_size =
            (u32::from_le_bytes(bad[footer + 4..footer + 8].try_into().unwrap()) + 1) * 4;
        let index = footer - index_size as usize;
        bad[index + 2] |= 0x7F; // first record's unpadded size
        assert!(
            xz_block_memory(&mut io::Cursor::new(&bad))
                .unwrap()
                .is_none()
        );
        let mut sink = Vec::new();
        let decoded = decoder(Format::Xz, io::Cursor::new(&bad))
            .and_then(|mut d| Ok(d.read_to_end(&mut sink)?));
        assert!(decoded.is_err());
    }

    #[test]
    fn names_the_inner_file() {
        assert_eq!(
            inner_name(Path::new("/x/notes.txt.gz"), Format::Gz),
            "notes.txt"
        );
        assert_eq!(inner_name(Path::new("DATA.XZ"), Format::Xz), "DATA");
        assert_eq!(inner_name(Path::new("blob"), Format::Zst), "blob.out");
        assert_eq!(inner_name(Path::new(".gz"), Format::Gz), ".gz.out");
    }
}
