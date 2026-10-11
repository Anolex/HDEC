# HDEC — Plan

Cross-platform GUI archiver for macOS (arm64, x86_64) and Windows (x64, arm64), written in 100% Rust.

## 1. Scope

### Create
| Format | Options | Crate |
|---|---|---|
| ZIP | Store/Deflate levels, AES-256 or ZipCrypto ("compatible") | `zip` (no default features) + `flate2` (miniz_oxide) |
| 7Z | LZMA2 levels, AES-256, encrypt file names, solid on/off | `sevenz-rust2` (uses `lzma-rust2`) |
| TAR, TAR.GZ, TAR.XZ, TAR.BZ2 | level | `tar` + `flate2`, `lzma-rust2`, `bzip2` (Rust backend) |
| GZ, XZ, BZ2 | single file only | same |

### Extract / list / test
Everything above, plus:
| Format | Crate / note |
|---|---|
| RAR4, RAR5 (incl. encrypted, multi-volume `.partN.rar` / `.rNN`) | **own decoder** (`formats/rar/`) |
| ZIP methods Deflate64, BZip2, LZMA, XZ, Zstd | `deflate64`, `bzip2`, `lzma-rust2`, `ruzstd` |
| ZST, TAR.ZST, LZMA | `ruzstd`, `lzma-rust2` |
| JAR, APK, XPI, EPUB, … | ZIP, detected by magic bytes |
| SFX `.exe` (ZIP/7Z/RAR) | scan for embedded signature |

### Encryption
- ZIP: AES-256 (WinZip AE-2) read/write; ZipCrypto read/write (warn: weak, but the only kind Explorer/Archive Utility open).
- 7Z: AES-256 + SHA-256 KDF, optional header encryption.
- RAR: RAR3 AES-128, RAR5 AES-256 + PBKDF2-HMAC-SHA256 (read only).

### Split volumes
- Write: byte-split any created archive into `name.ext.001`, `.002`, … (presets: 100 MB, 700 MB, 4 GB FAT32, custom).
- Read: `.001` sets (any format), RAR multi-volume, spanned ZIP (`.z01…/.zip`).

### Modify
Add / delete / rename entries. Always written to a temp file next to the original, then atomically renamed.
- ZIP: raw-copy unchanged entries (no recompression).
- 7Z, TAR family: full rewrite.
- RAR: read-only (creating RAR is forbidden by the unRAR license and not a goal).

### Out of scope (decided)
ACE, ARJ, BH, PMA, ALZ, EGG, PEA, ZPAQ (dead / single-vendor) · UU/UUE/XXE, AES Crypt (not archives) · ISO, IMG, BIN, UDF, DAA, ISZ, WIM, MSI, NSIS, ASAR (images/installers) · LZH, ZIPX, SFX creation · Zstd creation (pure-Rust encoder too weak; revisit when `ruzstd` matures).

## 2. Non-negotiables
1. **Pure Rust.** Nothing compiles C/C++ into the binary. OS APIs through Rust bindings (`objc2`, `windows`) are fine. Check: `cargo tree -i cc --target <t> -e normal,build` prints nothing for all four shipping targets (see CLAUDE.md). `--target all` does show `cc`, but only via Linux/Android-only winit deps, which we don't ship.
2. **Safe extraction.** Every path from an archive goes through `safe_path::join`. See §5.
3. **No data loss.** Never overwrite a source archive or an existing file until the replacement is fully written; write to `*.hdec-tmp`, then rename. Delete partials on error/cancel.
4. **Passwords** live in `zeroize::Zeroizing<String>`, are never logged, never stored on disk.

## 3. Architecture

```
HDEC/
  Cargo.toml                 workspace, shared deps + versions pinned here
  crates/hdec-core/          library, no GUI deps
    src/lib.rs               public API, Error, Format + detect, dispatch
    src/safe_path.rs
    src/volume.rs            split writer + multi-part reader (Read + Seek)
    src/formats/zip.rs
    src/formats/sevenz.rs
    src/formats/tar.rs       tar + tar.gz/xz/bz2/zst
    src/formats/stream.rs    single-stream gz/xz/bz2/zst/lzma
    src/formats/rar/         own RAR decoder
    tests/                   integration tests + fixtures/
  crates/hdec-gui/           binary `hdec` (eframe/egui)
  fuzz/                      cargo-fuzz targets (from P6)
```

Files appear when a phase needs them, not before.

### Core API (sketch)
```rust
pub enum Format { Zip, SevenZ, Rar, Tar, TarGz, TarXz, TarBz2, TarZst, Gz, Xz, Bz2, Zst, Lzma }

pub struct Entry { pub path: String, pub size: u64, pub packed: Option<u64>,
                   pub modified: Option<SystemTime>, pub is_dir: bool, pub encrypted: bool }

pub struct Ctx<'a> { pub password: Option<Zeroizing<String>>,
                     pub cancel: &'a AtomicBool,
                     pub progress: &'a dyn Fn(Progress) }

pub fn detect(path: &Path) -> Result<Format>;          // magic bytes, extension only as fallback
pub fn list(path: &Path, ctx: &Ctx) -> Result<Vec<Entry>>;
pub fn extract(path: &Path, dest: &Path, only: Option<&[String]>, ow: Overwrite, ctx: &Ctx) -> Result<()>;
pub fn test(path: &Path, ctx: &Ctx) -> Result<()>;     // decode + CRC, write nothing
pub fn create(sources: &[PathBuf], dest: &Path, opts: &CreateOptions, ctx: &Ctx) -> Result<()>;
pub fn modify(path: &Path, edits: &[Edit], ctx: &Ctx) -> Result<()>;

pub enum Error { PasswordRequired, WrongPassword, Unsupported(String), Corrupt(String),
                 UnsafePath(String), Cancelled, Io(io::Error) }
```
Dispatch is a `match format` in `lib.rs`. Each format module exposes the same free functions. No trait until a real need shows up.

### GUI (egui)
- Main window: toolbar (Open, New, Extract, Add, Delete, Test), breadcrumb path inside the archive, entry table (name, size, packed, modified, 🔒), status bar with progress + Cancel.
- Drag & drop: archive → open it; other files → Create dialog.
- Create dialog: name, format, level, password + confirm, ZIP cipher (AES-256 / ZipCrypto), encrypt names (7Z), split size.
- Extract dialog: destination, "to subfolder" toggle, overwrite policy (ask / skip / overwrite / rename).
- Password modal on `PasswordRequired` / `WrongPassword`, then retry.
- Work runs on a `std::thread`; progress over `mpsc`; `ctx.request_repaint()`.
- `hdec <archive>` opens it (needed for file associations).

## 4. Phases

Each phase ends with: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, pure-Rust check, all green.

| # | Phase | Deliverable | Done when |
|---|---|---|---|
| P0 ✅ | Scaffold | workspace, `Error`, `Format` + `detect`, `safe_path` + tests, egui window with empty table | window opens; checks green; pure-Rust check verified |
| P1 | ZIP | list / extract / test / create; all read methods; AES + ZipCrypto | round-trip tests; reads fixtures from macOS `zip`, 7-Zip, Windows Explorer; Explorer/Archive Utility open our ZipCrypto output |
| P2 | GUI MVP | browse, extract (all/selected), create ZIP, drag & drop, progress/cancel, password modal | usable for daily ZIP work on macOS + Windows |
| P3 | TAR + streams | tar, tar.gz/xz/bz2/zst, gz/xz/bz2/zst/lzma | round-trip + fixtures from bsdtar/GNU tar (incl. long names, pax headers, symlinks) |
| P4 | 7Z | list / extract / create, AES, header encryption, solid | 7-Zip opens our output; we open 7-Zip's (LZMA, LZMA2, BCJ, PPMd if crate supports, encrypted) |
| P5 | Volumes + modify | split write, `.001` / spanned-ZIP read, add/delete/rename | split → join → identical; modify keeps untouched ZIP entries byte-identical |
| P6 | RAR5 | list / extract / test, encryption, multi-volume, `fuzz/` harness | all RAR5 fixtures decode with matching CRC; 1 h fuzz run clean |
| P7 | RAR3/4 | LZ + PPMd var.H + RarVM standard filters, AES-128, old volumes | RAR 2.9–4.x fixtures decode; fuzz clean |
| P8 | Extras | SFX scan, legacy filename encodings (CP437/CP866/Shift-JIS via `encoding_rs`), optional: Z, LZ, BR, CAB, DEB | per-item fixture tests |
| P9 | Release | file associations, context menu (Windows registry verbs, macOS Services), icons, `cargo-packager` (.dmg / .msi), signing + notarization, CI on macos + windows | signed installers install and open archives by double-click |

### RAR notes (P6–P7, largest risk)
- Port from libarchive's BSD-licensed `archive_read_support_format_rar5.c` / `_rar.c`; unRAR source as reference only (its license allows decompressors, forbids recreating the compressor).
- RAR5 first: simpler LZ + Huffman, filters (Delta, E8, E8E9, ARM).
- RAR3/4: LZ, PPMd variant H, RarVM; implement only the standard filters recognised by hash (as libarchive does), no general VM.
- Fixtures must be generated with real WinRAR / `rar` (RARLAB trial) — commit them, do not need `rar` in CI.

## 5. Extraction safety (`safe_path`)
- Reject: absolute paths, `..` components, Windows drive / UNC / `\\?\` prefixes, NUL bytes. Normalise `\` → `/`.
- Windows: rename reserved names (`CON`, `NUL`, `COM1`…), strip trailing dots/spaces, replace `<>:"|?*`.
- Symlinks: create only if the resolved target stays inside `dest`, else skip with a warning. On Windows, skip symlinks.
- Never follow an existing symlink inside `dest` while writing.
- Never write more bytes than the entry header declares (bomb guard); check free disk space against total declared size before starting.

## 6. Testing
- Unit tests next to the code (`safe_path`, `volume`, detection).
- `crates/hdec-core/tests/`: round-trip per format × (store, max level, password, unicode names, empty dirs, 0-byte files, >4 GB sparse/ZIP64 where cheap).
- `tests/fixtures/`: small archives made by external tools (7-Zip, WinRAR, Explorer, bsdtar, macOS `zip`/`ditto`), plus malicious ones (zip-slip, symlink escape, bomb, truncated, wrong CRC). Keep each fixture tiny; generator commands go in `fixtures/README.md`.
- Fuzz every parser we write ourselves (RAR, later Z/LZ).

## 7. Risks
| Risk | Mitigation |
|---|---|
| RAR decoder takes longer than planned | ship P0–P5 first; RAR is additive |
| `zip` crate majors churn (v9 now) | pin versions in workspace; no default features |
| `zip` crate's zstd feature pulls C | disable it; decode Zstd entries ourselves via raw reader + `ruzstd` |
| `lzma-rust2` slower than liblzma | acceptable; multithreaded LZMA2 later if needed |
| egui doesn't look native | accepted trade-off for pure Rust + speed of development |
| Notarization / Authenticode need paid certs | needed only at P9 |

## 8. Decisions log
- 2026-10-11: eframe 0.36 uses the new `App::ui(&mut Ui)` API and `egui::Panel` (not `TopBottomPanel`).
- 2026-10-11: 100% Rust; GUI = egui/eframe; two crates (core + gui); free functions + `match` dispatch; scope per §1.
