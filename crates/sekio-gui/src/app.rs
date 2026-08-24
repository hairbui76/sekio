//! The eframe application: keyboard handling, per-variant painting, and the
//! UI half of the worker protocol. Nothing here blocks — every frame polls the
//! worker with `try_recv` and paints whatever state it has. The same is true of
//! everything added for the desktop-app shape: the native dialog runs on its
//! own thread (`dialog.rs`), the browser's directory listings go through the
//! same worker as previews (`worker.rs`), and the recent-files file is read and
//! written on a third thread (`recent.rs`). The UI thread only ever polls.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use egui::{FontId, RichText, TextureHandle, TextureOptions, Vec2, ViewportCommand};
use sekio_core::{ListEntry, MetaField, PreviewContent, Reflow};

use crate::browser::{self, Activate, Browser};
use crate::config;
use crate::dialog;
use crate::fonts;
use crate::hotkey::{self, Action as PressAction};
use crate::icon;
use crate::recent::{self, Recent};
use crate::selection;
use crate::state::{close_action, human_size, Close, Mode, RequestTracker, Siblings};
use crate::style::{self, Palette, MONO_SIZE};
use crate::table;
use crate::timing::Timing;
use crate::tray;
use crate::worker::{Kind, Loaded, Outcome, Request, Response, Worker};
use sekio_core::paths;

const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 20.0;

/// How many characters the text area has to gain or lose before the preview is
/// worth laying out again. Under four characters the difference is at most a
/// character or two spread over a handful of columns — invisible — while a
/// one-character threshold would fire on almost every frame of a window drag.
/// At `MONO_SIZE` four characters is roughly 30 px.
/// How many recent files the tray menu offers.
///
/// Short on purpose: a tray menu opens against a screen edge with no room to
/// scroll, and the home screen is where the full list lives.
const TRAY_RECENT: usize = 8;

const REFLOW_THRESHOLD: usize = 4;

/// How long a new width has to hold still before we act on it. Long enough
/// that dragging a window edge across half a screen costs one render rather
/// than one per frame, short enough that letting go of the mouse feels
/// immediate.
const REFLOW_SETTLE: Duration = Duration::from_millis(120);

/// What the central panel is currently showing.
enum View {
    /// Nothing is loaded. For a window launched with no path this is the home
    /// screen — the app name, the keys, the recent files and an obvious way to
    /// open something. For a daemon between popups it is the same state with
    /// the window hidden, so nothing of it is ever seen.
    Home,
    Loading,
    Ready(Box<Shown>),
    Failed(String),
}

impl View {
    /// Is a file (or an attempt at one) on screen, as opposed to the home
    /// screen? This is what decides what Esc means — see `state::close_action`.
    fn is_showing(&self) -> bool {
        !matches!(self, Self::Home)
    }
}

/// A preview that is on screen, together with everything derived from it that
/// we refuse to recompute per frame.
struct Shown {
    loaded: Box<Loaded>,
    /// Built once per preview, on its first frame (see `style::text_job`).
    text_job: Option<egui::text::LayoutJob>,
    /// Column widths for a `Table`, measured once per preview rather than per
    /// frame: sizing a column lays real text out (see `table::Grid::measure`).
    grid: Option<table::Grid>,
    /// Uploaded to the GPU exactly once per preview.
    texture: Option<TextureHandle>,
    elapsed: Duration,
}

/// Something the user asked for by clicking, applied after the frame is laid
/// out. Painting borrows half the app, so the widgets report intent instead of
/// mutating state under themselves.
enum Action {
    /// Show the native "Open file" dialog (or fall back to the browser).
    OpenDialog,
    /// Show or hide the built-in browser pane.
    ToggleBrowser,
    CloseBrowser,
    /// Chosen in the settings menu: repaint in this mode and remember it.
    SetTheme(style::Theme),
    /// Preview this path, at the user's request.
    Open(PathBuf),
    /// List this directory in the browser pane.
    Descend(PathBuf),
    /// List the directory above the browser pane's.
    Parent,
    /// Re-render the current workbook showing this sheet.
    SetSheet(usize),
}

/// Everything the app needs at startup. A struct rather than a fistful of
/// positional arguments, and the one place where "one-shot popup", "window the
/// user launched" and "resident daemon" differ.
pub struct Startup {
    pub worker: Worker,
    pub tracker: RequestTracker,
    /// The path to show immediately, or `None` for a window opened with no
    /// argument (home screen) or a daemon waiting for its first handoff (which
    /// keeps its window hidden until one arrives).
    pub path: Option<PathBuf>,
    /// How this process was started; decides what dismissing does.
    pub mode: Mode,
    pub wrap: bool,
    pub borderless: bool,
    pub timing: Timing,
    /// Paths arriving from the daemon socket thread; `None` in one-shot mode.
    pub incoming: Option<Receiver<PathBuf>>,
    /// Paths the global hotkey resolved, already looked up off the UI thread
    /// (see `hotkey.rs`); `None` when no hotkey is registered.
    pub presses: Option<Receiver<PathBuf>>,
    /// The tray icon and the menu choices coming back from it. `None` whenever
    /// there is no icon — not a daemon, `tray = false`, or a session with
    /// nowhere to put one (see `tray::spawn`).
    pub tray: Option<(Box<dyn tray::Tray>, Receiver<tray::Event>)>,
    /// The combination actually registered, as the user would type it. Shown
    /// ticked in the tray's Hotkey submenu; `None` means none is held.
    pub hotkey_spec: Option<String>,
    /// Where a hotkey chosen from the tray is written back to. `None` when
    /// this environment has no config directory, in which case a choice
    /// applies to this run and is not remembered.
    pub config_path: Option<PathBuf>,
    /// What the user asked for — `System`, not the mode it resolved to. The
    /// settings menu ticks this, so it has to survive the round trip through
    /// the desktop's answer.
    pub theme: style::Theme,
}

pub struct SekioApp {
    worker: Worker,
    /// Every colour this frame is painted in. Held rather than looked up so no
    /// widget has to reach for a global, and so both modes are reachable from a
    /// test that never opens a window.
    palette: Palette,
    /// The syntax theme the *worker* is currently building previews with;
    /// `None` is core's own (dark) default. Kept UI-side so a mode switch can
    /// tell whether the worker has anything to do — see [`SekioApp::poll_theme`].
    syntect: Option<&'static str>,
    /// Generation counter for the main preview.
    tracker: RequestTracker,
    /// A second, independent counter for the browser pane's listings, so
    /// descending a directory never cancels the preview being read (and a slow
    /// preview never delays the pane).
    browse_tracker: RequestTracker,
    siblings: Siblings,
    path: PathBuf,
    view: View,
    /// Image zoom; 1.0 means "scaled to fit".
    zoom: f32,
    timing: Timing,
    first_paint_logged: bool,
    /// The window is fitted to the content once per previewed path.
    sized: bool,
    borderless: bool,
    wrap: bool,
    mode: Mode,
    /// Daemon mode when `Some`: paths handed over by the socket thread.
    incoming: Option<Receiver<PathBuf>>,
    /// Hotkey presses that already resolved to a file.
    presses: Option<Receiver<PathBuf>>,
    tray: Option<Box<dyn tray::Tray>>,
    tray_events: Option<Receiver<tray::Event>>,
    /// The last menu handed to the tray, so an unchanged one is not re-sent.
    /// `Menu` derives `PartialEq` for exactly this.
    tray_menu: tray::Menu,
    /// The combination currently grabbed; `None` when none is.
    hotkey_spec: Option<String>,
    config_path: Option<PathBuf>,
    /// The *preference*, as opposed to `palette.theme`, which is what it
    /// resolved to this frame. Both are needed: one is what the menu shows, the
    /// other is what the window is painted in.
    theme: style::Theme,
    /// The app mark on the home screen, uploaded once and kept. `None` until
    /// the home screen is first painted, and stays `None` if the embedded PNG
    /// will not decode — in which case the wordmark simply stands alone.
    logo: Option<egui::TextureHandle>,
    /// Set by the tray's Quit. A daemon normally *refuses* to close (see
    /// [`SekioApp::handle_close_request`]), and this is the one thing that
    /// means it: end the process, do not hide.
    quitting: bool,
    visible: bool,
    browser: Browser,
    /// Paths chosen in the native dialog, delivered from its thread. `None`
    /// means "the dialog closed without a choice" — cancelled, or never shown.
    picked: Receiver<Option<PathBuf>>,
    picks: Sender<Option<PathBuf>>,
    /// A dialog thread is running; the Open button waits rather than stacking
    /// a second dialog on top of the first.
    dialog_open: bool,
    /// Why the last Open fell back to the built-in browser, shown on the home
    /// screen so "the dialog did nothing" is never what it looks like.
    dialog_note: Option<&'static str>,
    recent: Recent,
    /// `recent` minus the entries that no longer exist, rebuilt when the list
    /// changes rather than stat-ing ten paths every frame.
    recent_shown: Vec<PathBuf>,
    recent_store: recent::Store,
    /// Characters the central panel could paint on the last frame, measured
    /// from the monospace font's own advance (see [`text_columns`]).
    text_columns: Option<usize>,
    /// Where the preview surface was painted last frame, so the wheel can tell
    /// "over the preview" from "over the browser pane".
    content_rect: Option<egui::Rect>,
    /// Which sheet of the current workbook, and which page of the current
    /// paged document, is on screen. Both reset when a different file is
    /// shown: they belong to the document, not to the window.
    sheet: usize,
    page: usize,
    /// Decides when a resize is worth re-requesting the preview for.
    reflow: Reflow,
    /// The in-flight preview is a re-layout of the file already on screen, so
    /// keep painting what we have rather than flashing "loading…".
    reflowing: bool,
}

impl SekioApp {
    /// `tracker` already carries the first in-flight request (when there is a
    /// path): `main` fires it before the window exists so the preview renders
    /// while the GL context is still being created. By the time this runs the
    /// result may already be waiting in the channel.
    pub fn new(ctx: &egui::Context, startup: Startup) -> Self {
        // Both palettes, so egui can switch between them itself at the start of
        // any pass. Which one is *preferred* is `style::install`'s business and
        // belongs to whoever read the config; left alone, egui's own default
        // preference is "follow the system", which is also sekio's.
        style::install_styles(ctx);
        // Before the first layout: egui's bundled faces cannot draw Vietnamese
        // (or anything else in Latin Extended Additional), and a file name
        // full of boxes is what the user sees first.
        fonts::install(ctx);
        // We drive zoom ourselves (image scale for pictures, UI scale for
        // everything else), so egui's built-in Ctrl+± must not also fire.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);

        let Startup {
            worker,
            tracker,
            path,
            mode,
            wrap,
            borderless,
            timing,
            incoming,
            presses,
            tray,
            hotkey_spec,
            config_path,
            theme,
        } = startup;
        let (tray, tray_events) = match tray {
            Some((tray, events)) => (Some(tray), Some(events)),
            None => (None, None),
        };
        let loaded = path.is_some();
        // Only a daemon with nothing to show starts hidden. A window the user
        // launched from a menu must appear, empty or not — that is the whole
        // point of the home screen.
        let visible = loaded || mode != Mode::Daemon;
        let path = path.unwrap_or_default();
        let siblings = if loaded {
            Siblings::scan(&path, wrap)
        } else {
            Siblings::default()
        };
        let (picks, picked) = mpsc::channel();
        Self {
            worker,
            // Whatever egui has resolved by now: the preference is already set
            // and the first `RawInput` has carried the desktop's answer.
            palette: Palette::for_theme(ctx.theme()),
            // What a freshly spawned worker builds its `Previewer` with.
            syntect: None,
            tracker,
            browse_tracker: RequestTracker::new(),
            siblings,
            path,
            view: if loaded { View::Loading } else { View::Home },
            zoom: 1.0,
            timing,
            first_paint_logged: false,
            sized: false,
            borderless,
            wrap,
            mode,
            incoming,
            presses,
            tray,
            tray_events,
            // Deliberately not the menu that was passed to `tray::spawn`: the
            // first `refresh_tray` compares against this and so always sends
            // once, which is what fills in the recent list as soon as the
            // store thread has read it.
            tray_menu: tray::Menu {
                hotkey: None,
                hotkey_choices: Vec::new(),
                recent: Vec::new(),
            },
            hotkey_spec,
            config_path,
            theme,
            logo: None,
            quitting: false,
            visible,
            browser: Browser::default(),
            picked,
            picks,
            dialog_open: false,
            dialog_note: None,
            recent: Recent::new(),
            recent_shown: Vec::new(),
            // Spawns one thread and returns; the list arrives on whichever
            // frame the read finishes, so the home screen paints immediately.
            recent_store: recent::Store::spawn(ctx.clone()),
            // Nothing has been laid out yet: `Reflow` starts at core's default
            // width, which is exactly what the first request (issued before
            // this window existed, with no hint) is rendered at.
            text_columns: None,
            content_rect: None,
            sheet: 0,
            page: 0,
            reflow: Reflow::new(REFLOW_THRESHOLD, REFLOW_SETTLE),
            reflowing: false,
        }
    }

    /// Follow the theme, once per frame.
    ///
    /// egui has already resolved "system" against whatever the desktop last
    /// said, so this is a comparison, not a lookup: a desktop that switches to
    /// light while sekio is open switches sekio on the very next frame. Two
    /// separate checks on purpose — the palette changes whenever the mode does,
    /// while the worker only hears about it when the *syntax* theme differs,
    /// which is what keeps a dark-mode start from re-rendering for nothing.
    fn poll_theme(&mut self, ctx: &egui::Context) {
        let theme = ctx.theme();
        if theme != self.palette.theme {
            self.palette = Palette::for_theme(theme);
            // The laid-out document has the old palette baked into it (see
            // `style::text_job`). Dropping it costs one re-layout on the next
            // frame; leaving it would paint yesterday's colours forever.
            if let View::Ready(shown) = &mut self.view {
                shown.text_job = None;
            }
        }

        let wanted = self.palette.syntect_theme();
        if wanted == self.syntect {
            return;
        }
        self.syntect = wanted;
        // Rebuilding the `Previewer` is tens of milliseconds of syntax-set
        // loading, so it happens on the worker thread, not here.
        self.worker.set_theme(wanted);
        if self.current().is_some() {
            // Deliberately a reflow rather than a navigation: the file has not
            // changed, so it keeps being painted while the re-render is in
            // flight instead of flashing "loading…" at a theme switch.
            self.request_reflow();
        }
    }

    /// The path on screen, or `None` on the home screen.
    fn current(&self) -> Option<&Path> {
        Some(self.path.as_path()).filter(|path| !path.as_os_str().is_empty())
    }

    /// Show a path that arrived over the daemon socket, from the hotkey, or
    /// from anything else that already decided what to preview.
    ///
    /// It goes through `request_current`, i.e. `RequestTracker::begin`, like
    /// every other navigation: whatever preview is in flight is cancelled and
    /// its result discarded when it lands, so a handoff during a slow render
    /// cannot paint the previous file.
    fn show(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.path = path;
        self.siblings = Siblings::scan(&self.path, self.wrap);
        self.view = View::Loading;
        self.zoom = 1.0;
        // A sheet and a page belong to the document, not to the window.
        self.sheet = 0;
        self.page = 0;
        // Each popup is a new file, so the window refits to it.
        self.sized = false;
        self.request_current();

        ctx.send_viewport_cmd(ViewportCommand::Title(format!(
            "sekio — {}",
            file_name(&self.path)
        )));
        if !self.visible {
            self.visible = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        }
        // Always re-raise: the window may be visible but behind the file
        // manager that triggered the popup.
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.timing.log("path shown");
    }

    /// Preview something the user opened *through this window* — the dialog,
    /// the browser, a drop, a recent entry.
    ///
    /// The one difference from `show` is the mode promotion: a popup that the
    /// user has started opening files in is not a popup any more, and Escape
    /// must stop throwing it away (see `state::Mode::promoted`).
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        // Best effort: the recent list and the sibling scan both want a path
        // that still means this file in the next process. A path that cannot
        // be canonicalised (it just vanished) is previewed anyway, so the
        // failure is a message in the window rather than a silent nothing —
        // but it still goes through `paths::plain`, so an unresolvable path is
        // not left in a different spelling from every other one.
        let path = paths::canonical(&path).unwrap_or_else(|_| paths::plain(&path));
        self.mode = self.mode.promoted();
        self.dialog_note = None;
        self.show(ctx, path);
    }

    /// Drop the preview and go back to the home screen, keeping the process
    /// (and its window) alive.
    fn go_home(&mut self, ctx: &egui::Context) {
        self.tracker.cancel_all();
        self.view = View::Home;
        self.path = PathBuf::new();
        self.siblings = Siblings::default();
        self.zoom = 1.0;
        self.sheet = 0;
        self.page = 0;
        self.sized = false;
        self.refresh_recent();
        ctx.set_zoom_factor(1.0);
        ctx.send_viewport_cmd(ViewportCommand::Title("sekio".to_owned()));
    }

    /// Esc / Space. What it means depends only on how the process was started
    /// — see `state::close_action`, which is where the rule is written down
    /// and tested:
    ///
    /// * `sekio-gui <path>` closes, exactly as it always has (Quick Look).
    /// * `--daemon` hides and stays warm.
    /// * `sekio-gui` with no path goes back to the home screen, and on the
    ///   home screen does nothing at all. Closing an application the user
    ///   deliberately launched, before they have opened anything in it, is
    ///   never what they meant.
    fn dismiss(&mut self, ctx: &egui::Context) {
        match close_action(self.mode, self.view.is_showing()) {
            Close::Window => {
                self.tracker.cancel_all();
                self.browse_tracker.cancel_all();
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Close::Hide => {
                self.tracker.cancel_all();
                self.browse_tracker.cancel_all();
                // Dropping the preview (and its GPU texture) so a resident
                // process does not sit on the last hexdump it was shown.
                self.view = View::Home;
                self.path = PathBuf::new();
                self.siblings = Siblings::default();
                self.zoom = 1.0;
                self.sheet = 0;
                self.page = 0;
                self.browser.close();
                self.visible = false;
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            Close::Home => self.go_home(ctx),
            Close::Nothing => {}
        }
    }

    /// Ctrl+Q and the window's close button mean the same thing: end this
    /// window. A daemon still only hides — it is meant to outlive its windows.
    fn quit(&mut self, ctx: &egui::Context) {
        if self.mode == Mode::Daemon {
            self.dismiss(ctx);
            return;
        }
        self.tracker.cancel_all();
        self.browse_tracker.cancel_all();
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// The window manager's close button must not kill a resident daemon: for
    /// it, "close" means the same as Esc — hide, stay warm. eframe closes the
    /// root viewport unless `CancelClose` is sent during this very frame,
    /// which is why this runs in `logic`.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        // `quitting` is the tray's Quit, which is the one close a daemon must
        // honour: it is the only way to stop a resident process that has no
        // window on screen to close.
        if self.quitting
            || self.mode != Mode::Daemon
            || !ctx.input(|i| i.viewport().close_requested())
        {
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        self.dismiss(ctx);
    }

    /// Drain the socket thread's channel. Only the newest path is shown: a
    /// burst of handoffs (a user leaning on the spacebar) costs one preview,
    /// not one per message.
    fn poll_incoming(&mut self, ctx: &egui::Context) {
        let Some(incoming) = &self.incoming else {
            return;
        };
        let mut latest = None;
        while let Ok(path) = incoming.try_recv() {
            latest = Some(path);
        }
        if let Some(path) = latest {
            self.show(ctx, path);
        }
    }

    /// Drain the hotkey thread's channel, newest press wins.
    ///
    /// The path arrives already resolved: the selection lookup (an IPC round
    /// trip that can take ~200 ms) happened on the hotkey thread precisely so
    /// this frame does not wait for it. A press on the file already on screen
    /// dismisses it, the way a second spacebar closes Quick Look.
    fn poll_presses(&mut self, ctx: &egui::Context) {
        let Some(presses) = &self.presses else {
            return;
        };
        let mut latest = None;
        while let Ok(path) = presses.try_recv() {
            latest = Some(path);
        }
        // A press that resolved to nothing never reaches this channel, so
        // `Ignore` here only happens on a frame with no press at all.
        let showing = if self.visible { self.current() } else { None };
        match hotkey::action(latest, showing) {
            PressAction::Show(path) => self.show(ctx, path),
            PressAction::Dismiss => self.dismiss(ctx),
            PressAction::Ignore => {}
        }
    }

    /// Act on what was chosen in the tray menu.
    ///
    /// Every one of these is something the window can already do; the tray is
    /// a second way in, not a second implementation. Drained fully rather than
    /// newest-wins — unlike a burst of handoffs, two menu choices are two
    /// deliberate acts and both deserve to happen.
    fn poll_tray(&mut self, ctx: &egui::Context) {
        let Some(events) = &self.tray_events else {
            return;
        };
        let pending: Vec<tray::Event> = events.try_iter().collect();
        for event in pending {
            match event {
                tray::Event::OpenFile => {
                    // A daemon's window is hidden; a file dialog owned by
                    // nothing visible is a dialog the user cannot place.
                    self.reveal(ctx);
                    self.open_dialog(ctx);
                }
                tray::Event::Preview(path) => self.open(ctx, path),
                tray::Event::SetHotkey(spec) => self.rebind_hotkey(ctx, spec),
                tray::Event::Quit => {
                    self.quitting = true;
                    self.tracker.cancel_all();
                    self.browse_tracker.cancel_all();
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    /// Bring a hidden daemon window up without changing what it is showing.
    fn reveal(&mut self, ctx: &egui::Context) {
        if !self.visible {
            self.visible = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        }
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    /// Grab `spec` instead of whatever is held now, and remember the choice.
    ///
    /// **The new grab is taken before the old one is released**, which is the
    /// whole reason this is not two lines. A failed grab — another application
    /// owns the combination — must leave the daemon exactly as it was rather
    /// than with no hotkey at all, and the config file must not end up naming
    /// a combination that does not work.
    ///
    /// The old grab is released by dropping the receiver its thread sends to,
    /// which that thread only notices on its next press. So the previous
    /// combination stays taken until it is pressed once more, and that press
    /// does nothing. Better than the alternative — the hotkey thread blocks in
    /// the platform's event loop, and there is no way to wake it that does not
    /// mean holding a handle that outlives the grab.
    fn rebind_hotkey(&mut self, ctx: &egui::Context, spec: String) {
        if self.hotkey_spec.as_deref() == Some(spec.as_str()) {
            return;
        }
        let key = match hotkey::parse(&spec) {
            Ok(key) => key,
            // Only reachable if the offered list and the parser disagree,
            // which is a bug rather than something the user did.
            Err(err) => {
                eprintln!("sekio-gui: cannot use hotkey {spec:?}: {err}");
                return;
            }
        };
        let wake = ctx.clone();
        let hotkeys = hotkey::listen(key, &spec, selection::for_this_platform(), move || {
            wake.request_repaint()
        });
        if let Some(warning) = hotkeys.status.warning() {
            eprintln!("{warning}");
            eprintln!("sekio-gui: keeping the previous hotkey");
            return;
        }
        // Now, and only now, the old thread is told to retire.
        self.presses = Some(hotkeys.presses);
        self.hotkey_spec = Some(spec.clone());
        if let Some(path) = &self.config_path {
            if let Err(err) = config::save_hotkey(path, &spec) {
                eprintln!("sekio-gui: hotkey changed but not saved: {err}");
            }
        }
        self.refresh_tray();
    }

    /// Repaint in `theme` from now on, and remember the choice.
    ///
    /// Only the *preference* is set here. Which palette that resolves to is
    /// egui's answer at the start of the next pass, and `poll_theme` picks it
    /// up there — so choosing System takes effect against whatever the desktop
    /// currently says without this function having to ask.
    fn set_theme(&mut self, ctx: &egui::Context, theme: style::Theme) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        style::install(ctx, theme);
        if let Some(path) = &self.config_path {
            if let Err(err) = config::save_theme(path, theme) {
                eprintln!("sekio-gui: theme changed but not saved: {err}");
            }
        }
    }

    /// Upload the app mark, once, the first time the home screen needs it.
    ///
    /// Deliberately not done in `new`: a popup summoned by the hotkey never
    /// shows the home screen, and a texture upload on the cold-start path
    /// would be paid for by every preview that never displays it.
    fn ensure_logo(&mut self, ctx: &egui::Context) {
        if self.logo.is_some() || !matches!(self.view, View::Home) {
            return;
        }
        let Some(icon) = icon::decode(icon::PNG) else {
            return;
        };
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [icon.width as usize, icon.height as usize],
            &icon.rgba,
        );
        self.logo = Some(ctx.load_texture("sekio-mark", image, egui::TextureOptions::LINEAR));
    }

    /// Rebuild the tray menu and send it if anything the user would see moved.
    fn refresh_tray(&mut self) {
        let Some(tray) = &mut self.tray else {
            return;
        };
        let menu = tray::Menu {
            hotkey: self.hotkey_spec.clone(),
            hotkey_choices: tray::hotkey_choices(),
            // A menu pinned to a panel edge is not a place to scroll: the
            // home screen is where the whole list lives.
            recent: self
                .recent_shown
                .iter()
                .take(TRAY_RECENT)
                .cloned()
                .collect(),
        };
        if menu != self.tray_menu {
            tray.update(&menu);
            self.tray_menu = menu;
        }
    }

    /// Drain the dialog thread's channel. A message — a path or a cancel —
    /// always means the dialog is gone, so the flag clears either way; the
    /// thread reports even if `rfd` panics inside it (see `dialog::spawn`).
    fn poll_picked(&mut self, ctx: &egui::Context) {
        let mut chosen = None;
        let mut closed = false;
        while let Ok(result) = self.picked.try_recv() {
            closed = true;
            if result.is_some() {
                chosen = result;
            }
        }
        if closed {
            self.dialog_open = false;
        }
        if let Some(path) = chosen {
            self.open(ctx, path);
        }
    }

    /// A file dropped on the window is the most natural way there is to open
    /// something in a previewer, so it goes through the same path as every
    /// other user-initiated open.
    fn poll_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| dropped_path(&i.raw.dropped_files));
        if let Some(path) = dropped {
            self.open(ctx, path);
        }
    }

    /// Pick up the recent list once the store thread has read it.
    ///
    /// Anything previewed before it landed stays newest: the file on disk is
    /// merged *under* what this session has already seen.
    fn poll_recent(&mut self) {
        let Some(mut merged) = self.recent_store.poll() else {
            return;
        };
        for path in self.recent.paths().iter().rev() {
            merged.add(path);
        }
        self.recent = merged;
        self.refresh_recent();
    }

    /// Remember a path we actually managed to preview, and queue the write.
    fn remember(&mut self, path: &Path) {
        if self.recent.add(path) {
            self.recent_store.remember(&self.recent);
            self.refresh_recent();
        }
    }

    /// Rebuild the home screen's list, dropping anything that has been deleted
    /// since. Done on change, not per frame — ten `stat`s a frame for a list
    /// nobody is looking at would be silly.
    fn refresh_recent(&mut self) {
        self.recent_shown = self.recent.existing();
        // The tray's Recent submenu is the same list, so the one place that
        // knows it changed is the one place that tells the tray.
        self.refresh_tray();
    }

    /// Cancel whatever is in flight and ask the worker for `self.path`.
    fn request_current(&mut self) {
        self.reflowing = false;
        self.send_preview();
    }

    /// Re-request the file already on screen because the window changed width.
    ///
    /// Deliberately *not* `request_current`: the user has not navigated, so the
    /// pane keeps painting what it has instead of flashing "loading…" behind a
    /// drag. It still goes through `RequestTracker::begin`, so a render already
    /// in flight is cancelled and its result discarded like any other
    /// superseded one.
    fn request_reflow(&mut self) {
        self.reflowing = true;
        self.send_preview();
    }

    fn send_preview(&mut self) {
        let (id, cancel) = self.tracker.begin();
        // Whatever width we asked for is now the one on screen, so the next
        // resize is measured against it.
        if let Some(width) = self.text_columns {
            self.reflow.issued(width);
        }
        self.worker.request(Request {
            id,
            path: self.path.clone(),
            cancel,
            kind: Kind::Preview,
            text_width: self.text_columns,
            sheet: self.sheet,
            page: self.page,
        });
    }

    /// Ask the worker to list the browser pane's directory. Core does the IO,
    /// off this thread, exactly as it does for a preview.
    fn request_listing(&mut self) {
        let (id, cancel) = self.browse_tracker.begin();
        self.worker.request(Request {
            id,
            path: self.browser.dir().to_path_buf(),
            cancel,
            kind: Kind::Browse,
            // A listing has no columns to lay out, and no parts to choose.
            text_width: None,
            sheet: 0,
            page: 0,
        });
    }

    /// Re-request the preview when the text area has settled at a materially
    /// different width.
    ///
    /// `true` means a request went out. The clock is a parameter so the rule is
    /// testable without an event loop.
    fn poll_reflow(&mut self, ctx: &egui::Context, now: std::time::Instant) -> bool {
        let Some(width) = self.text_columns else {
            return false;
        };
        let reflowable = matches!(
            &self.view,
            View::Ready(shown) if needs_relayout(&shown.loaded.preview.content)
        );
        if !reflowable {
            return false;
        }
        if self.reflow.observe(width, now).is_some() {
            self.request_reflow();
            return true;
        }
        // egui stops painting once nothing is changing, and the last frame of a
        // drag is exactly when the settle timer starts — so ask for one more
        // frame after it expires, or the resize would sit there unrendered
        // until the user happened to move the mouse.
        if width.abs_diff(self.reflow.current()) >= REFLOW_THRESHOLD {
            ctx.request_repaint_after(REFLOW_SETTLE);
        }
        false
    }

    fn browse(&mut self, dir: PathBuf) {
        self.browser.show(dir);
        self.request_listing();
    }

    fn toggle_browser(&mut self) {
        if self.browser.is_open() {
            self.browser.close();
            return;
        }
        if self.browser.dir().as_os_str().is_empty() {
            let dir = browser::start_dir(self.current());
            self.browse(dir);
        } else {
            // Back where we left it, with a refresh in flight in case the
            // directory changed while the pane was shut.
            self.browser.reopen();
            self.request_listing();
        }
    }

    /// Ctrl+O and the Open buttons.
    ///
    /// The native dialog is preferred, but it is not guaranteed to exist: on
    /// Linux it is the XDG desktop portal, and plenty of sessions have no
    /// portal service running. Rather than opening nothing, we say why and
    /// open the built-in browser, which needs no portal, no GTK and no
    /// external process at all.
    fn open_dialog(&mut self, ctx: &egui::Context) {
        if self.dialog_open {
            return;
        }
        match dialog::availability() {
            dialog::Availability::Native => {
                let start = Some(browser::start_dir(self.current()));
                if dialog::spawn(start, self.picks.clone(), ctx.clone()) {
                    self.dialog_open = true;
                    self.dialog_note = None;
                } else {
                    self.fall_back("could not start the file dialog — using the built-in browser");
                }
            }
            dialog::Availability::Unavailable(reason) => self.fall_back(reason),
        }
    }

    fn fall_back(&mut self, reason: &'static str) {
        self.dialog_note = Some(reason);
        let dir = browser::start_dir(self.current());
        self.browse(dir);
    }

    /// Move `delta` files within the directory and re-preview immediately.
    fn navigate(&mut self, delta: isize) {
        if self.siblings.is_empty() {
            return;
        }
        if let Some(next) = self.siblings.step(delta) {
            self.path = next;
            self.view = View::Loading;
            self.zoom = 1.0;
            // `request_current` cancels the in-flight token first, so a huge
            // file being decoded is abandoned rather than finished.
            self.request_current();
        }
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        while let Some(response) = self.worker.poll() {
            match response.kind {
                Kind::Preview => self.accept_preview(ctx, response),
                Kind::Browse => self.accept_listing(response),
            }
        }
    }

    fn accept_preview(&mut self, ctx: &egui::Context, response: Response) {
        // Generation check: anything but the newest request is stale.
        if !self.tracker.accept(response.id) {
            return;
        }
        let reflowing = std::mem::take(&mut self.reflowing);
        match response.outcome {
            Outcome::Ready(loaded) => {
                // One upload per preview; the handle lives until the next
                // preview replaces it (dropping it frees the GPU texture).
                let texture = loaded.image.as_ref().map(|img| {
                    ctx.load_texture("sekio-preview", img.clone(), TextureOptions::LINEAR)
                });
                // A window the user launched and sized themselves is theirs;
                // only a popup refits itself around each file. Nor do we
                // resize under an open browser pane.
                if !self.sized && self.mode != Mode::App && !self.browser.is_open() {
                    self.sized = true;
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(desired_size(
                        &loaded.preview.content,
                    )));
                }
                self.remember(&response.path);
                self.view = View::Ready(Box::new(Shown {
                    loaded,
                    text_job: None,
                    grid: None,
                    texture,
                    elapsed: response.elapsed,
                }));
            }
            Outcome::Failed(message) => self.view = View::Failed(message),
            // Cancelled results are normal control flow: a newer request
            // is already on its way, so keep showing "loading…" — or, when the
            // cancelled render was only a re-layout of what is already on
            // screen, keep showing that rather than blanking it.
            Outcome::Cancelled if !reflowing => self.view = View::Loading,
            Outcome::Cancelled => {}
        }
        ctx.send_viewport_cmd(ViewportCommand::Title(format!(
            "sekio — {}",
            file_name(&response.path)
        )));
    }

    /// A directory listing for the browser pane. Its own generation counter,
    /// so a listing that lost the race is dropped rather than painted.
    fn accept_listing(&mut self, response: Response) {
        if !self.browse_tracker.accept(response.id) {
            return;
        }
        match response.outcome {
            Outcome::Ready(loaded) => match loaded.preview.content {
                PreviewContent::Listing { entries } => {
                    self.browser.fill(&response.path, entries);
                }
                // Not a directory after all (it changed under us): the pane
                // says so rather than showing something meaningless.
                _ => {
                    self.browser.fail(&response.path);
                }
            },
            Outcome::Failed(_) => {
                self.browser.fail(&response.path);
            }
            Outcome::Cancelled => {}
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let keys = ctx.input(|i| Keys {
            escape: i.key_pressed(egui::Key::Escape),
            space: i.key_pressed(egui::Key::Space),
            up: i.key_pressed(egui::Key::ArrowUp),
            down: i.key_pressed(egui::Key::ArrowDown),
            left: i.key_pressed(egui::Key::ArrowLeft),
            right: i.key_pressed(egui::Key::ArrowRight),
            enter: i.key_pressed(egui::Key::Enter),
            open: i.modifiers.command && i.key_pressed(egui::Key::O),
            browse: i.modifiers.command && i.key_pressed(egui::Key::B),
            quit: i.modifiers.command && i.key_pressed(egui::Key::Q),
            zoom_in: i.modifiers.command
                && (i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals)),
            zoom_out: i.modifiers.command && i.key_pressed(egui::Key::Minus),
            zoom_reset: i.modifiers.command && i.key_pressed(egui::Key::Num0),
        });

        if keys.quit {
            self.quit(ctx);
            return;
        }
        if keys.open {
            self.open_dialog(ctx);
        }
        if keys.browse {
            self.toggle_browser();
        }

        // Whether a text box owns the keyboard. The search field in the
        // browser pane is the only one, and while it has focus a space is a
        // space and an arrow moves the caret — not "dismiss the window" and
        // "change directory". Up, down and Enter still steer the list, which
        // is what makes the box usable without reaching for the mouse.
        let typing = ctx.egui_wants_keyboard_input();

        // Escape backs out of the browser pane first — it is the thing the
        // user most recently opened, and inside it the search comes off before
        // the pane does. Space is the Quick Look dismiss and goes straight to
        // the rule.
        if keys.escape && self.browser.is_open() {
            if !self.browser.clear_filter() {
                self.browser.close();
            }
        } else if keys.escape || (keys.space && !typing) {
            self.dismiss(ctx);
            return;
        }

        if self.browser.is_open() {
            // While the pane is up the arrows steer it; sibling navigation is
            // what they do the rest of the time.
            if keys.up {
                self.browser.move_cursor(-1);
            }
            if keys.down {
                self.browser.move_cursor(1);
            }
            if keys.left && !typing {
                let parent = self.browser.parent();
                if let Some(parent) = parent {
                    self.browse(parent);
                }
            }
            if (keys.right && !typing) || keys.enter {
                let activated = self.browser.activate(self.browser.cursor());
                if let Some(activated) = activated {
                    self.apply(ctx, activated.into());
                }
            }
        } else if keys.left || keys.up {
            self.navigate(-1);
        } else if keys.right || keys.down {
            self.navigate(1);
        }

        if keys.zoom_in {
            self.apply_zoom(ctx, ZOOM_STEP);
        }
        if keys.zoom_out {
            self.apply_zoom(ctx, 1.0 / ZOOM_STEP);
        }
        if keys.zoom_reset {
            self.zoom = 1.0;
            ctx.set_zoom_factor(1.0);
        }
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::OpenDialog => self.open_dialog(ctx),
            Action::ToggleBrowser => self.toggle_browser(),
            Action::CloseBrowser => self.browser.close(),
            Action::SetTheme(theme) => self.set_theme(ctx, theme),
            Action::Open(path) => self.open(ctx, path),
            Action::Descend(dir) => self.browse(dir),
            // Re-renders the file already on screen with a different part of
            // it chosen. Straight through `send_preview`, so the in-flight
            // render is cancelled and a stale one is discarded exactly as it
            // is for any other request. Paging does the same thing from the
            // wheel handler, which has no painted control to route through.
            Action::SetSheet(sheet) => {
                if self.sheet != sheet {
                    self.sheet = sheet;
                    self.send_preview();
                }
            }
            Action::Parent => {
                let parent = self.browser.parent();
                if let Some(parent) = parent {
                    self.browse(parent);
                }
            }
        }
    }

    /// Images zoom themselves; text/hex/listing zoom the whole UI, which is
    /// the closest thing to "make the font bigger" egui offers for free.
    fn apply_zoom(&mut self, ctx: &egui::Context, factor: f32) {
        if self.has_image() {
            self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        } else {
            ctx.set_zoom_factor((ctx.zoom_factor() * factor).clamp(0.5, 4.0));
        }
    }

    fn has_image(&self) -> bool {
        matches!(&self.view, View::Ready(shown) if shown.texture.is_some())
    }

    /// Is the pointer over the preview surface, as opposed to a panel beside
    /// it? `None` for either question means "assume not", so a frame with no
    /// pointer never zooms.
    fn pointer_over_preview(&self, ctx: &egui::Context) -> bool {
        match (self.content_rect, ctx.pointer_latest_pos()) {
            (Some(rect), Some(pos)) => rect.contains(pos),
            _ => false,
        }
    }

    /// Scroll wheel over an image zooms it (everything else scrolls normally).
    /// The scroll delta is *consumed* so the surrounding `ScrollArea` does not
    /// pan at the same time.
    ///
    /// Only while the pointer is actually over the preview. The delta is read
    /// from the context, not from a widget, so an unguarded version ate every
    /// wheel event in the window — with the browser pane open over a PDF, the
    /// file tree could not be scrolled at all because each notch was being
    /// spent zooming the page behind it.
    fn handle_wheel_zoom(&mut self, ctx: &egui::Context) {
        if !self.has_image() || !self.pointer_over_preview(ctx) {
            return;
        }
        let (scroll, zooming) = ctx.input_mut(|i| {
            let delta = i.smooth_scroll_delta.y;
            i.smooth_scroll_delta = Vec2::ZERO;
            (delta, i.modifiers.ctrl || i.modifiers.command)
        });
        if scroll == 0.0 {
            return;
        }
        // On a document with pages, the wheel turns them and Ctrl zooms —
        // which is the way round every PDF reader does it, and the only
        // arrangement in which a multi-page document can be read at all.
        // Everything else is a single picture, where the wheel is the zoom.
        match self.paged() {
            Some((current, total)) if !zooming => {
                let next = if scroll < 0.0 {
                    (current + 1).min(total.saturating_sub(1))
                } else {
                    current.saturating_sub(1)
                };
                if next != current {
                    self.page = next;
                    self.send_preview();
                }
            }
            _ => {
                self.zoom = (self.zoom * (1.0 + scroll * 0.002)).clamp(ZOOM_MIN, ZOOM_MAX);
            }
        }
    }

    /// `(current, total)` for the paged document on screen, zero-based.
    ///
    /// Read back out of the field core emits rather than tracked here, so the
    /// count is whatever the document really has and paging cannot run past
    /// the end of it. Core only emits `page` when there is more than one, so
    /// `None` is the ordinary answer for an image.
    fn paged(&self) -> Option<(usize, usize)> {
        let View::Ready(shown) = &self.view else {
            return None;
        };
        let PreviewContent::Image { fields, .. } = &shown.loaded.preview.content else {
            return None;
        };
        let value = &fields.iter().find(|field| field.key == "page")?.value;
        let (current, total) = value.split_once(" of ")?;
        let current: usize = current.trim().parse().ok()?;
        let total: usize = total.trim().parse().ok()?;
        Some((current.checked_sub(1)?, total))
    }

    fn header(&self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        let response = egui::Panel::top("sekio-header")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // Only the file's name. On the home screen the wordmark
                    // below already says "sekio", and repeating it in the bar
                    // above just reads as the same word twice.
                    if let Some(path) = self.current() {
                        ui.label(RichText::new(file_name(path)).strong());
                    }
                    if let (Some(pos), true) = (self.siblings.position(), self.siblings.len() > 1) {
                        ui.label(
                            RichText::new(format!("{pos} / {}", self.siblings.len()))
                                .color(self.palette.dim),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Rightmost, where a settings control is looked for.
                        self.settings_menu(ui);
                        // One glyph that says what mode it is in and cycles to
                        // the next: a three-item list in a menu is a lot of
                        // furniture for a control whose whole job is "not that
                        // one".
                        let next = self.theme.cycle();
                        if style::icon_button(ui, self.theme.icon(), 15.0)
                            .on_hover_text(format!(
                                "{} — click for {}",
                                self.theme.describe(),
                                next.describe().to_lowercase()
                            ))
                            .clicked()
                        {
                            action = Some(Action::SetTheme(next));
                        }
                        if self.tracker.is_pending() {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                        if let View::Ready(shown) = &self.view {
                            if shown.loaded.preview.truncated {
                                ui.label(RichText::new("truncated").color(self.palette.dim));
                            }
                        }
                        // Always reachable, whatever is on screen: this is the
                        // "open something" the app was missing.
                        if ui
                            .add_enabled(!self.dialog_open, egui::Button::new("Open…"))
                            .on_hover_text("Open a file (Ctrl+O)")
                            .clicked()
                        {
                            action = Some(Action::OpenDialog);
                        }
                        if style::selectable(ui, self.browser.is_open(), RichText::new("Browse"))
                            .on_hover_text("Built-in file browser (Ctrl+B)")
                            .clicked()
                        {
                            action = Some(Action::ToggleBrowser);
                        }
                    });
                });
            })
            .response;

        // A borderless popup still has to be movable: dragging the header moves
        // the window, the way a title bar would.
        if self.borderless && response.interact(egui::Sense::drag()).drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        action
    }

    /// The gear: switch theme, and see what this is and where it keeps its
    /// settings.
    ///
    /// A menu rather than a settings *window*, because there are two things in
    /// it. Everything else worth setting is either a one-shot flag or lives in
    /// `gui.toml`, and a preferences dialog that duplicated the file would be
    /// two places to disagree.
    fn settings_menu(&self, ui: &mut egui::Ui) {
        ui.menu_button("⚙", |ui| {
            ui.set_min_width(212.0);

            // Theme lives on the header's own glyph now; what is left here is
            // the things there is nowhere else to say.
            menu_heading(ui, &self.palette, "About");
            ui.label(format!("sekio {}", env!("CARGO_PKG_VERSION")));
            ui.label(
                RichText::new("Quick preview for any file")
                    .color(self.palette.dim)
                    .size(11.0),
            );
            ui.add_space(4.0);
            // The one fact that is otherwise only discoverable by reading the
            // documentation, and the answer to "where do I change the hotkey?".
            match &self.config_path {
                Some(path) => {
                    ui.label(RichText::new("Settings").color(self.palette.dim).size(11.0));
                    ui.label(
                        RichText::new(browser::compact(path))
                            .monospace()
                            .size(10.0)
                            .color(self.palette.dim),
                    )
                    .on_hover_text(path.display().to_string());
                }
                None => {
                    ui.label(
                        RichText::new("No settings file in this environment")
                            .color(self.palette.dim)
                            .size(11.0),
                    );
                }
            }
            if let Some(spec) = &self.hotkey_spec {
                ui.add_space(4.0);
                ui.label(RichText::new("Hotkey").color(self.palette.dim).size(11.0));
                ui.label(RichText::new(spec).monospace().size(10.0));
            }
            ui.add_space(4.0);
            ui.hyperlink_to(
                RichText::new("github.com/hairbui76/sekio").size(11.0),
                "https://github.com/hairbui76/sekio",
            );
        })
        .response
        .on_hover_text("About sekio");
    }

    fn footer(&self, ui: &mut egui::Ui) {
        let View::Ready(shown) = &self.view else {
            return;
        };
        let palette = &self.palette;
        egui::Panel::bottom("sekio-footer").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match &shown.loaded.preview.content {
                    PreviewContent::Table { .. } => {
                        // How big the sheet is, and — when a cap or the column
                        // window cut it short — how much of it is on screen.
                        if let Some(view) = table_view(&shown.loaded.preview.content) {
                            dim_label(ui, palette, view.footer(shown.loaded.preview.truncated));
                        }
                    }
                    PreviewContent::Text { lines, language } => {
                        dim_label(ui, palette, format!("{language} · {} lines", lines.len()));
                    }
                    PreviewContent::Image {
                        original_width,
                        original_height,
                        format,
                        fields,
                        ..
                    } => {
                        dim_label(
                            ui,
                            palette,
                            format!("{format} · {original_width}×{original_height}"),
                        );
                        if self.zoom != 1.0 {
                            dim_label(ui, palette, format!("{:.0}%", self.zoom * 100.0));
                        }
                        for field in fields {
                            dim_label(ui, palette, format!("{}: {}", field.key, field.value));
                        }
                    }
                    PreviewContent::Listing { entries } => {
                        dim_label(ui, palette, format!("{} entries", entries.len()));
                    }
                    PreviewContent::Metadata { fields, .. } => {
                        dim_label(ui, palette, format!("{} fields", fields.len()));
                    }
                    PreviewContent::HexDump {
                        file_size, mime, ..
                    } => {
                        dim_label(
                            ui,
                            palette,
                            format!(
                                "{} · {}",
                                mime.as_deref().unwrap_or("binary"),
                                human_size(*file_size)
                            ),
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    dim_label(ui, palette, format!("{} ms", shown.elapsed.as_millis()));
                });
            });
        });
    }

    /// The built-in browser, as a resizable pane down the left-hand side.
    fn browser_pane(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        if !self.browser.is_open() {
            return None;
        }
        let palette = self.palette;
        let browser = &mut self.browser;
        egui::Panel::left("sekio-browser")
            .resizable(true)
            // Bounded at both ends. Without a max the pane grows to whatever
            // its widest filename asks for, which is how it ended up owning
            // half the window and left nothing for the drag to do.
            .default_size(320.0)
            .size_range(200.0..=640.0)
            .show(ui, |ui| paint_browser(ui, browser, &palette))
            .inner
    }

    fn body(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let zoom = self.zoom;
        let palette = self.palette;
        // Cloned before `view` is borrowed mutably below. A `TextureHandle` is
        // a reference to an upload egui already owns, so this costs nothing.
        let logo = self.logo.clone();
        let home = HomeScreen {
            recent: &self.recent_shown,
            note: self.dialog_note,
            dialog_open: self.dialog_open,
        };
        let view = &mut self.view;
        // How wide the preview surface really is, in characters. Measured here
        // rather than from the window, because the browser pane and the
        // scrollbar both eat into it.
        let mut columns = None;
        let panel = egui::CentralPanel::default()
            // The one surface that is not chrome: the preview sits on a card
            // raised off the panels around it, which is the whole of the
            // hierarchy this app needs. Same margin as egui's own central
            // panel, so nothing moves and `text_columns` still measures the
            // width the preview really gets.
            .frame(
                egui::Frame::new()
                    .fill(palette.card)
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ui, |ui| {
                columns = Some(text_columns(ui));
                match view {
                    View::Home => paint_home(ui, &home, &palette, logo.as_ref()),
                    View::Loading => {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("loading…").color(palette.dim));
                        });
                        None
                    }
                    // A failed preview is a message in the window, never a
                    // crash; the header still names the file it belongs to.
                    View::Failed(message) => {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                RichText::new(format!("cannot preview: {message}"))
                                    .color(palette.error),
                            );
                        });
                        None
                    }
                    View::Ready(shown) => paint_content(ui, shown, zoom, &palette),
                }
            });
        self.content_rect = Some(panel.response.rect);
        self.text_columns = columns;
        panel.inner
    }
}

/// Is this content laid out by *core*, for a width core has to be told?
///
/// Only text is. An image or a hexdump lays out the same at any width, and
/// re-decoding a photo on every window drag would cost a decode, a GPU upload
/// and a visible flicker for nothing.
///
/// A `Table` used to be text — core flattened a spreadsheet into space-aligned
/// lines, so the only way to use a wider window was to ask for the whole
/// workbook again at a bigger `text_width`. It now hands over the grid itself,
/// and every width in it is decided on this side by `table::Grid`, from content
/// and from the space the pane actually has. Re-requesting one on resize would
/// re-read the workbook to produce byte-identical IR, so a table is explicitly
/// not reflowable: dragging a window edge now just moves the columns.
fn needs_relayout(content: &PreviewContent) -> bool {
    match content {
        PreviewContent::Text { .. } => true,
        PreviewContent::Table { .. }
        | PreviewContent::Image { .. }
        | PreviewContent::Listing { .. }
        | PreviewContent::Metadata { .. }
        | PreviewContent::HexDump { .. } => false,
    }
}

/// How many monospace characters fit across `ui`.
///
/// Measured from the font itself: `glyph_width` is the advance epaint will
/// actually lay the galley out with, so this stays right if the font or
/// [`MONO_SIZE`] ever changes — which a pixels-per-character constant would
/// not. `'0'` is representative because the face is monospace: every glyph in
/// it has the same advance.
fn text_columns(ui: &egui::Ui) -> usize {
    let advance = ui
        .ctx()
        .fonts_mut(|f| f.glyph_width(&FontId::monospace(MONO_SIZE), '0'));
    if !advance.is_finite() || advance <= 0.0 {
        return sekio_core::DEFAULT_TEXT_WIDTH;
    }
    // Leave the vertical scrollbar its lane, so a table that only just fits
    // does not also raise a horizontal one.
    let usable = ui.available_width() - ui.spacing().scroll.bar_width - 2.0;
    (usable / advance).floor().clamp(1.0, 4096.0) as usize
}

struct Keys {
    escape: bool,
    space: bool,
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    enter: bool,
    open: bool,
    browse: bool,
    quit: bool,
    zoom_in: bool,
    zoom_out: bool,
    zoom_reset: bool,
}

impl From<Activate> for Action {
    fn from(activate: Activate) -> Self {
        match activate {
            Activate::Descend(dir) => Self::Descend(dir),
            Activate::Preview(path) => Self::Open(path),
        }
    }
}

impl eframe::App for SekioApp {
    /// Runs before every repaint (and when the window is hidden): all the
    /// non-painting work — polling the worker and reacting to keys — lives
    /// here so it happens even on frames we skip drawing.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Before anything that paints or renders: a mode switch has to reach
        // the worker before this frame's request does, or the preview comes
        // back highlighted for the palette we just left.
        self.poll_theme(ctx);
        // Then, so a path that arrived while the window was hidden is picked
        // up on the very repaint the socket thread asked for.
        self.poll_incoming(ctx);
        // Same route as a socket handoff: `show` -> `request_current` ->
        // `RequestTracker::begin`, so a press during a slow render cancels it.
        self.poll_presses(ctx);
        self.poll_tray(ctx);
        self.poll_picked(ctx);
        self.poll_drops(ctx);
        self.poll_recent();
        self.poll_worker(ctx);
        self.ensure_logo(ctx);
        self.handle_close_request(ctx);
        self.handle_keys(ctx);
        self.handle_wheel_zoom(ctx);
        // Last, and against the width the previous frame measured: a window
        // that has settled at a new size needs the preview laid out for it.
        self.poll_reflow(ctx, std::time::Instant::now());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Panels first, central panel last — egui lays them out in that order.
        let mut action = self.header(ui);
        self.footer(ui);
        action = action.or_else(|| self.browser_pane(ui));
        action = action.or_else(|| self.body(ui));
        paint_drop_hint(ui.ctx(), &self.palette);

        if let Some(action) = action {
            let ctx = ui.ctx().clone();
            self.apply(&ctx, action);
        }

        if !self.first_paint_logged {
            self.first_paint_logged = true;
            self.timing.log("first paint");
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Don't let a long-running render keep the process alive after the
        // window closes.
        self.tracker.cancel_all();
        self.browse_tracker.cancel_all();
    }
}

/// The first path in a drop. A multi-file drop previews one of them — the
/// arrow keys reach the rest, since they are siblings by definition.
fn dropped_path(files: &[egui::DroppedFile]) -> Option<PathBuf> {
    files.iter().find_map(|file| file.path.clone())
}

/// While a drag is over the window, say what letting go will do. Painted on a
/// foreground layer so it covers the panes as well as the preview.
fn paint_drop_hint(ctx: &egui::Context, palette: &Palette) {
    let (hovering, screen) = ctx.input(|i| (!i.raw.hovered_files.is_empty(), i.content_rect()));
    if !hovering {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("sekio-drop-hint"),
    ));
    painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(190));
    painter.rect_stroke(
        screen.shrink(10.0),
        8.0,
        egui::Stroke::new(2.0, palette.accent),
        egui::StrokeKind::Inside,
    );
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        "Drop to preview",
        egui::FontId::proportional(22.0),
        egui::Color32::WHITE,
    );
}

/// Everything the home screen paints, gathered up so it can borrow the app
/// immutably while the central panel holds `&mut View`.
struct HomeScreen<'a> {
    recent: &'a [PathBuf],
    note: Option<&'static str>,
    dialog_open: bool,
}

/// The "nothing loaded yet" screen: what this is, how to open something, what
/// was opened last time, and every key that does anything.
fn paint_home(
    ui: &mut egui::Ui,
    home: &HomeScreen<'_>,
    palette: &Palette,
    logo: Option<&egui::TextureHandle>,
) -> Option<Action> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let available = ui.available_width();
            // Wide enough for two columns side by side on a maximised window,
            // and still bounded: a wordmark and a key list stretched across
            // 2000 px of monitor is not more readable, only emptier.
            let width = available.clamp(280.0, 720.0);
            // A little more air above when there is room for it, so a large
            // window does not look like a small one with the content stuck to
            // the top edge.
            ui.add_space(if available > 900.0 { 44.0 } else { 26.0 });
            ui.vertical_centered(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        action = home_column(ui, home, palette, width, logo);
                    },
                );
            });
            ui.add_space(32.0);
        });
    action
}

fn home_column(
    ui: &mut egui::Ui,
    home: &HomeScreen<'_>,
    palette: &Palette,
    width: f32,
    logo: Option<&egui::TextureHandle>,
) -> Option<Action> {
    let mut action = None;

    // Mark, wordmark and version on one line, centred as a unit.
    //
    // `vertical_centered` centres each child it is given, and a `horizontal`
    // child claims the whole width — so the row's contents started at the left
    // edge while the mark above them sat in the middle. The row is measured
    // and padded here instead, which is the only way to centre a mixed run of
    // widgets in egui.
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let name_width = text_width(ui, "sekio", 32.0);
    let version_width = mono_width(ui, &version, 11.0);
    let mark = if logo.is_some() {
        MARK_SIZE + 10.0
    } else {
        0.0
    };
    let intro = mark + name_width + 6.0 + version_width;

    ui.add_space(32.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(((width - intro) / 2.0).max(0.0));
        if let Some(logo) = logo {
            ui.add(
                egui::Image::new(logo)
                    .fit_to_exact_size(Vec2::splat(MARK_SIZE))
                    .corner_radius(9.0),
            );
            ui.add_space(10.0);
        }
        ui.label(RichText::new("sekio").size(32.0).strong());
        ui.add_space(6.0);
        // Baseline-aligned with the wordmark rather than centred against it:
        // a build number hanging in the middle of a 32 px word reads as part
        // of the name.
        ui.label(
            RichText::new(&version)
                .monospace()
                .size(11.0)
                .color(palette.faint),
        );
    });
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Quick preview for any file").color(palette.dim));
    });

    ui.add_space(32.0);

    // One primary and one secondary, equal width. The pair is the whole point
    // of the screen, so it gets the column rather than sitting inline with a
    // hint the way a toolbar would.
    let gap = 12.0;
    let half = ((width - gap) / 2.0).floor();
    let button = Vec2::new(half, style::CONTROL_HEIGHT);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        if ui
            .add_enabled(
                !home.dialog_open,
                style::primary_button(palette, "Open file…").min_size(button),
            )
            .clicked()
        {
            action = Some(Action::OpenDialog);
        }
        if ui
            .add(egui::Button::new(RichText::new("Browse files").size(14.0)).min_size(button))
            .clicked()
        {
            action = Some(Action::ToggleBrowser);
        }
    });

    ui.add_space(16.0);
    ui.vertical_centered(|ui| {
        if home.dialog_open {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new("waiting for the dialog…").color(palette.dim));
            });
        } else {
            ui.label(
                RichText::new("or drop a file anywhere in this window")
                    .color(palette.dim)
                    .size(13.0),
            );
        }
    });
    if let Some(note) = home.note {
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(note).color(palette.warn).size(12.0));
        });
    }

    // Stacked, full width, one rhythm apart. They used to sit side by side to
    // use a wide window; the design system reads the home column as a single
    // vertical list, and Recent is the thing people came for.
    ui.add_space(32.0);
    if let Some(chosen) = recent_block(ui, home, palette) {
        action = Some(chosen);
    }
    ui.add_space(32.0);
    keys_block(ui, palette);

    action
}

/// What was previewed last, most recent first.
fn recent_block(ui: &mut egui::Ui, home: &HomeScreen<'_>, palette: &Palette) -> Option<Action> {
    let mut action = None;
    section(ui, palette, "Recent");
    if home.recent.is_empty() {
        ui.label(
            RichText::new("Nothing yet — what you preview shows up here.")
                .color(palette.dim)
                .size(12.0),
        );
        return action;
    }
    for path in home.recent.iter().take(HOME_RECENT) {
        // Rows are separated by a rule rather than by air. A recent list is
        // scanned down the left edge, and a hairline under each entry is what
        // turns eight labels into eight rows.
        ui.add_space(ROW_PAD);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let total = ui.available_width();

            // The folder is measured before either is drawn, because the name
            // has to be told how much room is left. Without that the two are
            // laid out independently and a long name runs straight underneath
            // its own folder label — which is exactly what a recent list is
            // full of.
            let folder = path.parent().map(browser::compact);
            let folder_width = folder
                .as_deref()
                .map(|text| text_width(ui, text, FOLDER_SIZE).min(total * 0.45))
                .unwrap_or(0.0);
            let name_width = (total - folder_width - 12.0).max(72.0);

            ui.allocate_ui_with_layout(
                Vec2::new(name_width, ui.spacing().interact_size.y),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    // Elided rather than wrapped: one row per file, always, or
                    // the list stops scanning as a list.
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    if ui
                        .add(egui::Link::new(file_name(path)))
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        action = Some(Action::Open(path.clone()));
                    }
                },
            );

            // Pushed to the far edge rather than trailing the name. Most of a
            // recent list is the same two or three folders, so left-aligned
            // they repeat as ragged noise between the names; against the right
            // edge they line up into a column the eye can skip.
            if let Some(folder) = folder {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    ui.label(RichText::new(folder).color(palette.faint).size(FOLDER_SIZE));
                });
            }
        });
        ui.add_space(ROW_PAD);
        let (rule, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), egui::Sense::hover());
        ui.painter()
            .hline(rule.x_range(), rule.center().y, (1.0, palette.outline));
    }
    action
}

/// Air above and below a recent row, either side of its rule. Two of these
/// plus the row itself is the design system's 44 px minimum.
const ROW_PAD: f32 = 8.0;

/// `text` cut to fit `max` points wide, with an ellipsis where it was cut.
///
/// egui's own `TextWrapMode::Truncate` does not reach the text inside a
/// `Button`'s atoms: the galley is laid out at its full width and then merely
/// *clipped* by whatever contains it. Clipping looks similar and is not the
/// same thing — a panel cannot be dragged narrower than the widest galley
/// inside it, so a 400-character filename still pinned the browser open even
/// though only 280 points of it were visible. Cutting the string is what
/// actually makes the row narrow.
///
/// Binary search over character boundaries, so a long name costs about nine
/// text layouts rather than one per character, and only when it overflows.
fn elide(ui: &egui::Ui, text: &str, size: f32, max: f32) -> String {
    if max <= 0.0 || text_width(ui, text, size) <= max {
        return text.to_owned();
    }
    let budget = max - text_width(ui, "…", size);
    if budget <= 0.0 {
        return "…".to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = lo.midpoint(hi).max(lo + 1);
        let candidate: String = chars[..mid].iter().collect();
        if text_width(ui, &candidate, size) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    out.push('…');
    out
}

/// The app mark on the home screen, sized to sit on the wordmark's line.
const MARK_SIZE: f32 = 36.0;

/// How wide `text` is in the monospace face at `size`.
fn mono_width(ui: &egui::Ui, text: &str, size: f32) -> f32 {
    ui.ctx().fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                text.to_owned(),
                egui::FontId::monospace(size),
                egui::Color32::PLACEHOLDER,
            )
            .size()
            .x
    })
}

/// Point size of the folder beside a recent file.
const FOLDER_SIZE: f32 = 11.0;

/// How wide `text` would be, so a neighbour can be given what is left.
fn text_width(ui: &egui::Ui, text: &str, size: f32) -> f32 {
    ui.ctx().fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                text.to_owned(),
                egui::FontId::proportional(size),
                egui::Color32::PLACEHOLDER,
            )
            .size()
            .x
    })
}

/// Every key that does anything, so the window never needs the manual.
fn keys_block(ui: &mut egui::Ui, palette: &Palette) {
    section(ui, palette, "Keys");

    // Rows are measured and broken here rather than left to
    // `horizontal_wrapped`. A keycap and its label have to stay together, so
    // each pair used to be its own nested `horizontal` — and a nested
    // horizontal does not take part in the parent's wrapping, so the whole
    // legend ran off the right edge of the column instead of folding onto a
    // second line.
    let widths: Vec<f32> = KEYS
        .iter()
        .map(|(key, what)| keycap_width(ui, key) + KEY_LABEL_GAP + text_width(ui, what, 12.0))
        .collect();

    for (i, row) in break_rows(&widths, ui.available_width(), KEY_PAIR_GAP)
        .iter()
        .enumerate()
    {
        if i > 0 {
            ui.add_space(8.0);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = KEY_LABEL_GAP;
            for (n, (key, what)) in KEYS[row.clone()].iter().enumerate() {
                if n > 0 {
                    ui.add_space(KEY_PAIR_GAP - KEY_LABEL_GAP);
                }
                style::kbd(ui, palette, key);
                ui.label(RichText::new(*what).color(palette.dim).size(12.0));
            }
        });
    }
}

/// Greedily pack `widths` into rows no wider than `avail`, separated by `gap`.
///
/// Pure so the packing can be asserted without a window: this is the logic
/// that decides whether the key legend folds onto a second line or runs off
/// the edge of the column, and it got that wrong once already.
///
/// An item wider than `avail` still gets a row — clipping one entry beats
/// looping forever or dropping it silently.
fn break_rows(widths: &[f32], avail: f32, gap: f32) -> Vec<std::ops::Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0.0_f32;
    for (i, w) in widths.iter().enumerate() {
        if i == start {
            used = *w;
            continue;
        }
        if used + gap + w > avail {
            rows.push(start..i);
            start = i;
            used = *w;
        } else {
            used += gap + w;
        }
    }
    if start < widths.len() {
        rows.push(start..widths.len());
    }
    rows
}

/// Between a keycap and the words describing it.
const KEY_LABEL_GAP: f32 = 5.0;
/// Between one key/label pair and the next.
const KEY_PAIR_GAP: f32 = 20.0;

/// How wide `style::kbd` will draw this key, including its padding and border.
fn keycap_width(ui: &egui::Ui, key: &str) -> f32 {
    // 6 px of inner margin either side, plus the 1 px stroke.
    mono_width(ui, key, 11.0) + 14.0
}

/// How many recent files the home screen lists. The whole point is the last
/// few things you looked at; a longer list is a file browser, and there is one
/// of those a keystroke away.
const HOME_RECENT: usize = 8;

/// The key list on the home screen. Kept in one place so it cannot drift from
/// `SekioApp::handle_keys`.
const KEYS: &[(&str, &str)] = &[
    ("Ctrl+O", "open a file"),
    ("Ctrl+B", "built-in file browser"),
    ("← → ↑ ↓", "previous / next file in the folder"),
    ("Ctrl +/-", "zoom, Ctrl+0 to reset"),
    ("Space", "close the preview"),
    ("Esc", "back to this screen"),
    ("Ctrl+Q", "quit"),
];

/// A small capitalised label above a group of menu items.
fn menu_heading(ui: &mut egui::Ui, palette: &Palette, title: &str) {
    ui.label(RichText::new(title).color(palette.dim).size(10.0).strong());
    ui.add_space(2.0);
}

fn section(ui: &mut egui::Ui, palette: &Palette, title: &str) {
    ui.label(RichText::new(title).color(palette.dim).size(11.0).strong());
    ui.add_space(2.0);
}

/// The browser pane: where we are, a way up, and one row per entry.
fn paint_browser(ui: &mut egui::Ui, browser: &mut Browser, palette: &Palette) -> Option<Action> {
    let mut action = None;

    // A titled head rather than two bare glyphs. The pane used to open with an
    // unlabelled "↑" and "✕" over a raw path, which reads as a debug panel.
    ui.horizontal(|ui| {
        ui.label(RichText::new("Browse files").strong().size(14.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if style::icon_button(ui, "×", 15.0)
                .on_hover_text("Close the pane (Esc)")
                .clicked()
            {
                action = Some(Action::CloseBrowser);
            }
            if browser.is_loading() {
                ui.add(egui::Spinner::new().size(11.0));
            }
        });
    });
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if ui
            .add_enabled(
                browser.parent().is_some(),
                egui::Button::new(RichText::new("↑").monospace().size(11.0)),
            )
            .on_hover_text("Parent directory (←)")
            .clicked()
        {
            action = Some(Action::Parent);
        }
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        ui.label(
            RichText::new(browser::compact(browser.dir()))
                .color(palette.dim)
                .size(11.0),
        );
    });

    // The search box, fzf-style: it has the keyboard the moment the pane
    // opens, so browsing a big directory is "start typing" rather than "reach
    // for the mouse".
    ui.add_space(8.0);
    let mut query = browser.filter().to_owned();
    let search = ui.add(
        egui::TextEdit::singleline(&mut query)
            .hint_text("Search this folder")
            .desired_width(f32::INFINITY),
    );
    if search.changed() {
        browser.set_filter(query);
    }
    if browser.take_focus_request() {
        search.request_focus();
    }

    ui.add_space(10.0);
    rule(ui, palette, ui.available_width());
    ui.add_space(10.0);

    // Places rail beside the listing where there is room, a strip above it
    // where there is not — the same fold the design system makes at 700 px.
    if ui.available_width() >= PLACES_SIDE_BY_SIDE {
        ui.horizontal_top(|ui| {
            let rail = PLACES_WIDTH;
            ui.allocate_ui_with_layout(
                Vec2::new(rail, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    if let Some(chosen) = places_rail(ui, browser, palette, true) {
                        action = Some(chosen);
                    }
                },
            );
            ui.add_space(12.0);
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    if let Some(chosen) = listing(ui, browser, palette) {
                        action = Some(chosen);
                    }
                },
            );
        });
    } else {
        if let Some(chosen) = places_rail(ui, browser, palette, false) {
            action = Some(chosen);
        }
        ui.add_space(10.0);
        if let Some(chosen) = listing(ui, browser, palette) {
            action = Some(chosen);
        }
    }

    action
}

/// A one-pixel rule in the palette's own outline, rather than
/// `ui.separator()`, which spans whatever the parent allocated and picks its
/// own colour.
fn rule(ui: &mut egui::Ui, palette: &Palette, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, (1.0, palette.outline));
}

/// Below this the places rail becomes a strip above the listing.
const PLACES_SIDE_BY_SIDE: f32 = 460.0;
/// Width of the rail when it is beside the listing.
const PLACES_WIDTH: f32 = 132.0;

/// The handful of directories worth one click, resolved once.
///
/// `is_dir` is a syscall, and this is painted every frame, so the answer is
/// cached: a home directory does not grow a Documents folder while the pane is
/// open, and if it does, reopening sekio is a fair price.
fn places() -> &'static [(String, PathBuf)] {
    static PLACES: std::sync::OnceLock<Vec<(String, PathBuf)>> = std::sync::OnceLock::new();
    PLACES.get_or_init(|| {
        let Some(home) = browser::home() else {
            return Vec::new();
        };
        let mut found = vec![("Home".to_owned(), home.clone())];
        for name in ["Documents", "Downloads", "Pictures", "Desktop"] {
            let path = home.join(name);
            if path.is_dir() {
                found.push((name.to_owned(), path));
            }
        }
        found
    })
}

fn places_rail(
    ui: &mut egui::Ui,
    browser: &Browser,
    palette: &Palette,
    stacked: bool,
) -> Option<Action> {
    let mut action = None;
    let entries = places();
    if entries.is_empty() {
        return action;
    }
    section(ui, palette, "Places");
    if stacked {
        // A wrapping strip above the listing when the pane is too narrow to
        // give the rail a column of its own.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
            for (name, path) in entries {
                let here = browser.dir() == path.as_path();
                if style::selectable(ui, here, RichText::new(name).size(12.0)).clicked() {
                    action = Some(Action::Descend(path.clone()));
                }
            }
        });
    } else {
        let width = ui.available_width();
        for (name, path) in entries {
            let here = browser.dir() == path.as_path();
            if style::selectable_sized(
                ui,
                here,
                RichText::new(name).size(12.0),
                [width, PLACE_HEIGHT],
            )
            .clicked()
            {
                action = Some(Action::Descend(path.clone()));
            }
        }
    }
    action
}

/// Height of a row in the places rail and the listing. Short of the design
/// system's 44 px because a file pane is a dense surface and eight visible
/// entries beat five, but tall enough that the row — not the word — is the
/// thing being pointed at.
const PLACE_HEIGHT: f32 = 26.0;

/// The directory listing itself.
fn listing(ui: &mut egui::Ui, browser: &mut Browser, palette: &Palette) -> Option<Action> {
    let mut action = None;

    if browser.has_failed() {
        ui.label(
            RichText::new("cannot list this directory")
                .color(palette.error)
                .size(12.0),
        );
        return action;
    } else if browser.is_loading() && browser.entries().is_empty() {
        ui.label(RichText::new("listing…").color(palette.dim).size(12.0));
        return action;
    } else if browser.entries().is_empty() {
        ui.label(RichText::new("empty").color(palette.dim).size(12.0));
        return action;
    } else if browser.visible_len() == 0 {
        // The directory has things in it; the search is what is hiding them,
        // and saying so is the difference between "empty folder" and "try a
        // different query".
        ui.label(
            RichText::new(format!("nothing matches {:?}", browser.filter()))
                .color(palette.dim)
                .size(12.0),
        );
        return action;
    }

    let dir_color = palette.accent;
    let cursor = browser.cursor();
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Truncated, never extended. An extended row is as wide as its
            // longest filename, and a panel cannot be dragged narrower than the
            // widest thing inside it — so one long name pinned the pane open.
            // The name is on the row's tooltip, so eliding it loses nothing.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            let width = ui.available_width();
            // What is left of the row once the button's own padding is taken.
            let room = width - 24.0;
            for (i, entry) in browser.visible().enumerate() {
                let full = if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                };
                let color = if entry.is_dir {
                    dir_color
                } else {
                    palette.text
                };
                let text = RichText::new(elide(ui, &full, 12.0, room)).color(color);
                // Sized to the full width so the row is the target. A bare
                // `selectable_label` is only as wide as its text, which paints
                // selection as a pill around the word and makes a directory
                // listing look like a tag cloud.
                if style::selectable_sized(ui, i == cursor, text.size(12.0), [width, PLACE_HEIGHT])
                    .on_hover_text(&entry.name)
                    .clicked()
                {
                    clicked = Some(i);
                }
            }
        });

    if let Some(i) = clicked {
        browser.select(i);
        action = browser.activate(i).map(Action::from);
    }
    action
}

fn dim_label(ui: &mut egui::Ui, palette: &Palette, text: String) {
    ui.label(RichText::new(text).color(palette.dim).size(11.0));
}

/// Borrow a `Table` out of the IR, so the painter and the footer read the same
/// six fields without either of them spelling them out again.
fn table_view(content: &PreviewContent) -> Option<table::Table<'_>> {
    match content {
        PreviewContent::Table {
            columns,
            rows,
            sheets,
            active_sheet,
            total_rows,
            total_cols,
        } => Some(table::Table {
            columns,
            rows,
            sheets,
            active_sheet: *active_sheet,
            total_rows: *total_rows,
            total_cols: *total_cols,
        }),
        _ => None,
    }
}

/// Returns whatever the user asked for by clicking inside the preview — today
/// only a different sheet.
fn paint_content(
    ui: &mut egui::Ui,
    shown: &mut Shown,
    zoom: f32,
    palette: &Palette,
) -> Option<Action> {
    match &shown.loaded.preview.content {
        PreviewContent::Table { .. } => {
            let view = table_view(&shown.loaded.preview.content)?;
            // Measured on the preview's first frame and kept: sizing a column
            // lays real text out, which is not a per-frame cost worth paying.
            let grid = shown
                .grid
                .get_or_insert_with(|| table::Grid::measure(ui.ctx(), &view));
            return table::paint(ui, &view, grid, palette).map(Action::SetSheet);
        }
        PreviewContent::Text { lines, .. } => {
            let job = shown
                .text_job
                .get_or_insert_with(|| style::text_job(lines, palette, MONO_SIZE));
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(style::NO_WRAP);
                    // The job is pre-built; egui memoizes the galley by its
                    // hash, so this does not re-layout every frame.
                    ui.label(job.clone());
                });
        }
        PreviewContent::Image { .. } => {
            if let Some(texture) = &shown.texture {
                paint_image(ui, texture, zoom);
            }
        }
        PreviewContent::Listing { entries } => paint_listing(ui, entries, palette),
        PreviewContent::Metadata { fields, .. } => {
            let texture = shown.texture.clone();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(texture) = &texture {
                        ui.vertical_centered(|ui| {
                            let size = fit(texture.size_vec2(), Vec2::new(240.0, 240.0), 1.0);
                            ui.image((texture.id(), size));
                        });
                        ui.add_space(8.0);
                    }
                    paint_fields(ui, fields, palette);
                });
        }
        PreviewContent::HexDump { data, .. } => paint_hex(ui, data, palette),
    }
    None
}

fn paint_image(ui: &mut egui::Ui, texture: &TextureHandle, zoom: f32) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let size = fit(texture.size_vec2(), ui.available_size(), zoom);
            ui.vertical_centered(|ui| {
                ui.add_space(((ui.available_height() - size.y) / 2.0).max(0.0));
                ui.image((texture.id(), size));
            });
        });
}

/// Scale `image` down to fit `avail` (never up, unless the user zoomed in).
fn fit(image: Vec2, avail: Vec2, zoom: f32) -> Vec2 {
    if image.x <= 0.0 || image.y <= 0.0 {
        return image;
    }
    let scale = (avail.x / image.x).min(avail.y / image.y).clamp(0.01, 1.0);
    image * scale * zoom
}

fn paint_listing(ui: &mut egui::Ui, entries: &[ListEntry], palette: &Palette) {
    // `show_rows` assumes every row is exactly this tall, so measure the font
    // we actually paint with rather than the theme's monospace text style.
    let row_height = mono_row_height(ui);
    let dir_color = palette.accent;
    let text_color = palette.text;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, entries.len(), |ui, range| {
            ui.style_mut().wrap_mode = Some(style::NO_WRAP);
            for entry in &entries[range] {
                let size = entry.size.map(human_size).unwrap_or_default();
                let name = if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                };
                ui.label(style::mono_job(
                    &[
                        (&format!("{size:>10}  "), palette.dim),
                        (&name, if entry.is_dir { dir_color } else { text_color }),
                    ],
                    MONO_SIZE,
                ));
            }
        });
}

/// Height of one line in the monospace font used by the row-based views.
fn mono_row_height(ui: &egui::Ui) -> f32 {
    ui.ctx()
        .fonts_mut(|f| f.row_height(&egui::FontId::monospace(MONO_SIZE)))
}

fn paint_fields(ui: &mut egui::Ui, fields: &[MetaField], palette: &Palette) {
    egui::Grid::new("sekio-fields")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for field in fields {
                ui.label(RichText::new(&field.key).color(palette.dim).monospace());
                ui.label(RichText::new(&field.value).color(palette.text).monospace());
                ui.end_row();
            }
        });
}

/// Offset / hex / ASCII columns, mirroring the CLI's hexdump layout.
fn paint_hex(ui: &mut egui::Ui, data: &[u8], palette: &Palette) {
    // `show_rows` assumes every row is exactly this tall, so measure the font
    // we actually paint with rather than the theme's monospace text style.
    let row_height = mono_row_height(ui);
    let rows = data.len().div_ceil(16);
    let text_color = palette.text;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            ui.style_mut().wrap_mode = Some(style::NO_WRAP);
            for row in range {
                let start = row * 16;
                let chunk = &data[start..(start + 16).min(data.len())];
                ui.label(style::mono_job(
                    &[
                        (&format!("{start:08x}  "), palette.dim),
                        (&hex_columns(chunk), text_color),
                        (&format!(" |{}|", ascii_columns(chunk)), palette.dim),
                    ],
                    MONO_SIZE,
                ));
            }
        });
}

fn hex_columns(chunk: &[u8]) -> String {
    let mut out = String::with_capacity(16 * 3 + 1);
    for j in 0..16 {
        match chunk.get(j) {
            Some(b) => out.push_str(&format!("{b:02x} ")),
            None => out.push_str("   "),
        }
        if j == 7 {
            out.push(' ');
        }
    }
    out
}

fn ascii_columns(chunk: &[u8]) -> String {
    chunk
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// A window size that suits the content, applied once when the first preview
/// lands (we cannot know it before the preview exists).
fn desired_size(content: &PreviewContent) -> Vec2 {
    const CHAR_W: f32 = 7.5;
    const LINE_H: f32 = 17.0;
    const CHROME: f32 = 72.0;
    let clamp = |w: f32, h: f32| Vec2::new(w.clamp(420.0, 1400.0), h.clamp(300.0, 900.0));

    match content {
        // A grid wants width per column and a line per row. The real column
        // widths are not known until the font has measured them, so this is a
        // first guess at a window to open at; the table scrolls sideways if it
        // turns out to want more.
        PreviewContent::Table { columns, rows, .. } => clamp(
            (columns.len().clamp(1, 10) as f32 * 13.0 + 6.0) * CHAR_W + 40.0,
            (rows.len() + 2) as f32 * LINE_H + CHROME,
        ),
        PreviewContent::Text { lines, .. } => {
            let cols = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.text.chars().count())
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(80)
                .clamp(40, 120) as f32;
            clamp(cols * CHAR_W + 40.0, lines.len() as f32 * LINE_H + CHROME)
        }
        PreviewContent::Image {
            image,
            original_width,
            original_height,
            ..
        } => {
            let (w, h) = if *original_width > 0 && *original_height > 0 {
                (*original_width as f32, *original_height as f32)
            } else {
                (image.width() as f32, image.height() as f32)
            };
            let scale = (1200.0 / w).min(800.0 / h).min(1.0);
            clamp(w * scale + 32.0, h * scale + CHROME)
        }
        PreviewContent::Listing { entries } => {
            let cols = entries
                .iter()
                .map(|e| e.name.chars().count() + 13)
                .max()
                .unwrap_or(50)
                .clamp(40, 100) as f32;
            clamp(cols * CHAR_W + 40.0, entries.len() as f32 * LINE_H + CHROME)
        }
        PreviewContent::Metadata { fields, .. } => {
            clamp(560.0, fields.len() as f32 * 22.0 + CHROME + 260.0)
        }
        PreviewContent::HexDump { data, .. } => clamp(
            78.0 * CHAR_W + 40.0,
            data.len().div_ceil(16) as f32 * LINE_H + CHROME,
        ),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_row_matches_the_cli_layout() {
        let chunk: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            hex_columns(&chunk),
            "00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f "
        );
        assert_eq!(ascii_columns(b"ab\x00~"), "ab.~");
    }

    #[test]
    fn short_hex_row_is_padded_to_full_width() {
        let full = hex_columns(&[0u8; 16]).len();
        assert_eq!(hex_columns(&[1, 2, 3]).len(), full);
    }

    #[test]
    fn fit_scales_down_but_not_up() {
        let big = fit(Vec2::new(2000.0, 1000.0), Vec2::new(500.0, 500.0), 1.0);
        assert!((big.x - 500.0).abs() < 0.1 && (big.y - 250.0).abs() < 0.1);
        let small = fit(Vec2::new(100.0, 50.0), Vec2::new(500.0, 500.0), 1.0);
        assert_eq!(small, Vec2::new(100.0, 50.0));
        let zoomed = fit(Vec2::new(100.0, 50.0), Vec2::new(500.0, 500.0), 2.0);
        assert_eq!(zoomed, Vec2::new(200.0, 100.0));
    }

    #[test]
    fn desired_size_stays_within_sane_bounds() {
        let content = PreviewContent::HexDump {
            data: vec![0; 4096],
            file_size: 4096,
            mime: None,
        };
        let size = desired_size(&content);
        assert!(size.x >= 420.0 && size.x <= 1400.0);
        assert!(size.y >= 300.0 && size.y <= 900.0);

        // A 16 384-column, 500-row sheet must not ask for a window the size of
        // a wall; it scrolls instead.
        let sheet = PreviewContent::Table {
            columns: (0..900).map(|i| i.to_string()).collect(),
            rows: vec![sekio_core::TableRow::default(); 500],
            sheets: Vec::new(),
            active_sheet: 0,
            total_rows: 500,
            total_cols: 16_384,
        };
        let size = desired_size(&sheet);
        assert!(size.x >= 420.0 && size.x <= 1400.0, "{size:?}");
        assert!(size.y >= 300.0 && size.y <= 900.0, "{size:?}");
    }

    /// The reflow machinery exists for exactly one variant, and the cost of it
    /// leaking to another is a full re-render per window drag.
    #[test]
    fn only_text_is_laid_out_by_core_for_a_width_we_supply() {
        assert!(needs_relayout(&PreviewContent::Text {
            lines: Vec::new(),
            language: String::new(),
        }));
        // A table carries the grid; the widths are decided in `table.rs`, so
        // re-requesting one at a new width would re-read the workbook for
        // byte-identical IR.
        assert!(!needs_relayout(&PreviewContent::Table {
            columns: Vec::new(),
            rows: Vec::new(),
            sheets: Vec::new(),
            active_sheet: 0,
            total_rows: 0,
            total_cols: 0,
        }));
        assert!(!needs_relayout(&PreviewContent::Listing {
            entries: Vec::new()
        }));
        assert!(!needs_relayout(&PreviewContent::HexDump {
            data: Vec::new(),
            file_size: 0,
            mime: None,
        }));
        assert!(!needs_relayout(&PreviewContent::Metadata {
            fields: Vec::new(),
            thumbnail: None,
        }));
    }

    #[test]
    fn the_home_screen_is_the_only_state_that_is_not_showing_a_file() {
        assert!(!View::Home.is_showing());
        assert!(View::Loading.is_showing());
        assert!(View::Failed("boom".to_owned()).is_showing());
    }

    fn dropped(path: Option<&str>) -> egui::DroppedFile {
        egui::DroppedFile {
            path: path.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn a_drop_takes_the_first_path_it_can_use() {
        assert_eq!(dropped_path(&[]), None);
        // A web-style drop carries bytes and no path: nothing to preview.
        assert_eq!(dropped_path(&[dropped(None)]), None);
        assert_eq!(
            dropped_path(&[
                dropped(None),
                dropped(Some("/d/a.txt")),
                dropped(Some("/d/b.txt"))
            ]),
            Some(PathBuf::from("/d/a.txt"))
        );
    }

    #[test]
    fn browser_activation_maps_onto_the_right_action() {
        assert!(matches!(
            Action::from(Activate::Descend(PathBuf::from("/d"))),
            Action::Descend(dir) if dir == Path::new("/d")
        ));
        // Activating a file is the same "the user opened this" path as the
        // dialog and a drop, so it promotes the mode too.
        assert!(matches!(
            Action::from(Activate::Preview(PathBuf::from("/d/a.txt"))),
            Action::Open(path) if path == Path::new("/d/a.txt")
        ));
    }

    /// The key legend used to be one `horizontal_wrapped` holding a nested
    /// `horizontal` per pair — and a nested horizontal does not take part in
    /// the parent's wrapping, so the legend ran off the right of the column
    /// instead of folding. Packing is asserted here rather than by eye.
    #[test]
    fn no_row_of_keys_is_wider_than_the_column() {
        let widths = [100.0, 100.0, 100.0, 100.0];
        let gap = 20.0;
        for avail in [80.0_f32, 120.0, 240.0, 360.0, 1000.0] {
            let rows = break_rows(&widths, avail, gap);
            assert!(!rows.is_empty(), "{avail} produced no rows");
            let packed: usize = rows.iter().map(|r| r.len()).sum();
            assert_eq!(packed, widths.len(), "every key must appear exactly once");
            for row in &rows {
                let used: f32 = widths[row.clone()].iter().sum::<f32>()
                    + gap * (row.len().saturating_sub(1)) as f32;
                // A single item may exceed the column; two never may.
                assert!(
                    row.len() == 1 || used <= avail,
                    "a row of {} is {used} wide in {avail}",
                    row.len()
                );
            }
        }
    }

    #[test]
    fn a_wide_column_keeps_every_key_on_one_row() {
        let widths = [50.0, 50.0, 50.0];
        assert_eq!(break_rows(&widths, 1000.0, 10.0), vec![0..3]);
    }

    #[test]
    fn an_item_wider_than_the_column_still_gets_a_row() {
        let widths = [500.0, 10.0];
        let rows = break_rows(&widths, 100.0, 10.0);
        assert_eq!(rows, vec![0..1, 1..2]);
    }

    #[test]
    fn every_key_the_home_screen_advertises_is_spelled_out() {
        for (key, what) in KEYS {
            assert!(!key.is_empty() && !what.is_empty());
        }
        let keys: Vec<&str> = KEYS.iter().map(|(key, _)| *key).collect();
        assert!(keys.contains(&"Ctrl+O"), "the fix for the whole complaint");
        assert!(keys.contains(&"Ctrl+B"), "the fallback that always works");
    }
}
