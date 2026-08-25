# Packaging

Release artifacts are produced by `.github/workflows/release.yml` when a `v*`
tag is pushed. As of v0.16.0 that is exactly three installers, all x86_64:

| Asset | Contents |
|---|---|
| `sekio_<version>-1_amd64.deb` | all three binaries + pdfium + desktop entry + systemd user unit |
| `sekio-<version>-1.x86_64.rpm` | same as the `.deb` |
| `sekio-x86_64-pc-windows-msvc.msi` | all three `.exe` + `pdfium.dll`, PATH entry, Start Menu shortcut |

Each is published with a `.sha256` beside it containing the bare hash.

Every package is built with `sekio-core/pdf-render` on and ships a copy of
pdfium, so PDFs preview as page images out of the box — see [Vendored
pdfium](#vendored-pdfium).

Portable archives, the `install.sh` curl-pipe-sh script and the Scoop manifest
were removed after v0.8.0: the three native installers are the supported paths,
and everything else builds from source with `cargo install`. That also removed
the whole cross-platform archive-building job from the release workflow, since
the `.deb`/`.rpm` and `.msi` jobs each build their own binaries.

## Which install path to document to a user

| Platform | Command |
|---|---|
| Debian / Ubuntu | `sudo apt install ./sekio_<version>-1_amd64.deb` |
| Fedora / RHEL / openSUSE | `sudo dnf install ./sekio-<version>-1.x86_64.rpm` |
| Windows | the `.msi` |
| Arch | AUR `PKGBUILD` in this directory |
| Anything else | `cargo install --path crates/sekio-cli` (and `-tui`, `-gui`) |

## Vendored pdfium

Every package ships a copy of pdfium, the PDF page renderer.

| | |
|---|---|
| Upstream | [`bblanchon/pdfium-binaries`](https://github.com/bblanchon/pdfium-binaries) — prebuilt pdfium from the Chromium project |
| Pinned tag | `chromium/8009` (pdfium 153.0.8009.0) |
| Files taken | `lib/libpdfium.so` (linux-x64), `bin/pdfium.dll` (win-x64), `licenses/pdfium.txt` |
| Licence | BSD-3-Clause, shipped as `LICENSE-pdfium.txt` in every package |
| Cost | `.deb` +2.5 MB, `.rpm` +2.9 MB, `.msi` +3.5 MB; 7.7 MB on disk |
| Fetched by | the `Fetch pdfium` step in `.github/workflows/release.yml`, into `packaging/vendor/` (gitignored) |

### Why it is vendored

pdfium is a *dynamically loaded* native library. Debian and Fedora do not
package it, Windows has no package manager to pull it from, and building it
means a Chromium checkout — so "install pdfium" is not an instruction anyone
who just installed a `.deb`, `.rpm` or `.msi` can act on. Without it a scanned
PDF, which is images all the way down with no text layer to fall back to, has
no preview at all: sekio can only show a metadata card saying so. Shipping
~2.5 MB of compressed library is the cheaper answer.

The binaries are dropped in place with no fixup — no `patchelf`, no rpath, no
`ldconfig`. sekio loads them by absolute path at runtime, trying in order:

1. `$SEKIO_PDFIUM_PATH` (a file, or a directory holding it) — always wins;
2. the directory of the running executable — the `.msi` layout;
3. `../lib/sekio/` relative to that — the `.deb`/`.rpm` layout, since the FHS
   allows no shared library under `/usr/bin`;
4. the system library, for anyone who has one.

Every step fails soft into the next, so a package with the library removed
behaves exactly like a `cargo install` build: text or a metadata card, never a
crash. The search lives in `bind()` in `crates/sekio-core/src/render/pdf.rs`.

### Bumping it

Upstream cuts a release most weeks; there is no reason to follow it closely.
When you do bump, in `.github/workflows/release.yml` change `PDFIUM_TAG` **and
both checksums together** — a tag alone is not a pin, since a tag can be moved:

```sh
tag=chromium/8009            # the new one
for p in linux win; do
  curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/$tag/pdfium-$p-x64.tgz" \
    | sha256sum
done
```

Then update the tag in this file's table and in the two command blocks below,
and dispatch the release workflow: the `msi` job is the only place WiX runs and
the only proof the new DLL packages cleanly.

Note `pdfium-render`, the Rust binding, is version-sensitive about the pdfium
ABI. If a bump makes page rendering start failing at load time on all
platforms at once, that is the pairing to check first — not the packaging.

## Debian/Ubuntu (`.deb`) and Fedora/RHEL (`.rpm`)

Both are built from the same metadata in `crates/sekio-cli/Cargo.toml` — one
`sekio` package with all three frontends, rather than three packages. Download
from the release page, then:

```sh
sudo apt install ./sekio_0.16.0-1_amd64.deb     # Debian/Ubuntu
sudo dnf install ./sekio-0.16.0-1.x86_64.rpm    # Fedora/RHEL/openSUSE
```

Layout:

```
/usr/bin/sekio, sekio-tui, sekio-gui
/usr/lib/sekio/libpdfium.so                the PDF page renderer (private copy)
/usr/share/applications/sekio.desktop      "Open with" entry in file managers
/usr/share/icons/hicolor/<size>/apps/sekio.png   what its Icon=sekio resolves to
/usr/lib/systemd/user/sekio.service        the preview daemon (NOT auto-enabled)
/usr/share/doc/sekio/                      README, integration.md, desktop.md
/usr/share/doc/sekio/copyright             (deb)
/usr/share/doc/sekio/LICENSE-pdfium.txt    (deb)
/usr/share/licenses/sekio/LICENSE-MIT      (rpm)
/usr/share/licenses/sekio/LICENSE-pdfium.txt  (rpm)
```

`/usr/lib/sekio/` rather than `/usr/lib/`: this is a private library for one
program, with no soname symlink and no `ldconfig` entry, and nothing else may
link against it. sekio finds it there by looking one level up and over from its
own executable path — `/usr/bin/sekio` → `/usr/lib/sekio/` — which is why the
directory name has to keep matching `LIBDIR` in
`crates/sekio-core/src/render/pdf.rs`.

### Icons

`sekio.desktop` carries `Icon=sekio` — a theme icon *name*, which desktops
resolve against the hicolor theme. The packages therefore install
`assets/icons/sekio-<n>.png` as
`/usr/share/icons/hicolor/<n>x<n>/apps/sekio.png` for n in 16, 24, 32, 48, 64,
128 and 256. Both asset lists in `crates/sekio-cli/Cargo.toml` spell out all
seven; the AUR `PKGBUILD` installs the same set in a loop.

The PNGs are committed, not generated during the build — see `assets/README.md`
for how they are made and for the one command that regenerates them. No release
runner needs an image tool installed.

Nothing runs `gtk-update-icon-cache` afterwards, and nothing needs to: Debian
ships a dpkg trigger on `/usr/share/icons/hicolor` (from `hicolor-icon-theme`,
which every desktop pulls in) and Fedora has the equivalent file trigger. Where
no cache exists at all, GTK and Qt read the directories directly.

### The preview daemon

The packages ship the systemd **user** unit but deliberately do not enable it:
no postinst tricks, no `[package.metadata.deb.systemd-units]`. A previewer
daemon that appears in every session without being asked for is not a good
default. Turn it on with one command:

```sh
systemctl --user enable --now sekio
```

`sekio-gui <path>` then hands off to the resident process in ~5 ms instead of
starting cold. Without the daemon everything still works; it just pays for a
fresh process each time.

The unit is anchored to `graphical-session.target` for two reasons, both of
which will bite anyone who tries to "simplify" it to `default.target`:
`sekio-gui --daemon` opens a hidden window at startup and so needs a display
connection immediately, and its handoff socket is named after
`$WAYLAND_DISPLAY`/`$DISPLAY` — a daemon started without those inherited binds a
different socket name than the client looks for, so every handoff silently
misses and opens a fresh window instead.

### Building them

From the workspace root:

```sh
cargo install cargo-deb cargo-generate-rpm --locked

# Both packagers expect the vendored library to already be there.
mkdir -p packaging/vendor/pdfium
curl -fsSL -o /tmp/pdfium.tgz \
  https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8009/pdfium-linux-x64.tgz
tar -xzf /tmp/pdfium.tgz -C packaging/vendor/pdfium --strip-components=1 \
  lib/libpdfium.so licenses/pdfium.txt
mv packaging/vendor/pdfium/pdfium.txt packaging/vendor/pdfium/LICENSE-pdfium.txt

cargo build --release --features sekio-core/video,sekio-core/pdf-render \
  -p sekio-cli -p sekio-tui -p sekio-gui
cargo deb -p sekio-cli --no-build --no-strip
cargo generate-rpm -p crates/sekio-cli
```

Inspect the result before shipping it:

```sh
dpkg-deb -c target/debian/*.deb ; dpkg-deb -I target/debian/*.deb
rpm -qlp target/generate-rpm/*.rpm ; rpm -qpR target/generate-rpm/*.rpm
```

Two things worth knowing about that build line:

- `--features sekio-core/video` compiles in a renderer that only shells out to
  ffmpegthumbnailer/ffmpeg at runtime, so it adds no build dependency — and it
  is what makes `Recommends: ffmpegthumbnailer | ffmpeg` honest.
- `--features sekio-core/pdf-render` is on for the same reason and one more.
  Neither Debian nor Fedora packages pdfium, so a `Recommends` would point at
  nothing — which is exactly why the package carries its own copy instead. It
  stays out of `sekio-core`'s default features so that `cargo install`, which
  ships no library, still gets the pure-Rust text/metadata fallback rather than
  a dependency it cannot satisfy.
- `--no-strip` because `[profile.release]` already sets `strip = true`.

The dependency lists are partly hand-written, which is unusual and deliberate.
`ldd` on the release build shows `sekio-gui` linking only libc, libm and
libgcc_s: eframe/winit/glutin **dlopen** the GL and windowing libraries, so no
ELF scanner — not cargo-deb's `$auto`, not cargo-generate-rpm's `auto-req` —
can see them. Left to autodetection the package installs cleanly and then
`sekio-gui` dies at runtime with a dlopen error, which is a far worse failure
than a slightly heavy dependency list. The rpm spells them as sonames rather
than package names so one list works across Fedora, RHEL and openSUSE, which
disagree about whether it is `mesa-libGL`, `libglvnd-glx` or `Mesa-libGL1`.

## Windows (`.msi`)

Built with [cargo-wix](https://github.com/volks73/cargo-wix) and **WiX Toolset
v3**. Double-click it, or:

```powershell
msiexec /i sekio-x86_64-pc-windows-msvc.msi
msiexec /i sekio-x86_64-pc-windows-msvc.msi /quiet    # unattended
msiexec /x sekio-x86_64-pc-windows-msvc.msi           # uninstall
```

Layout:

```
C:\Program Files\sekio\bin\sekio.exe, sekio-tui.exe, sekio-gui.exe
C:\Program Files\sekio\bin\pdfium.dll        the PDF page renderer
C:\Program Files\sekio\License.rtf           sekio's own licence, shown by the installer
C:\Program Files\sekio\LICENSE-pdfium.txt    pdfium's
```

Icons on Windows come from two independent places, and both are needed:

- **`sekio-gui.exe` carries an icon resource**, embedded at build time by
  `crates/sekio-gui/build.rs` from `assets/sekio.ico`. That is what Explorer,
  the taskbar and the Start Menu shortcut show. `sekio.exe` and `sekio-tui.exe`
  deliberately have none — they are console programs.
- **`main.wxs` declares `<Icon>` + `ARPPRODUCTICON`** from the same
  `assets/sekio.ico`. That is the icon in Add/Remove Programs, which reads the
  MSI, not the installed files.

`build.rs` only runs its half on a Windows *host* (it needs the Windows SDK's
`rc.exe`), which is exactly what the `msi` job is. A `sekio-gui.exe`
cross-compiled from Linux has no icon resource; that is a deliberate trade so
`cargo check --target x86_64-pc-windows-msvc` keeps working on a Linux box.

`pdfium.dll` sits in `bin`, beside the executables, because that is the first
place sekio looks — and the first place Windows' own loader looks. Do not move
it to the install root.

Two things the user can untick in the feature tree:

- **PATH Environment Variable** — adds that `bin` directory to the system PATH.
- **Start Menu Shortcut** — a "sekio" entry that opens `sekio-gui`, the
  graphical previewer, which is what someone clicking a Start Menu entry
  expects. It used to point at `sekio-tui`, back when `sekio-gui` required a
  path argument and a no-argument launch failed immediately; it now opens to a
  file browser. `sekio-gui` is also wired into Explorer's "Open with" — see
  `docs/desktop.md`.

### `packaging/wix/main.wxs` is checked in, on purpose

Do not replace it with `cargo wix init` at build time. `init` and `print` mint a
fresh `UpgradeCode` on every run, and a changing `UpgradeCode` turns what should
be an upgrade into a second, side-by-side installation. The same applies to the
`Path` component's GUID. **Never change either GUID in that file.**

`packaging/wix/License.rtf` is the EULA shown in the installer, generated once
with `cargo wix print MIT`.

Note that `main.wxs` declares `encoding='windows-1252'`, so keep it ASCII — a
UTF-8 em dash in a comment is enough to make it fail to parse. XML comments also
cannot contain a double hyphen, which is why the build commands live here rather
than in a comment in that file.

### Building it

WiX v3 is Windows-only; this cannot be built or fully validated on Linux. From
the workspace root, on Windows:

```powershell
choco install wixtoolset -y          # v3.x. NOT `dotnet tool install wix` (v4/v5)
cargo install cargo-wix --locked

# main.wxs references both of these; light fails late without them.
mkdir packaging\vendor\pdfium
curl.exe -fsSL -o $env:TEMP\pdfium.tgz `
  https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8009/pdfium-win-x64.tgz
tar -xzf $env:TEMP\pdfium.tgz -C packaging\vendor\pdfium --strip-components=1 `
  bin/pdfium.dll licenses/pdfium.txt
move packaging\vendor\pdfium\pdfium.txt packaging\vendor\pdfium\LICENSE-pdfium.txt

cargo build --release --features sekio-core/pdf-render `
  -p sekio-cli -p sekio-tui -p sekio-gui
cargo wix -p sekio-cli --no-build --nocapture
```

`[package.metadata.wix]` sets `no-build`, because cargo-wix's own build step
would compile only `sekio-cli` and leave the other two `.exe` missing — light
then fails late with an unhelpful "cannot find file". Every path in that section
is relative to the current directory, so all of this must run from the workspace
root.

On a Linux machine you can still get most of the way: `cargo wix -p sekio-cli -vvvv`
resolves the manifest, the `.wxs`, and the full candle command line before
failing on the missing `candle.exe`, and the XML can be checked against WiX v3's
own `wix.xsd` with `xmllint --schema`. What only CI can prove is candle/light
actually compiling and linking it, ICE validation, and the installer running.
Use `workflow_dispatch` on the release workflow to exercise the MSI job without
cutting a release.

## Arch Linux (AUR)

`PKGBUILD` builds all three binaries with `--all-features` and installs
`sekio.desktop` so file managers can offer sekio under "Open with".

`--all-features` includes `pdf-render`, but the AUR package vendors no pdfium
of its own — unlike the `.deb`/`.rpm`/`.msi` it relies on the system library,
which on Arch means the `pdfium-binaries` AUR package. Without it PDFs fall
back to text, and a scanned one to the metadata card. Adding
`pdfium-binaries: PDF page previews` to `optdepends` would say so properly.

To publish: clone the AUR repo, copy `PKGBUILD`, then

```sh
updpkgsums                       # refreshes sha256sums from the real tarball
makepkg --printsrcinfo > .SRCINFO
makepkg -si                      # verify it builds and installs cleanly
```

## Windows (winget)

winget requires a three-file manifest set (version, installer, locale) under
`manifests/h/hairbui76/sekio/<version>/`. `InstallerType: msi` needs no
`NestedInstallerFiles` entries and gives winget a real uninstall entry. Generate the skeleton with
`wingetcreate new <release-url>` rather than hand-writing it — it computes the
installer hash and validates the schema — then submit via `wingetcreate submit`.

## Not yet packaged

Homebrew and any macOS target are deliberately out of scope; see ROADMAP.md.
