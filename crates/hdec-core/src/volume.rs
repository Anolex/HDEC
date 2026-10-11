//! Byte-split archives (`name.7z.001`, `.002`, …): read as one continuous
//! stream, and split a finished archive into parts.

use crate::fs::OutFile;
use crate::{Ctx, Meter, Result};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One archive, possibly spread over several files.
pub(crate) struct Volume {
    parts: Vec<(File, u64)>,
    pos: u64,
    len: u64,
}

/// `x.7z.003` → `x.7z`; `None` if the name has no `.NNN` suffix.
pub(crate) fn base_path(path: &Path) -> Option<PathBuf> {
    let name = path.to_str()?;
    let (base, num) = name.rsplit_once('.')?;
    (num.len() == 3 && num.bytes().all(|b| b.is_ascii_digit()) && num != "000").then(|| base.into())
}

pub(crate) fn part_path(base: &Path, n: u32) -> PathBuf {
    let mut s = OsString::from(base.as_os_str());
    s.push(format!(".{n:03}"));
    s.into()
}

impl Volume {
    /// Opens a plain file, or — given any part of a split set — all parts from `.001` on.
    pub fn open(path: &Path) -> Result<Self> {
        let paths: Vec<PathBuf> = match base_path(path) {
            Some(base) => (1..)
                .map(|n| part_path(&base, n))
                .take_while(|p| p.is_file())
                .collect(),
            None => vec![path.to_path_buf()],
        };
        if paths.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("first volume of {} is missing", path.display()),
            )
            .into());
        }
        let mut parts = Vec::with_capacity(paths.len());
        for p in paths {
            let file = File::open(p)?;
            let len = file.metadata()?.len();
            parts.push((file, len));
        }
        let len = parts.iter().map(|(_, l)| l).sum();
        Ok(Self { parts, pos: 0, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }
}

impl Read for Volume {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut start = 0;
        for (file, len) in &mut self.parts {
            if self.pos < start + *len {
                let offset = self.pos - start;
                file.seek(SeekFrom::Start(offset))?;
                let want = buf.len().min((*len - offset) as usize);
                let n = file.read(&mut buf[..want])?;
                self.pos += n as u64;
                return Ok(n);
            }
            start += *len;
        }
        Ok(0)
    }
}

impl Seek for Volume {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos =
            new.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        Ok(self.pos)
    }
}

/// Splits `path` into `path.001`, `path.002`, … of `size` bytes and removes it.
/// On failure or cancel, removes the parts and `path`, leaving nothing behind.
/// ponytail: needs the full archive on disk first (2× space); a splitting writer
/// would need seek support across parts for ZIP/7z header patching.
pub(crate) fn split(path: &Path, size: u64, ctx: &Ctx) -> Result<()> {
    let result = write_parts(path, size, ctx);
    if result.is_err() {
        for n in 1.. {
            if fs::remove_file(part_path(path, n)).is_err() {
                break;
            }
        }
    }
    let _ = fs::remove_file(path);
    result
}

fn write_parts(path: &Path, size: u64, ctx: &Ctx) -> Result<()> {
    let mut src = File::open(path)?;
    let len = src.metadata()?.len();
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut meter = Meter::new(ctx, len);
    let mut n = 1;
    loop {
        let mut out = OutFile::create(part_path(path, n))?;
        meter.copy(&name, &mut (&mut src).take(size.max(1)), &mut out)?;
        out.commit()?;
        n += 1;
        if src.stream_position()? >= len {
            break;
        }
    }
    // Leftovers from an earlier, longer split would otherwise be read as part of this one.
    while fs::remove_file(part_path(path, n)).is_ok() {
        n += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parts() {
        assert_eq!(
            base_path(Path::new("/a/x.7z.003")),
            Some(PathBuf::from("/a/x.7z"))
        );
        assert_eq!(
            base_path(Path::new("x.zip.001")),
            Some(PathBuf::from("x.zip"))
        );
        assert_eq!(base_path(Path::new("x.zip")), None);
        assert_eq!(base_path(Path::new("x.1234")), None);
        assert_eq!(base_path(Path::new("x.000")), None);
        assert_eq!(part_path(Path::new("x.7z"), 12), PathBuf::from("x.7z.012"));
    }

    #[test]
    fn reads_and_seeks_across_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("data.bin");
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for (i, chunk) in data.chunks(300).enumerate() {
            fs::write(part_path(&base, i as u32 + 1), chunk).unwrap();
        }
        let mut v = Volume::open(&part_path(&base, 2)).unwrap();
        assert_eq!(v.len(), 1000);
        let mut all = Vec::new();
        v.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        v.seek(SeekFrom::Start(295)).unwrap();
        let mut buf = [0; 10];
        v.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[295..305]);
        v.seek(SeekFrom::End(-5)).unwrap();
        let mut tail = Vec::new();
        v.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, data[995..]);
    }
}
