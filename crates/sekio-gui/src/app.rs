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
use crate::dialog;
use crate::fonts;
use crate::hotkey::{self, Action as PressAction};
use crate::recent::{self, Recent};
use crate::state::{close_action, human_size, Close, Mode, RequestTracker, Siblings};
use crate::style::{self, MONO_SIZE};
use crate::table;
use crate::timing::Timing;
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
    /// Preview this path, at the user's request.
    Open(PathBuf),
    /// List this directory in the browser pane.
    Descend(PathBuf),
    /// List the directory above the browser pane's.
    Parent,
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
}

pub struct SekioApp {
    worker: Worker,
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
        ctx.set_visuals(egui::Visuals::dark());
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
        } = startup;
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
            reflow: Reflow::new(REFLOW_THRESHOLD, REFLOW_SETTLE),
            reflowing: false,
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
        if self.mode != Mode::Daemon || !ctx.input(|i| i.viewport().close_requested()) {
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
            // A listing has no columns to lay out.
            text_width: None,
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

        // Escape backs out of the browser pane first — it is the thing the
        // user most recently opened. Space is the Quick Look dismiss and goes
        // straight to the rule.
        if keys.escape && self.browser.is_open() {
            self.browser.close();
        } else if keys.escape || keys.space {
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
            if keys.left {
                let parent = self.browser.parent();
                if let Some(parent) = parent {
                    self.browse(parent);
                }
            }
            if keys.right || keys.enter {
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
            Action::Open(path) => self.open(ctx, path),
            Action::Descend(dir) => self.browse(dir),
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

    /// Scroll wheel over an image zooms it (everything else scrolls normally).
    /// The scroll delta is *consumed* so the surrounding `ScrollArea` does not
    /// pan at the same time.
    fn handle_wheel_zoom(&mut self, ctx: &egui::Context) {
        if !self.has_image() {
            return;
        }
        let scroll = ctx.input_mut(|i| {
            let delta = i.smooth_scroll_delta.y;
            i.smooth_scroll_delta = Vec2::ZERO;
            delta
        });
        if scroll != 0.0 {
            self.zoom = (self.zoom * (1.0 + scroll * 0.002)).clamp(ZOOM_MIN, ZOOM_MAX);
        }
    }

    fn header(&self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        let response = egui::Panel::top("sekio-header")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // There is no path on the home screen, or in a daemon
                    // between popups.
                    let title = match self.current() {
                        Some(path) => file_name(path),
                        None => "sekio".to_owned(),
                    };
                    ui.label(RichText::new(title).strong());
                    if let (Some(pos), true) = (self.siblings.position(), self.siblings.len() > 1) {
                        ui.label(
                            RichText::new(format!("{pos} / {}", self.siblings.len()))
                                .color(style::DIM),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.tracker.is_pending() {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                        if let View::Ready(shown) = &self.view {
                            if shown.loaded.preview.truncated {
                                ui.label(RichText::new("truncated").color(style::DIM));
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
                        if ui
                            .selectable_label(self.browser.is_open(), "Browse")
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

    fn footer(&self, ui: &mut egui::Ui) {
        let View::Ready(shown) = &self.view else {
            return;
        };
        egui::Panel::bottom("sekio-footer").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match &shown.loaded.preview.content {
                    PreviewContent::Table { .. } => {
                        // How big the sheet is, and — when a cap or the column
                        // window cut it short — how much of it is on screen.
                        if let Some(view) = table_view(&shown.loaded.preview.content) {
                            dim_label(ui, view.footer(shown.loaded.preview.truncated));
                        }
                    }
                    PreviewContent::Text { lines, language } => {
                        dim_label(ui, format!("{language} · {} lines", lines.len()));
                    }
                    PreviewContent::Image {
                        original_width,
                        original_height,
                        format,
                        fields,
                        ..
                    } => {
                        dim_label(ui, format!("{format} · {original_width}×{original_height}"));
                        if self.zoom != 1.0 {
                            dim_label(ui, format!("{:.0}%", self.zoom * 100.0));
                        }
                        for field in fields {
                            dim_label(ui, format!("{}: {}", field.key, field.value));
                        }
                    }
                    PreviewContent::Listing { entries } => {
                        dim_label(ui, format!("{} entries", entries.len()));
                    }
                    PreviewContent::Metadata { fields, .. } => {
                        dim_label(ui, format!("{} fields", fields.len()));
                    }
                    PreviewContent::HexDump {
                        file_size, mime, ..
                    } => {
                        dim_label(
                            ui,
                            format!(
                                "{} · {}",
                                mime.as_deref().unwrap_or("binary"),
                                human_size(*file_size)
                            ),
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    dim_label(ui, format!("{} ms", shown.elapsed.as_millis()));
                });
            });
        });
    }

    /// The built-in browser, as a resizable pane down the left-hand side.
    fn browser_pane(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        if !self.browser.is_open() {
            return None;
        }
        let browser = &mut self.browser;
        egui::Panel::left("sekio-browser")
            .default_size(260.0)
            .min_size(150.0)
            .show(ui, |ui| paint_browser(ui, browser))
            .inner
    }

    fn body(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let zoom = self.zoom;
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
        let action = egui::CentralPanel::default()
            .show(ui, |ui| {
                columns = Some(text_columns(ui));
                match view {
                    View::Home => paint_home(ui, &home),
                    View::Loading => {
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new("loading…").color(style::DIM));
                        });
                        None
                    }
                    // A failed preview is a message in the window, never a
                    // crash; the header still names the file it belongs to.
                    View::Failed(message) => {
                        let color = ui.visuals().error_fg_color;
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                RichText::new(format!("cannot preview: {message}")).color(color),
                            );
                        });
                        None
                    }
                    View::Ready(shown) => {
                        paint_content(ui, shown, zoom);
                        None
                    }
                }
            })
            .inner;
        self.text_columns = columns;
        action
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
        // First, so a path that arrived while the window was hidden is picked
        // up on the very repaint the socket thread asked for.
        self.poll_incoming(ctx);
        // Same route as a socket handoff: `show` -> `request_current` ->
        // `RequestTracker::begin`, so a press during a slow render cancels it.
        self.poll_presses(ctx);
        self.poll_picked(ctx);
        self.poll_drops(ctx);
        self.poll_recent();
        self.poll_worker(ctx);
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
        paint_drop_hint(ui.ctx());

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
fn paint_drop_hint(ctx: &egui::Context) {
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
        egui::Stroke::new(2.0, style::DIM),
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
fn paint_home(ui: &mut egui::Ui, home: &HomeScreen<'_>) -> Option<Action> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(26.0);
            // A fixed-width column, centred: `ui.horizontal` inside it then
            // lays out left-to-right the way it looks like it should.
            let width = ui.available_width().clamp(220.0, 460.0);
            ui.vertical_centered(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        action = home_column(ui, home);
                    },
                );
            });
            ui.add_space(24.0);
        });
    action
}

fn home_column(ui: &mut egui::Ui, home: &HomeScreen<'_>) -> Option<Action> {
    let mut action = None;

    ui.label(RichText::new("sekio").size(34.0).strong());
    ui.label(
        RichText::new(format!(
            "quick preview for any file · v{}",
            env!("CARGO_PKG_VERSION")
        ))
        .color(style::DIM),
    );

    ui.add_space(18.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!home.dialog_open, egui::Button::new("Open file…"))
            .clicked()
        {
            action = Some(Action::OpenDialog);
        }
        if ui.button("Browse files").clicked() {
            action = Some(Action::ToggleBrowser);
        }
        if home.dialog_open {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("waiting for the dialog…").color(style::DIM));
        }
    });
    ui.add_space(6.0);
    ui.label(
        RichText::new("…or drop a file anywhere in this window.")
            .color(style::DIM)
            .size(12.0),
    );
    if let Some(note) = home.note {
        ui.add_space(4.0);
        ui.label(
            RichText::new(note)
                .color(ui.visuals().warn_fg_color)
                .size(12.0),
        );
    }

    ui.add_space(20.0);
    section(ui, "Recent");
    if home.recent.is_empty() {
        ui.label(
            RichText::new("nothing yet — what you preview shows up here.")
                .color(style::DIM)
                .size(12.0),
        );
    } else {
        for path in home.recent {
            ui.horizontal(|ui| {
                ui.style_mut().wrap_mode = Some(style::NO_WRAP);
                if ui.link(file_name(path)).clicked() {
                    action = Some(Action::Open(path.clone()));
                }
                if let Some(parent) = path.parent() {
                    ui.label(
                        RichText::new(browser::compact(parent))
                            .color(style::DIM)
                            .size(11.0),
                    );
                }
            });
        }
    }

    ui.add_space(20.0);
    section(ui, "Keys");
    egui::Grid::new("sekio-keys")
        .num_columns(2)
        .spacing([18.0, 3.0])
        .show(ui, |ui| {
            for (key, what) in KEYS {
                ui.label(RichText::new(*key).monospace().size(11.0));
                ui.label(RichText::new(*what).color(style::DIM).size(11.0));
                ui.end_row();
            }
        });

    action
}

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

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).color(style::DIM).size(11.0).strong());
    ui.add_space(2.0);
}

/// The browser pane: where we are, a way up, and one row per entry.
fn paint_browser(ui: &mut egui::Ui, browser: &mut Browser) -> Option<Action> {
    let mut action = None;

    ui.horizontal(|ui| {
        if ui
            .add_enabled(browser.parent().is_some(), egui::Button::new("↑"))
            .on_hover_text("Parent directory (←)")
            .clicked()
        {
            action = Some(Action::Parent);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("✕")
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
    ui.label(
        RichText::new(browser::compact(browser.dir()))
            .color(style::DIM)
            .size(11.0),
    );
    ui.separator();

    if browser.has_failed() {
        ui.label(
            RichText::new("cannot list this directory")
                .color(ui.visuals().error_fg_color)
                .size(12.0),
        );
    } else if browser.is_loading() && browser.entries().is_empty() {
        ui.label(RichText::new("listing…").color(style::DIM).size(12.0));
    } else if browser.entries().is_empty() {
        ui.label(RichText::new("empty").color(style::DIM).size(12.0));
    }

    let dir_color = ui.visuals().hyperlink_color;
    let cursor = browser.cursor();
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(style::NO_WRAP);
            for (i, entry) in browser.entries().iter().enumerate() {
                let text = if entry.is_dir {
                    RichText::new(format!("{}/", entry.name)).color(dir_color)
                } else {
                    RichText::new(entry.name.clone())
                };
                if ui.selectable_label(i == cursor, text.size(12.0)).clicked() {
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

fn dim_label(ui: &mut egui::Ui, text: String) {
    ui.label(RichText::new(text).color(style::DIM).size(11.0));
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

fn paint_content(ui: &mut egui::Ui, shown: &mut Shown, zoom: f32) {
    match &shown.loaded.preview.content {
        PreviewContent::Table { .. } => {
            let Some(view) = table_view(&shown.loaded.preview.content) else {
                return;
            };
            // Measured on the preview's first frame and kept: sizing a column
            // lays real text out, which is not a per-frame cost worth paying.
            let grid = shown
                .grid
                .get_or_insert_with(|| table::Grid::measure(ui.ctx(), &view));
            table::paint(ui, &view, grid);
        }
        PreviewContent::Text { lines, .. } => {
            let job = shown.text_job.get_or_insert_with(|| {
                style::text_job(lines, ui.visuals().text_color(), MONO_SIZE)
            });
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
        PreviewContent::Listing { entries } => paint_listing(ui, entries),
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
                    paint_fields(ui, fields);
                });
        }
        PreviewContent::HexDump { data, .. } => paint_hex(ui, data),
    }
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

fn paint_listing(ui: &mut egui::Ui, entries: &[ListEntry]) {
    // `show_rows` assumes every row is exactly this tall, so measure the font
    // we actually paint with rather than the theme's monospace text style.
    let row_height = mono_row_height(ui);
    let dir_color = ui.visuals().hyperlink_color;
    let text_color = ui.visuals().text_color();
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
                        (&format!("{size:>10}  "), style::DIM),
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

fn paint_fields(ui: &mut egui::Ui, fields: &[MetaField]) {
    egui::Grid::new("sekio-fields")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for field in fields {
                ui.label(RichText::new(&field.key).color(style::DIM).monospace());
                ui.label(RichText::new(&field.value).monospace());
                ui.end_row();
            }
        });
}

/// Offset / hex / ASCII columns, mirroring the CLI's hexdump layout.
fn paint_hex(ui: &mut egui::Ui, data: &[u8]) {
    // `show_rows` assumes every row is exactly this tall, so measure the font
    // we actually paint with rather than the theme's monospace text style.
    let row_height = mono_row_height(ui);
    let rows = data.len().div_ceil(16);
    let text_color = ui.visuals().text_color();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            ui.style_mut().wrap_mode = Some(style::NO_WRAP);
            for row in range {
                let start = row * 16;
                let chunk = &data[start..(start + 16).min(data.len())];
                ui.label(style::mono_job(
                    &[
                        (&format!("{start:08x}  "), style::DIM),
                        (&hex_columns(chunk), text_color),
                        (&format!(" |{}|", ascii_columns(chunk)), style::DIM),
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
