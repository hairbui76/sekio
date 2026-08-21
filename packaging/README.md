# Packaging

Release artifacts are produced by `.github/workflows/release.yml` when a `v*`
tag is pushed: `sekio-<target>.tar.gz` for Linux (x86_64, aarch64) and
`sekio-<target>.zip` for Windows, each containing `sekio`, `sekio-tui`, and
`sekio-gui` (the GUI is omitted from cross-compiled targets).

The manifests here point at the real repository but still carry placeholder
checksums, because nothing has been released yet. Fill in the real hashes when
the first `v*` tag is pushed and the release artifacts exist — see each
section below for how.

## Install from source

```sh
cargo install --path crates/sekio-cli     # sekio
cargo install --path crates/sekio-tui     # sekio-tui
cargo install --path crates/sekio-gui     # sekio-gui
```

Optional formats are off by default because they need external dependencies:

```sh
cargo install --path crates/sekio-cli --features sekio-core/pdf     # needs pdfium
cargo install --path crates/sekio-cli --features sekio-core/video   # needs ffmpeg
```

## Arch Linux (AUR)

`PKGBUILD` builds all three binaries with `--all-features` and installs
`sekio.desktop` so file managers can offer sekio under "Open with".

To publish: clone the AUR repo, copy `PKGBUILD`, then

```sh
updpkgsums                       # fills sha256sums from the real tarball
makepkg --printsrcinfo > .SRCINFO
makepkg -si                      # verify it builds and installs cleanly
```

Replace `sha256sums=('SKIP')` before publishing — `SKIP` disables integrity
checking and is only acceptable while testing locally.

## Windows (Scoop)

`scoop/sekio.json` has `checkver`/`autoupdate` wired to GitHub releases, so
once published it tracks new tags on its own. The `hash.url` field expects the
release to include a `.sha256` file next to each archive; either add that step
to the release workflow or replace `hash` with a literal checksum per release.

Submit to the `extras` bucket, or host your own:

```sh
scoop bucket add sekio https://github.com/hairbui76/scoop-sekio
scoop install sekio
```

## Windows (winget)

winget requires a three-file manifest set (version, installer, locale) under
`manifests/h/hairbui76/sekio/<version>/`. The archive form needs
`InstallerType: zip` plus `NestedInstallerFiles` entries for each `.exe`.
Generate the skeleton with `wingetcreate new <release-url>` rather than
hand-writing it — it computes the installer hash and validates the schema —
then submit via `wingetcreate submit`.

## Not yet packaged

Homebrew and any macOS target are deliberately out of scope; see ROADMAP.md.
