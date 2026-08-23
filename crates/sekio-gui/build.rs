//! Embeds the Windows icon resource into `sekio-gui.exe`.
//!
//! Without this, Explorer, the taskbar and the Start Menu shortcut installed by
//! `packaging/wix/main.wxs` all show the default "unknown application" icon.
//! `sekio` and `sekio-tui` deliberately get no build script: they are console
//! programs, and a console program's icon is the terminal's, not its own.
//!
//! This is separate from, and does not replace, the *window* icon in
//! `src/icon.rs`. The resource below is what the shell reads off the file on
//! disk; `IconData` is what the compositor puts in the title bar of a running
//! window. Both are needed, and both come from `assets/`.
//!
//! **Why `winresource`.** It is the maintained fork of the unmaintained
//! `winres`, and it does exactly this one job declaratively — icon plus the
//! VersionInfo strings Explorer's Properties tab reads — with no `.rc` file to
//! check in and keep in sync with `Cargo.toml`. `embed-resource`, the other
//! candidate, is the more general tool: it compiles a `.rc` file you write
//! yourself, which is what you want when the resource script grows manifests,
//! dialogs or string tables. This one needs an icon and four strings, so the
//! declarative crate is the smaller moving part. Both ultimately shell out to
//! the same `rc.exe`, so the choice costs nothing either way at build time.
//!
//! **Why the host gate, not a target gate.** `#[cfg(windows)]` here is the
//! *host* — build scripts are compiled and run for the host — and so is the
//! `[target.'cfg(windows)'.build-dependencies]` entry that provides the crate,
//! which keeps the two exactly in step. That matters because embedding a
//! resource means running the Windows Resource Compiler, and
//! `cargo check --target x86_64-pc-windows-msvc` on Linux (see CLAUDE.md — it
//! is the standing Windows safety net, and it works precisely because nothing
//! in this tree needs an MSVC toolchain) *does* run build scripts. A target
//! gate would send that check looking for `rc.exe` on a Linux box and fail it.
//!
//! The trade-off is explicit: a `sekio-gui.exe` cross-built from Linux carries
//! no icon resource. The shipped one is not — `.github/workflows/release.yml`
//! builds the `.msi` job on `windows-latest`, where this runs and the Windows
//! SDK's `rc.exe` is present.

fn main() {
    // Not conditional: cargo only re-runs a build script when something it was
    // told to watch changes, and once *any* `rerun-if-changed` is printed the
    // default "rerun on any change in the package" rule is off. Printing these
    // on every host keeps the rule the same everywhere.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/sekio.ico");

    #[cfg(windows)]
    embed_icon();
}

#[cfg(windows)]
fn embed_icon() {
    use std::path::PathBuf;

    // `assets/` sits at the workspace root, two levels above this manifest.
    let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/sekio.ico");
    // A build-time panic, which is a failed build with a clear message — not a
    // runtime one. A Windows build that quietly produced an icon-less binary
    // is exactly the bug this file exists to fix, so it must be loud.
    assert!(
        icon.exists(),
        "missing {} — regenerate it with `python3 assets/generate.py`",
        icon.display()
    );

    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon.to_string_lossy());
    // What Explorer's Properties > Details tab shows. `winresource` fills in
    // FileVersion and ProductVersion from `CARGO_PKG_VERSION` on its own;
    // these are the ones it would otherwise leave to the crate name.
    res.set("ProductName", "sekio");
    res.set("FileDescription", "sekio — quick-view any file");
    res.set("LegalCopyright", "MIT licensed. sekio contributors.");
    res.set("OriginalFilename", "sekio-gui.exe");

    if let Err(err) = res.compile() {
        // Also a failed build rather than a `cargo:warning`. The whole point of
        // this file is that an icon-less sekio-gui.exe is a bug, and a warning
        // in a build log is exactly how that bug would ship a second time.
        //
        // The one thing that realistically goes wrong here is a Windows host
        // with no Resource Compiler: `winresource` finds `rc.exe` through the
        // Windows SDK, so a Rust-only toolchain (rustup + the MSVC linker, no
        // SDK) will land here. Both CI jobs that build on windows-latest have
        // the SDK.
        panic!(
            "failed to embed the Windows icon resource: {err}\n\
             This needs rc.exe from the Windows SDK. Install the \
             \"Desktop development with C++\" workload (or the standalone \
             Windows SDK) and build again."
        );
    }
}
