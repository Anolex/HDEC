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
| RAR (RAR5) | Store + levels 1/3/5, AES-256 with encrypted headers, non-solid | `rars` writer (clean-room, Apache-2.0) |

### Extract / list / test
Everything above, plus:
| Format | Crate / note |
|---|---|
| RAR 1.3–7 (incl. encrypted, encrypted headers, multi-volume `.partN.rar` / `.rNN`) | `rars` (pure Rust, Apache-2.0) |
| ZIP methods Deflate64, BZip2, LZMA, XZ, PPMd (Zstd deferred: rare in ZIP, needs a raw-read path because `zip`'s zstd feature is C) | `zip` features |
| ZST, TAR.ZST, LZMA, LZ, TAR.LZ | `ruzstd`, `lzma-rust2` |
| JAR, APK, XPI, EPUB, … | ZIP, detected by magic bytes |
| SFX `.exe` (ZIP/7Z/RAR) | scan for embedded signature |

### Encryption
- ZIP: AES-256 (WinZip AE-2) read/write; ZipCrypto read/write (warn: weak, but the only kind Explorer/Archive Utility open).
- 7Z: AES-256 + SHA-256 KDF, optional header encryption.
- RAR: RAR3 AES-128, RAR5 AES-256 + PBKDF2-HMAC-SHA256; writes RAR5 AES-256 with encrypted headers.

### Split volumes
- Write: byte-split any created archive into `name.ext.001`, `.002`, … (presets: 100 MB, 700 MB, 4 GB FAT32, custom).
- Read: `.001` sets (any format), RAR multi-volume. Spanned ZIP (`.z01…/.zip`) deferred: needs per-disk offset remapping the `zip` crate doesn't do.

### Modify
Add / delete / rename entries. Always written to a temp file next to the original, then atomically renamed.
- ZIP: raw-copy unchanged entries (no recompression).
- 7Z, TAR family, RAR: full rewrite (extract, edit, re-create), then `test` the rebuilt archive before it replaces the old one.
- RAR: multi-volume sets and archives locked by their creator are refused; edits write RAR5 (RAR4 input becomes RAR5); comments and recovery records are not kept.

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
- Drag out: entries dragged out of the window are extracted where they're dropped (Finder, Explorer, other apps), each under its own name.
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
| P10 🟡 | Performance | done: extraction writer (cached folder checks, direct `create_new` writes, writer threads), parallel ZIP/RAR/7Z/XZ/multi-stream-BZ2 decode, decoder read-ahead thread, multicore LZMA2 (7Z/XZ), parallel deflate (ZIP/GZ) and bzip2 create, `examples/bench.rs`. Todo: measure on Windows 10 (writer thread count), RAR writer speed (upstream `rars`), single-stream gzip/bzip2 parallel decode, PGO | bench numbers in §8 hold on macOS and Windows 10 |

### RAR notes
- Found `rars` (crates.io, Apache-2.0, pure Rust) after planning: reads RAR 1.3 through 7. We use it with `default-features = false, features = ["encryption", "write"]`; `write` only adds `getrandom` (pure Rust).
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
- 2026-10-19: Windows virtual-file drag, reworked after testing (Explorer kept copying after our job ended; files went missing on a select-all drag). `FileContents` is now answered at once with our own `IStream` (in the MTA); its reads wait for the file instead of `GetData` doing so, as Explorer expects of slow sources. The job covers extraction and Explorer's reading (one bar, half each) and ends once Explorer has read every file, ended the drop, let go of everything, or the job is cancelled; files Explorer didn't read are listed by name. Extraction is stopped only once nothing can read the rest (and that isn't reported as a cancel); a failure Explorer reports through `EndOperation` is shown as an error. The staging folder is removed only after the last stream and the data object are gone.
- 2026-10-18: Windows drag-out no longer freezes the window: virtual files instead of placeholders. Our `IDataObject` offers `FileGroupDescriptorW` (every file and folder, from the listing) and `FileContents` streams, and supports `IDataObjectAsyncCapability`, so Explorer returns from the drop at once and reads the streams on its own thread with its own copy progress. Extraction still goes through `fs::Extractor`, into a staging folder, started on release; a stream request waits until its file is reported by the new `ExtractOptions::on_file` (called after the file is checked and committed, so a corrupt entry is never handed out). The data object lives in the MTA (`CoIncrementMTAUsage`, created on a helper thread, handed to the UI thread as an `AgileReference` proxy), so those waits run on COM worker threads, never the UI thread. Names over 259 UTF-16 units (the descriptor's limit) fall back to the old placeholders. Trade-off: virtual files only reach targets that read them (Explorer, the desktop, file dialogs, mail apps); apps that take only file paths (`CF_HDROP`) refuse the drop. Quitting while a target is still reading warns once. Supersedes the 2026-10-14 rejection of virtual files.
- 2026-10-14: Drag out to Finder/Explorer. egui/winit can't start an OS drag, so once an entry drag leaves the window it is handed to the OS natively, and entries are extracted only on drop, via `ExtractOptions::flatten` (each selected item lands under its own name; a hard link to something outside the selection is skipped) with Overwrite::Rename. macOS (`mac_drag.rs`): one `NSFilePromiseProvider` per entry; Finder names the folder and the first promise extracts everything straight into it on a serial background queue, shown as an ordinary job (progress, Cancel). Windows (`win_drag.rs`), like 7-Zip: empty placeholders in a temp folder, the shell's own data object for them (`SHCreateShellItemArrayFromIDLists` → `BHID_DataObject`, `SHDoDragDrop` for the drag image) and our `IDropSource`, which swaps placeholders for the extracted entries when the button is released over a target, then lets the drop through; Explorer copies/moves them on. The window doesn't repaint during that extraction (it runs inside `DoDragDrop`). Rejected: Windows virtual files (`CFSTR_FILECONTENTS` streams don't fit `fs::Extractor`), guessing the drop folder (WinRAR's shell hook). Encrypted entries without a known password: the password is asked (and checked on the smallest locked entry) before the drag, since nothing can prompt after the drop. An OS drag swallows the mouse-up, so the next frame gets a synthetic release (`raw_input_hook`). New pure-Rust deps: `windows`/`windows-core` 0.62 (already in the tree via wgpu/accesskit), `block2`, `raw-window-handle`.
- 2026-10-12: Symlink targets may use `..` only at the start (it then climbs from the link's real folder); after a name it could climb out through another symlink (`s -> .`, `t -> s/../x`). Hard-link targets may not pass through any symlink. Corruption sweep (840 mangled archives, plus 360 encrypted): no panics or hangs; every checksummed format rejected every change (plain TAR has no checksums).
- 2026-10-12: Performance, part 2. ZIP create: entries ≤ 1 MiB compress on all cores (one-entry archive in memory, raw-copied in order); bigger files are cut into 1 MiB chunks in the same job queue, deflated pigz-style (primed with the previous 32 KiB, joined by sync flushes) and wrapped in a minimal ZIP64 one-entry archive. ZipCrypto stays serial (its entries don't survive `raw_copy_file`). GZ uses the same parallel deflate (one ordinary member). BZ2 is written as one stream per 900 kB chunk (pbzip2-style) and multi-stream files decode in parallel (a stream decoding past 64 MiB is redone in order, streaming). Writer threads: 4 on macOS — measured best in every location (8 was up to 2x slower in watched folders like the user temp dir); Windows 4–16, unmeasured. Extraction speed depends a lot on the destination: the same TAR extracts in 0.7 s into /private/tmp and 0.9–1.7 s into the user temp dir. RAR create stays slow: `rars` runs whole-member waves capped by its 256 MiB budget plus per-member filter search (~9 MB/s at level 1, ~2 MB/s at level 3); solid would be 3x faster to create but 2.5x slower to extract, so no. Fat LTO measured: no gain. Numbers (M3 Max, 15.7k files / 534 MB, Normal, `examples/bench.rs`): create ZIP 1.05 s, 7Z 17 s, TAR.GZ 0.8 s, TAR.XZ 14 s, TAR.BZ2 2.7 s; extract ZIP 0.9–1.1 s, 7Z 1.3–1.5 s, TAR 0.85 s, TAR.GZ 1.25 s, TAR.XZ 1.1 s, TAR.BZ2 1.6 s, RAR 2.7 s.
- 2026-10-12: Performance (M3 Max, 15.7k files / 534 MB, Normal). Extraction was bound by filesystem calls, not decoding: per file we did lstat per path component, a temp file + rename, and everything on one thread. Now `fs::Extractor` checks each folder once, writes new files directly with `create_new` (temp + rename only when replacing), and hands files and symlinks to 2–8 writer threads (APFS gets slower beyond ~8); hard links last. ZIP decodes entries on several threads (own archive handle each), single-file RAR uses `rars`' parallel decoder (`parallel` feature, rayon), streamed formats decode on a read-ahead thread. 7Z/XZ use multicore LZMA2 in chunks of 3× the dictionary (~1% larger), which also makes them decode in parallel; encoder threads are capped by a 2 GiB memory budget (so Max runs on one thread). Parallel XZ decoding is only used after our own check of the index and block headers (it buffers whole blocks sized by the file). Extract: ZIP 3.5→0.9 s, TAR 2.8→0.9 s, TAR.GZ 3.3→1.35 s, 7Z 6.9→1.3 s, TAR.XZ 8.0→1.1 s, RAR 9.7→2.8 s. Create: 7Z 89→19 s, TAR.XZ 108→15 s.
- 2026-10-12: RAR create and edit via `rars`' writer. The earlier "read-only" rule was over-cautious: the unRAR *source* license forbids using that code to build a RAR creator, and HDEC uses none of it; `rars` is an independent implementation. Every full rewrite (7Z, TAR, RAR) now tests the rebuilt archive before replacing the original.
- 2026-10-12: Edits are staged: the first edit copies the archive to a temp dir and every edit applies to that copy (which is also what's listed, extracted and previewed). Save (⌘S/Ctrl+S) writes it back via `*.hdec-tmp` + rename; opening another archive or closing the window with unsaved edits asks Save / Don't save / Cancel. Default window 728×448. New archive takes a name + folder instead of a full path.
- 2026-10-12: Folders are added recursively (create and modify). One picker for files and folders together (double-click opens a folder, Open takes the selection): macOS uses NSOpenPanel via rfd `pick_files_or_folders`; Windows' native dialog can't mix them, so it gets the in-app `picker.rs`. The New archive dialog lists its contents, which can be removed, extended via Add… or by dropping onto the window.
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
