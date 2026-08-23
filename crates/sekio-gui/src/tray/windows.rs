//! The Windows tray, as a `Shell_NotifyIcon` in the notification area.
//!
//! No crate for this: `tray-icon` would work here, but it is the same crate
//! this workspace refuses on Linux (libappindicator via GTK, a C dependency),
//! and taking it on one platform only would mean two unrelated mechanisms for
//! one feature. `Shell_NotifyIcon` is a handful of calls through the `windows`
//! crate that is already a dependency for the Explorer selection walk.
//!
//! Four things here have a cheap wrong answer, and each is commented where it
//! happens rather than only listed here:
//!
//! * **Everything Win32 lives on one thread.** An `HWND` belongs to the thread
//!   that created it and a menu is thread-affine too, so [`spawn`] starts a
//!   dedicated thread that registers the class, creates a message-only window
//!   and runs the pump. [`Tray::update`] is called on the UI thread and must
//!   not block: it hands the new [`Menu`] over a channel and posts a private
//!   message to wake the pump, which rebuilds the `HMENU` on the owning thread.
//! * **One callback convention, not two.** The icon is switched to
//!   `NOTIFYICON_VERSION_4` and the window procedure reads the notification out
//!   of `lParam`'s low word and the anchor point out of `wParam`. Mixing that
//!   with the pre-Vista convention — the whole of `lParam` is the message, the
//!   position comes from `GetCursorPos` — is the classic bug, so both are
//!   spelled out and which one applies is decided once, by whether
//!   `NIM_SETVERSION` succeeded.
//! * **The popup needs the foreground dance.** `SetForegroundWindow` before
//!   `TrackPopupMenu` and a dummy `PostMessageW` after, or the menu stays on
//!   screen after the user clicks away.
//! * **Explorer restarts.** When it does, every tray icon in the session is
//!   destroyed and each application has to add its own back. The
//!   `TaskbarCreated` registered message is what says so.
//!
//! Nothing here panics and nothing here is required: [`spawn`] returns `None`
//! for a session with nowhere to put an icon and the daemon carries on.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreateIconIndirect, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HMENU,
    ICONINFO, MF_BYCOMMAND, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG,
    SM_CXSMICON, SM_CYSMICON, SM_MENUDROPALIGNMENT, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use super::{Event, Menu, Tray};

// ----------------------------------------------------------------- the labels

/// The label of the item that opens the file dialog.
const OPEN_LABEL: &str = "Open a file…";
/// The label of the recent-files submenu.
const RECENT_LABEL: &str = "Recent";
/// Shown, greyed, when nothing has been previewed yet. A submenu that opens
/// onto nothing looks broken; one that says why does not.
const NO_RECENT_LABEL: &str = "No recent files";
/// The label of the hotkey submenu.
const HOTKEY_LABEL: &str = "Hotkey";
/// Shown, greyed, above the choices when no hotkey could be registered.
const NO_HOTKEY_LABEL: &str = "No hotkey registered";
/// The label that stops the daemon.
const QUIT_LABEL: &str = "Quit";

/// What the notification area shows when the pointer rests on the icon.
const TOOLTIP: &str = "sekio";

/// How many recent entries reach the menu. The daemon keeps a short list
/// anyway; this only bounds what a hand-edited one could do to a popup that
/// has to fit beside a taskbar.
const MAX_RECENT: usize = 32;

/// Longest label drawn before it is elided. A tray menu is pinned to a screen
/// edge, and one very long file name would otherwise set the width of every
/// row in it.
const MAX_LABEL_CHARS: usize = 60;

// --------------------------------------------------------------- the messages

/// Our own window messages, all inside the `WM_APP..WM_APP+0x4000` range
/// reserved for an application's private use — the range a window class may
/// define for itself, as opposed to `WM_USER`, which belongs to whoever
/// defined the class.
///
/// The icon's `uCallbackMessage`. Every notification the shell sends about the
/// icon arrives as this.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
/// Posted by [`Tray::update`] from the UI thread: a new [`Menu`] is waiting on
/// the channel and the pump should rebuild the popup.
const WM_TRAY_MENU_CHANGED: u32 = WM_APP + 2;
/// Posted by [`WindowsTray::drop`]: remove the icon and end the pump.
const WM_TRAY_SHUTDOWN: u32 = WM_APP + 3;

/// `NIN_KEYSELECT`, which the `windows` crate does not define even though it
/// defines its neighbour. Sent when the icon is activated from the keyboard —
/// the same intent as a click, so it is handled the same way.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

/// This process only ever shows one icon, so its id is a constant. It has to
/// be stable across a `TaskbarCreated` re-add, which rules out anything
/// derived from a handle.
const ICON_ID: u32 = 1;

/// Ceiling on the icon we will decode. Only ever applied to bytes this crate
/// compiled in itself, but a bounded decoder is one less surprise for a
/// resident daemon.
const MAX_ICON_DIM: u32 = 1024;

/// Fallback for `SM_CXSMICON`/`SM_CYSMICON` when `GetSystemMetrics` gives back
/// something unusable. 16 is what every Windows since 95 has answered at 100%
/// scaling, so a wrong-but-plausible icon beats no icon.
const FALLBACK_ICON_SIZE: u32 = 16;

// ------------------------------------------------------------ the menu, as IR

/// One row of the popup, in the order it is drawn.
///
/// The popup is described before it is built so that "the menu is wired to the
/// right events" — the part of this file most likely to rot, and the part a
/// screenshot would otherwise be the only check on — can be asserted with no
/// window, no notification area and no session. Everything below this type is
/// a mechanical translation into `HMENU` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// A horizontal rule.
    Separator,
    /// A greyed, unselectable row: a heading, or "there is nothing here".
    Note(String),
    /// A selectable row. `checked` draws the radio dot.
    Command {
        label: String,
        id: u32,
        event: Event,
        checked: bool,
    },
    /// A nested popup.
    Submenu { label: String, items: Vec<Item> },
}

/// Hand out the next command id.
///
/// Ids start at 1 because `TrackPopupMenu` with `TPM_RETURNCMD` reports "the
/// user chose nothing" as 0, so 0 can never name an item. They are handed out
/// in draw order, which is what lets [`append_items`] recover the contiguous
/// range `CheckMenuRadioItem` wants without being told it separately.
fn next_id(counter: &mut u32) -> u32 {
    let id = *counter;
    *counter += 1;
    id
}

/// Describe the popup for `menu`.
fn plan(menu: &Menu) -> Vec<Item> {
    let mut counter = 1u32;

    let open = Item::Command {
        label: escape_mnemonics(OPEN_LABEL),
        id: next_id(&mut counter),
        event: Event::OpenFile,
        checked: false,
    };
    let recent = Item::Submenu {
        label: escape_mnemonics(RECENT_LABEL),
        items: recent_items(&menu.recent, &mut counter),
    };
    let hotkey = Item::Submenu {
        label: escape_mnemonics(HOTKEY_LABEL),
        items: hotkey_items(menu.hotkey.as_deref(), &menu.hotkey_choices, &mut counter),
    };
    let quit = Item::Command {
        label: escape_mnemonics(QUIT_LABEL),
        id: next_id(&mut counter),
        event: Event::Quit,
        checked: false,
    };

    vec![open, recent, hotkey, Item::Separator, quit]
}

/// The recent submenu, most-recent-first as it arrives.
///
/// Labelled by file name, not by path: a tray menu is a narrow strip pinned to
/// a screen edge, and a full path either elides to uselessness or drags the
/// menu across the desktop. The `Event` still carries the whole path.
fn recent_items(recent: &[std::path::PathBuf], counter: &mut u32) -> Vec<Item> {
    if recent.is_empty() {
        return vec![Item::Note(escape_mnemonics(NO_RECENT_LABEL))];
    }
    recent
        .iter()
        .take(MAX_RECENT)
        .map(|path| Item::Command {
            label: escape_mnemonics(&elide(&short_name(path))),
            id: next_id(counter),
            event: Event::Preview(path.clone()),
            checked: false,
        })
        .collect()
}

/// The hotkey submenu: every offered combination, the live one radio-checked.
///
/// The second header arm covers a hotkey set by hand in the config file. It is
/// live, but it is not one of the offered choices, so nothing would be checked
/// and the menu would look like nothing is bound.
fn hotkey_items(current: Option<&str>, choices: &[String], counter: &mut u32) -> Vec<Item> {
    let mut items = Vec::with_capacity(choices.len() + 1);

    let header = match current {
        None => Some(NO_HOTKEY_LABEL.to_owned()),
        Some(spec) if !choices.iter().any(|c| c == spec) => Some(format!("Current: {spec}")),
        Some(_) => None,
    };
    if let Some(label) = header {
        items.push(Item::Note(escape_mnemonics(&elide(&label))));
    }

    for spec in choices {
        items.push(Item::Command {
            label: escape_mnemonics(spec),
            id: next_id(counter),
            event: Event::SetHotkey(spec.clone()),
            checked: current == Some(spec.as_str()),
        });
    }

    // A submenu with no rows at all opens onto a sliver of nothing. Only
    // reachable if the caller offers no choices, but that is a `Vec` the tray
    // does not own.
    if items.is_empty() {
        items.push(Item::Note(escape_mnemonics(NO_HOTKEY_LABEL)));
    }
    items
}

/// Every `(id, event)` pair in the tree, so a command id coming back out of
/// `TrackPopupMenu` maps to an [`Event`] in one place.
fn commands(items: &[Item]) -> Vec<(u32, Event)> {
    let mut out = Vec::new();
    collect_commands(items, &mut out);
    out
}

fn collect_commands(items: &[Item], out: &mut Vec<(u32, Event)>) {
    for item in items {
        match item {
            Item::Command { id, event, .. } => out.push((*id, event.clone())),
            Item::Submenu { items, .. } => collect_commands(items, out),
            Item::Separator | Item::Note(_) => {}
        }
    }
}

/// What to call a path in a narrow menu.
///
/// Split on both separators by hand rather than through `Path::file_name`,
/// which answers for the OS running the code: on a Linux host `C:\dir\a.txt`
/// has no separators at all, so a test would see the whole path called a file
/// name and pass for the wrong reason before failing on the Windows runner
/// (CLAUDE.md). Falls back to the whole thing for the shapes with no final
/// component — a root, or a path ending in a separator.
fn short_name(path: &Path) -> String {
    let text = path.to_string_lossy();
    match text.rsplit(['/', '\\']).next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => text.into_owned(),
    }
}

/// Cut a label down to something a popup can draw, on a character boundary.
fn elide(label: &str) -> String {
    if label.chars().count() <= MAX_LABEL_CHARS {
        return label.to_owned();
    }
    let kept: String = label
        .chars()
        .take(MAX_LABEL_CHARS.saturating_sub(1))
        .collect();
    format!("{kept}…")
}

/// Double every ampersand before a label reaches a menu.
///
/// Win32 reads `&` as the mnemonic marker: a lone one vanishes and underlines
/// the next character. Without this, `R&D notes.txt` is drawn as `RD notes.txt`
/// — quietly wrong, and one of those file names is all it takes.
fn escape_mnemonics(label: &str) -> String {
    label.replace('&', "&&")
}

// ------------------------------------------------------------ the public face

/// Start the tray. `None` when there is nowhere to put an icon.
pub fn spawn(icon: &'static [u8], menu: Menu) -> Option<(Box<dyn Tray>, Receiver<Event>)> {
    let (events_tx, events_rx) = mpsc::channel();
    let (updates_tx, updates_rx) = mpsc::channel();
    // The tray thread reports whether it got as far as a live icon. Everything
    // that can fail — the class, the window, `NIM_ADD` — fails over there,
    // because all of it has to happen on the thread that will own the HWND.
    let (ready_tx, ready_rx) = mpsc::channel();

    let handle = thread::Builder::new()
        .name("sekio-tray".to_owned())
        .spawn(move || {
            // SAFETY: every Win32 handle created below is created, used and
            // destroyed inside this closure, on this thread and nowhere else.
            unsafe { tray_thread(icon, menu, events_tx, updates_rx, &ready_tx) }
        })
        .ok()?;

    // A dead thread closes the channel, which is a `RecvError` and so also a
    // `None` — a panic on the tray thread must not take the daemon with it.
    let hwnd = match ready_rx.recv() {
        Ok(Some(hwnd)) => hwnd,
        Ok(None) | Err(_) => {
            let _ = handle.join();
            return None;
        }
    };

    let tray = WindowsTray {
        hwnd,
        updates: updates_tx,
        thread: Some(handle),
    };
    Some((Box::new(tray), events_rx))
}

/// The handle the daemon holds. Owns nothing Win32 — only the address of the
/// window it may post to, and the thread that does own everything.
struct WindowsTray {
    /// The tray window, as a plain address. `HWND` is a raw pointer and so not
    /// `Send`; posting to a window from another thread is the one Win32
    /// operation that is documented to be safe, and a number crosses the
    /// thread boundary without claiming anything more than that.
    hwnd: usize,
    updates: Sender<Menu>,
    thread: Option<JoinHandle<()>>,
}

impl WindowsTray {
    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }
}

impl Tray for WindowsTray {
    /// Hand the new menu over and return.
    ///
    /// Called on the UI thread, so it may not touch the HWND's menu or build
    /// one: an `HMENU` belongs to the thread that created it. The rebuild
    /// happens on the tray thread, woken by the post.
    fn update(&mut self, menu: &Menu) {
        if self.updates.send(menu.clone()).is_err() {
            return;
        }
        // SAFETY: `PostMessageW` is thread-safe by design; a dead window makes
        // it fail, which is the same nothing as a dropped update.
        unsafe {
            let _ = PostMessageW(
                Some(self.hwnd()),
                WM_TRAY_MENU_CHANGED,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    fn describe(&self) -> String {
        "Shell_NotifyIcon in the Windows notification area".to_owned()
    }
}

impl Drop for WindowsTray {
    fn drop(&mut self) {
        // SAFETY: only a post. The tray thread does the destroying, on the
        // thread that owns the handles.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd()), WM_TRAY_SHUTDOWN, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            // Joining is what makes "dropping the tray removes the icon" true
            // rather than eventual: the notification area still shows a stale
            // icon until `NIM_DELETE` has actually run.
            let _ = thread.join();
        }
    }
}

// ------------------------------------------------------------- the tray thread

/// Everything the window procedure needs, on the thread that owns it all.
///
/// A thread local rather than `GWLP_USERDATA`: the pointer dance buys nothing
/// here — this thread has exactly one window — and it would trade a borrow
/// checker for a raw pointer that `TrackPopupMenu`'s modal loop can re-enter.
struct TrayThread {
    hwnd: HWND,
    hicon: HICON,
    /// The live popup. Replaced wholesale on an update; `DestroyMenu` on the
    /// root takes its submenus with it.
    menu: HMENU,
    commands: Vec<(u32, Event)>,
    events: Sender<Event>,
    updates: Receiver<Menu>,
    /// The `TaskbarCreated` message number, resolved once at startup.
    taskbar_created: u32,
    /// Whether `NIM_SETVERSION` took. Decides which callback convention the
    /// window procedure reads, and nothing else.
    version4: bool,
    /// True while `TrackPopupMenu`'s modal loop is running. That loop
    /// dispatches to this same window procedure, so a menu rebuild arriving
    /// mid-popup would free the `HMENU` under the user's cursor.
    showing: bool,
}

thread_local! {
    static TRAY: RefCell<Option<TrayThread>> = const { RefCell::new(None) };
}

/// The body of the tray thread: set up, report, pump, tear down.
///
/// # Safety
///
/// Must be called exactly once per thread, and on a thread that does nothing
/// else with the handles it creates.
unsafe fn tray_thread(
    icon: &'static [u8],
    menu: Menu,
    events: Sender<Event>,
    updates: Receiver<Menu>,
    ready: &Sender<Option<usize>>,
) {
    let started = unsafe { start(icon, menu, events, updates) };
    let Some(hwnd) = started else {
        let _ = ready.send(None);
        return;
    };

    if ready.send(Some(hwnd.0 as usize)).is_err() {
        // Nobody is holding the tray, so nobody will ever drop it and ask for
        // the icon back. Do it now rather than leave one in the notification
        // area for the life of the process.
        unsafe { teardown() };
        return;
    }

    unsafe { pump() };
    unsafe { teardown() };
}

/// Register the class, create the message-only window, add the icon.
///
/// # Safety
///
/// Runs on the tray thread; every handle it creates is left in [`TRAY`] for
/// that thread alone.
unsafe fn start(
    icon: &'static [u8],
    menu: Menu,
    events: Sender<Event>,
    updates: Receiver<Menu>,
) -> Option<HWND> {
    let class = w!("sekio_tray_host");
    // No instance handle means no window class, so there is no tray to start.
    let instance = unsafe { module_handle() }?;

    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    // A second `spawn` in the same process finds the class already there,
    // which `RegisterClassW` reports as 0. That is not a failure — the window
    // below is the real test of whether the class exists.
    let _ = unsafe { RegisterClassW(&wc) };

    // `HWND_MESSAGE` as the parent makes this a message-only window: no
    // pixels, no taskbar button, no enumeration by other applications. It
    // exists solely to have a thread and a window procedure for the shell to
    // send the icon's notifications to.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("sekio tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .ok()?;

    let items = plan(&menu);
    let hmenu = unsafe { build_menu(&items) };
    let hicon = unsafe { load_icon(icon) };

    let (Some(hmenu), Some(hicon)) = (hmenu, hicon) else {
        if let Some(hmenu) = hmenu {
            unsafe {
                let _ = DestroyMenu(hmenu);
            }
        }
        if let Some(hicon) = hicon {
            unsafe {
                let _ = DestroyIcon(hicon);
            }
        }
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return None;
    };

    // Registered once at startup and compared against every incoming message:
    // when Explorer restarts it destroys every tray icon in the session and
    // broadcasts this, and an application that ignores it loses its icon
    // silently and permanently.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

    let version4 = unsafe { add_icon(hwnd, hicon) };
    if !version4.added {
        unsafe {
            let _ = DestroyMenu(hmenu);
            let _ = DestroyIcon(hicon);
            let _ = DestroyWindow(hwnd);
        }
        return None;
    }

    TRAY.with(|slot| {
        *slot.borrow_mut() = Some(TrayThread {
            hwnd,
            hicon,
            menu: hmenu,
            commands: commands(&items),
            events,
            updates,
            taskbar_created,
            version4: version4.version4,
            showing: false,
        });
    });

    Some(hwnd)
}

/// The message pump.
///
/// # Safety
///
/// Runs on the thread that owns the window.
unsafe fn pump() {
    let mut msg = MSG::default();
    // `> 0` and not `as_bool`: `GetMessageW` answers -1 for an error, which is
    // truthy, and a loop that treats it as a message spins forever.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Give back everything the thread owns. Safe to reach with the icon already
/// removed and the window already destroyed — the shutdown path does both, and
/// a pump that ended some other way has done neither.
///
/// # Safety
///
/// Runs on the thread that owns the handles, after the pump has stopped.
unsafe fn teardown() {
    let state = TRAY.with(|slot| slot.borrow_mut().take());
    let Some(state) = state else { return };
    unsafe {
        let nid = icon_data(state.hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let _ = DestroyMenu(state.menu);
        let _ = DestroyIcon(state.hicon);
    }
}

// -------------------------------------------------------- the window procedure

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Messages arrive before `start` has filled the slot in (`CreateWindowExW`
    // sends WM_NCCREATE and friends synchronously) and after `teardown` has
    // emptied it. Both are ordinary; the default procedure handles them.
    let known = TRAY.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|state| (state.taskbar_created, state.version4))
    });
    let Some((taskbar_created, version4)) = known else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };

    match msg {
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        WM_TRAY_MENU_CHANGED => {
            unsafe { refresh_menu() };
            return LRESULT(0);
        }
        WM_TRAY_SHUTDOWN => {
            // Order matters: the icon has to go while its window still
            // exists, and destroying the window is what posts WM_DESTROY and
            // so ends the pump.
            unsafe {
                let nid = icon_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_TRAY_CALLBACK => {
            unsafe { on_callback(hwnd, wparam, lparam, version4) };
            return LRESULT(0);
        }
        _ => {}
    }

    if msg == taskbar_created {
        // Explorer came back and took the icon with it on the way out.
        let hicon = TRAY.with(|slot| slot.borrow().as_ref().map(|state| state.hicon));
        if let Some(hicon) = hicon {
            let _ = unsafe { add_icon(hwnd, hicon) };
        }
        return LRESULT(0);
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Turn one notification about the icon into a gesture.
///
/// Left click — and keyboard activation, and a double click, which is just a
/// left click the user was enthusiastic about — opens the file dialog. It is
/// the "show me something" gesture, and the alternative is a click that does
/// nothing. Right click shows the menu, which is where everything else lives.
///
/// # Safety
///
/// Runs on the thread that owns the window.
unsafe fn on_callback(hwnd: HWND, wparam: WPARAM, lparam: LPARAM, version4: bool) {
    let (notification, anchor) = if version4 {
        // NOTIFYICON_VERSION_4: the low word of `lParam` is the notification
        // and its high word is the icon id; `wParam` carries the anchor point
        // the shell wants the menu placed at, in screen coordinates. Reading
        // the whole of `lParam` as the message here — the pre-Vista rule —
        // would give a number with the icon id shifted into it, and reaching
        // for `GetCursorPos` would place the menu wherever the pointer had
        // drifted to by the time the message was handled.
        let notification = (lparam.0 as u32) & 0xffff;
        let x = i32::from((wparam.0 as u32 & 0xffff) as u16 as i16);
        let y = i32::from(((wparam.0 as u32 >> 16) & 0xffff) as u16 as i16);
        (notification, POINT { x, y })
    } else {
        // The original convention, reached only if `NIM_SETVERSION` was
        // refused: `lParam` *is* the message and carries no position at all.
        let mut point = POINT::default();
        if unsafe { GetCursorPos(&mut point) }.is_err() {
            return;
        }
        (lparam.0 as u32, point)
    };

    // Under version 4 the shell sends NIN_SELECT in place of WM_LBUTTONUP and
    // WM_CONTEXTMENU in place of WM_RBUTTONUP. Accepting the older pair as
    // well *only* when version 4 was refused is what keeps one click from
    // being counted twice.
    let opens = match notification {
        NIN_SELECT | NIN_KEYSELECT => true,
        WM_LBUTTONUP | WM_LBUTTONDBLCLK => !version4,
        _ => false,
    };
    if opens {
        dispatch(Event::OpenFile);
        return;
    }

    let shows_menu = notification == WM_CONTEXTMENU || (!version4 && notification == WM_RBUTTONUP);
    if shows_menu {
        unsafe { popup(hwnd, anchor) };
    }
}

/// Show the popup and act on what was chosen.
///
/// # Safety
///
/// Runs on the thread that owns the window and the menu.
unsafe fn popup(hwnd: HWND, anchor: POINT) {
    // Take the handle and drop the borrow before tracking: `TrackPopupMenu`
    // runs its own modal message loop, which dispatches straight back into
    // this window procedure. A borrow held across it would panic the daemon
    // the first time the user moved the mouse.
    let hmenu = TRAY.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow.as_mut()?;
        if state.showing {
            return None;
        }
        state.showing = true;
        Some(state.menu)
    });
    let Some(hmenu) = hmenu else { return };

    // The documented pair, and both halves are needed. Without
    // `SetForegroundWindow` the menu never takes the focus it needs to notice
    // the user clicking elsewhere, so it stays on screen. Without the dummy
    // post afterwards it can still linger, because the menu code only tidies
    // up when the next message arrives and a message-only window may not get
    // one for a long time.
    let _ = unsafe { SetForegroundWindow(hwnd) };

    // Right-aligned menus, for a right-handed layout or an RTL locale. One
    // call, so there is no reason not to.
    let align = if unsafe { GetSystemMetrics(SM_MENUDROPALIGNMENT) } != 0 {
        TPM_RIGHTALIGN
    } else {
        TPM_LEFTALIGN
    };
    // `TPM_RETURNCMD` so the chosen id comes back here rather than arriving
    // later as a WM_COMMAND: the id-to-`Event` mapping then lives in one
    // place, next to the table that produced the ids.
    let chosen = unsafe {
        TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | align,
            anchor.x,
            anchor.y,
            None,
            hwnd,
            None,
        )
    };

    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };

    let event = TRAY.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow.as_mut()?;
        state.showing = false;
        let id = u32::try_from(chosen.0).ok()?;
        state
            .commands
            .iter()
            .find(|(command, _)| *command == id)
            .map(|(_, event)| event.clone())
    });
    if let Some(event) = event {
        dispatch(event);
    }

    // An update that arrived while the menu was up was deferred rather than
    // dropped; this is where it lands.
    unsafe { refresh_menu() };
}

/// Rebuild the popup from the newest [`Menu`] waiting on the channel.
///
/// # Safety
///
/// Runs on the thread that owns the menu.
unsafe fn refresh_menu() {
    let latest = TRAY.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let state = borrow.as_mut()?;
        if state.showing {
            // Freeing the HMENU under a menu the user is reading would be a
            // use-after-free. `popup` calls back here when it closes, and the
            // menu is still sitting on the channel until then.
            return None;
        }
        // Drain: only the newest description matters, and a burst of updates
        // should cost one rebuild rather than one each.
        let mut latest = None;
        while let Ok(menu) = state.updates.try_recv() {
            latest = Some(menu);
        }
        latest
    });
    let Some(menu) = latest else { return };

    // Build the replacement before touching the live one: a failure here
    // leaves the old menu in place, which is stale, rather than none at all,
    // which is a tray icon that does nothing.
    let items = plan(&menu);
    let Some(next) = (unsafe { build_menu(&items) }) else {
        return;
    };
    let next_commands = commands(&items);

    let previous = TRAY.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return Some(next);
        };
        let previous = state.menu;
        state.menu = next;
        state.commands = next_commands;
        Some(previous)
    });
    if let Some(previous) = previous {
        unsafe {
            let _ = DestroyMenu(previous);
        }
    }
}

/// Tell the daemon what the user asked for.
///
/// A failed send means the receiver is gone and there is nobody left to tell.
fn dispatch(event: Event) {
    let events = TRAY.with(|slot| slot.borrow().as_ref().map(|state| state.events.clone()));
    if let Some(events) = events {
        let _ = events.send(event);
    }
}

// -------------------------------------------------------------- the HMENU side

/// Build the popup described by `items`. `None` if any part of it failed,
/// with nothing left behind.
///
/// # Safety
///
/// Runs on the thread that will own the menu.
unsafe fn build_menu(items: &[Item]) -> Option<HMENU> {
    let hmenu = unsafe { CreatePopupMenu() }.ok()?;
    if unsafe { append_items(hmenu, items) }.is_none() {
        unsafe {
            let _ = DestroyMenu(hmenu);
        }
        return None;
    }
    Some(hmenu)
}

/// Append one level of the tree, then radio-check whatever was marked.
///
/// # Safety
///
/// `hmenu` must be a live menu owned by this thread.
unsafe fn append_items(hmenu: HMENU, items: &[Item]) -> Option<()> {
    let mut lowest: Option<u32> = None;
    let mut highest: u32 = 0;
    let mut checked: Option<u32> = None;

    for item in items {
        match item {
            Item::Separator => {
                unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) }.ok()?;
            }
            Item::Note(label) => {
                let text = wide(label);
                // Greyed *and* disabled: greyed alone still highlights on
                // hover, which invites a click that does nothing.
                unsafe {
                    AppendMenuW(
                        hmenu,
                        MF_STRING | MF_GRAYED | MF_DISABLED,
                        0,
                        PCWSTR(text.as_ptr()),
                    )
                }
                .ok()?;
            }
            Item::Command {
                label,
                id,
                checked: is_checked,
                ..
            } => {
                let text = wide(label);
                unsafe { AppendMenuW(hmenu, MF_STRING, *id as usize, PCWSTR(text.as_ptr())) }
                    .ok()?;
                lowest = Some(lowest.map_or(*id, |low| low.min(*id)));
                highest = highest.max(*id);
                if *is_checked {
                    checked = Some(*id);
                }
            }
            Item::Submenu { label, items } => {
                let sub = unsafe { build_menu(items) }?;
                let text = wide(label);
                // Ownership passes to the parent here: `DestroyMenu` on the
                // root takes attached submenus with it. If the append fails
                // it did not, so this is the one place that has to clean up.
                if unsafe { AppendMenuW(hmenu, MF_POPUP, sub.0 as usize, PCWSTR(text.as_ptr())) }
                    .is_err()
                {
                    unsafe {
                        let _ = DestroyMenu(sub);
                    }
                    return None;
                }
            }
        }
    }

    // The ids in one level are handed out in draw order, so the commands here
    // are a contiguous run and `CheckMenuRadioItem` — which wants a range and
    // clears the rest of it — can be given that run without being told it
    // separately. Notes and separators carry id 0 and are outside it.
    if let (Some(first), Some(check)) = (lowest, checked) {
        let _ = unsafe { CheckMenuRadioItem(hmenu, first, highest, check, MF_BYCOMMAND.0) };
    }

    Some(())
}

/// A NUL-terminated UTF-16 copy of `text`, for the `W` entry points.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// --------------------------------------------------------------- the icon side

/// What `add_icon` managed.
struct Added {
    /// The icon is in the notification area.
    added: bool,
    /// `NIM_SETVERSION` took, so the version 4 callback convention applies.
    version4: bool,
}

/// The `NOTIFYICONDATAW` that names this process's one icon. Enough on its own
/// for `NIM_DELETE`, which only needs the window and the id.
fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// Add the icon, then ask for the modern callback convention.
///
/// # Safety
///
/// `hwnd` must be a live window owned by this thread and `hicon` a live icon.
unsafe fn add_icon(hwnd: HWND, hicon: HICON) -> Added {
    let mut nid = icon_data(hwnd);
    // NIF_SHOWTIP alongside NIF_TIP because version 4 otherwise suppresses the
    // standard tooltip and expects the application to draw its own; sekio has
    // no reason to, and an icon with no tooltip at all is worse.
    nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY_CALLBACK;
    nid.hIcon = hicon;
    write_tip(&mut nid.szTip, TOOLTIP);

    if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
        return Added {
            added: false,
            version4: false,
        };
    }

    // `NIM_SETVERSION` has to come *after* the add — it changes the behaviour
    // of an icon that already exists — and it is what switches the callback to
    // the convention `on_callback` reads.
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let version4 = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) }.as_bool();

    Added {
        added: true,
        version4,
    }
}

/// Copy `text` into a fixed `szTip` array, always NUL-terminated.
fn write_tip(slot: &mut [u16; 128], text: &str) {
    let source = text.encode_utf16().chain(std::iter::once(0));
    for (cell, unit) in slot.iter_mut().zip(source) {
        *cell = unit;
    }
    // A tooltip longer than the array would otherwise run off the end of it.
    if let Some(last) = slot.last_mut() {
        *last = 0;
    }
}

/// Turn the embedded PNG into an `HICON` at the notification area's size.
///
/// # Safety
///
/// Runs on the thread that will own the icon.
unsafe fn load_icon(png: &[u8]) -> Option<HICON> {
    let width = unsafe { metric(SM_CXSMICON) };
    let height = unsafe { metric(SM_CYSMICON) };
    let scaled = decode_scaled(png, width, height)?;
    let pixels = to_premultiplied_bgra(&scaled);

    // A negative `biHeight` asks for a top-down DIB, so row 0 of the bitmap is
    // the top row of the image and the buffer can be copied straight across.
    // The alternative — the default bottom-up layout — needs the rows reversed
    // on the way in, and forgetting to is an upside-down icon that nothing but
    // a screenshot will tell you about.
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width as i32;
    info.bmiHeader.biHeight = -(height as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;

    let mut bits: *mut c_void = std::ptr::null_mut();
    let colour =
        unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        unsafe {
            let _ = DeleteObject(colour.into());
        }
        return None;
    }
    // SAFETY: `CreateDIBSection` succeeded for exactly this geometry, so the
    // section is width * height * 4 bytes, which is `pixels.len()`.
    unsafe {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
    }

    // The 1bpp AND mask, all zero, meaning "opaque everywhere": with a 32-bit
    // colour bitmap the alpha channel is what actually decides transparency,
    // and a mask left uninitialised would punch holes in the icon at random.
    // Rows in a 1bpp bitmap are DWORD-aligned.
    let mask_stride = (width as usize).div_ceil(32) * 4;
    let mask_bits = vec![0u8; mask_stride * height as usize];
    let mask = unsafe {
        CreateBitmap(
            width as i32,
            height as i32,
            1,
            1,
            Some(mask_bits.as_ptr().cast()),
        )
    };
    if mask.is_invalid() {
        unsafe {
            let _ = DeleteObject(colour.into());
        }
        return None;
    }

    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: colour,
    };
    let hicon = unsafe { CreateIconIndirect(&info) }.ok();

    // `CreateIconIndirect` copies both bitmaps, so they are ours to free
    // whether it worked or not.
    unsafe {
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(colour.into());
    }

    hicon
}

/// `GetSystemMetrics`, with a sane answer for the values that are not.
///
/// # Safety
///
/// Trivially safe; `unsafe` only because the import is.
unsafe fn metric(index: windows::Win32::UI::WindowsAndMessaging::SYSTEM_METRICS_INDEX) -> u32 {
    match unsafe { GetSystemMetrics(index) } {
        size if size > 0 => size as u32,
        _ => FALLBACK_ICON_SIZE,
    }
}

/// Decode the PNG and scale it to exactly `width` x `height`.
///
/// `None` for anything that is not a decodable PNG of a sane size — the same
/// answer `icon.rs` gives, and for the same reason: an icon is a decoration
/// and a resident daemon must survive one going missing.
fn decode_scaled(png: &[u8], width: u32, height: u32) -> Option<image::RgbaImage> {
    if width == 0 || height == 0 {
        return None;
    }
    // `with_format` rather than `with_guessed_format`: the only caller passes
    // a PNG this crate compiled in, so there is nothing to sniff.
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_ICON_DIM);
    limits.max_image_height = Some(MAX_ICON_DIM);
    reader.limits(limits);

    let source = reader.decode().ok()?.into_rgba8();
    if source.width() == 0 || source.height() == 0 {
        return None;
    }
    if source.dimensions() == (width, height) {
        return Some(source);
    }
    Some(image::imageops::resize(
        &source,
        width,
        height,
        image::imageops::FilterType::Lanczos3,
    ))
}

/// RGBA8 to the blue-green-red-alpha byte order a 32-bit DIB stores, with the
/// colour channels premultiplied by alpha.
///
/// Both halves are silent when wrong. Leaving the channels in RGBA order gives
/// an icon with red and blue swapped — a plausible-looking picture in the wrong
/// palette, which no test that only checks the buffer length would catch.
/// Skipping the premultiply gives edges that glow: `DrawIconEx` composites a
/// 32-bit icon through `AlphaBlend`, which is defined on premultiplied source
/// pixels, so a straight-alpha buffer is blended as if every translucent pixel
/// were brighter than it is.
fn to_premultiplied_bgra(rgba: &image::RgbaImage) -> Vec<u8> {
    let mut out = rgba.as_raw().clone();
    for pixel in out.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        // Rounded rather than truncated: the truncating form loses a level on
        // nearly every pixel, which shows up as a faintly darker icon.
        let premultiply = |channel: u8| ((u32::from(channel) * alpha + 127) / 255) as u8;
        let (red, green, blue) = (pixel[0], pixel[1], pixel[2]);
        pixel[0] = premultiply(blue);
        pixel[1] = premultiply(green);
        pixel[2] = premultiply(red);
    }
    out
}

/// This module's `HINSTANCE`.
///
/// `None` if the handle cannot be read, which callers treat as "no tray": a
/// window class needs an instance to belong to.
///
/// # Safety
///
/// Trivially safe; `unsafe` only because the import is.
unsafe fn module_handle() -> Option<HINSTANCE> {
    unsafe { GetModuleHandleW(PCWSTR::null()) }
        .ok()
        .map(|module| HINSTANCE(module.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> Menu {
        Menu {
            hotkey: Some("Ctrl+Alt+Space".to_owned()),
            hotkey_choices: super::super::hotkey_choices(),
            recent: vec![
                PathBuf::from(r"C:\Users\ada\notes.txt"),
                PathBuf::from(r"C:\Users\ada\report.pdf"),
            ],
        }
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .map(|item| match item {
                Item::Separator => "-",
                Item::Note(label) => label,
                Item::Command { label, .. } => label,
                Item::Submenu { label, .. } => label,
            })
            .collect()
    }

    fn submenu<'a>(items: &'a [Item], label: &str) -> &'a [Item] {
        items
            .iter()
            .find_map(|item| match item {
                Item::Submenu { label: name, items } if name == label => Some(items.as_slice()),
                _ => None,
            })
            .expect("the popup must have this submenu")
    }

    #[test]
    fn the_popup_has_the_four_things_it_promises() {
        let items = plan(&sample());
        assert_eq!(
            labels(&items),
            vec![OPEN_LABEL, RECENT_LABEL, HOTKEY_LABEL, "-", QUIT_LABEL]
        );
    }

    #[test]
    fn open_and_quit_carry_their_events() {
        let items = plan(&sample());
        let table = commands(&items);
        assert!(table.iter().any(|(_, event)| *event == Event::OpenFile));
        assert!(table.iter().any(|(_, event)| *event == Event::Quit));
    }

    /// Zero is what `TrackPopupMenu` reports for "the user chose nothing", so
    /// no item may ever be given it.
    #[test]
    fn no_command_uses_the_id_that_means_nothing_was_chosen() {
        let items = plan(&sample());
        assert!(commands(&items).iter().all(|(id, _)| *id != 0));
    }

    #[test]
    fn every_command_id_is_unique() {
        let items = plan(&sample());
        let mut ids: Vec<u32> = commands(&items).iter().map(|(id, _)| *id).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            before,
            "two items would answer to the same click"
        );
    }

    /// `CheckMenuRadioItem` takes a range and clears everything in it, so the
    /// choices in the hotkey submenu have to be a contiguous run of ids.
    #[test]
    fn the_hotkey_choices_are_a_contiguous_id_range() {
        let items = plan(&sample());
        let ids: Vec<u32> = commands(submenu(&items, HOTKEY_LABEL))
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert!(ids.len() > 1);
        for pair in ids.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "ids {ids:?} are not contiguous");
        }
    }

    #[test]
    fn recent_entries_are_labelled_by_name_and_carry_the_whole_path() {
        let items = plan(&sample());
        let recent = submenu(&items, RECENT_LABEL);
        assert_eq!(labels(recent), vec!["notes.txt", "report.pdf"]);
        assert_eq!(
            commands(recent)
                .into_iter()
                .map(|(_, event)| event)
                .collect::<Vec<_>>(),
            vec![
                Event::Preview(PathBuf::from(r"C:\Users\ada\notes.txt")),
                Event::Preview(PathBuf::from(r"C:\Users\ada\report.pdf")),
            ]
        );
    }

    #[test]
    fn recent_keeps_the_order_it_was_given() {
        let mut menu = sample();
        menu.recent.reverse();
        let items = plan(&menu);
        assert_eq!(
            labels(submenu(&items, RECENT_LABEL)),
            vec!["report.pdf", "notes.txt"]
        );
    }

    #[test]
    fn an_empty_recent_list_says_so_and_offers_nothing_to_click() {
        let mut menu = sample();
        menu.recent.clear();
        let items = plan(&menu);
        let recent = submenu(&items, RECENT_LABEL);
        assert_eq!(recent, [Item::Note(NO_RECENT_LABEL.to_owned())]);
        assert!(commands(recent).is_empty());
    }

    #[test]
    fn a_long_recent_list_is_capped() {
        let mut menu = sample();
        menu.recent = (0..MAX_RECENT * 3)
            .map(|n| PathBuf::from(format!(r"C:\tmp\{n}.txt")))
            .collect();
        let items = plan(&menu);
        assert_eq!(submenu(&items, RECENT_LABEL).len(), MAX_RECENT);
    }

    #[test]
    fn the_live_hotkey_is_the_checked_one_and_the_only_one() {
        let items = plan(&sample());
        let hotkey = submenu(&items, HOTKEY_LABEL);
        let checked: Vec<&str> = hotkey
            .iter()
            .filter_map(|item| match item {
                Item::Command {
                    label,
                    checked: true,
                    ..
                } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(checked, vec!["Ctrl+Alt+Space"]);
    }

    #[test]
    fn choosing_a_hotkey_sends_that_spec() {
        let items = plan(&sample());
        let events: Vec<Event> = commands(submenu(&items, HOTKEY_LABEL))
            .into_iter()
            .map(|(_, event)| event)
            .collect();
        for spec in super::super::hotkey_choices() {
            assert!(events.contains(&Event::SetHotkey(spec)));
        }
    }

    #[test]
    fn no_hotkey_greys_a_header_and_still_offers_the_choices() {
        let mut menu = sample();
        menu.hotkey = None;
        let items = plan(&menu);
        let hotkey = submenu(&items, HOTKEY_LABEL);
        assert_eq!(
            hotkey.first(),
            Some(&Item::Note(NO_HOTKEY_LABEL.to_owned()))
        );
        assert_eq!(commands(hotkey).len(), menu.hotkey_choices.len());
        assert!(!hotkey
            .iter()
            .any(|item| matches!(item, Item::Command { checked: true, .. })));
    }

    /// A hotkey set by hand in the config file is live but is not one of the
    /// offered choices, so nothing would be checked and the menu would look
    /// like nothing is bound.
    #[test]
    fn a_hotkey_outside_the_choices_still_gets_a_header() {
        let mut menu = sample();
        menu.hotkey = Some("Ctrl+Alt+F12".to_owned());
        let items = plan(&menu);
        let hotkey = submenu(&items, HOTKEY_LABEL);
        assert_eq!(
            hotkey.first(),
            Some(&Item::Note("Current: Ctrl+Alt+F12".to_owned()))
        );
    }

    #[test]
    fn a_hotkey_submenu_is_never_empty() {
        let mut menu = sample();
        menu.hotkey = Some("Ctrl+Alt+Space".to_owned());
        menu.hotkey_choices.clear();
        let items = plan(&menu);
        assert!(!submenu(&items, HOTKEY_LABEL).is_empty());
    }

    /// Host-independent on purpose: split by hand on both separators, so this
    /// asserts a string rewrite rather than what the host thinks a path means
    /// (CLAUDE.md).
    #[test]
    fn a_name_is_taken_after_either_separator() {
        assert_eq!(
            short_name(Path::new(r"C:\Users\ada\notes.txt")),
            "notes.txt"
        );
        assert_eq!(short_name(Path::new("/home/ada/notes.txt")), "notes.txt");
        assert_eq!(short_name(Path::new("notes.txt")), "notes.txt");
    }

    #[test]
    fn a_path_with_no_final_component_keeps_its_whole_label() {
        assert_eq!(short_name(Path::new(r"C:\Users\ada\")), r"C:\Users\ada\");
        assert_eq!(short_name(Path::new("")), "");
    }

    #[test]
    fn ampersands_survive_into_the_label() {
        let items = plan(&Menu {
            hotkey: None,
            hotkey_choices: Vec::new(),
            recent: vec![PathBuf::from(r"C:\tmp\R&D notes.txt")],
        });
        assert_eq!(
            labels(submenu(&items, RECENT_LABEL)),
            vec!["R&&D notes.txt"]
        );
    }

    #[test]
    fn a_very_long_name_is_elided_on_a_character_boundary() {
        let long = format!("{}.txt", "ä".repeat(MAX_LABEL_CHARS * 2));
        let elided = elide(&long);
        assert_eq!(elided.chars().count(), MAX_LABEL_CHARS);
        assert!(elided.ends_with('…'));
    }

    #[test]
    fn a_short_name_is_left_alone() {
        assert_eq!(elide("notes.txt"), "notes.txt");
    }

    #[test]
    fn a_wide_string_is_nul_terminated() {
        assert_eq!(wide("hi"), vec![b'h' as u16, b'i' as u16, 0]);
    }

    #[test]
    fn a_tooltip_longer_than_the_field_is_still_terminated() {
        let mut slot = [1u16; 128];
        write_tip(&mut slot, &"x".repeat(500));
        assert_eq!(slot[127], 0);
    }

    #[test]
    fn a_short_tooltip_is_terminated_where_it_ends() {
        let mut slot = [1u16; 128];
        write_tip(&mut slot, "sekio");
        assert_eq!(
            &slot[..6],
            &[
                b's' as u16,
                b'e' as u16,
                b'k' as u16,
                b'i' as u16,
                b'o' as u16,
                0
            ]
        );
    }

    /// The one thing about the icon that is invisible until someone looks at
    /// a screenshot: channel order, and premultiplication.
    #[test]
    fn pixels_come_out_bgra_and_premultiplied() {
        let mut image = image::RgbaImage::new(2, 1);
        image.put_pixel(0, 0, image::Rgba([10, 20, 30, 255]));
        image.put_pixel(1, 0, image::Rgba([255, 255, 255, 0]));
        let bytes = to_premultiplied_bgra(&image);
        // Opaque: reordered to blue, green, red, alpha and otherwise untouched.
        assert_eq!(&bytes[..4], &[30, 20, 10, 255]);
        // Fully transparent: every colour channel multiplied away, which is
        // what stops a transparent pixel from fringing white.
        assert_eq!(&bytes[4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn half_transparent_pixels_are_scaled_not_truncated() {
        let mut image = image::RgbaImage::new(1, 1);
        image.put_pixel(0, 0, image::Rgba([255, 255, 255, 128]));
        assert_eq!(to_premultiplied_bgra(&image), vec![128, 128, 128, 128]);
    }

    #[test]
    fn the_icon_decodes_to_the_size_it_was_asked_for() {
        let scaled = decode_scaled(crate::icon::PNG, 16, 16).expect("the icon must decode");
        assert_eq!(scaled.dimensions(), (16, 16));
    }

    #[test]
    fn a_corrupt_icon_is_no_icon_rather_than_a_panic() {
        assert!(decode_scaled(b"not a png", 16, 16).is_none());
        assert!(decode_scaled(&[], 16, 16).is_none());
        assert!(decode_scaled(crate::icon::PNG, 0, 16).is_none());
    }

    /// The private messages must sit in the range Windows reserves for a
    /// window class's own use, and must not collide with each other.
    #[test]
    fn the_private_messages_are_distinct_and_in_range() {
        let ours = [WM_TRAY_CALLBACK, WM_TRAY_MENU_CHANGED, WM_TRAY_SHUTDOWN];
        for message in ours {
            assert!((WM_APP..WM_APP + 0x4000).contains(&message));
        }
        let mut sorted = ours;
        sorted.sort_unstable();
        sorted.iter().reduce(|previous, next| {
            assert_ne!(previous, next);
            next
        });
    }
}
