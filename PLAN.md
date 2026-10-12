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
| RAR 1.3–7 (incl. encrypted, encrypted headers, multi-volume `.partN.rar` / `.rNN`) | `rars` (pure Rust, Apache-2.0), read-only features |
| ZIP methods Deflate64, BZip2, LZMA, XZ, PPMd (Zstd deferred: rare in ZIP, needs a raw-read path because `zip`'s zstd feature is C) | `zip` features |
| ZST, TAR.ZST, LZMA, LZ, TAR.LZ | `ruzstd`, `lzma-rust2` |
| JAR, APK, XPI, EPUB, … | ZIP, detected by magic bytes |
| SFX `.exe` (ZIP/7Z/RAR) | scan for embedded signature |

### Encryption
- ZIP: AES-256 (WinZip AE-2) read/write; ZipCrypto read/write (warn: weak, but the only kind Explorer/Archive Utility open).
- 7Z: AES-256 + SHA-256 KDF, optional header encryption.
- RAR: RAR3 AES-128, RAR5 AES-256 + PBKDF2-HMAC-SHA256 (read only).

### Split volumes
- Write: byte-split any created archive into `name.ext.001`, `.002`, … (presets: 100 MB, 700 MB, 4 GB FAT32, custom).
- Read: `.001` sets (any format), RAR multi-volume. Spanned ZIP (`.z01…/.zip`) deferred: needs per-disk offset remapping the `zip` crate doesn't do.

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
    src/formats/rar.rs       adapter over `rars`
    tests/                   integration tests + fixtures/
  crates/hdec-gui/           binary `hdec` (eframe/egui)
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
- Extract dialog: destination, "to subfolder" toggle, overwrite policy (skip / replace / rename).
- Password modal on `PasswordRequired` / `WrongPassword`, then retry.
- Work runs on a `std::thread`; progress over `mpsc`; `ctx.request_repaint()`.
- `hdec <archive>` opens it (needed for file associations).

## 4. Phases

Each phase ends with: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, pure-Rust check, all green.

| # | Phase | Deliverable | Done when |
|---|---|---|---|
| P0 ✅ | Scaffold | workspace, `Error`, `Format` + `detect`, `safe_path` + tests, egui window with empty table | window opens; checks green; pure-Rust check verified |
| P1 ✅ | ZIP | list / extract / test / create; all read methods; AES + ZipCrypto | round-trip tests; reads fixtures from macOS `zip`, 7-Zip, Windows Explorer; Explorer/Archive Utility open our ZipCrypto output |
| P2 ✅ | GUI MVP | browse, extract (all/selected), create ZIP, drag & drop, progress/cancel, password modal | usable for daily ZIP work on macOS + Windows |
| P3 ✅ | TAR + streams | tar, tar.gz/xz/bz2/zst, gz/xz/bz2/zst/lzma | round-trip + fixtures from bsdtar/GNU tar (incl. long names, pax headers, symlinks) |
| P4 ✅ | 7Z | list / extract / create, AES, header encryption, solid | 7-Zip opens our output; we open 7-Zip's (LZMA, LZMA2, BCJ, PPMd if crate supports, encrypted) |
| P5 ✅ | Volumes + modify | split write, `.001` / spanned-ZIP read, add/delete/rename | split → join → identical; modify keeps untouched ZIP entries byte-identical |
| P6+P7 ✅ | RAR | list / extract / test for RAR 1.3–7 via `rars`: encryption, encrypted headers, solid, multi-volume, symlinks | fixtures decode; **still to do: fixtures from real WinRAR** |
| P8 ✅ | Extras | SFX `.exe` (7Z with header-CRC check, RAR via `rars`, ZIP via EOCD) and LZ / TAR.LZ. Deferred until asked: legacy filename code pages, Z, BR, CAB, DEB | per-item tests |
| P9 🟡 | Release | done: CI (macOS + Windows: fmt, clippy, tests, no-C check), release profile, no console window on Windows. Todo: icon, macOS .app + handling Finder "open document" Apple Events (winit doesn't forward them), Windows installer with file associations + Explorer verbs, signing + notarization | signed installers install and open archives by double-click |

### RAR notes
- Found `rars` (crates.io, Apache-2.0, pure Rust) after planning: reads RAR 1.3 through 7. We use it with `default-features = false, features = ["encryption"]`, so no RAR encoder is compiled in.
- Risk: young crate ("agentic development experiment" per its README). Mitigation: our fixtures, real-WinRAR fixtures once available, and fuzzing if bugs show up.
- `extract_to` skips RAR5 redirections; `formats/rar.rs` recreates symlinks from member metadata (hard links/junctions/file copies are reported as skipped).

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
- Fuzz every parser we write ourselves (none yet; RAR comes from `rars`).

## 7. Risks
| Risk | Mitigation |
|---|---|
| `rars` is young | fixtures + real-WinRAR fixtures; swap/patch upstream if it misbehaves |
| `zip` crate majors churn (v9 now) | pin versions in workspace; no default features |
| `zip` crate's zstd feature pulls C | disable it; decode Zstd entries ourselves via raw reader + `ruzstd` |
| `lzma-rust2` slower than liblzma | acceptable; multithreaded LZMA2 later if needed |
| egui doesn't look native | accepted trade-off for pure Rust + speed of development |
| Notarization / Authenticode need paid certs | needed only at P9 |

## 8. Decisions log
- 2026-10-12: Files opened from an archive are watched (mtime, polled every second); a change prompts to save it back via `Edit::Add` into the entry's folder. Read-only archives (RAR, split, SFX, single streams) show a "Read-only" badge and say why on hover.
- 2026-10-12: File-manager editing: `Edit::Rename` takes several (from, to) pairs (moves), `Edit::CreateDir`; `modify` rejects collisions, invalid names and moving a folder into itself. `Entry.link` carries symlink targets. GUI: drag rows onto folders/tree/breadcrumb to move, double-click opens a file with the default app (extracted to a temp dir), New folder, empty-area context menu, F2 / Cmd+Shift+N. Dropping files on an open editable archive always adds them to the current folder. `zip` needs its `unreserved` feature to raw-copy Info-ZIP extra fields.
- 2026-10-12: UI redesigned from an HTML mockup (`docs/ui-mockup.html`): toolbar with icons, breadcrumb, folder tree, sortable table with ratio, filter across the whole archive, context menu, keyboard shortcuts, card dialogs, dark + light themes. Icons via `egui-phosphor`. Headless screenshots via `egui_kittest` (dev-only).
- 2026-10-11: RAR via `rars` instead of a hand-written decoder (P6–P7 collapsed). `Ctx` cancel/progress became `Arc`s because `rars` wants `'static` writers.
- 2026-10-11: Split = create whole archive, then cut into parts (needs 2× disk briefly). Modify: ZIP raw-copies entries; 7Z/TAR extract to a scratch dir next to the archive, edit, re-create at Normal level; refuses if any entry would be skipped as unsafe.
- 2026-10-11: 7Z: solid blocks of 256 MiB, file names always encrypted when a password is set, unix modes/symlinks stored p7zip-style (attribute 0x8000 + st_mode<<16). Interop verified against bsdtar (libarchive) both ways; AES interop with real 7-Zip still to do.
- 2026-10-11: Streamed formats (tar.*, gz/xz/bz2/zst/lzma) report progress as compressed bytes read; LZMA/XZ dictionaries over 2 GiB are refused. Hard links in tar are only created to targets inside the destination.
- 2026-10-11: Unsafe entries (traversal, escaping symlinks) are skipped and returned as warnings, not fatal. Overwrite policy is chosen up front (no mid-extraction "ask").
- 2026-10-11: ZIP times are written as local wall-clock (DOS) via `jiff`'s system tz.
- 2026-10-11: eframe 0.36 uses the new `App::ui(&mut Ui)` API and `egui::Panel` (not `TopBottomPanel`).
- 2026-10-11: 100% Rust; GUI = egui/eframe; two crates (core + gui); free functions + `match` dispatch; scope per §1.
