//! The native "Open file" dialog, kept off the UI thread.
//!
//! `rfd` offers a blocking API and an async one. We use the **blocking** one on
//! a dedicated thread, and deliver the chosen path back over an `mpsc` channel
//! exactly like `worker.rs` does, because:
//!
//! * the async API returns a bare `Future` with no executor attached, so
//!   driving it would mean adding one (`pollster`, `futures`) purely to wait
//!   for a modal dialog — and on both of our platforms `rfd` implements that
//!   future by spawning a thread and blocking on it anyway;
//! * `rfd::FileDialog` is `Send`, and its Windows backend calls `CoInitializeEx`
//!   on whichever thread runs it, so a fresh thread is the supported way there;
//! * the UI already has a channel-plus-`request_repaint` pattern for exactly
//!   this shape of work. One mechanism, not two.
//!
//! Calling the blocking dialog on the UI thread would be a freeze at best: on
//! Linux the XDG portal path blocks on a DBus read with no timeout, so a portal
//! that never answers would hang the event loop forever.
//!
//! Availability is the other half. On Linux `rfd` talks to the XDG desktop
//! portal over the session bus and falls back to `zenity`; with neither
//! present there is no dialog to open, and pressing the button must open the
//! built-in browser instead of appearing to do nothing.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// What this session can offer when the user asks to open a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// A native dialog can be shown.
    Native,
    /// It cannot, and this is why. The caller opens the built-in browser.
    Unavailable(&'static str),
}

/// The facts the decision is made from, separated out so the rule is testable
/// without an environment to fake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Probe {
    /// The platform's dialog needs no external service (Windows).
    pub built_in: bool,
    /// A DBus session bus this process can reach — where the XDG desktop
    /// portal lives.
    pub session_bus: bool,
    /// `zenity` on `PATH`, which is what `rfd` falls back to when the portal
    /// does not answer.
    pub zenity: bool,
}

/// Why we would rather show the built-in browser.
const NO_DIALOG: &str =
    "no desktop portal (no DBus session bus) and no zenity on PATH — using the built-in browser";

/// Pure: given what we found, is there a native dialog to show?
pub fn availability_from(probe: Probe) -> Availability {
    if probe.built_in || probe.session_bus || probe.zenity {
        Availability::Native
    } else {
        Availability::Unavailable(NO_DIALOG)
    }
}

/// Look at this machine. A handful of env reads and `stat`s — cheap enough to
/// do on demand, and deliberately not done at startup, which has a budget.
pub fn availability() -> Availability {
    availability_from(probe())
}

fn probe() -> Probe {
    Probe {
        // `cfg!` rather than `#[cfg]` so both platforms compile the same code
        // and neither grows a dead branch.
        built_in: cfg!(windows),
        session_bus: session_bus(),
        zenity: on_path("zenity"),
    }
}

/// Is there a session bus? `$DBUS_SESSION_BUS_ADDRESS` is the answer when it is
/// set; otherwise the systemd-era default socket under `$XDG_RUNTIME_DIR`.
fn session_bus() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .is_some_and(|dir| dir.join("bus").exists())
}

/// Is `name` an executable on `PATH`? Existence is enough: we only need to know
/// whether `rfd`'s fallback has anything to spawn.
fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// Show the dialog on its own thread and send the result back.
///
/// `Some(path)` is a pick; `None` is "the user cancelled, or the dialog could
/// not be shown" — the two are indistinguishable through `rfd`, and both mean
/// the same thing to the UI: stop waiting, change nothing.
///
/// Returns whether the thread started. A refused spawn is reported as a
/// `None` on the channel by the caller, never a panic.
pub fn spawn(start: Option<PathBuf>, tx: Sender<Option<PathBuf>>, ctx: egui::Context) -> bool {
    std::thread::Builder::new()
        .name("sekio-open-dialog".to_owned())
        .spawn(move || {
            // The guard reports *something* even if `rfd` panics on the way
            // (its portal backend has unwraps of its own). Without it a panic
            // here would leave the UI thinking a dialog is still open, with no
            // way to ask for another.
            let mut report = Report {
                tx,
                ctx,
                sent: false,
            };
            let mut dialog = rfd::FileDialog::new().set_title("Open with sekio");
            if let Some(dir) = start.filter(|dir| dir.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            // No `set_parent`: it would mean shipping a raw window handle to
            // this thread, and the only thing it buys is modality.
            let picked = dialog.pick_file();
            report.send(picked);
        })
        .is_ok()
}

/// Sends exactly once, whatever happens to the thread.
struct Report {
    tx: Sender<Option<PathBuf>>,
    ctx: egui::Context,
    sent: bool,
}

impl Report {
    fn send(&mut self, picked: Option<PathBuf>) {
        self.sent = true;
        let _ = self.tx.send(picked);
        self.ctx.request_repaint();
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.sent {
            let _ = self.tx.send(None);
            self.ctx.request_repaint();
        }
    }
}

/// One line for `--doctor` and `--probe`.
pub fn describe(availability: Availability) -> &'static str {
    match availability {
        Availability::Native => "yes — a native file dialog can be shown",
        Availability::Unavailable(reason) => reason,
    }
}

/// The paths `--doctor` looked at, so a "no" says where to go next.
pub fn evidence() -> [(&'static str, String); 2] {
    let probe = probe();
    [
        (
            "portal",
            if probe.built_in {
                "not needed on this platform".to_owned()
            } else if probe.session_bus {
                "a DBus session bus is reachable".to_owned()
            } else {
                "no DBus session bus".to_owned()
            },
        ),
        (
            "zenity",
            if probe.zenity {
                "on PATH (rfd's fallback)".to_owned()
            } else {
                "not on PATH".to_owned()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_always_has_a_dialog() {
        assert_eq!(
            availability_from(Probe {
                built_in: true,
                session_bus: false,
                zenity: false,
            }),
            Availability::Native
        );
    }

    #[test]
    fn a_portal_or_zenity_is_enough_on_linux() {
        let portal = Probe {
            built_in: false,
            session_bus: true,
            zenity: false,
        };
        let zenity = Probe {
            built_in: false,
            session_bus: false,
            zenity: true,
        };
        assert_eq!(availability_from(portal), Availability::Native);
        assert_eq!(availability_from(zenity), Availability::Native);
    }

    #[test]
    fn with_neither_the_browser_takes_over() {
        let bare = Probe::default();
        match availability_from(bare) {
            Availability::Unavailable(reason) => {
                assert!(
                    reason.contains("built-in browser"),
                    "the reason must point at the fallback: {reason}"
                );
            }
            Availability::Native => panic!("a bare session has no native dialog"),
        }
        assert_eq!(describe(availability_from(bare)), NO_DIALOG);
    }

    #[test]
    fn probing_this_machine_never_panics() {
        // Whatever this box has, asking is safe and answers something.
        let _ = availability();
        let _ = evidence();
    }
}
