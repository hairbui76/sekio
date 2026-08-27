//! The tray icon: proof that a resident sekio is there, and a way to reach it.
//!
//! Two backends, because there is no portable way to do this and the obvious
//! crate is not an option — `tray-icon` reaches the Linux tray through
//! libappindicator via GTK, and a C dependency would break the rule that keeps
//! this project's Windows and cross builds working (see CLAUDE.md).
//!
//! - Linux: StatusNotifierItem over D-Bus (`ksni`), pure Rust. Hosted by KDE,
//!   XFCE and Cinnamon; on GNOME **only** with the AppIndicator extension.
//! - Windows: `Shell_NotifyIcon`, through the `windows` crate already here.
//!
//! `spawn` returning `None` is an ordinary outcome, not a failure: plenty of
//! sessions have nowhere to put an icon. The daemon must keep serving its
//! socket and its hotkey regardless — the tray is an affordance, never the
//! mechanism.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

#[cfg(unix)]
pub mod linux;
#[cfg(windows)]
pub mod windows;

/// What the menu should currently show. Rebuilt and handed to `Tray::update`
/// whenever the hotkey or the recent list changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The hotkey as the user would type it, e.g. `"Ctrl+Shift+Space"`.
    /// `None` when none is registered.
    pub hotkey: Option<String>,
    /// Alternatives offered for changing it. Capturing a real keypress needs a
    /// window, and the tray has none, so the menu offers a short list instead.
    pub hotkey_choices: Vec<String>,
    /// Most-recent-first, already filtered to files that still exist.
    pub recent: Vec<PathBuf>,
}

/// What the user asked for by clicking. The daemon acts on these; the tray
/// never previews anything itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Bring the window up. What activating the icon means — a resident
    /// daemon's window is hidden, and putting it back on screen is the whole
    /// reason to click an icon that represents it.
    Show,
    /// Show the file dialog, as the window's Open button does.
    OpenFile,
    /// Look for a newer release.
    CheckUpdate,
    /// Preview a path — a recent entry was chosen.
    Preview(PathBuf),
    /// Register this hotkey instead, and persist it.
    SetHotkey(String),
    /// Stop the daemon.
    Quit,
}

/// A live tray icon. Dropping it removes the icon.
pub trait Tray: Send {
    /// Replace the menu contents. Called on the UI thread, so it must not block.
    fn update(&mut self, menu: &Menu);
    /// What is hosting the icon, for `--doctor`.
    fn describe(&self) -> String;
}

/// Start the tray for this platform.
///
/// `icon` is a PNG. Returns `None` when there is no tray host — a headless
/// session, or GNOME without the AppIndicator extension — in which case the
/// caller carries on without one.
pub fn spawn(
    icon: &'static [u8],
    menu: Menu,
    wake: impl Fn() + Send + 'static,
) -> Option<(Box<dyn Tray>, Receiver<Event>)> {
    #[cfg(unix)]
    {
        linux::spawn(icon, menu, wake)
    }
    #[cfg(windows)]
    {
        windows::spawn(icon, menu, wake)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (icon, menu);
        None
    }
}

/// The hotkeys offered in the menu. Deliberately short and deliberately all
/// modified: an unmodified key grabbed globally is taken away from every other
/// application, which is why `Space` alone is not among them.
pub fn hotkey_choices() -> Vec<String> {
    [
        "Ctrl+Shift+Space",
        "Ctrl+Alt+Space",
        "Super+P",
        "Alt+F1",
        "Ctrl+Shift+P",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offered_hotkeys_all_carry_a_modifier() {
        for spec in hotkey_choices() {
            assert!(
                spec.contains('+'),
                "{spec} has no modifier; grabbing it would steal the key from every other app"
            );
        }
    }

    #[test]
    fn a_menu_compares_by_value_so_updates_can_be_skipped() {
        let a = Menu {
            hotkey: Some("Ctrl+Shift+Space".into()),
            hotkey_choices: hotkey_choices(),
            recent: vec![PathBuf::from("/tmp/a.txt")],
        };
        assert_eq!(a.clone(), a);
    }
}
