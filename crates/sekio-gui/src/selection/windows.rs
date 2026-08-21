//! Asking Explorer, over COM, what the user currently has selected.
//!
//! Windows has no "give me the Finder selection" call. What it does have is
//! the shell's automation surface: every open Explorer window (and the desktop
//! itself) registers in the `ShellWindows` collection, and each registration
//! can be walked down to the view that owns the selection. The chain is
//!
//! ```text
//! GetForegroundWindow
//!   CLSID_ShellWindows -> IShellWindows -> Item(i) -> IDispatch
//!     IServiceProvider -> QueryService(SID_STopLevelBrowser) -> IShellBrowser
//!       IOleWindow::GetWindow  ... is this the focused window?
//!       QueryActiveShellView -> IShellView -> IFolderView2
//!         GetSelection -> IShellItemArray -> IShellItem
//!           GetDisplayName(SIGDN_FILESYSPATH) -> a real path, or nothing
//! ```
//!
//! Every one of those steps is allowed to fail, and a failure anywhere means
//! "sekio does not know what you meant" — never an error, never a panic. This
//! runs on the hotkey thread of a resident daemon, so a panic would take the
//! user's background process with it, and a stall would show up as lag between
//! the keypress and the window.

use std::ffi::c_void;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IServiceProvider, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{
    IFolderView2, IShellBrowser, IShellItemArray, IShellWindows, SID_STopLevelBrowser,
    ShellWindows, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, IsChild, GA_ROOT};

use super::{Origin, Selection, Source};

/// How many selected items we are willing to inspect before giving up.
///
/// sekio previews one file, so the answer is nearly always item 0. The cap
/// only exists so that selecting ten thousand virtual items cannot turn one
/// keypress into ten thousand cross-process COM calls.
const MAX_CANDIDATES: usize = 32;

/// Reads the focused Explorer window's selection.
pub struct Explorer;

impl Explorer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Explorer {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for Explorer {
    fn current(&self) -> Option<Selection> {
        // The apartment guard must outlive every COM pointer taken out under
        // it, so the whole walk happens in a callee that returns a plain
        // `PathBuf`: by the time `apartment` drops, nothing COM is alive.
        let apartment = Apartment::enter()?;
        let found = foreground_selection();
        drop(apartment);
        Some(Selection::new(found?, Origin::FileManager))
    }

    fn describe(&self) -> &'static str {
        "Windows Explorer (COM)"
    }
}

/// This thread's COM apartment, for the duration of one lookup.
///
/// The hotkey thread is not the UI thread and has no apartment of its own, so
/// we make one. If the thread already belongs to an apartment of a different
/// kind, `CoInitializeEx` refuses with `RPC_E_CHANGED_MODE`; that is fine —
/// COM will marshal for us — but the apartment is then somebody else's and we
/// must not tear it down.
struct Apartment {
    /// True only when this guard is the one that initialised COM here, and
    /// therefore owes a matching `CoUninitialize`.
    owned: bool,
}

impl Apartment {
    fn enter() -> Option<Self> {
        // SAFETY: `CoInitializeEx` borrows nothing and is callable from any
        // thread at any time; it reports its outcome entirely through the
        // returned HRESULT rather than through out-parameters.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // Already initialised with a different threading model. Proceed,
            // but leave the apartment exactly as we found it.
            Some(Self { owned: false })
        } else if hr.is_ok() {
            // S_OK (fresh) and S_FALSE (nested) both count as an
            // initialisation that has to be balanced.
            Some(Self { owned: true })
        } else {
            None
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances exactly one successful `CoInitializeEx` on this
            // same thread. `Explorer::current` drops every interface pointer
            // before this guard, so nothing outlives the apartment.
            unsafe { CoUninitialize() };
        }
    }
}

/// Walk the shell windows, find the one that owns the focused HWND, and return
/// the first previewable path in its selection.
fn foreground_selection() -> Option<PathBuf> {
    // SAFETY: `GetForegroundWindow` takes no arguments and returns a plain
    // handle value, or null when nothing is focused.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }

    // SAFETY: `ShellWindows` is the well-known CLSID for the shell's window
    // collection and `IShellWindows` is the interface it publishes. A missing
    // or unreachable Explorer surfaces as an `Err`, never as a bad pointer.
    let shell_windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }.ok()?;

    // SAFETY: `shell_windows` is a live interface pointer owned by this frame.
    let count = unsafe { shell_windows.Count() }.ok()?;

    for index in 0..count {
        let index = VARIANT::from(index);
        // SAFETY: `shell_windows` is live and `index` is a VT_I4 variant we
        // own; `Item` copies it and does not take ownership of the payload.
        // Windows opening or closing mid-walk makes this fail rather than
        // return garbage, so a failure just means "skip this one".
        let Ok(dispatch) = (unsafe { shell_windows.Item(&index) }) else {
            continue;
        };

        // Not every registered shell window is an Explorer browser (an
        // Internet Explorer leftover, a common dialog); those simply do not
        // offer the service, and QueryService says so.
        let Ok(provider) = dispatch.cast::<IServiceProvider>() else {
            continue;
        };
        // SAFETY: `provider` is live for this iteration, and `SID_STopLevelBrowser`
        // is a static GUID that outlives the call.
        let Ok(browser) =
            (unsafe { provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) })
        else {
            continue;
        };

        // SAFETY: `browser` is live; `GetWindow` (via `IOleWindow`) only reads
        // the browser frame's handle into a local.
        let Ok(hwnd) = (unsafe { browser.GetWindow() }) else {
            continue;
        };
        if !owns_focus(hwnd, foreground) {
            continue;
        }

        // This is the window the user is looking at. Whatever happens from
        // here, no other shell window is a better answer, so stop walking.
        return selection_of(&browser);
    }

    None
}

/// The selection of one shell browser, as a path sekio can preview.
fn selection_of(browser: &IShellBrowser) -> Option<PathBuf> {
    // SAFETY: `browser` is a live interface pointer borrowed from the caller.
    let view = unsafe { browser.QueryActiveShellView() }.ok()?;
    let folder = view.cast::<IFolderView2>().ok()?;

    // SAFETY: `folder` is live. `false` is `fNoneImpliesFolder`: an empty
    // selection must stay empty rather than quietly standing in for the folder
    // being browsed, because "nothing selected" has to mean "do nothing".
    let items = unsafe { folder.GetSelection(false) }.ok()?;
    // SAFETY: `items` is live and owned by this frame.
    let count = unsafe { items.GetCount() }.ok()?;

    // Lazily, so a one-item selection costs exactly one `GetDisplayName`.
    pick_previewable((0..count).map(|i| item_path(&items, i)), super::usable)
}

/// Whether `hwnd` is, or lives inside, the focused top-level window.
///
/// An Explorer browser's own HWND is the top-level frame, so equality covers
/// the common case. The desktop is the reason for the other two checks: its
/// shell view is a child of Progman/WorkerW, and the foreground window when
/// the desktop has focus is that parent, not the view.
fn owns_focus(hwnd: HWND, foreground: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    if hwnd == foreground {
        return true;
    }
    // SAFETY: both handles are plain values and both calls are pure queries
    // that answer null/false for handles that have since gone away.
    unsafe { GetAncestor(hwnd, GA_ROOT) == foreground || IsChild(foreground, hwnd).as_bool() }
}

/// The filesystem path of one item in a shell selection, if it has one.
///
/// Virtual items — Recycle Bin, "This PC", a Control Panel entry, a library
/// header, an unsynced cloud placeholder — have no `SIGDN_FILESYSPATH`, and
/// the call fails for them. That failure is the point: it is what keeps a
/// pathless shell item from reaching the previewer.
fn item_path(items: &IShellItemArray, index: u32) -> Option<PathBuf> {
    // SAFETY: `items` is a live pointer borrowed from the caller and `index`
    // is below the count it just reported. A racing deselection makes this
    // fail rather than read out of bounds.
    let item = unsafe { items.GetItemAt(index) }.ok()?;

    // SAFETY: `item` is live. On success the shell hands us a NUL-terminated
    // UTF-16 string allocated with CoTaskMemAlloc, which we now own.
    let name: PWSTR = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    if name.is_null() {
        return None;
    }

    // SAFETY: `name` is non-null and NUL-terminated, so `as_wide` stops at the
    // terminator. The slice borrows the shell's buffer, so it is copied before
    // that buffer is released below.
    let wide = unsafe { name.as_wide() }.to_vec();

    // SAFETY: `name` came from the COM task allocator, is freed exactly once,
    // and is not touched again afterwards.
    unsafe { CoTaskMemFree(Some(name.0 as *const c_void)) };

    // `OsString::from_wide` rather than `to_string`: NTFS names are arbitrary
    // UTF-16 and need not be valid Unicode, and a lossy path is a wrong path.
    Some(PathBuf::from(OsString::from_wide(&wide)))
}

/// Choose the path to preview out of a selection, in view order.
///
/// `None` entries are items that have no filesystem path at all. They are
/// skipped rather than fatal, so selecting the Recycle Bin *and* a document
/// still previews the document. Among the items that do resolve, the first one
/// wins: sekio shows one file at a time, and "the first of what you picked" is
/// the only choice that needs no explaining.
fn pick_previewable<I>(candidates: I, usable: impl Fn(&Path) -> bool) -> Option<PathBuf>
where
    I: IntoIterator<Item = Option<PathBuf>>,
{
    candidates
        .into_iter()
        .take(MAX_CANDIDATES)
        .flatten()
        .find(|path| usable(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    // This whole module is `#[cfg(windows)]` from `selection/mod.rs`, so these
    // tests are not built on Linux at all. They are kept free of COM and of
    // the filesystem anyway — the `usable` predicate is injected — so they
    // describe the decision logic rather than the machine they run on, and
    // none of them needs a live Explorer.

    fn path(s: &str) -> Option<PathBuf> {
        Some(PathBuf::from(s))
    }

    /// Any path is fine; isolates ordering from filesystem state.
    fn anything(_: &Path) -> bool {
        true
    }

    #[test]
    fn an_empty_selection_yields_none() {
        assert_eq!(pick_previewable(Vec::new(), anything), None);
    }

    #[test]
    fn a_single_item_is_returned() {
        let picked = pick_previewable(vec![path(r"C:\notes\a.md")], anything);
        assert_eq!(picked, Some(PathBuf::from(r"C:\notes\a.md")));
    }

    #[test]
    fn the_first_of_several_is_chosen() {
        let picked = pick_previewable(
            vec![
                path(r"C:\notes\a.md"),
                path(r"C:\notes\b.md"),
                path(r"C:\notes\c.md"),
            ],
            anything,
        );
        assert_eq!(picked, Some(PathBuf::from(r"C:\notes\a.md")));
    }

    #[test]
    fn a_non_filesystem_item_is_rejected() {
        // A lone Recycle Bin selection resolves to nothing at all.
        assert_eq!(pick_previewable(vec![None], anything), None);
    }

    #[test]
    fn a_non_filesystem_item_does_not_hide_a_real_one() {
        let picked = pick_previewable(vec![None, path(r"C:\notes\b.md")], anything);
        assert_eq!(picked, Some(PathBuf::from(r"C:\notes\b.md")));
    }

    #[test]
    fn a_selection_of_only_virtual_items_yields_none() {
        assert_eq!(pick_previewable(vec![None, None, None], anything), None);
    }

    #[test]
    fn an_unusable_path_is_skipped() {
        // Stands in for `super::usable`: rejects anything not under C:\real.
        // Compared as text so the test says the same thing on any host.
        let usable = |p: &Path| p.to_string_lossy().starts_with(r"C:\real");
        let picked = pick_previewable(
            vec![path(r"C:\ghost\gone.md"), path(r"C:\real\here.md")],
            usable,
        );
        assert_eq!(picked, Some(PathBuf::from(r"C:\real\here.md")));
    }

    #[test]
    fn nothing_usable_yields_none() {
        let picked = pick_previewable(vec![path(r"C:\ghost\gone.md")], |_| false);
        assert_eq!(picked, None);
    }

    #[test]
    fn the_scan_is_bounded_and_lazy() {
        use std::cell::Cell;

        // One usable item sitting just past the cap must not be reached, and
        // nothing past the first hit may be resolved at all.
        let resolved = Cell::new(0usize);
        let items = (0..MAX_CANDIDATES + 10).map(|i| {
            resolved.set(resolved.get() + 1);
            if i == MAX_CANDIDATES {
                path(r"C:\notes\late.md")
            } else {
                None
            }
        });
        assert_eq!(pick_previewable(items, anything), None);
        assert_eq!(resolved.get(), MAX_CANDIDATES);

        let resolved = Cell::new(0usize);
        let items = (0..MAX_CANDIDATES).map(|_| {
            resolved.set(resolved.get() + 1);
            path(r"C:\notes\a.md")
        });
        assert!(pick_previewable(items, anything).is_some());
        assert_eq!(resolved.get(), 1, "resolution must stop at the first hit");
    }

    #[test]
    fn a_null_window_never_owns_focus() {
        let foreground = HWND(0x1234 as *mut c_void);
        assert!(!owns_focus(HWND(std::ptr::null_mut()), foreground));
    }

    #[test]
    fn the_focused_window_owns_itself() {
        let foreground = HWND(0x1234 as *mut c_void);
        assert!(owns_focus(foreground, foreground));
    }

    #[test]
    fn the_strategy_names_itself() {
        assert_eq!(Explorer::new().describe(), "Windows Explorer (COM)");
    }
}
