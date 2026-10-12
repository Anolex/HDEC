# HDEC

Cross-platform (macOS + Windows) GUI archiver in 100% Rust. Full plan, scope and phase status: [PLAN.md](PLAN.md).

## Layout
- `crates/hdec-core` — library: format detection, list/extract/test/create/modify. No GUI deps.
- `crates/hdec-gui` — binary `hdec`, eframe/egui front end. Talks to core only through its public API. `theme.rs` holds colors, fonts (Phosphor icons) and custom widgets; the design reference is `docs/ui-mockup.html`. After UI changes, render the screenshots and look at them.
- Formats live in `crates/hdec-core/src/formats/<format>.rs`; dispatch is a `match Format` in `lib.rs`.

## Commands
```bash
cargo build
cargo test
cargo run -p hdec-gui -- [archive]
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test -p hdec-gui -- --ignored screenshots   # renders every UI state (dark + light) to target/ui-shots/*.png
for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do cargo tree -i cc --target $t -e normal,build; done   # each must print "nothing to print"
```

## Hard rules
- **No C/C++ in the dependency tree.** Before adding a crate, check it (and its features) doesn't build C. `zip`: `default-features = false`, never enable `zstd`. `sevenz-rust2`: never `zstd`. `rars`: `encryption` + `write` only (no `parallel`/`recovery` unless checked). `bzip2`/`flate2`: Rust backends only.
- **Every filesystem path derived from an archive goes through `safe_path::join`.** No exceptions, including symlink targets.
- **Never overwrite** a source archive or existing file in place: write `*.hdec-tmp`, then rename; remove partials on error/cancel.
- Passwords: `Zeroizing<String>`, never logged, never persisted.
- Don't write more bytes than an entry declares.
- RAR writing goes only through `rars` (clean-room, Apache-2.0, written from a public spec). Never use, port or consult RARLAB's unRAR source: its license forbids using it to build a RAR creator.
- Crate versions are pinned in the root `Cargo.toml` `[workspace.dependencies]`.

## Conventions
- Errors: `hdec_core::Error` (`PasswordRequired`, `WrongPassword`, `Unsupported`, `Corrupt`, `UnsafePath`, `Cancelled`, `Io`). The GUI decides how to react; core never prompts.
- Long operations take `&Ctx` (password, cancel flag, progress callback) and check `cancel` between entries/blocks.
- Free functions per format module; no traits until a second real use exists.
- Tests: unit tests inline; per-format round-trip + fixture tests in `crates/hdec-core/tests/`. Fixtures are tiny, made by external tools, generator commands in `tests/fixtures/README.md`. Malicious fixtures (zip-slip, symlink escape, bomb, truncated) are required for every format.
- Self-written parsers get a `fuzz/` target (none so far; RAR uses the `rars` crate).
- After finishing a phase, tick it in PLAN.md §4 and add any decision to §8.
