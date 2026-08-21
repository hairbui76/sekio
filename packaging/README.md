# Packaging

Release artifacts are produced by `.github/workflows/release.yml` when a `v*`
tag is pushed. As of v0.2.0 that is:

| Asset | Contents |
|---|---|
| `sekio-x86_64-unknown-linux-gnu.tar.gz` | `sekio`, `sekio-tui`, `sekio-gui` |
| `sekio-x86_64-pc-windows-msvc.zip` | all three `.exe` |
| `sekio_<version>-1_amd64.deb` | all three + desktop entry + systemd user unit |
| `sekio-<version>-1.x86_64.rpm` | same as the `.deb` |
| `sekio-x86_64-pc-windows-msvc.msi` | all three `.exe`, PATH entry, Start Menu shortcut |

Every one of them is published with a `.sha256` beside it containing the bare
hash, which is what `install.sh` verifies against and what Scoop's `autoupdate`
reads.

Releases are x86_64 only: the `.deb`, `.rpm` and `.msi` are the supported
install paths, with the portable archives kept because `install.sh` and the
Scoop manifest download them. Other architectures build from source.

## Which install path to document to a user

| Platform | Command |
|---|---|
| Linux, any distro | `curl -fsSL https://raw.githubusercontent.com/hairbui76/sekio/main/install.sh \| sh` |
| Debian / Ubuntu | `sudo apt install ./sekio_<version>-1_amd64.deb` |
| Fedora / RHEL / openSUSE | `sudo dnf install ./sekio-<version>-1.x86_64.rpm` |
| Arch | AUR `PKGBUILD` in this directory |
| Windows | the `.msi`, or `scoop install sekio` |
| From source | `cargo install --path crates/sekio-cli` (and `-tui`, `-gui`) |

## `install.sh` (curl-pipe-sh, Linux)

```sh
curl -fsSL https://raw.githubusercontent.com/hairbui76/sekio/main/install.sh | sh
```

Detects the architecture, resolves the latest release tag from the GitHub API,
downloads the archive **and its `.sha256`**, verifies the checksum before
extracting anything, and installs the binaries to `~/.local/bin`.

The checksum step is not optional decoration. It is the only thing standing
between a curl-pipe-sh installer and executing whatever arrives over the wire,
so it fails loudly and installs nothing on a mismatch. There is no flag to skip
it, and the script refuses to continue if it cannot find `sha256sum`, `shasum`,
or `openssl`.

Options:

```sh
... | sh -s -- --version v0.2.0        # pin a release (or SEKIO_VERSION=v0.2.0)
... | sh -s -- --prefix /usr/local/bin # install elsewhere (or SEKIO_PREFIX=...)
... | sh -s -- --uninstall             # remove the binaries again
```

It is POSIX `sh`, never reads stdin (so piping it is safe), cleans up its temp
directory with a `trap`, and warns with a copy-pasteable line if the install
prefix is not on `$PATH`.

## Debian/Ubuntu (`.deb`) and Fedora/RHEL (`.rpm`)

Both are built from the same metadata in `crates/sekio-cli/Cargo.toml` — one
`sekio` package with all three frontends, rather than three packages. Download
from the release page, then:

```sh
sudo apt install ./sekio_0.2.0-1_amd64.deb     # Debian/Ubuntu
sudo dnf install ./sekio-0.2.0-1.x86_64.rpm    # Fedora/RHEL/openSUSE
```

Layout:

```
/usr/bin/sekio, sekio-tui, sekio-gui
/usr/share/applications/sekio.desktop      "Open with" entry in file managers
/usr/lib/systemd/user/sekio.service        the preview daemon (NOT auto-enabled)
/usr/share/doc/sekio/                      README, integration.md, desktop.md
/usr/share/doc/sekio/copyright             (deb)
/usr/share/licenses/sekio/LICENSE-MIT      (rpm)
```

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

cargo build --release --features sekio-core/video \
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

- `--features sekio-core/video` is on for the packages but **not** for the
  portable archives. The video renderer only shells out to
  ffmpegthumbnailer/ffmpeg at runtime, so compiling it in adds no build
  dependency — and it is what makes `Recommends: ffmpegthumbnailer | ffmpeg`
  honest. A tarball cannot express an optional dependency, so it stays on
  defaults. `pdf-render` is left out of both: pdfium is not packaged by Debian or
  Fedora, so a dependency would have nothing to point at.
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

It installs all three `.exe` into `C:\Program Files\sekio\bin`, and offers two
things the user can untick in the feature tree:

- **PATH Environment Variable** — adds that `bin` directory to the system PATH.
- **Start Menu Shortcut** — a "sekio" entry that opens `sekio-tui`. It points
  at the TUI rather than the GUI because `sekio-gui` requires a path argument,
  so a no-argument menu entry for it would open and immediately fail. The GUI
  is reached from Explorer's "Open with" instead — see `docs/desktop.md`.

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

cargo build --release -p sekio-cli -p sekio-tui -p sekio-gui
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

To publish: clone the AUR repo, copy `PKGBUILD`, then

```sh
updpkgsums                       # refreshes sha256sums from the real tarball
makepkg --printsrcinfo > .SRCINFO
makepkg -si                      # verify it builds and installs cleanly
```

## Windows (Scoop)

`scoop/sekio.json` has `checkver`/`autoupdate` wired to GitHub releases, so it
tracks new tags on its own. Its `hash.url` reads the `.sha256` published beside
each archive, which the release workflow produces.

Submit to the `extras` bucket, or host your own:

```sh
scoop bucket add sekio https://github.com/hairbui76/scoop-sekio
scoop install sekio
```

## Windows (winget)

winget requires a three-file manifest set (version, installer, locale) under
`manifests/h/hairbui76/sekio/<version>/`. Now that an `.msi` exists, prefer it
over the zip form — `InstallerType: msi` needs no `NestedInstallerFiles`
entries and gives winget a real uninstall entry. Generate the skeleton with
`wingetcreate new <release-url>` rather than hand-writing it — it computes the
installer hash and validates the schema — then submit via `wingetcreate submit`.

## Not yet packaged

Homebrew and any macOS target are deliberately out of scope; see ROADMAP.md.
