//! Compression filters (gz, bz2, xz, zst, lzma) and single-file compressed streams.

use crate::fs::{self as hfs, OutFile};
use crate::volume::Volume;
use crate::{
    CountingRead, CreateOptions, Ctx, Entry, Error, ExtractOptions, Format, Level, Meter, Result,
};
use bzip2::read::MultiBzDecoder;
use bzip2::write::BzEncoder;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use lzma_rust2::{LzipReader, LzmaReader, XzOptions, XzReader, XzWriter};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Refuse LZMA/XZ streams that need more than this much dictionary memory.
const MEM_LIMIT_KB: u32 = 2 << 20; // 2 GiB

pub(crate) fn decoder<'a>(format: Format, r: impl BufRead + 'a) -> Result<Box<dyn Read + 'a>> {
    Ok(match format {
        Format::Tar => Box::new(r),
        Format::Gz | Format::TarGz => Box::new(MultiGzDecoder::new(r)),
        Format::Bz2 | Format::TarBz2 => Box::new(MultiBzDecoder::new(r)),
        Format::Xz | Format::TarXz => Box::new(XzReader::new_mem_limit(r, true, MEM_LIMIT_KB)),
        Format::Zst | Format::TarZst => Box::new(ZstReader::new(r)?),
        Format::Lzma => Box::new(LzmaReader::new_mem_limit(r, MEM_LIMIT_KB, None)?),
        Format::Lz | Format::TarLz => Box::new(LzipReader::new(r)),
        f => unreachable!("{f:?} is not a stream format"),
    })
}

pub(crate) enum Encoder<W: Write> {
    Plain(W),
    Gz(GzEncoder<W>),
    Bz2(BzEncoder<W>),
    Xz(XzWriter<W>),
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
                Encoder::Gz(GzEncoder::new(w, flate2::Compression::new(n)))
            }
            Format::Bz2 | Format::TarBz2 => {
                Encoder::Bz2(BzEncoder::new(w, bzip2::Compression::new(n.max(1))))
            }
            Format::Xz | Format::TarXz => Encoder::Xz(XzWriter::new(w, XzOptions::with_preset(n))?),
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
            Encoder::Gz(e) => e.finish()?,
            Encoder::Bz2(e) => e.finish()?,
            Encoder::Xz(e) => e.finish()?,
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
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Encoder::Plain(w) => w.flush(),
            Encoder::Gz(w) => w.flush(),
            Encoder::Bz2(w) => w.flush(),
            Encoder::Xz(w) => w.flush(),
        }
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
    let consumed = Cell::new(0);
    let mut meter = Meter::streamed(ctx, file.len(), &consumed);
    let mut r = decoder(
        format,
        BufReader::new(CountingRead {
            inner: file,
            count: &consumed,
        }),
    )?;
    match dest {
        None => {
            meter.copy(&name, &mut r, &mut io::sink())?;
        }
        Some(dest) => {
            if let Some(out_path) = hfs::output_path(dest, &name, opts.overwrite)? {
                let mut out = OutFile::create(out_path)?;
                meter.copy(&name, &mut r, &mut out)?;
                out.commit()?;
            }
        }
    }
    Ok(Vec::new())
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
