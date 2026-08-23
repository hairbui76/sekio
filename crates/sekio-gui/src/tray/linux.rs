//! The Linux tray, as a StatusNotifierItem over D-Bus.
//!
//! `ksni` and not `tray-icon`: the latter reaches the tray through
//! libappindicator via GTK, and a C dependency would break the rule that keeps
//! this workspace's Windows cross-check working (see CLAUDE.md). `ksni` is
//! zbus underneath and pulls nothing but Rust.
//!
//! **Stock GNOME has no tray, and that is not a bug here.** GNOME Shell ships
//! no StatusNotifierHost, so nothing owns `org.kde.StatusNotifierWatcher` and
//! [`spawn`] correctly returns `None`; the daemon keeps its socket and its
//! hotkey and simply has nowhere to put an icon. Installing the AppIndicator
//! and KStatusNotifierItem Support extension provides the watcher, and the
//! icon then appears with no change here. KDE, XFCE, Cinnamon, Budgie and most
//! panels (waybar, ags) host it out of the box.
//!
//! Three things this module is careful about, each of which has a cheap wrong
//! answer:
//!
//! * **Detection asks the bus daemon, never the watcher.** `NameHasOwner` on
//!   `org.freedesktop.DBus` reports who is *already* there. A method call on
//!   the watcher itself — or `StartServiceByName` — would D-Bus-*activate* it,
//!   starting a service the user never ran, or hanging while it fails to
//!   start. `selection/linux.rs` walked into exactly this and settled on the
//!   same shell-out to `gdbus`/`dbus-send`; this follows it rather than
//!   inventing a second answer.
//! * **Nothing blocks the caller.** [`spawn`] runs on the main thread during
//!   startup and [`Tray::update`] on the UI thread, so the D-Bus service, the
//!   registration handshake and every menu update happen on a worker thread
//!   with a bounded wait in front of them. A wedged bus costs a timeout, never
//!   the daemon.
//! * **Nothing panics.** A failed decode, a missing tool, a dead bus and a
//!   refused registration are all `None`. An icon is an affordance.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ksni::blocking::TrayMethods as _;

use super::{Event, Menu, Tray};

/// The well-known name a StatusNotifierHost registers. Its owner — or absence
/// of one — is the whole question this module asks before starting anything.
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";

/// How long `gdbus`/`dbus-send` gets to answer `NameHasOwner`. Generous for a
/// local socket round-trip and short enough that a broken bus is a hiccup in
/// startup rather than a hang.
const BUS_QUERY_TIMEOUT: Duration = Duration::from_millis(150);

/// How long the registration handshake gets before [`spawn`] gives up and
/// reports no tray. The worker thread still finishes and tears itself down, so
/// a late success removes its own icon instead of leaving an orphan.
const REGISTER_BUDGET: Duration = Duration::from_millis(1000);

/// Between `try_wait` polls of the bus tool. Small enough not to dominate the
/// query, large enough not to spin a core.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// A `NameHasOwner` reply is one boolean. Anything longer is not a reply we
/// understand, and reading it unbounded would be a way for a hostile `PATH`
/// entry to feed the daemon a gigabyte.
const MAX_REPLY: u64 = 4096;

/// Sizes offered to the host, largest kept as-is. Panels ask for something
/// around 22px and scale whatever they get; handing them a pre-scaled pixmap
/// means a good downscale instead of whatever the host does in a hurry.
/// Nothing is ever scaled *up* — an upscaled icon is worse than a small one.
const PIXMAP_SIZES: [u32; 2] = [22, 32];

/// Ceiling on the icon we will decode. Only ever applied to bytes this crate
/// compiled in itself, but a bounded decoder is one less surprise for a
/// resident daemon.
const MAX_ICON_DIM: u32 = 1024;

// ------------------------------------------------------------------ the menu

/// The label of the item that opens the file dialog.
const OPEN_LABEL: &str = "Open a file…";
/// The label of the recent-files submenu.
const RECENT_LABEL: &str = "Recent";
/// Shown, disabled, when nothing has been previewed yet. A submenu that opens
/// onto nothing looks broken; one that says why does not.
const NO_RECENT_LABEL: &str = "No recent files";
/// The label of the hotkey submenu.
const HOTKEY_LABEL: &str = "Hotkey";
/// Shown, disabled, above the choices when no hotkey could be registered —
/// which is the common case on Wayland, where another process may hold the
/// combination or the compositor may not allow global grabs at all.
const NO_HOTKEY_LABEL: &str = "No hotkey registered";
/// The label that stops the daemon.
const QUIT_LABEL: &str = "Quit";

/// Build the item tree the tray shows for `menu`.
///
/// Split out as a plain function of `&Menu` so the structure can be asserted
/// on with no session bus and no icon: CI has neither, and "the menu is wired
/// to the right events" is the part of this file most likely to rot.
fn build_menu(menu: &Menu) -> Vec<ksni::MenuItem<SekioTray>> {
    use ksni::menu::{StandardItem, SubMenu};

    vec![
        StandardItem {
            label: OPEN_LABEL.into(),
            icon_name: "document-open".into(),
            activate: Box::new(|tray: &mut SekioTray| tray.emit(Event::OpenFile)),
            ..Default::default()
        }
        .into(),
        SubMenu {
            label: RECENT_LABEL.into(),
            icon_name: "document-open-recent".into(),
            submenu: recent_items(&menu.recent),
            ..Default::default()
        }
        .into(),
        SubMenu {
            label: HOTKEY_LABEL.into(),
            icon_name: "input-keyboard".into(),
            submenu: hotkey_items(menu.hotkey.as_deref(), &menu.hotkey_choices),
            ..Default::default()
        }
        .into(),
        ksni::MenuItem::Separator,
        StandardItem {
            label: QUIT_LABEL.into(),
            icon_name: "application-exit".into(),
            activate: Box::new(|tray: &mut SekioTray| tray.emit(Event::Quit)),
            ..Default::default()
        }
        .into(),
    ]
}

/// The recent submenu, most-recent-first as it arrives.
///
/// Labelled by file name, not by path: a tray menu is a narrow strip pinned to
/// a panel edge, and a full path either elides to uselessness or drags the
/// menu across the screen. The `Event` still carries the whole path.
fn recent_items(recent: &[PathBuf]) -> Vec<ksni::MenuItem<SekioTray>> {
    use ksni::menu::StandardItem;

    if recent.is_empty() {
        return vec![StandardItem {
            label: NO_RECENT_LABEL.into(),
            enabled: false,
            ..Default::default()
        }
        .into()];
    }

    recent
        .iter()
        .map(|path| {
            let chosen = path.clone();
            StandardItem {
                label: escape_mnemonics(&short_name(path)),
                activate: Box::new(move |tray: &mut SekioTray| {
                    tray.emit(Event::Preview(chosen.clone()))
                }),
                ..Default::default()
            }
            .into()
        })
        .collect()
}

/// The hotkey submenu: every offered combination, with the live one ticked.
///
/// Checkmarks rather than a `RadioGroup` because a radio group always has a
/// selection, and "no hotkey is registered" is a state this menu genuinely has
/// to be able to show. A header line says so instead of leaving the user to
/// infer it from an absence of ticks.
fn hotkey_items(current: Option<&str>, choices: &[String]) -> Vec<ksni::MenuItem<SekioTray>> {
    use ksni::menu::{CheckmarkItem, StandardItem};

    let mut items: Vec<ksni::MenuItem<SekioTray>> = Vec::with_capacity(choices.len() + 1);

    // The second arm covers a hotkey set by hand in the config file: it is
    // live, but it is not one of the offered choices, so no checkmark would
    // appear and the menu would look like nothing is bound.
    let header = match current {
        None => Some(NO_HOTKEY_LABEL.to_owned()),
        Some(spec) if !choices.iter().any(|c| c == spec) => Some(format!("Current: {spec}")),
        Some(_) => None,
    };
    if let Some(label) = header {
        items.push(
            StandardItem {
                label: escape_mnemonics(&label),
                enabled: false,
                ..Default::default()
            }
            .into(),
        );
    }

    for spec in choices {
        let chosen = spec.clone();
        items.push(
            CheckmarkItem {
                label: escape_mnemonics(spec),
                checked: current == Some(spec.as_str()),
                activate: Box::new(move |tray: &mut SekioTray| {
                    tray.emit(Event::SetHotkey(chosen.clone()))
                }),
                ..Default::default()
            }
            .into(),
        );
    }

    items
}

/// What to call a path in a narrow menu. Falls back to the whole thing for the
/// shapes that have no final component — a root, or a path ending in `..`.
fn short_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Double every underscore before a label goes out over dbusmenu.
///
/// The protocol reads `_` as a mnemonic marker: a lone one vanishes and makes
/// the next character an access key. Without this, `my_notes.txt` is shown as
/// `mynotes.txt` — quietly wrong, and file names full of underscores are
/// exactly what a recent list is made of.
fn escape_mnemonics(label: &str) -> String {
    label.replace('_', "__")
}

// -------------------------------------------------------------- the SNI item

/// The item ksni serves. Owns the current [`Menu`] and the wire back to the
/// daemon; it never previews or decides anything itself.
struct SekioTray {
    /// Pre-decoded ARGB32 pixmaps. Empty when the icon could not be decoded,
    /// in which case a themed icon name is offered instead.
    pixmaps: Vec<ksni::Icon>,
    menu: Menu,
    events: Sender<Event>,
}

impl SekioTray {
    /// Hand a click to the daemon and return immediately.
    ///
    /// A menu callback runs on the D-Bus service thread while the menu is on
    /// screen, so anything slow here freezes the menu under the user's cursor.
    /// A send on an unbounded channel cannot block, and a failed send means
    /// the daemon is already gone — there is nobody left to tell.
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

impl ksni::Tray for SekioTray {
    /// Left click opens the menu. Every action this tray offers is in the
    /// menu, so the alternative would be a click that does nothing.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        // Stable across sessions on purpose: hosts key their per-item settings
        // (position in the panel, hidden/shown) off this.
        "sekio".into()
    }

    fn title(&self) -> String {
        "sekio".into()
    }

    fn icon_name(&self) -> String {
        // Only as a fallback: hosts prefer a themed name over a pixmap, and
        // `sekio` is installed into hicolor by the .deb/.rpm packaging but not
        // by a plain `cargo install`. Offering it only when the embedded PNG
        // failed to decode keeps the icon working in both cases.
        if self.pixmaps.is_empty() {
            "sekio".into()
        } else {
            String::new()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.pixmaps.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "sekio".into(),
            description: match &self.menu.hotkey {
                Some(spec) => format!("Preview the selected file with {spec}"),
                None => "No hotkey registered".into(),
            },
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        build_menu(&self.menu)
    }
}

// ------------------------------------------------------------------- the icon

/// Decode the embedded PNG into the ARGB32 pixmaps ksni wants, at a couple of
/// sizes so the host can pick rather than rescale.
///
/// Returns an empty vector rather than an error: a tray with a fallback icon
/// name is still a tray.
fn pixmaps(png: &[u8]) -> Vec<ksni::Icon> {
    // `with_format` rather than `with_guessed_format`: the only caller passes
    // a PNG this crate compiled in, so there is nothing to sniff.
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(png), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_ICON_DIM);
    limits.max_image_height = Some(MAX_ICON_DIM);
    reader.limits(limits);

    let Ok(source) = reader.decode() else {
        return Vec::new();
    };
    let (width, height) = (source.width(), source.height());
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let rgba = source.to_rgba8();
    let mut icons = vec![to_argb32(&rgba)];
    // Only square sources get extra sizes: `resize_exact` on a non-square icon
    // would distort it, and picking a crop is not this module's decision.
    if width == height {
        for size in PIXMAP_SIZES {
            // Never upscale — a 22px icon stretched to 64 is worse than the
            // 64px original the host would have scaled down itself.
            if size < width {
                let scaled = image::imageops::resize(
                    &rgba,
                    size,
                    size,
                    image::imageops::FilterType::Lanczos3,
                );
                icons.push(to_argb32(&scaled));
            }
        }
    }
    icons
}

/// RGBA8 to the ARGB32-in-network-byte-order the StatusNotifierItem spec asks
/// for: alpha first, then R, G, B.
///
/// Getting this backwards is the classic failure — the icon comes out
/// blue-tinted (the channels rotated) or invisible (a colour byte read as
/// alpha) — and it is invisible in a unit test unless the byte order is
/// asserted directly, which [`tests`] does.
fn to_argb32(rgba: &image::RgbaImage) -> ksni::Icon {
    let (width, height) = rgba.dimensions();
    let mut data = rgba.as_raw().clone();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

// --------------------------------------------------------------- bus presence

/// Which command-line D-Bus client is available. `dbus-send` ships with the
/// bus daemon itself and `gdbus` with anything GTK-adjacent, so a session that
/// has a bus almost always has one of them.
///
/// A client rather than zbus directly because zbus is not a dependency of this
/// crate — it arrives under `ksni`, and reaching through a dependency's
/// dependency is how a build breaks on an unrelated version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusTool {
    Gdbus,
    DbusSend,
}

impl BusTool {
    fn detect() -> Option<(Self, PathBuf)> {
        if let Some(bin) = find_on_path("gdbus") {
            return Some((Self::Gdbus, bin));
        }
        find_on_path("dbus-send").map(|bin| (Self::DbusSend, bin))
    }

    fn query(self, bin: &Path, name: &str) -> Command {
        let mut cmd = Command::new(bin);
        match self {
            Self::Gdbus => {
                cmd.arg("call")
                    .arg("--session")
                    .arg("--dest")
                    .arg("org.freedesktop.DBus")
                    .arg("--object-path")
                    .arg("/org/freedesktop/DBus")
                    .arg("--method")
                    .arg("org.freedesktop.DBus.NameHasOwner")
                    .arg(name);
            }
            Self::DbusSend => {
                cmd.arg("--session")
                    .arg("--print-reply")
                    .arg("--dest=org.freedesktop.DBus")
                    .arg("/org/freedesktop/DBus")
                    .arg("org.freedesktop.DBus.NameHasOwner")
                    .arg(format!("string:{name}"));
            }
        }
        cmd
    }
}

/// Is anybody already holding `name` on the session bus?
///
/// Asked of the bus daemon, which answers from its own table without starting
/// anything. That is the entire point: a call to the watcher would activate
/// it, and "no tray host is running" would turn into "one now is".
///
/// Every failure is `false` — no tool, no bus, a timeout, an unparseable
/// reply. Guessing `true` would only move the failure into `ksni::spawn`.
fn watcher_has_owner(name: &str) -> bool {
    let Some((tool, bin)) = BusTool::detect() else {
        return false;
    };
    let mut cmd = tool.query(&bin, name);
    // stdout is a pipe, not a file: the reply is a few dozen bytes, orders of
    // magnitude below the pipe buffer, so the child can never block writing it
    // and this cannot deadlock against the poll loop below.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    if !wait_bounded(&mut child, Instant::now() + BUS_QUERY_TIMEOUT) {
        return false;
    }

    let Some(stdout) = child.stdout.take() else {
        return false;
    };
    let mut reply = String::new();
    if stdout.take(MAX_REPLY).read_to_string(&mut reply).is_err() {
        return false;
    }
    // `(true,)` from gdbus, `boolean true` from dbus-send.
    reply.contains("true")
}

/// Poll until the child exits or the deadline passes, killing and reaping it
/// in the latter case. This is what keeps a wedged `gdbus` from wedging
/// startup.
fn wait_bounded(child: &mut Child, deadline: Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => {
                kill_and_reap(child);
                return false;
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(child);
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Always both: `kill` only sends the signal, and `wait` is what stops a
/// resident daemon from collecting zombies for the rest of its life.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// First executable called `name` on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        // An empty PATH entry means "the current directory" to some shells.
        // Running whatever happens to sit in the cwd is a well-known foot-gun.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let full = dir.join(name);
        if is_executable_file(&full) {
            return Some(full);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ------------------------------------------------------------------ the tray

/// The live tray, from the daemon's side. Holds no D-Bus state at all: the
/// service, the ksni handle and the shutdown all live on the worker thread,
/// which is what lets both trait methods be non-blocking.
struct LinuxTray {
    /// The menu currently on the wire, so an unchanged `update` costs a
    /// comparison instead of a thread hop.
    current: Menu,
    /// To the worker. Unbounded, so `send` returns immediately; a closed
    /// channel means the service died and there is nothing to update.
    ///
    /// Dropping it *is* the shutdown: the worker's `recv` fails, it tells ksni
    /// to withdraw the item and waits for the host to drop it, then exits.
    /// That wait happens on the worker rather than in a `Drop` here, because
    /// `Drop` runs on the UI thread and an icon lingering for a few
    /// milliseconds beats a shutdown that can hang.
    menus: Sender<Menu>,
}

impl Tray for LinuxTray {
    fn update(&mut self, menu: &Menu) {
        if self.current == *menu {
            return;
        }
        self.current = menu.clone();
        let _ = self.menus.send(menu.clone());
    }

    fn describe(&self) -> String {
        format!("StatusNotifierItem on the session bus (hosted via {WATCHER_NAME})")
    }
}

/// Start the tray, or report that there is nowhere to put one.
///
/// `icon` is PNG bytes. `None` covers every "no tray here" case — headless, no
/// bus tool, no StatusNotifierHost (stock GNOME), a refused registration — and
/// is an ordinary outcome the caller carries on from.
pub fn spawn(icon: &'static [u8], menu: Menu) -> Option<(Box<dyn Tray>, Receiver<Event>)> {
    // No session bus means no tray, and no reason to fork a process to
    // rediscover that. Checked before anything else because it is the case a
    // headless CI run and a bare `ssh` session both land in. The value itself
    // is never used — zbus reads it again — so the `?` is the whole check.
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS")?;
    if !watcher_has_owner(WATCHER_NAME) {
        return None;
    }

    let (events, event_rx) = mpsc::channel::<Event>();
    let (menus, menu_rx) = mpsc::channel::<Menu>();
    let (ready, ready_rx) = mpsc::channel::<bool>();

    let item = SekioTray {
        pixmaps: pixmaps(icon),
        menu: menu.clone(),
        events,
    };

    // Everything that can touch D-Bus happens here. `ksni::blocking::spawn`
    // performs the registration handshake inline, and `Handle::update` waits
    // for the service to acknowledge — neither belongs on the caller's thread.
    // Detached on purpose: it outlives this call and ends when `menus` is
    // dropped, not when anyone joins it.
    let started = thread::Builder::new()
        .name("sekio-tray".into())
        .spawn(move || {
            let handle = match item.spawn() {
                Ok(handle) => {
                    let _ = ready.send(true);
                    handle
                }
                // No host, no bus, or a watcher that refused us. The receiver
                // may already have timed out and gone; either way we are done.
                Err(_) => {
                    let _ = ready.send(false);
                    return;
                }
            };
            while let Ok(next) = menu_rx.recv() {
                handle.update(move |item: &mut SekioTray| item.menu = next);
            }
            // The daemon dropped the tray (or gave up waiting for us). Take
            // the icon down and wait for that to land, which is safe here
            // because this thread is about to end anyway.
            handle.shutdown().wait();
        });
    if started.is_err() {
        return None;
    }

    // Bounded, because "the bus is up but the watcher is not answering" is a
    // real state and startup must not sit in it. Giving up drops `menus`,
    // which is precisely what makes a late-succeeding worker withdraw its own
    // icon instead of leaving one behind.
    if ready_rx.recv_timeout(REGISTER_BUDGET) != Ok(true) {
        return None;
    }

    Some((
        Box::new(LinuxTray {
            current: menu,
            menus,
        }),
        event_rx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything here asserts on the item tree and on what a click sends.
    /// None of it needs a session bus, which CI does not have — and the tree
    /// is the part most likely to drift, since a wrong label or a mis-wired
    /// callback compiles perfectly.
    fn probe(menu: Menu) -> (SekioTray, Receiver<Event>) {
        let (events, rx) = mpsc::channel();
        (
            SekioTray {
                pixmaps: Vec::new(),
                menu,
                events,
            },
            rx,
        )
    }

    fn a_menu() -> Menu {
        Menu {
            hotkey: Some("Ctrl+Shift+Space".into()),
            hotkey_choices: super::super::hotkey_choices(),
            recent: vec![
                PathBuf::from("/tmp/newest.txt"),
                PathBuf::from("/home/x/notes/older.md"),
            ],
        }
    }

    fn label(item: &ksni::MenuItem<SekioTray>) -> Option<&str> {
        match item {
            ksni::MenuItem::Standard(i) => Some(&i.label),
            ksni::MenuItem::Checkmark(i) => Some(&i.label),
            ksni::MenuItem::SubMenu(i) => Some(&i.label),
            _ => None,
        }
    }

    fn labels(items: &[ksni::MenuItem<SekioTray>]) -> Vec<Option<&str>> {
        items.iter().map(label).collect()
    }

    fn submenu<'a>(
        items: &'a [ksni::MenuItem<SekioTray>],
        want: &str,
    ) -> &'a [ksni::MenuItem<SekioTray>] {
        for item in items {
            if let ksni::MenuItem::SubMenu(sub) = item {
                if sub.label == want {
                    return &sub.submenu;
                }
            }
        }
        panic!("no submenu labelled {want}");
    }

    /// Click an item and report what reached the daemon.
    fn click(
        tray: &mut SekioTray,
        rx: &Receiver<Event>,
        item: &ksni::MenuItem<SekioTray>,
    ) -> Event {
        match item {
            ksni::MenuItem::Standard(i) => (i.activate)(tray),
            ksni::MenuItem::Checkmark(i) => (i.activate)(tray),
            other => panic!("{:?} is not clickable", label(other)),
        }
        rx.try_recv().expect("a click must reach the daemon")
    }

    #[test]
    fn the_top_level_is_open_recent_hotkey_then_quit() {
        let items = build_menu(&a_menu());
        assert_eq!(
            labels(&items),
            vec![
                Some(OPEN_LABEL),
                Some(RECENT_LABEL),
                Some(HOTKEY_LABEL),
                None, // the separator
                Some(QUIT_LABEL),
            ]
        );
        assert!(matches!(items[3], ksni::MenuItem::Separator));
    }

    #[test]
    fn open_asks_for_the_file_dialog() {
        let (mut tray, rx) = probe(a_menu());
        let items = build_menu(&tray.menu.clone());
        assert_eq!(click(&mut tray, &rx, &items[0]), Event::OpenFile);
    }

    #[test]
    fn quit_asks_the_daemon_to_stop() {
        let (mut tray, rx) = probe(a_menu());
        let items = build_menu(&tray.menu.clone());
        assert_eq!(click(&mut tray, &rx, &items[4]), Event::Quit);
    }

    #[test]
    fn recent_entries_keep_their_order_and_are_labelled_by_file_name() {
        let items = build_menu(&a_menu());
        let recent = submenu(&items, RECENT_LABEL);
        assert_eq!(labels(recent), vec![Some("newest.txt"), Some("older.md")]);
    }

    #[test]
    fn a_recent_entry_previews_its_whole_path_not_its_label() {
        let (mut tray, rx) = probe(a_menu());
        let items = build_menu(&tray.menu.clone());
        let recent = submenu(&items, RECENT_LABEL);
        assert_eq!(
            click(&mut tray, &rx, &recent[1]),
            Event::Preview(PathBuf::from("/home/x/notes/older.md"))
        );
    }

    /// dbusmenu eats a lone underscore and turns the next character into an
    /// access key, so `release_notes.md` would be shown as `releasenotes.md`.
    #[test]
    fn underscores_in_a_file_name_survive_the_menu() {
        let menu = Menu {
            recent: vec![PathBuf::from("/tmp/release_notes.md")],
            ..a_menu()
        };
        let items = build_menu(&menu);
        assert_eq!(
            labels(submenu(&items, RECENT_LABEL)),
            vec![Some("release__notes.md")]
        );
    }

    #[test]
    fn an_empty_recent_list_says_so_instead_of_opening_onto_nothing() {
        let menu = Menu {
            recent: Vec::new(),
            ..a_menu()
        };
        let items = build_menu(&menu);
        let recent = submenu(&items, RECENT_LABEL);
        assert_eq!(labels(recent), vec![Some(NO_RECENT_LABEL)]);
        match &recent[0] {
            ksni::MenuItem::Standard(i) => {
                assert!(!i.enabled, "the placeholder must not be clickable")
            }
            other => panic!("expected a standard item, got {:?}", label(other)),
        }
    }

    #[test]
    fn exactly_the_registered_hotkey_is_checked() {
        let menu = a_menu();
        let items = build_menu(&menu);
        let hotkey = submenu(&items, HOTKEY_LABEL);
        assert_eq!(
            labels(hotkey),
            menu.hotkey_choices
                .iter()
                .map(|s| Some(s.as_str()))
                .collect::<Vec<_>>()
        );
        let checked: Vec<&str> = hotkey
            .iter()
            .filter_map(|item| match item {
                ksni::MenuItem::Checkmark(i) if i.checked => Some(i.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(checked, vec!["Ctrl+Shift+Space"]);
    }

    #[test]
    fn choosing_a_hotkey_asks_for_that_exact_spec() {
        let (mut tray, rx) = probe(a_menu());
        let items = build_menu(&tray.menu.clone());
        let hotkey = submenu(&items, HOTKEY_LABEL);
        assert_eq!(
            click(&mut tray, &rx, &hotkey[1]),
            Event::SetHotkey("Ctrl+Alt+Space".into())
        );
    }

    #[test]
    fn with_no_hotkey_a_disabled_header_says_so_and_the_choices_remain() {
        let menu = Menu {
            hotkey: None,
            ..a_menu()
        };
        let items = build_menu(&menu);
        let hotkey = submenu(&items, HOTKEY_LABEL);
        assert_eq!(label(&hotkey[0]), Some(NO_HOTKEY_LABEL));
        match &hotkey[0] {
            ksni::MenuItem::Standard(i) => {
                assert!(!i.enabled, "the header is a label, not a choice")
            }
            other => panic!("expected a standard item, got {:?}", label(other)),
        }
        assert_eq!(hotkey.len(), menu.hotkey_choices.len() + 1);
        assert!(
            hotkey
                .iter()
                .all(|item| !matches!(item, ksni::MenuItem::Checkmark(i) if i.checked)),
            "nothing is registered, so nothing may be ticked"
        );
    }

    /// A hotkey set by hand in the config need not be one of the five offered.
    /// It is still live, and the menu has to say so rather than showing five
    /// unticked choices.
    #[test]
    fn a_hotkey_outside_the_offered_list_is_named_in_a_header() {
        let menu = Menu {
            hotkey: Some("Ctrl+Alt+Shift+F9".into()),
            ..a_menu()
        };
        let items = build_menu(&menu);
        let hotkey = submenu(&items, HOTKEY_LABEL);
        assert_eq!(label(&hotkey[0]), Some("Current: Ctrl+Alt+Shift+F9"));
    }

    #[test]
    fn a_path_with_no_file_name_falls_back_to_the_whole_thing() {
        assert_eq!(short_name(Path::new("/")), "/");
        assert_eq!(short_name(Path::new("/tmp/a.txt")), "a.txt");
    }

    // ------------------------------------------------------------ the icon

    #[test]
    fn the_embedded_icon_yields_pixmaps_at_several_sizes() {
        let icons = pixmaps(crate::icon::PNG);
        let sizes: Vec<i32> = icons.iter().map(|i| i.width).collect();
        assert_eq!(sizes, vec![64, 22, 32]);
        for icon in &icons {
            assert_eq!(
                icon.data.len(),
                (icon.width as usize) * (icon.height as usize) * 4,
                "an ARGB32 pixmap is exactly four bytes per pixel"
            );
        }
    }

    /// The failure this guards against is silent: a swapped channel order
    /// still produces a buffer of the right length, and shows up only as a
    /// blue-tinted or invisible icon on someone's panel.
    #[test]
    fn pixmap_bytes_are_alpha_first_not_rgba() {
        let rgba = image::ImageReader::with_format(
            std::io::Cursor::new(crate::icon::PNG),
            image::ImageFormat::Png,
        )
        .decode()
        .expect("the compiled-in icon must decode")
        .to_rgba8();
        let argb = to_argb32(&rgba);

        let out = argb.data.as_chunks::<4>().0;
        let src = rgba.as_raw().as_chunks::<4>().0;
        assert_eq!(out.len(), src.len());
        for (i, (argb, &[r, g, b, a])) in out.iter().zip(src).enumerate() {
            assert_eq!(*argb, [a, r, g, b], "pixel {i} is not big-endian ARGB");
        }
    }

    /// And the same assertion the other way round: the logo has transparent
    /// corners and opaque artwork, so reading a colour byte as alpha would
    /// leave every pixel opaque.
    #[test]
    fn the_icon_keeps_its_transparent_background() {
        let icons = pixmaps(crate::icon::PNG);
        let native = icons.first().expect("the compiled-in icon must decode");
        let alpha: Vec<u8> = native
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[0])
            .collect();
        assert!(alpha.contains(&0), "no transparent background survived");
        assert!(alpha.iter().any(|&a| a > 250), "nothing opaque was drawn");
    }

    #[test]
    fn a_corrupt_icon_yields_no_pixmaps_rather_than_panicking() {
        assert!(pixmaps(b"not a png at all").is_empty());
        assert!(pixmaps(&[]).is_empty());
        assert!(pixmaps(b"\x89PNG\r\n\x1a\n").is_empty());
        assert!(pixmaps(&crate::icon::PNG[..crate::icon::PNG.len() / 2]).is_empty());
    }

    // ------------------------------------------------------------ the bus

    #[test]
    fn find_on_path_answers_nothing_for_a_name_that_is_not_there() {
        // The cwd-as-PATH-entry foot-gun: `find_on_path` must not resolve a
        // name against the current directory.
        assert!(find_on_path("sekio-definitely-not-a-real-binary-xyzzy").is_none());
    }

    /// The one thing that can be checked about the query without a bus: it is
    /// aimed at the bus daemon, never at the watcher. Calling the watcher
    /// would D-Bus-activate it.
    #[test]
    fn the_owner_query_goes_to_the_bus_daemon_not_the_watcher() {
        for tool in [BusTool::Gdbus, BusTool::DbusSend] {
            let cmd = tool.query(Path::new("/bin/true"), WATCHER_NAME);
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert!(
                args.iter()
                    .any(|a| a.contains("org.freedesktop.DBus.NameHasOwner")),
                "{tool:?} must ask NameHasOwner: {args:?}"
            );
            assert!(
                args.iter()
                    .any(|a| a == "org.freedesktop.DBus" || a == "--dest=org.freedesktop.DBus"),
                "{tool:?} must address the bus daemon: {args:?}"
            );
            assert!(
                !args
                    .iter()
                    .any(|a| a.starts_with("--dest") && a.contains("StatusNotifier")),
                "{tool:?} must never send a method call to the watcher: {args:?}"
            );
        }
    }

    #[test]
    fn a_wedged_bus_tool_is_killed_rather_than_waited_on() {
        let Some(sleeper) = find_on_path("sleep") else {
            return; // no `sleep` on this machine; nothing to prove
        };
        let mut cmd = Command::new(sleeper);
        cmd.arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("sleep must start");
        let started = Instant::now();
        assert!(!wait_bounded(
            &mut child,
            started + Duration::from_millis(50)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline was not honoured"
        );
    }

    // ---------------------------------------------------------- the handle

    #[test]
    fn an_unchanged_menu_costs_nothing_and_a_changed_one_is_forwarded() {
        let (menus, rx) = mpsc::channel();
        let menu = a_menu();
        let mut tray = LinuxTray {
            current: menu.clone(),
            menus,
        };

        tray.update(&menu);
        assert!(rx.try_recv().is_err(), "an identical menu must not be sent");

        let changed = Menu {
            hotkey: Some("Super+P".into()),
            ..menu
        };
        tray.update(&changed);
        assert_eq!(rx.try_recv().ok(), Some(changed));
    }

    /// A dead service must not turn a menu refresh into a panic on the UI
    /// thread.
    #[test]
    fn updating_after_the_service_died_is_silent() {
        let (menus, rx) = mpsc::channel();
        drop(rx);
        let mut tray = LinuxTray {
            current: a_menu(),
            menus,
        };
        tray.update(&Menu {
            hotkey: None,
            ..a_menu()
        });
    }

    #[test]
    fn describe_names_what_is_hosting_the_icon() {
        let (menus, _rx) = mpsc::channel();
        let tray = LinuxTray {
            current: a_menu(),
            menus,
        };
        assert!(tray.describe().contains(WATCHER_NAME));
    }
}
