//! Asking GitHub whether there is a newer sekio, and handing the answer to the
//! platform's installer.
//!
//! Three things about the shape of this are deliberate.
//!
//! **It never checks on its own.** A file previewer that phones home the moment
//! it starts is a different program from the one people installed. Every
//! request here begins with a click on "Check for updates", and nothing in this
//! module runs otherwise.
//!
//! **It shells out to `curl` rather than linking an HTTP client.** Every TLS
//! stack available to Rust today bottoms out in C or assembly — `ring`,
//! `aws-lc-rs`, `openssl-sys` — and this workspace deliberately contains no
//! `-sys` crate at all, which is what lets `cargo check --target
//! x86_64-pc-windows-msvc` work from Linux with no MSVC toolchain. `curl` is
//! the same bargain `render/video.rs` makes with ffmpeg: find it on PATH, run
//! it under a deadline, treat its absence as an ordinary outcome — and start
//! it with no console of its own, or Windows flashes a black window over the
//! preview every time the check runs.
//!
//! **It reads the version out of a redirect, not out of the API.**
//! `/releases/latest` redirects to `/releases/tag/vX.Y.Z`, so the version is in
//! the final URL and no JSON has to be parsed. The asset names have carried
//! their version since 0.16.1, so the download URL follows from the version
//! alone.
//!
//! sekio does not become a package manager. It downloads the right file and
//! hands it to whatever installs such files on this system — `msiexec` on
//! Windows, the desktop's package handler on Linux — both of which will ask
//! for the elevation sekio does not have.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// Where releases live. One place, so the check and the download cannot drift.
const REPO: &str = "https://github.com/hairbui76/sekio";

/// Long enough for a slow link, short enough that a hung proxy does not leave
/// "Checking…" on screen forever.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// A download is a package, not a redirect: minutes, not seconds.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// What the settings menu is showing about updates.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    /// Nothing asked for yet.
    #[default]
    Idle,
    Checking,
    /// This build is the newest there is.
    Current,
    /// A newer release exists.
    Available(String),
    /// The check or the download could not be completed. Never fatal: the
    /// releases page is always still there to open by hand.
    Failed(String),
    Downloading(String),
    /// Downloaded and handed to the installer.
    Handed(PathBuf),
}

/// A version as three numbers, for comparing. Anything that does not parse is
/// not compared — an unreadable tag must not be announced as an upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    /// `v0.16.1`, `0.16.1`, `0.16.1-rc1` → `0.16.1`. `None` when the three
    /// numbers are not all there.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        // A pre-release suffix is dropped rather than ordered: sekio has never
        // published one, and inventing an ordering for a case that does not
        // occur is how you get a downgrade offered as an upgrade.
        let core = text
            .split(['-', '+'])
            .next()
            .unwrap_or(text)
            .trim_end_matches('.');
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self(major, minor, patch))
    }
}

/// Which package this system installs, and therefore which asset to fetch.
///
/// `None` where sekio was built from source or installed some other way: there
/// is no file to hand over, and the honest answer is to open the releases page
/// and let the reader choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    Msi,
    Deb,
    Rpm,
}

impl Package {
    /// The asset name for `version`, exactly as the release workflow writes it.
    pub fn asset(self, version: &str) -> String {
        match self {
            Self::Msi => format!("sekio-{version}-x86_64-pc-windows-msvc.msi"),
            Self::Deb => format!("sekio_{version}-1_amd64.deb"),
            Self::Rpm => format!("sekio-{version}-1.x86_64.rpm"),
        }
    }

    /// Where that asset can be downloaded from.
    pub fn url(self, version: &str) -> String {
        format!(
            "{REPO}/releases/download/v{version}/{}",
            self.asset(version)
        )
    }
}

/// What this system installs, decided by what is on disk rather than by what
/// was compiled: one Linux binary serves Debian and Fedora alike.
#[cfg(windows)]
pub fn package() -> Option<Package> {
    Some(Package::Msi)
}

#[cfg(not(windows))]
pub fn package() -> Option<Package> {
    package_from(
        Path::new("/etc/debian_version"),
        Path::new("/etc/redhat-release"),
    )
}

/// Split out so the decision can be asserted without inventing a filesystem.
#[cfg(not(windows))]
pub fn package_from(debian: &Path, redhat: &Path) -> Option<Package> {
    if debian.exists() {
        Some(Package::Deb)
    } else if redhat.exists() {
        Some(Package::Rpm)
    } else {
        None
    }
}

/// The releases page, for every path that cannot do better.
pub fn releases_page() -> String {
    format!("{REPO}/releases/latest")
}

/// The version named by the URL `/releases/latest` redirects to.
///
/// Split from the request so the parsing — the part that can be wrong in a
/// quiet way — is testable without a network.
pub fn version_from_redirect(url: &str) -> Option<String> {
    let tag = url.trim().rsplit("/tag/").next()?;
    if tag == url.trim() {
        return None;
    }
    let tag = tag.trim_end_matches('/');
    // Parsed only to reject nonsense; the string is what the download URL needs.
    Version::parse(tag)?;
    Some(tag.trim_start_matches('v').to_owned())
}

/// Decide what to show, given this build's version and the newest published.
pub fn compare(current: &str, latest: &str) -> State {
    let (Some(current), Some(newest)) = (Version::parse(current), Version::parse(latest)) else {
        return State::Failed("could not read the published version".to_owned());
    };
    if newest > current {
        State::Available(latest.trim_start_matches('v').to_owned())
    } else {
        State::Current
    }
}

// ---------------------------------------------------------------------------
// Doing it
// ---------------------------------------------------------------------------

/// Run a check on its own thread. The result arrives once, on the frame it
/// lands; `ctx` is woken so the menu repaints without the pointer moving.
pub fn check(ctx: egui::Context, current: &'static str) -> Receiver<State> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("sekio-update".to_owned())
        .spawn(move || {
            let state = match latest_version() {
                Ok(latest) => compare(current, &latest),
                Err(why) => State::Failed(why),
            };
            let _ = tx.send(state);
            ctx.request_repaint();
        });
    if spawned.is_err() {
        // The channel is already closed; the caller sees no message and stays
        // where it was rather than hanging on "Checking…".
    }
    rx
}

/// Download `version`'s package and hand it to the installer, off the UI
/// thread.
pub fn install(ctx: egui::Context, version: String) -> Receiver<State> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("sekio-update-download".to_owned())
        .spawn(move || {
            let state = match download_and_hand_over(&version) {
                Ok(path) => State::Handed(path),
                Err(why) => State::Failed(why),
            };
            let _ = tx.send(state);
            ctx.request_repaint();
        });
    let _ = spawned;
    rx
}

fn download_and_hand_over(version: &str) -> Result<PathBuf, String> {
    let Some(package) = package() else {
        return Err("this build was not installed from a package".to_owned());
    };
    let target = std::env::temp_dir().join(package.asset(version));
    fetch(&package.url(version), &target)?;
    hand_to_installer(&target)?;
    Ok(target)
}

/// `curl`, if there is one. Absence is an ordinary outcome, like ffmpeg's.
fn curl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| ["curl", "curl.exe"].map(|name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The final URL after following redirects, which is where the version is.
fn latest_version() -> Result<String, String> {
    let Some(curl) = curl() else {
        return Err("curl is not installed, so sekio cannot check".to_owned());
    };
    let mut command = Command::new(curl);
    sekio_core::process::hide_console(&mut command);
    command
        .arg("--silent")
        .arg("--show-error")
        .arg("--location")
        // Head only: the page body is a few hundred kilobytes of HTML and none
        // of it is wanted.
        .arg("--head")
        .arg("--max-time")
        .arg(CHECK_TIMEOUT.as_secs().to_string())
        .arg("--output")
        .arg(devnull())
        .arg("--write-out")
        .arg("%{url_effective}")
        .arg(releases_page())
        .stdin(Stdio::null())
        .stderr(Stdio::null());

    let out = command
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err("could not reach github.com".to_owned());
    }
    let url = String::from_utf8_lossy(&out.stdout);
    version_from_redirect(&url).ok_or_else(|| "github.com did not name a version".to_owned())
}

fn fetch(url: &str, target: &Path) -> Result<(), String> {
    let Some(curl) = curl() else {
        return Err("curl is not installed, so sekio cannot download".to_owned());
    };
    let mut command = Command::new(curl);
    sekio_core::process::hide_console(&mut command);
    let out = command
        .arg("--silent")
        .arg("--show-error")
        .arg("--location")
        // Anything but 2xx is a failure with no file written, rather than an
        // HTML error page saved under a .msi name.
        .arg("--fail")
        .arg("--max-time")
        .arg(DOWNLOAD_TIMEOUT.as_secs().to_string())
        .arg("--output")
        .arg(target)
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(target);
        return Err("the download did not complete".to_owned());
    }
    Ok(())
}

/// Hand the file to whatever installs such files here.
///
/// Neither branch installs anything itself: both raise the platform's own
/// installer, which is what asks for the elevation sekio does not have and
/// must not try to acquire.
#[cfg(windows)]
fn hand_to_installer(path: &Path) -> Result<(), String> {
    Command::new("msiexec")
        .arg("/i")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not start the installer: {e}"))
}

#[cfg(not(windows))]
fn hand_to_installer(path: &Path) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open the package: {e}"))
}

#[cfg(windows)]
fn devnull() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn devnull() -> &'static str {
    "/dev/null"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_and_without_the_v() {
        assert_eq!(Version::parse("0.16.1"), Some(Version(0, 16, 1)));
        assert_eq!(Version::parse("v0.16.1"), Some(Version(0, 16, 1)));
        assert_eq!(Version::parse(" v1.2.3 "), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("1.2.3-rc1"), Some(Version(1, 2, 3)));
    }

    #[test]
    fn nonsense_is_not_a_version() {
        for text in ["", "v", "1.2", "1.2.3.4", "latest", "v1.x.0", "1..3"] {
            assert_eq!(Version::parse(text), None, "{text:?} parsed");
        }
    }

    #[test]
    fn versions_order_by_number_not_by_string() {
        // The three cases a string comparison gets wrong. "0.9.0" sorts after
        // "0.16.0" as text, and "0.16.10" sorts before "0.16.2" — which is why
        // this is not done with `<` on `&str`.
        assert!(Version::parse("0.9.0") < Version::parse("0.16.0"));
        assert!(Version::parse("0.16.10") > Version::parse("0.16.2"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));

        // And the same cases through `compare`, which is what the menu shows.
        assert_eq!(
            compare("0.9.0", "v0.16.0"),
            State::Available("0.16.0".into())
        );
        assert_eq!(compare("0.16.10", "v0.16.2"), State::Current);
    }

    #[test]
    fn only_a_higher_version_is_an_update() {
        assert_eq!(
            compare("0.16.1", "v0.17.0"),
            State::Available("0.17.0".into())
        );
        assert_eq!(compare("0.16.1", "v0.16.1"), State::Current);
        // A rollback on the releases page is not an update.
        assert_eq!(compare("0.16.1", "v0.16.0"), State::Current);
    }

    #[test]
    fn an_unreadable_published_version_is_a_failure_not_an_update() {
        assert!(matches!(compare("0.16.1", "nightly"), State::Failed(_)));
        assert!(matches!(compare("nonsense", "v1.0.0"), State::Failed(_)));
    }

    #[test]
    fn the_version_comes_out_of_the_redirect_url() {
        assert_eq!(
            version_from_redirect("https://github.com/hairbui76/sekio/releases/tag/v0.16.1"),
            Some("0.16.1".to_owned())
        );
        assert_eq!(
            version_from_redirect("https://github.com/hairbui76/sekio/releases/tag/v1.2.3/"),
            Some("1.2.3".to_owned())
        );
    }

    #[test]
    fn a_url_that_did_not_redirect_to_a_tag_names_no_version() {
        for url in [
            "https://github.com/hairbui76/sekio/releases/latest",
            "https://github.com/login",
            "",
            "https://github.com/hairbui76/sekio/releases/tag/nightly",
        ] {
            assert_eq!(version_from_redirect(url), None, "{url:?}");
        }
    }

    /// The names here are the release workflow's, and a drift between the two
    /// is a download that 404s.
    #[test]
    fn asset_names_match_what_the_release_workflow_publishes() {
        assert_eq!(
            Package::Msi.asset("0.16.1"),
            "sekio-0.16.1-x86_64-pc-windows-msvc.msi"
        );
        assert_eq!(Package::Deb.asset("0.16.1"), "sekio_0.16.1-1_amd64.deb");
        assert_eq!(Package::Rpm.asset("0.16.1"), "sekio-0.16.1-1.x86_64.rpm");
    }

    #[test]
    fn a_download_url_is_the_tag_and_the_asset() {
        assert_eq!(
            Package::Deb.url("0.16.1"),
            "https://github.com/hairbui76/sekio/releases/download/v0.16.1/sekio_0.16.1-1_amd64.deb"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn the_package_follows_the_distribution_not_the_build() {
        let dir = std::env::temp_dir().join(format!("sekio-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let debian = dir.join("debian_version");
        let redhat = dir.join("redhat-release");
        let missing = dir.join("neither");

        std::fs::write(&debian, b"13\n").expect("write");
        assert_eq!(package_from(&debian, &missing), Some(Package::Deb));

        std::fs::write(&redhat, b"Fedora\n").expect("write");
        assert_eq!(package_from(&missing, &redhat), Some(Package::Rpm));

        // Debian wins when both are present: a system with both files is
        // overwhelmingly a Debian one carrying a compatibility shim.
        assert_eq!(package_from(&debian, &redhat), Some(Package::Deb));

        // Built from source, or something else entirely: no file to hand over.
        assert_eq!(package_from(&missing, &missing), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
