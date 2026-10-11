# Fixtures

Tiny archives made by external tools. Regenerate with the commands below.

`tree/` = `a.txt` ("hello"), `sub/b.txt` ("world"), `empty/`.

| File | Made with |
|---|---|
| `infozip.zip` | macOS Info-ZIP: `zip -qr infozip.zip tree` |
| `zipcrypto_infozip.zip` | `zip -qr -P secret zipcrypto_infozip.zip tree` |
| `slip.zip` | Python `zipfile`: entries `../evil.txt`, `/abs.txt`, `ok.txt` |
| `symlink_escape.zip` | Python `zipfile`: symlink `link -> ../../outside`, `link/pwn.txt`, symlink `good -> ok.txt`, `ok.txt` |

TAR fixtures use `tree/` plus a 120-char directory `ddd…/ünï.txt` ("long") and symlink `link -> a.txt`.

| File | Made with |
|---|---|
| `bsdtar_pax.tar.gz` | `tar --format pax -czf … tree` (macOS bsdtar 3.5.3) |
| `gnutar.tar.zst` | `tar --format gnutar -cf - tree \| zstd -19` |
| `bsdtar.tar.xz` | `tar -cf - tree \| xz -9` |
| `bsdtar.tar.bz2` | `tar -cjf … tree` |
| `evil.tar` | Python `tarfile`: `../evil.txt`, `/abs.txt`, symlink `link -> ../../outside`, `link/pwn.txt`, hard link `hard -> ../../etc/passwd`, `ok.txt`, a FIFO |
| `note.txt.{gz,xz,bz2,lzma}` | `gzip -9c` / `xz -c` / `bzip2 -c` / `lzma -c` of "hello stream" |
| `multi_member.gz` | two gzip members: "part1 " + "part2" |
| `multi_frame.zst` | two zstd frames: "hello stream" + " more" |
| `bsdtar_{lzma1,lzma2,bzip2,deflate,ppmd,store}.7z` | `tar --format 7zip --options 7zip:compression=<codec> -cf … tree` (bsdtar 3.5.3) |

No 7-Zip / WinRAR fixtures yet: AES 7z and all RAR fixtures need the real tools (see PLAN.md).

RAR fixtures are generated with the `rars` crate's own writer (no WinRAR here), using the
generator at the bottom of this file. Tree: `tree/a.txt` ("hello"), `tree/sub/b.txt` ("world"),
`tree/sub/ünï.txt` ("unicode"), `tree/empty/`, `tree/big.bin` (300 kB pattern); RAR5 also
has symlinks `tree/link -> a.txt` and `tree/evil -> ../../outside`. Password: `secret`.

| File | Contents |
|---|---|
| `rar5.rar` | RAR5, compressed, with symlinks |
| `rar5_solid_pw.rar` | RAR5 solid + password |
| `rar5_headers_pw.rar` | RAR5 with encrypted headers |
| `rar4.rar` | RAR 2.9 format |
| `rar4_solid_pw.rar` | RAR 2.9 solid + password |
| `rar5_vol.part{1,2,3}.rar` | stored 3-volume set: `a.txt`, `spanned.bin` (30 000 bytes, `i % 251`) |

### RAR generator

A throwaway binary with `rars = { version = "0.10.0", default-features = false, features = ["encryption", "write"] }`, run as `rarprobe <fixtures dir>`:

```rust
// Generates RAR fixtures with rars' writer: rarprobe <out_dir>
use rars::{ArchiveVersion, Builder};
use std::path::Path;

fn tree(format: ArchiveVersion, symlink: bool) -> Builder {
    let unix = format.family() == rars::ArchiveFamily::Rar50Plus;
    let mode = |m| unix.then_some(m);
    // RAR5 mtime is Unix seconds; RAR4 is a raw DOS timestamp (2017-07-14 02:40:00 local).
    let t = if unix { 1_500_000_000 } else { (37 << 25) | (7 << 21) | (14 << 16) | (2 << 11) | (40 << 5) };
    let mut b = Builder::new(format);
    b.add_directory(b"tree".to_vec(), Some(t), mode(0o755)).unwrap();
    b.add_bytes(b"tree/a.txt".to_vec(), b"hello".to_vec(), Some(t), mode(0o644)).unwrap();
    b.add_directory(b"tree/sub".to_vec(), Some(t), mode(0o755)).unwrap();
    b.add_bytes(b"tree/sub/b.txt".to_vec(), b"world".to_vec(), Some(t), mode(0o644)).unwrap();
    b.add_bytes("tree/sub/ünï.txt".as_bytes().to_vec(), b"unicode".to_vec(), Some(t), mode(0o644)).unwrap();
    b.add_directory(b"tree/empty".to_vec(), Some(t), mode(0o755)).unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    b.add_bytes(b"tree/big.bin".to_vec(), big, Some(t), mode(0o644)).unwrap();
    if symlink {
        b.add_unix_symlink(b"tree/link".to_vec(), b"a.txt".to_vec(), false, Some(t), Some(0o777)).unwrap();
        b.add_unix_symlink(b"tree/evil".to_vec(), b"../../outside".to_vec(), false, Some(t), Some(0o777)).unwrap();
    }
    b
}

fn main() {
    let out = std::env::args().nth(1).unwrap();
    let out = Path::new(&out);
    let pw = Some(b"secret".to_vec());
    let write = |name: &str, b: Builder| b.write_to_path(&out.join(name), None).unwrap();
    write("rar5.rar", tree(ArchiveVersion::Rar50, true));
    write("rar5_solid_pw.rar", tree(ArchiveVersion::Rar50, false).solid(true).password(pw.clone()));
    write("rar5_headers_pw.rar", tree(ArchiveVersion::Rar50, false).password(pw.clone()).header_encryption(true));
    write("rar4.rar", tree(ArchiveVersion::Rar29, false));
    write("rar4_solid_pw.rar", tree(ArchiveVersion::Rar29, false).solid(true).password(pw.clone()));
    let mut v = Builder::new(ArchiveVersion::Rar50).store(true).volume_size(Some(12_000));
    v.add_bytes(b"a.txt".to_vec(), b"hello".to_vec(), Some(1_500_000_000), Some(0o644)).unwrap();
    let data: Vec<u8> = (0..30_000u32).map(|i| (i % 251) as u8).collect();
    v.add_bytes(b"spanned.bin".to_vec(), data, Some(1_500_000_000), Some(0o644)).unwrap();
    let vols = v.build_volumes(None).unwrap();
    for (i, v) in vols.iter().enumerate() {
        std::fs::write(out.join(format!("rar5_vol.part{}.rar", i + 1)), v).unwrap();
    }
    println!("{} volumes", vols.len());
}
```
