# ROADMAP

Phased plan for sekio. Guiding principles throughout: every format lands in
`sekio-core` as a new/existing `PreviewContent` variant so all frontends get it
at once; every renderer polls `CancelToken` and respects `PreviewOptions`
limits; Linux + Windows are both first-class (macOS deliberately out of scope
for now); heavy dependencies are feature-gated.

## Phase 0 — Foundation (done)

- [x] Workspace: `sekio-core` + `sekio-cli` + `sekio-tui` + `sekio-gui`
- [x] `PreviewContent` IR: `Text`, `Image`, `Listing`, `Metadata`, `HexDump`
- [x] Content-based detection (`infer` magic bytes + UTF-8 heuristic)
- [x] Text via syntect (pure-Rust `fancy-regex` backend), 24-bit RGB spans
- [x] Image decode + downscale; CLI halfblock renderer
- [x] Directory listings; hexdump fallback with MIME label
- [x] Cancellation token plumbed through the pipeline
- [x] CLI: EPIPE-clean exit, `--color` for pipes, `--lines`, `--width`

## Phase 1 — Core format coverage (done)

- [x] **Archives → `Listing`**: zip, tar, tar.gz/.tgz, plain .gz. Streamed,
      never fully read; gzip/tar ambiguity resolved by inflating one 512-byte
      block and checking the `ustar` magic. 7z/rar have no reader dep and fall
      through to the hexdump.
- [x] **Markdown → styled `Text`**: comrak AST walk (never HTML), rendered for
      reading — headings, emphasis, code blocks, lists, quotes, tables,
      footnotes, task items.
- [x] **SVG → `Image`** via resvg, rasterized at target size (never full-size
      then downscaled), with correct un-premultiplied alpha.
- [x] **`Metadata` IR variant** (key/value fields + optional thumbnail)
  - [x] Audio: tags, duration, codec, sample rate, channels, bit depth,
        bitrate, embedded cover art via symphonia
  - [x] Image EXIF: camera, lens, date, exposure, aperture, ISO, focal length
        — plus orientation applied, so phone photos aren't sideways
  - [x] Binaries: mime + size line above the hexdump
- [x] **PDF → `Text`** by default via pure-Rust `pdf-extract`, or `Image`
      (first page) via pdfium-render behind the `pdf-render` feature. The
      tiers chain: a missing pdfium falls through to text rather than to a
      metadata card, and only a PDF with no text layer at all (a scan) shows
      metadata explaining why.
- [x] **Video → `Image`** (frame grab) shelling out to ffmpegthumbnailer or
      ffmpeg behind the `video` feature, with a hard 5s child timeout and a
      `Metadata` fallback when neither is on PATH
- [x] **Non-UTF-8 text**: `chardetng` + `encoding_rs`, so legacy CJK and
      Latin-1 files decode instead of being mangled or read as binary
- [x] **Office documents**: xlsx, xlsm, xlsb, xls and ods as aligned tables
      via calamine; docx and pptx as text via zip + quick-xml, with slides in
      numeric order. Detected by reading the zip central directory for
      `word/document.xml`, `xl/workbook.xml`, `ppt/presentation.xml` or the
      ODF `mimetype` member — never by extension, so a docx named
      `mystery.bin` still previews correctly. Legacy binary `.doc`/`.ppt` have
      no pure-Rust reader; they are handled by the opt-in `office-legacy`
      feature, which shells out to LibreOffice the way `video` shells out to
      ffmpeg.
- [x] Tests: end-to-end dispatch tests in `crates/sekio-core/tests/preview.rs`
      plus per-renderer unit tests; office fixtures are built programmatically
      rather than committed as binaries

## Phase 2 — TUI (`sekio-tui`)

- [x] Two-pane ratatui browser: directory list + live preview
- [x] Navigation (j/k, Enter, Backspace, Home/End) and preview scrolling
      (PageUp/Down, Ctrl-d/u, g/G), mouse wheel
- [x] Images via `ratatui-image` (kitty/iTerm2/sixel with halfblock fallback).
      Default features disabled: they pull `chafa-dyn`, which needs libchafa
      via pkg-config and breaks Windows.
- [x] Worker thread + generation counter; navigation cancels the in-flight
      preview and stale results are discarded
- [x] Terminal hygiene: alternate screen and raw mode restored on every exit
      path including panic; clear error instead of hanging when not a tty
- [x] `--halfblocks` to skip the capability query: after its timeout,
      ratatui-image's query thread stays parked reading stdin and eats every
      keypress on terminals that never answer `CSI 5n`, making the app
      unquittable
- [x] Fallback viewport when the pty reports a 0x0 window size (this reports
      success, so nothing errors — ratatui just paints an empty screen)
- [x] TOML config file (`$XDG_CONFIG_HOME/sekio/config.toml`, `%APPDATA%` on
      Windows) for theme and default limits, with `--config` / `--no-config`.
      Flags beat config beats defaults; a malformed config warns and starts on
      defaults rather than failing. Documented in `config.example.toml`.

## Phase 3 — GUI (`sekio-gui`)

- [x] eframe/egui popup window painting every IR variant, `--borderless` for
      a decoration-free popup
- [x] Same worker-thread + `CancelToken` pattern as the TUI, with the worker
      additionally coalescing queued requests so holding an arrow key renders
      once per settled selection
- [x] Texture caching (uploaded once per preview, not per frame)
- [x] Cold-start instrumentation behind `--timing`, and `--probe` to measure
      it headlessly. Measured 3.8 ms to first preview; the UI-thread work
      before the first frame is ~0.3 ms
- [x] Sibling navigation, zoom, Esc/Space to close
- [x] Linux single-instance daemon (`--daemon`): socket under
      `$XDG_RUNTIME_DIR` keyed by uid and session, ~5 ms handoff, falls back
      to opening a window when no daemon is running, and recovers from a
      stale socket left by a killed daemon
- [x] File-manager keybinding recipes for Nautilus, Dolphin, Thunar, Nemo and
      tiling WMs, plus daemon autostart (`docs/desktop.md`)
- [x] Global hotkey (`--hotkey`, default `Ctrl+Shift+Space`) that previews
      whatever the file manager has selected. A bare `Space` is deliberately
      not the default: grabbing it globally steals it from every other
      application, so the spacebar flow is served by the socket handoff
      instead. Registration failure is never fatal — the daemon still serves
      its socket.
- [x] Windows: Explorer selection over COM (`IShellWindows` → `IShellBrowser`
      → `IFolderView2` → `IShellItemArray`), including the desktop, skipping
      virtual items with no filesystem path
- [x] Linux: best-effort selection — PRIMARY, then CLIPBOARD, then a bare
      name resolved against the file manager's open folders. Partial by
      nature; see the coverage table below.
- [x] `--doctor`: reports the selection strategy, what it can currently read
      and from where, whether the hotkey parsed and actually registered, and
      whether a daemon is running — each failure with a suggested next step
- [x] Windows daemon mode: the same `--daemon`, over a named pipe
      `\\.\pipe\sekio-gui-<sid>-<session>` with an SDDL DACL naming only the
      current user's SID. `ERROR_ACCESS_DENIED` from `CreateNamedPipeW` — not
      an "exists" error — is the already-running signal, and a busy pipe is
      retried once through `WaitNamedPipeW` rather than blocking a popup
- [x] Tray icon while resident: StatusNotifierItem over D-Bus on Linux
      (`ksni`, pure Rust — `tray-icon` was rejected for reaching the tray
      through GTK), `Shell_NotifyIcon` on Windows. Menu offers Open, Recent,
      Hotkey and Quit; a hotkey chosen there is written back to `gui.toml`.
      No tray host is an ordinary outcome, not a failure — stock GNOME needs
      the AppIndicator extension, and the daemon serves its socket regardless
- [x] Autostart on by default from all three installers: `systemctl --global
      enable` for the systemd *user* unit from the deb/rpm maintainer scripts
      (a root postinst cannot reach `--user`), an `HKMU\...\Run` value from
      the MSI, offered as a checkbox and reversible in one command
- [x] `gui.toml` (`$XDG_CONFIG_HOME/sekio/`, `%APPDATA%` on Windows):
      `hotkey`, `tray`, `theme`, `lines`, `wrap`, with `--config` /
      `--no-config`. Same precedence rules as the TUI's, and writable — the
      tray's hotkey choice is persisted through it in place, comments kept
- [x] Light / dark / system theme, following the desktop live. Not one palette
      inverted: each mode also picks the syntax theme drawn for its own
      background (core's `base16-ocean.dark`, `Catppuccin Latte` for light),
      so the worker rebuilds its `Previewer` on a switch
- [ ] Verify Wayland and X11 on a real desktop — this machine is headless, so
      only the no-display paths have been exercised

### Selection coverage on Linux

No Linux file manager exposes its selection over any public API, so this is
honestly partial:

| Desktop | What works |
|---|---|
| KDE / Dolphin | Selecting files fills PRIMARY, so the hotkey works without copying |
| GNOME / Nautilus | Nautilus does not publish its selection; press Ctrl+C first, then the hotkey |
| XFCE, Nemo, Caja, PCManFM | Copy-then-hotkey works; live selection varies by manager |
| Anywhere | A path or `file://` URI copied from a terminal, editor or browser |

Requires `wl-clipboard`, `xclip`, or `xsel`; `--doctor` says so when none is
installed. Windows needs none of this — Explorer answers directly.

## Phase 4 — Integration & distribution

- [x] Preview-backend recipes for fzf, lf, yazi, ranger, Neovim
      (`docs/integration.md`)
- [x] CI: build + test on ubuntu-latest and windows-latest, feature-matrix
      checks, rustfmt + clippy gates, and a Windows cross-check from Linux
- [x] Removed the last C dependency: `zip`'s `zstd` feature pulled `zstd-sys`,
      which broke the no-C-toolchain rule and made cross-compiling to Windows
      fail with `failed to find tool "lib.exe"`. With it gone the whole
      workspace type-checks for `x86_64-pc-windows-msvc` from Linux, so the
      "Windows is first-class" claim is now actually verified rather than
      asserted.
- [x] Release workflow: x86_64 Linux and Windows. aarch64 was dropped — it
      cross-compiled without the GUI, so it shipped two of three binaries
- [x] Latency benchmark (`cargo run --release -p sekio-core --example bench`)
- [x] Packaging: deb, rpm and msi built in CI, plus an AUR `PKGBUILD`,
      `sekio.desktop` and the winget process (`packaging/README.md`). The
      Scoop manifest and `install.sh` were dropped with the portable
      tar.gz/zip archives — both existed only to fetch assets that are no
      longer published, so `cargo install` covers what the three installers
      do not.
- [x] Publish tagged releases with real URLs. `v0.10.0` ships deb, rpm and
      msi plus their `.sha256` files; the AUR `PKGBUILD` keeps
      `sha256sums=('SKIP')` deliberately, so a release needs no follow-up
      hash commit
- [ ] Publish `sekio-core` to crates.io once the IR stabilizes

## Known performance notes

Measured with the bench example on this machine (release build, 500-line cap):

| Input | Median |
|---|---|
| Directory / hexdump | 0.01 ms |
| Markdown, 500 lines | 0.4 ms |
| TOML, 46 lines | 1.1 ms |
| SVG 800x600 | 9 ms |
| **Rust source, 480 lines** | **33 ms** |
| Log, 500 lines (budget hit) | 43 ms |
| PNG 1920x1080 | 59 ms |
| GUI cold start to first preview | 3.8 ms |

Syntax highlighting dominates everything else: roughly 70–125 µs per line of
Rust, about 100x the markdown renderer. That is syntect's `fancy-regex`
backend, chosen deliberately so Windows builds need no C toolchain —
oniguruma would be faster but reintroduces that dependency.

**Highlighting is time-boxed.** Some grammars are pathological: the `log`
syntax costs ~250 µs/line, so a 500-line log took 129 ms before the budget
existed — 370x the plain-text cost it had when `log` wasn't in the syntax set
at all. Byte and line caps don't bound that, so `text.rs` also caps wall-clock
time (`HIGHLIGHT_BUDGET`, 40 ms): past the budget the remaining lines are
emitted unstyled and the language is labelled "(highlighting timed out)"
rather than silently showing half-styled text.

Remaining ideas, roughly in order of value:

- Incremental highlighting for the TUI and GUI: highlight the visible window
  and extend on scroll. syntect is line-sequential and supports resuming, so
  this is the real fix rather than a bigger budget.
- The stateless CLI should ask for the lines it will actually display; the lf
  and fzf recipes in `docs/integration.md` already pass the pane height. The
  TUI can't do this — it scrolls within a preview and needs a buffer larger
  than the viewport.
- PNG cost is decode + resize and is largely inherent, though a
  downscale-on-decode path would help large photos.

## Blocked on things a build machine can't provide

The three unchecked items above are not skipped work — each needs something
this environment doesn't have:

| Item | Needs |
|---|---|
| Windows Explorer spacebar hook | A Windows machine to develop and test a shell extension. `docs/desktop.md` documents an AutoHotkey workaround meanwhile. |
| Wayland/X11 verification | A real display server. Only the no-display error path has been exercised here; the GUI's own logic is covered by 257 headless tests plus `--probe`. |
| crates.io publish | Credentials, and a decision that `PreviewContent` is stable. It has changed twice already (adding `Metadata`, then `Image.fields`), so this should wait. |

## Non-goals (for now)

- macOS support (revisit after Phase 4)
- Editing files — sekio is strictly a viewer
- Office formats (docx/xlsx) — needs LibreOffice shell-out or heavy parsing;
  reconsider now that `Metadata` exists as a cheap fallback
