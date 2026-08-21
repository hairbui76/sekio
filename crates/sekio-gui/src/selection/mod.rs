//! Finding the file the user means when they press the hotkey.
//!
//! macOS Quick Look can just ask the Finder. Nothing equivalent exists on the
//! platforms sekio targets, so each one gets its own strategy behind a common
//! trait, and every strategy is allowed to fail: a hotkey press that cannot
//! resolve a file does nothing visible rather than guessing wrong.
//!
//! - Windows: ask Explorer over COM for the focused window's selection.
//! - Linux: no universal API. Best effort per desktop, then the clipboard.
//!
//! Both are `Source` implementations, and the daemon only ever sees the trait.

use std::path::PathBuf;

#[cfg(unix)]
pub mod linux;
#[cfg(windows)]
pub mod windows;

/// Where a resolved path came from. Shown in `--doctor` output so a user can
/// tell "sekio found nothing" from "sekio fell back to the clipboard".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Read from the file manager's actual selection.
    FileManager,
    /// A path sitting in the clipboard.
    ///
    /// Only the Linux strategy falls back this far: Explorer answers directly,
    /// so on Windows this variant is legitimately never constructed. Scoped to
    /// the variant rather than the enum so a genuinely dead `FileManager`
    /// would still be reported.
    #[cfg_attr(windows, allow(dead_code))]
    Clipboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub path: PathBuf,
    pub origin: Origin,
}

impl Selection {
    pub fn new(path: PathBuf, origin: Origin) -> Self {
        Self { path, origin }
    }
}

/// A way to discover what the user currently has selected.
///
/// Implementations must be cheap and non-blocking: this runs on a hotkey
/// press, and anything slow shows up as lag between the keypress and the
/// window. Anything that could hang (an IPC call, a subprocess) must be
/// bounded by a short timeout and give up rather than stall.
pub trait Source: Send + Sync {
    /// The selected file, or `None` when it cannot be determined. `None` is a
    /// normal outcome, not an error — no file manager focused, nothing
    /// selected, or a desktop this strategy does not understand.
    fn current(&self) -> Option<Selection>;

    /// Human-readable name of the strategy, for `--doctor`.
    fn describe(&self) -> &'static str;
}

/// The strategy for this platform.
pub fn for_this_platform() -> Box<dyn Source> {
    #[cfg(windows)]
    {
        Box::new(windows::Explorer::new())
    }
    #[cfg(unix)]
    {
        Box::new(linux::Desktop::new())
    }
}

/// Reject anything that is not a readable file or directory before handing it
/// to the previewer. A clipboard in particular holds arbitrary text, and most
/// of it is not a path.
pub fn usable(path: &std::path::Path) -> bool {
    path.is_absolute() && path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_rejects_non_paths_and_relatives() {
        assert!(!usable(std::path::Path::new("just some copied text")));
        assert!(!usable(std::path::Path::new("relative/path")));
        assert!(!usable(std::path::Path::new("/definitely/not/here/at/all")));
    }

    #[test]
    fn usable_accepts_a_real_absolute_path() {
        let dir = std::env::temp_dir();
        assert!(usable(&dir), "temp dir should be usable: {}", dir.display());
    }

    #[test]
    fn the_platform_source_describes_itself() {
        let source = for_this_platform();
        assert!(!source.describe().is_empty());
        // Must not panic or block when nothing is selected.
        let _ = source.current();
    }
}
