# HDEC

A fast, cross-platform archiver for macOS and Windows, written in 100% Rust
(no C/C++ in the dependency tree). Personal project; use at your own risk.

- **Open, extract, test:** ZIP (incl. Deflate64, BZip2, LZMA, XZ, PPMd), 7Z,
  RAR 1.3–7 (incl. multi-volume), TAR with gz/xz/bz2/zst/lz, single-file
  GZ/XZ/BZ2/ZST/LZ/LZMA, `.001` split sets, self-extracting `.exe` archives.
- **Create:** ZIP, 7Z, RAR5, TAR/TAR.GZ/TAR.XZ/TAR.BZ2, GZ/XZ/BZ2, optionally split into parts.
- **Encrypt:** AES-256 for ZIP, 7Z (with encrypted file names) and RAR; ZipCrypto for compatibility.
- **Edit in place:** add, delete, rename and move entries in ZIP, 7Z, TAR and RAR archives;
  changes are staged until you save.
- **Fast:** multithreaded compression and extraction for every format that allows it.
- **Safe:** entries that would escape the destination (`../`, absolute paths,
  symlink tricks) are skipped; existing files are never overwritten in place.

## Download

Every push builds `hdec.exe` (Windows 10+) and `HDEC.app` (macOS 11+, Apple
Silicon): open the latest run under **Actions** and download the artifacts.

The Mac app is not notarized, so macOS blocks it the first time. After moving
it to Applications, either right-click it → **Open**, or run
`xattr -dr com.apple.quarantine /Applications/HDEC.app`.

## Build

Needs a recent stable Rust (edition 2024).

```bash
cargo build --release -p hdec-gui        # target/release/hdec(.exe)
packaging/macos/bundle.sh                # macOS: target/release/HDEC.app
cargo test
```

## License

GPL-3.0-only, see [LICENSE](LICENSE). RAR support uses the clean-room
[`rars`](https://crates.io/crates/rars) crate, not RARLAB's unRAR source.
