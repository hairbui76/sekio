# sekio — features and how they work

Quick-view for any filetype. One detection-and-rendering core, three ways to
look at it:

| Binary | What it is |
|---|---|
| `sekio` | Prints a preview and exits. Also the backend behind fzf and lf preview panes. |
| `sekio-tui` | Browse a directory with a live preview beside it, without leaving the terminal. |
| `sekio-gui` | A window. Open files, drop them in, or summon it with a hotkey. |

## The one idea

All three programs share a single library. It takes a path and produces one of
five results — styled text, an image, a listing, a set of key/value facts, or a
hexdump — and the frontends do nothing but paint that. A format added once
appears in all three at the same time.

Three rules hold everywhere, and they explain most of sekio's behaviour:

- **It reads the file, not the name.** Type is decided by magic bytes and, for
  Office documents, by the parts inside the container. A PNG named `notes.txt`
  still previews as an image.
- **Work is capped, not just output.** Byte, line, entry and dimension limits
  stop the *reading*, so nothing stalls on a 4 GB file. Syntax highlighting also
  has a 40 ms budget, because some grammars are pathologically slow.
- **Previews are cancellable.** Move to another file and the one in flight is
  abandoned mid-work; a result that arrives late is thrown away rather than
  painted.

When a file is malformed or unsupported it falls back to a hexdump with the
detected type, rather than showing an error.

## What it previews

Everything marked **built in** works from a fresh install with nothing else on
the machine. The **needs a program** rows require something external at preview
time and degrade gracefully without it.

| Kind | Shown as | | Notes |
|---|---|---|---|
| Code & text | Highlighted text | built in | bat's extended syntax set — TOML, TypeScript, Dockerfile and the rest — with ~30 themes. Legacy encodings are decoded, not mangled. |
| Markdown | Formatted text | built in | Rendered for reading — headings, lists, quotes, tables — not highlighted as source. |
| PDF | The page, as an image | built in | The installers ship pdfium, so pages render — scans included. Built from source without it, you get the extracted text instead. |
| Images | Image | built in | PNG, JPEG, GIF, WebP, BMP, ICO, TIFF. EXIF shown and orientation applied, so phone photos are not sideways. |
| SVG | Image | built in | Rasterised at the size actually needed. |
| Spreadsheets | A real table | built in | xlsx, xlsm, xlsb, xls, ods — sheet names, column letters, a row-number gutter. The GUI scrolls a wide sheet sideways instead of eliding it. |
| Documents | Text | built in | docx, and pptx with slides in order. |
| Archives | Listing | built in | zip, tar, tar.gz, gz. Streamed — a huge archive is not unpacked to list it. |
| Audio | Facts + cover art | built in | Tags, duration, codec, sample rate, embedded artwork. |
| Directories | Listing | built in | Directories first, then case-insensitive by name. |
| Video | Frame grab | needs a program | ffmpegthumbnailer or ffmpeg on `PATH`. Without one you get the file's facts. |
| `.doc` / `.ppt` | Converted text | needs a program | The old binary Office formats, via LibreOffice. Roughly two seconds each. |
| Anything else | Hexdump | built in | With the detected MIME type above it. |

## `sekio` — the command

Prints a preview to standard output and exits.

```sh
sekio src/main.rs        # highlighted code
sekio photo.jpg          # the image, drawn in the terminal
sekio report.pdf         # the page, or its text in a source build
sekio archive.tar.gz     # what's inside
sekio ~/Downloads        # a directory listing
```

### Options

| Flag | What it does |
|---|---|
| `--lines N` | How many lines of text to emit. Default 200. |
| `--width N` | Columns the preview is laid out for: image scaling and spreadsheet column widths. Defaults to the terminal width. |
| `--theme NAME` | Syntax theme. |
| `--list-themes` | Print the ~30 available themes and exit. |
| `--color` | Force colour even when output is a pipe. Needed for preview panes. |
| `--no-color` | Plain text, no escapes. |

### As a preview pane

This is what `--color` exists for: a pane is a pipe, but it still wants colour.
sekio also exits quietly when the reader closes the pipe, which preview panes do
constantly.

```sh
fzf --preview 'sekio --color --width $FZF_PREVIEW_COLUMNS {}'
```

Recipes for lf, yazi, ranger and Neovim are in [integration.md](integration.md).

## `sekio-tui` — the browser

A two-pane terminal browser: the directory on the left, a live preview of
whatever the cursor is on to the right. Previews run on a background thread, so
moving quickly through large files stays smooth.

```sh
sekio-tui           # the current directory
sekio-tui ~/code    # somewhere else
```

### Keys

| Key | Action |
|---|---|
| `j` `k` `↓` `↑` | Move the cursor |
| `Enter` `l` `→` | Enter a directory |
| `Backspace` `h` `←` | Go to the parent |
| `Home` / `End` | First / last entry |
| `PgUp` `PgDn` `Space` | Scroll the preview by a page |
| `Ctrl-u` / `Ctrl-d` | Scroll by half a page |
| `g` / `G` | Top / bottom of the preview |
| `r` | Reload |
| `q` `Esc` `Ctrl-c` | Quit |

### Images in a terminal

Where the terminal supports kitty, iTerm2 or sixel graphics, images are drawn
properly; elsewhere they fall back to half-block characters. If your terminal
never answers the capability query and keystrokes stop registering, start it
with `--halfblocks` to skip the query entirely.

### Configuration

Optional, at `~/.config/sekio/config.toml` (or `%APPDATA%\sekio\config.toml`).
It sets the syntax theme, the interface colours, and the default limits.
Command-line flags beat the config file, which beats the built-in defaults; a
broken config prints a warning and starts on defaults rather than refusing to
run. `--no-config` ignores it. See
[../crates/sekio-tui/config.example.toml](../crates/sekio-tui/config.example.toml).

## `sekio-gui` — the window

Run it with no arguments and it opens a home screen: an **Open file…** button, a
built-in file browser, and the files you looked at recently. You can also drop a
file anywhere on the window. Run it with a path and it previews that
immediately.

```sh
sekio-gui                # home screen
sekio-gui photo.jpg      # straight to the file
```

The Open button uses your desktop's native file dialog. Where that is
unavailable — no portal service running, for instance — it falls back to the
built-in browser, which needs nothing installed and therefore always works.

### Keys

| Key | Action |
|---|---|
| `Ctrl+O` | Open a file |
| `Ctrl+B` | Toggle the built-in browser |
| `←` `→` `↑` `↓` | Previous / next file in the same folder |
| `Ctrl+ +` / `Ctrl+ -` | Zoom; `Ctrl+0` resets |
| `Space` | Close the preview |
| `Esc` | Back to the home screen |
| `Ctrl+Q` | Close the window |

### Staying resident

Opening a window costs a fresh process every time. Keep one alive instead and
each preview becomes a handoff of roughly five milliseconds — over a Unix
socket on Linux, a named pipe on Windows.

**The installers set this up for you.** The `.deb` and `.rpm` enable the
systemd user unit for every account, and the MSI writes one `Run` entry; either
way a daemon is there from your next login, with a tray icon to prove it. To
run one by hand instead — a source build, or a second one for testing:

```sh
sekio-gui --daemon &
```

Turning it off again:

```sh
sudo systemctl --global disable sekio   # Linux, machine-wide
systemctl --user mask sekio             # Linux, this account only
```

On Windows, clear the checkbox during setup, or switch **sekio** off under
Task Manager → Startup apps.

The daemon is only ever an optimisation. With none running, `sekio-gui <path>`
simply opens a window itself, and a socket left behind by a crashed daemon is
detected and cleaned up.

### The tray icon

A resident daemon has no visible window, so it puts an icon in the system tray
— that is how you know it is there, and how you reach it without one. The menu
offers **Open a file…**, a **Recent** submenu of what you have previewed, a
**Hotkey** submenu for changing the combination, and **Quit**.

Choosing a hotkey from the menu rewrites `gui.toml` in place, so it survives a
restart. Set `tray = false` there if you would rather not have an icon at all;
the socket and the hotkey do not depend on it.

Some sessions have nowhere to put an icon, and that is not a failure — the
daemon carries on regardless. On Linux the icon is a StatusNotifierItem, which
KDE, XFCE and Cinnamon host natively; **stock GNOME does not**, and needs the
AppIndicator extension. `--doctor` reports what is hosting it, or that nothing
is.

### The hotkey

A running daemon answers a global hotkey — `Ctrl+Shift+Space` by default — and
previews whatever your file manager currently has selected.

There are three ways to change it, and they are ranked in that order: the
`--hotkey "Super+P"` flag, a `hotkey = "Super+P"` line in `gui.toml`, and the
tray's **Hotkey** submenu (which writes that line for you). `--no-hotkey` turns
it off for one run. The config file is the one that matters in practice: a
hotkey passed as a flag has to be repeated in every autostart entry and
shortcut that starts the daemon, and cannot be changed from the tray at all.

**Why not just Space, like macOS?** Grabbing an unmodified Space globally would
take it away from every other application on the system — you could no longer
type a space anywhere. No third-party program can offer the real macOS behaviour
on Linux or Windows.

Reading the selection also differs by platform:

| Platform | What works |
|---|---|
| Windows | Explorer answers directly; it is exact. |
| KDE / Dolphin | Selecting files fills the primary selection, so it generally works as you would hope. |
| GNOME / Nautilus | Nautilus does not publish its selection — press `Ctrl+C` first. |
| Other Linux desktops | Copy-then-hotkey works; live selection varies. |
| Anywhere | A path or `file://` URI copied from a terminal, editor or browser. |

Linux needs `wl-clipboard`, `xclip` or `xsel` installed. `--doctor` says so when
none is found. See [desktop.md](desktop.md) for binding it in a file manager.

### Light, dark, and following the desktop

The **gear in the top right** is where this lives, along with the version and
the path to your settings file. Pick "Follow the desktop", "Light" or "Dark";
the choice is written straight into `gui.toml`, so it survives a restart.

By default the window paints in whichever mode your desktop is set to and
**changes with it while it is open** — switch your desktop to light and sekio is
light on the next frame. `--theme dark`, `--theme light` and `--theme system`
do the same from the command line.

This is not one palette inverted. Syntax colours do not come from the window at
all: they are baked into the preview by the core library, using a syntax theme
chosen for the background they will be drawn on. Switching modes rebuilds that,
so code is always coloured for the surface it is sitting on rather than
washed out on the wrong one.

### Configuration

`sekio-gui` reads `gui.toml` from `$XDG_CONFIG_HOME/sekio/` (or `~/.config/`)
on Linux, `%APPDATA%\sekio\` on Windows — a documented copy is
[`gui.example.toml`](../crates/sekio-gui/gui.example.toml). Seven keys:
`hotkey`, `tray`, `theme`, `lines`, `wrap`, `updates` and `on_close`.

It is `gui.toml` and not `config.toml` because `sekio-tui` keeps its own
`config.toml` in the same directory and the two schemas are different.

Three rules worth knowing:

* **Flags beat the file, the file beats the defaults.** Every setting exists at
  all three layers, and `--doctor` prints which layer each one came from.
* **A bad file never stops the program.** An unknown key or an unparseable
  value is a warning on stderr; sekio starts on the layer below. A typo is
  never silently ignored, and never fatal either.
* **The file is the one that matters for the hotkey.** A `--hotkey` has to be
  repeated in every autostart entry and shortcut that starts the daemon; a line
  in `gui.toml` is read by all of them.

`--config PATH` reads a different file; `--no-config` ignores every file.

### Closing the window

Closing a resident daemon is the one dismissal with two reasonable answers, so
the first time you do it sekio asks which you meant:

* **Minimize to tray** — the window goes away, the process stays. The hotkey
  keeps working and the next preview is a five-millisecond handoff rather than
  a cold start. This is what the ✕ has always done.
* **Quit** — the process ends. The tray icon goes with it, and so does the
  hotkey, until you start sekio again.

Tick **Don't ask again** and the answer is written to `gui.toml` as
`on_close`, so every close after that does the same thing without asking.
Escape, or a click outside the dialog, backs out and changes nothing — the box
is only acted on together with the button you press.

The question is only asked where both answers exist: a window you opened with
`sekio-gui` has nothing to stay resident for and simply closes, and a daemon in
a session with no tray host still just hides, because there would be no icon to
get it back from. The tray's own **Quit** never asks.

### Updates

sekio checks once, at startup, whether a newer release exists, and says nothing
unless there is one. When there is, the footer offers to fetch it and open it
in the installer — `msiexec` on Windows, the desktop's own package handler on
Linux, both of which ask for the elevation sekio does not have and does not
try to acquire. A build that did not come from a package says so and offers the
releases page instead.

The check is also in the gear menu, for asking again without restarting.

It is the only thing in sekio that touches the network, it happens once per
process and never on a timer, and a one-shot preview popup does not do it at
all. `updates = false` in `gui.toml`, or `--no-update-check`, turns it off
entirely.

## The optional formats

Three renderers depend on a program that is not shipped with sekio, so they are
compiled in only if you ask. The `.deb` and `.rpm` packages include video; the
rest need a build.

| Feature | Adds | Requires |
|---|---|---|
| `video` | A frame from a video file | ffmpegthumbnailer or ffmpeg |
| `pdf-render` | PDF page one as an image, instead of its text | the pdfium library — **already included in the `.deb`, `.rpm` and `.msi`** |
| `office-legacy` | Old binary `.doc` and `.ppt` | LibreOffice |

```sh
cargo install --path crates/sekio-cli \
    --features sekio-core/video,sekio-core/office-legacy
```

Each one degrades rather than failing: without the program you get the file's
facts and a line saying what to install.

## When a file won't preview

### You see a hexdump instead of content

That is the deliberate fallback for a file sekio cannot read as anything better.
It means one of three things: the format genuinely is not supported, the file is
malformed, or it needs one of the optional programs above. The MIME type printed
on the first line tells you which.

### The window won't open

Run `sekio-gui` from a terminal so you can see the error. The most common one is
no display server — expected over a plain SSH session, where `sekio` and
`sekio-tui` work but the window cannot.

### The hotkey does nothing

Run the built-in diagnostic:

```sh
sekio-gui --doctor
```

It reports which selection strategy is active, what it can read right now,
whether the hotkey actually registered, and whether a daemon is running — each
failure with a suggested next step.

A hotkey press that cannot resolve a file does nothing at all: no window, no
error. That silence is exactly what "the hotkey did nothing" looks like, which
is why the diagnostic exists.

### Nothing runs at all

Check what actually got installed. All three binaries land in `/usr/bin` from
the Linux packages, and on the `PATH` from the Windows installer.

```sh
sekio --version
sekio-tui --version
sekio-gui --version
```

---

Linux and Windows, x86_64. macOS is not a target. Install from the `.deb`,
`.rpm` or `.msi` on the [releases page](https://github.com/hairbui76/sekio/releases),
or build from source with `cargo install`.
