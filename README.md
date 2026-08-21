# sekio

Fast quick-view for any filetype. One core, three frontends.

```
crates/
  sekio-core   detection + rendering into a frontend-neutral PreviewContent IR
  sekio-cli    `sekio <path>` — ANSI output; usable as an fzf/lf/yazi preview backend
  sekio-tui    two-pane terminal browser with real terminal graphics
  sekio-gui    Quick Look-style popup window (Linux + Windows)
```

## Install

Releases ship three installers, all x86_64:

**Debian / Ubuntu**

```sh
sudo apt install ./sekio_0.3.0-1_amd64.deb
```

**Fedora / RHEL / openSUSE**

```sh
sudo dnf install ./sekio-0.3.0-1.x86_64.rpm
```

Both install all three binaries, an "Open with" desktop entry, and a systemd
**user** unit for the preview daemon. The unit ships disabled; turn it on when
you want previews to open instantly:

```sh
systemctl --user enable --now sekio
```

**Windows** — run `sekio-x86_64-pc-windows-msvc.msi`. It installs the three
binaries, adds them to `PATH`, and adds a Start Menu entry.

Download all three from the
[releases page](https://github.com/hairbui76/sekio/releases), each with a
`.sha256` beside it.

**Arch** — the AUR `PKGBUILD` in [packaging/](packaging/).

**Any other platform, or to build from source:**

```sh
cargo install --path crates/sekio-cli     # sekio
cargo install --path crates/sekio-tui     # sekio-tui
cargo install --path crates/sekio-gui     # sekio-gui
```

See [packaging/README.md](packaging/README.md) for details.

## Try it

```sh
cargo run -p sekio-cli -- src/main.rs        # syntax-highlighted code
cargo run -p sekio-cli -- photo.jpg          # image, rendered in the terminal
cargo run -p sekio-cli -- README.md          # markdown, rendered for reading
cargo run -p sekio-cli -- archive.tar.gz     # archive contents
cargo run -p sekio-cli -- track.mp3          # tags, duration, cover art
cargo run -p sekio-cli -- some/directory     # directory listing
cargo run -p sekio-cli -- /bin/ls            # mime + hexdump fallback

cargo run -p sekio-tui -- .                  # browse and preview
cargo run -p sekio-gui -- photo.jpg          # popup window
```

On Linux, keep a warm instance around so popups open instantly:

```sh
sekio-gui --daemon &        # once per session
sekio-gui photo.jpg         # ~5 ms handoff instead of a fresh process
```

If no daemon is running, `sekio-gui <path>` just opens a window as usual — the
daemon is an optimization, never a requirement.

The daemon also answers a global hotkey (`Ctrl+Shift+Space` by default) and
previews whatever your file manager has selected:

```sh
sekio-gui --daemon --hotkey 'Ctrl+Shift+Space' &
sekio-gui --doctor          # when the hotkey does nothing, run this first
```

A bare `Space` is deliberately not the default — grabbing it globally would
steal it from every other application. Selection detection is exact on Windows
(Explorer answers over COM) and best-effort on Linux, where no file manager
publishes its selection; `--doctor` reports which strategy is active and what
it can currently see. See [docs/desktop.md](docs/desktop.md).

`sekio-tui` reads `$XDG_CONFIG_HOME/sekio/config.toml` (`%APPDATA%\sekio\` on
Windows) for themes and default limits; see
[crates/sekio-tui/config.example.toml](crates/sekio-tui/config.example.toml).

## What it previews

| Kind | Rendered as | Notes |
|---|---|---|
| Code and plain text | Syntax-highlighted text | bat's extended syntax set (TOML, TypeScript, Dockerfile, …) and ~30 themes; legacy encodings decoded, not mangled |
| Markdown | Formatted text | Rendered for reading, not source highlighting |
| Images | Image | PNG/JPEG/GIF/WebP/BMP/ICO/TIFF, plus EXIF and auto-rotation |
| SVG | Image | Rasterized with resvg |
| Archives | Listing | zip, tar, tar.gz, gz |
| Spreadsheets | Aligned table | xlsx, xlsm, xlsb, xls, ods |
| Documents | Formatted text | docx, and pptx with slides in order |
| Audio | Metadata + cover art | Tags, duration, codec, sample rate |
| Directories | Listing | |
| PDF | Text, or first page as an image | Text works out of the box; `--features pdf-render` plus pdfium renders the page instead |
| Video | Frame grab | Needs `--features video` and ffmpeg/ffmpegthumbnailer |
| Legacy `.doc`/`.ppt` | Converted text | Needs `--features office-legacy` and LibreOffice |
| Anything else | Hexdump | With the detected mime type |

Unsupported or malformed files degrade to a hexdump rather than failing.

## Use it as a preview backend

```sh
fzf --preview 'sekio --color --width $FZF_PREVIEW_COLUMNS {}'
```

See [docs/integration.md](docs/integration.md) for lf, yazi, ranger, and
Neovim recipes, and [docs/desktop.md](docs/desktop.md) for binding the GUI to
a key in Nautilus, Dolphin, Thunar, or your window manager.

## Design

- **One IR, thin frontends.** `sekio-core` turns a path into
  `PreviewContent::{Text, Image, Listing, Metadata, HexDump}`. Frontends only
  paint. A new filetype added in core lights up everywhere at once.
- **Detection reads the file, not its name.** An Office document is recognised
  by the parts inside its zip, so a `.docx` renamed `mystery.bin` still
  previews as a document.
- **Cancellation-first.** `preview(path, opts, &CancelToken)` polls the token
  at work boundaries so a frontend can abort stale previews while the user
  flips through files. Both frontends run previews on a worker thread and
  discard results that arrive after the user has moved on.
- **Cap the work, not just the output.** Byte, line, entry, and dimension
  limits live in `PreviewOptions` in core, and renderers stop reading at the
  cap rather than loading everything and truncating. Nothing stalls on a 4 GB
  file.
- **Detection by content.** Magic bytes first, then encoding detection.
  Extensions only disambiguate formats that magic bytes cannot see (SVG,
  Markdown) and pick syntax highlighting — a PNG named `.txt` still previews
  as an image.

## Building

Pure-Rust formats are on by default, so no C toolchain is required and Windows
builds work out of the box. The two formats with external dependencies are
opt-in:

```sh
cargo build --release                                # everything default
# Opt-in formats, each needing an external program at runtime:
cargo build --release --features sekio-core/pdf            # pdfium
cargo build --release --features sekio-core/video          # ffmpeg
cargo build --release --features sekio-core/office-legacy  # LibreOffice
```

macOS is not a target for now.

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## License

MIT
