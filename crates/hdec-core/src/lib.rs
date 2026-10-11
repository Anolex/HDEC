pub mod safe_path;

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
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
}

/// Detects the format by magic bytes; the file name only decides tar-vs-plain
/// for compressed streams and covers formats without magic (v7 tar, .lzma).
pub fn detect(path: &Path) -> Result<Format> {
    let mut head = Vec::with_capacity(512);
    File::open(path)?.take(512).read_to_end(&mut head)?;
    let name = path
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
