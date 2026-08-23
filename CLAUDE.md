# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

sekio is a fast quick-view tool for any filetype: one core library (`sekio-core`) that turns a path into a frontend-neutral `PreviewContent` IR, with thin frontends that only paint that IR — `sekio-cli`, `sekio-tui`, and `sekio-gui`. Targets Linux and Windows only; macOS is explicitly out of scope. See ROADMAP.md for the phased plan and measured latency numbers.

## Commands

```sh
cargo build                                    # whole workspace
cargo build -p sekio-cli                       # one crate
cargo test -p sekio-core                       # tests for one crate
cargo test -p sekio-core looks_like_text       # single test by name filter
cargo test -p sekio-core --all-features        # includes pdf + video renderers
cargo run -p sekio-cli -- <path>               # preview a file/dir/binary
cargo run -p sekio-cli -- --color <path>       # force ANSI when piping (fzf-style)
cargo run --release -p sekio-core --example bench   # preview latency per format
cargo check --target x86_64-pc-windows-msvc --workspace   # verify Windows from Linux
```

CI pins `dtolnay/rust-toolchain@stable`, which is **ahead of this machine's
default toolchain**. New clippy lints therefore fail CI while passing here —
that happened twice (`chunks_exact_to_as_chunks`, then `collapsible_if`) before
`stable` was installed alongside the default. Lint under the same compiler CI
uses, not the default one:

```sh
RUSTFLAGS="-D warnings" cargo +stable clippy --workspace --all-targets
```

Build and test with the default toolchain as usual; it is only lints that
differ. When one does trip, take the lint's own suggestion rather than adding
an `allow`.

CI sets `RUSTFLAGS: -D warnings`, so a warning fails the build there but not
locally. Before pushing, mirror it — especially for the Windows target, where
`#[cfg]`-gated code produces dead-code warnings that never appear on Linux:

```sh
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets
RUSTFLAGS="-D warnings" cargo check --workspace --target x86_64-pc-windows-msvc
```

The Windows cross-check works because the dependency tree contains no C `-sys`
crates: `cargo check` doesn't link, so no MSVC toolchain is needed. Use it
before claiming a change is Windows-safe. If it ever starts failing with
`failed to find tool "lib.exe"`, a C dependency has crept in — find it with
`cargo tree -e normal | grep -- -sys` and remove or feature-gate it.

Machine-specific gotcha: `~/.local/bin/cc` on this machine shadows the real C compiler with an unrelated CLI tool. `.cargo/config.toml` pins the linker to `/usr/bin/gcc` — do not delete that file, and prefix cargo invocations with `CC=/usr/bin/gcc` when a build script compiles C. If a build fails with `cc: error: unrecognized arguments`, that shadow is the cause. That file is gitignored on purpose: a hardcoded gcc path is right for this machine and wrong for everyone else, so recreate it locally rather than committing it.

## Architecture

The entire design hangs on one boundary: **core produces IR, frontends paint IR.** A new filetype is added only in `sekio-core` and lights up in every frontend at once. Never render ANSI/ratatui/egui-specific output inside core, and never do filetype detection or file I/O inside a frontend.

Flow through `sekio-core` (`Previewer::preview` in `lib.rs`):

1. `detect.rs` reads a head sample (≤64 KB) and classifies by content, not extension: magic bytes via `infer` first, then `chardetng`/`encoding_rs` for the text/binary split and legacy-encoding detection. Extensions only disambiguate what magic bytes cannot see (SVG and Markdown are both plain text) and pick syntax highlighting. The head sample is passed to renderers so small files aren't read twice.
2. `render/*.rs` produce a `PreviewContent` variant. The dispatcher wraps every format renderer in an `or_hex!` macro: a renderer that fails on a malformed file degrades to a hexdump rather than failing the preview. `PreviewError::Cancelled` is never swallowed by that fallback.

Three invariants every renderer must uphold:

- **Cancellation-first.** `preview()` takes a `CancelToken`; renderers poll it at work boundaries (every N loop iterations, between decode/resize steps). New renderers must poll it too — a renderer that can't be cancelled mid-work is a bug.
- **Cap the work, not just the output.** All limits (`max_bytes`, `max_lines`, `image_max_dim`, `max_entries`) live in `PreviewOptions`, and renderers must stop *reading/decoding* at the cap, never load-then-truncate. Nothing may stall on a 4 GB file. Set `Preview.truncated` when a cap bites.
- **Never panic.** Malformed input returns `PreviewError::Format`, which the dispatcher turns into a hexdump. No `.unwrap()` on parsed data.

`Previewer` is constructed once and reused: loading syntect's syntax and theme sets is the expensive part.

Frontends share a pattern worth preserving: previews run on a worker thread owning the single `Previewer`, each request carries a monotonic generation id, navigation cancels the in-flight token, and results older than the newest request are discarded rather than painted.

## Constraints

- **Windows is a first-class target.** No Unix-only APIs outside `#[cfg]`; prefer pure-Rust crates over anything needing a C toolchain. This is why syntect uses `default-fancy` (fancy-regex, not oniguruma) and why `ratatui-image` has default features off (they pull `chafa-dyn`, which needs libchafa via pkg-config). Don't change either.
- Feature gates live *inside* each renderer module: when a feature is off, `render` still exists and returns `PreviewError::Format`, so the dispatcher degrades. `video`, `pdf-render` and `office-legacy` are off by default because they need an external program at runtime (ffmpeg, pdfium, LibreOffice); each degrades to something useful rather than erroring when it is absent.
- PDF has two tiers and they chain: `pdf` (default, pure-Rust text via `pdf-extract`) and `pdf-render` (opt-in pdfium page image). A missing pdfium falls through to text, not to metadata. `pdf-extract` panics on some malformed files, so the call is wrapped in `catch_unwind` **and** a thread-local-scoped panic hook — without the hook the default one still prints to stderr, which is visible noise in a preview pane.
- Office formats are detected by reading the zip central directory for `word/document.xml`, `xl/workbook.xml`, `ppt/presentation.xml` or the ODF `mimetype` member — never by extension. Legacy binary `.doc`/`.ppt` are OLE files with no pure-Rust reader; they go through the opt-in `office-legacy` LibreOffice shell-out, which MUST pass `-env:UserInstallation` or it fights the user's own LibreOffice session.
- Selection detection for the hotkey lives in `sekio-gui/src/selection/` behind one trait. `current()` returning `None` is normal, not an error — a hotkey press that resolves nothing must do nothing visible. On X11 `global-hotkey`'s `register()` returns `Ok(())` even with no display, so check `$DISPLAY`/`$WAYLAND_DISPLAY` before believing it.
- Windows verbatim paths (`\\?\C:\...`) are normalised away by `sekio_core::paths::canonical`, which every frontend uses instead of `fs::canonicalize`. Fixing it there rather than at each display site means no new UI can reintroduce it, and it covers the window title and the on-disk recent list too.
- **Any test touching Windows path shapes must be host-independent, and this has now bitten three times.** `Path::is_absolute`, `Path::file_name` and `std::path::Prefix` all answer for the OS running the test: on Linux `C:\a\b.txt` has no separators, so `file_name` returns the whole string and an assertion about a full path passes for the wrong reason, then fails on the Windows runner. Assert on string rewrites (which are host-independent) or on what the UI actually paints, never on what the host thinks a path means.
- **Never use `Path::is_absolute` in a function that takes a platform as a parameter.** It answers for the host OS, so a Unix branch tested on a Windows runner rejects `/home/x` and the function disagrees with itself depending on where it was compiled. This has already caused two CI failures (`sekio-tui/src/config.rs`, `sekio-gui/src/recent.rs`); both now use a local byte-level check instead. Any test that emulates a platform needs the same care.
- CLI conventions: EPIPE is a clean exit, not an error (preview panes close pipes constantly), and `--color` forces ANSI through pipes. Keep both intact for fzf/lf integration.
- Syntax highlighting dominates latency (~70–125 µs per line of Rust — see ROADMAP.md). Frontends should pass a `max_lines` matching what they can actually display rather than a large fixed value.
- Syntaxes and themes come from `two-face` (bat's extended set: TOML, TypeScript, Dockerfile, …), pinned to `syntect-default-fancy`. Do **not** let it default to `syntect-onig` — that pulls oniguruma, the C dependency the whole build avoids.
- Highlighting is time-boxed by `HIGHLIGHT_BUDGET` in `render/text.rs`, because some grammars (notably `log`) cost ~250 µs/line and size caps can't bound that. Past the budget the rest of the file is emitted unstyled and the language string is suffixed "(highlighting timed out)". Keep that honest label if you touch this.
