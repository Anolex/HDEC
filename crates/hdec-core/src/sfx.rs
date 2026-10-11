//! Archives appended to an executable stub (7-Zip / WinRAR / ZIP self-extractors).

use crate::{Format, Result};
use std::io::{self, Read, Seek, SeekFrom};

const SCAN_LIMIT: u64 = 16 << 20;
const SEVEN_Z: &[u8] = b"7z\xBC\xAF\x27\x1C";

/// Format and offset of an archive embedded in `r`, if any.
/// ZIP and RAR readers locate their own start, so their offset is only informative.
pub(crate) fn find(r: &mut (impl Read + Seek)) -> Result<Option<(Format, u64)>> {
    let mut buf = Vec::new();
    r.seek(SeekFrom::Start(0))?;
    r.by_ref().take(SCAN_LIMIT).read_to_end(&mut buf)?;
    if let Some(off) = seven_z_start(&buf) {
        return Ok(Some((Format::SevenZ, off as u64)));
    }
    if let Some(sig) = rars::find_archive_start(&buf, rars::SFX_SCAN_LIMIT)
        && sig.family != rars::ArchiveFamily::Rar13
    {
        return Ok(Some((Format::Rar, sig.offset as u64)));
    }
    // ZIP: end-of-central-directory record within the last 64 KiB (+ its own 22 bytes).
    let len = r.seek(SeekFrom::End(0))?;
    let tail_len = len.min(65_557);
    r.seek(SeekFrom::Start(len - tail_len))?;
    let mut tail = Vec::new();
    r.by_ref().take(tail_len).read_to_end(&mut tail)?;
    if tail.windows(4).any(|w| w == b"PK\x05\x06") {
        return Ok(Some((Format::Zip, 0)));
    }
    Ok(None)
}

/// A 7z signature whose start header CRC checks out (the stub itself may contain the bare signature).
fn seven_z_start(buf: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = buf[from..]
        .windows(SEVEN_Z.len())
        .position(|w| w == SEVEN_Z)
        .map(|i| i + from)
    {
        if let Some(header) = buf.get(i..i + 32) {
            let stored = u32::from_le_bytes(header[8..12].try_into().unwrap());
            if crc32fast::hash(&header[12..32]) == stored {
                return Some(i);
            }
        }
        from = i + 1;
    }
    None
}

/// Presents `inner` from `base` onwards as if it started at 0.
pub(crate) struct Skip<R> {
    pub inner: R,
    pub base: u64,
}

impl<R: Read> Read for Skip<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for Skip<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let abs = match pos {
            SeekFrom::Start(p) => self.inner.seek(SeekFrom::Start(self.base + p))?,
            other => self.inner.seek(other)?,
        };
        abs.checked_sub(self.base)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before archive start"))
    }
}
