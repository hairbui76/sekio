//! The preview worker thread.
//!
//! The UI thread must never touch `Previewer`: constructing it loads syntect's
//! syntax and theme sets (tens of milliseconds) and a single preview can take
//! arbitrarily long on a huge file. So one worker thread owns the single
//! `Previewer`, takes requests over an `mpsc` channel, and sends results back
//! over another. The UI polls with `try_recv` and is woken by
//! `Context::request_repaint`.
//!
//! Cancellation: every request carries its own `CancelToken`. The UI cancels
//! the in-flight token the instant the user moves to another file, so the
//! renderer bails out at its next work boundary and the queue drains fast.
//!
//! Theme: syntax colours are 24-bit RGB by the time they reach the IR, so
//! "highlight with a light theme" is a decision only this thread can act on —
//! and acting on it means building a *new* `Previewer`, which is the expensive
//! thing this module exists to do exactly once. So a change arrives on its own
//! channel, is applied here rather than on the UI thread, and the UI re-asks
//! for whatever it is showing (see `app::SekioApp::poll_theme`), which is what
//! makes the file on screen actually change colour.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use sekio_core::{CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, Previewer};

use crate::timing::Timing;

/// What a request is *for*. The worker treats both identically — a preview is
/// a preview — but the UI keeps a separate generation counter per kind, so
/// listing a directory for the browser pane does not cancel the preview the
/// user is reading, and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The main pane: the file the user is looking at.
    Preview,
    /// The browser pane's directory listing.
    Browse,
}

pub struct Request {
    pub id: u64,
    pub path: PathBuf,
    pub cancel: CancelToken,
    pub kind: Kind,
    /// Characters the text area could paint when this request was issued.
    /// Per-request rather than per-worker because the window can be resized at
    /// any moment; `None` means "no hint", and core falls back to its default.
    pub text_width: Option<usize>,
}

/// A finished preview plus its image already converted to egui's pixel layout.
/// The conversion is a full copy of the bitmap, so it happens here rather than
/// on the UI thread; uploading it to the GPU still has to happen UI-side.
pub struct Loaded {
    pub preview: Preview,
    pub image: Option<egui::ColorImage>,
}

pub enum Outcome {
    Ready(Box<Loaded>),
    /// Rendering failed — shown in-window, never a crash.
    Failed(String),
    /// Normal control flow (the user moved on); the UI ignores these.
    Cancelled,
}

pub struct Response {
    pub id: u64,
    pub path: PathBuf,
    pub outcome: Outcome,
    pub elapsed: Duration,
    pub kind: Kind,
}

/// UI-side handle: send requests, poll responses.
pub struct Worker {
    tx: Sender<Request>,
    rx: Receiver<Response>,
    /// Syntax-theme changes. A separate channel rather than a field on
    /// `Request` because `Request` is built by callers this module does not
    /// own, and because a theme change is not a render: it must be picked up
    /// even when it arrives between two of them.
    themes: Sender<Option<String>>,
}

impl Worker {
    /// Spawn the worker. `ctx` is used only to wake the UI when a result is
    /// ready — no busy-polling loop on the UI side.
    pub fn spawn(ctx: egui::Context, opts: PreviewOptions, timing: Timing) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, res_rx) = mpsc::channel::<Response>();
        let (theme_tx, theme_rx) = mpsc::channel::<Option<String>>();

        // `Previewer::new()` runs *here*, not before the window opens, so the
        // first frame can be painted while the syntax sets are still loading.
        std::thread::spawn(move || run(req_rx, res_tx, theme_rx, ctx, opts, timing));

        Self {
            tx: req_tx,
            rx: res_rx,
            themes: theme_tx,
        }
    }

    /// Queue a preview. Fails silently if the worker died — the UI keeps its
    /// "loading…" state rather than panicking.
    pub fn request(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    /// Ask for the named syntax theme from now on; `None` means core's own
    /// default. Applied on the worker thread before the next render, so the UI
    /// never waits for a `Previewer` to be rebuilt — and silently ignored if
    /// the worker is gone, exactly like [`Worker::request`].
    pub fn set_theme(&self, name: Option<&str>) {
        let _ = self.themes.send(name.map(str::to_owned));
    }

    /// Non-blocking poll, called once per frame. A disconnected worker is not
    /// an error here: the UI simply keeps showing whatever it already has.
    pub fn poll(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }

    /// Block until the next result. Only used by `--probe`; the UI thread must
    /// never call this.
    pub fn wait(&self) -> Option<Response> {
        self.rx.recv().ok()
    }

    /// Build a handle over channels the caller owns, instead of over a spawned
    /// thread. The UI cannot tell the difference — it only ever sends
    /// `Request`s and polls `Response`s — which is what lets the headless
    /// rendering tests paint a chosen `PreviewContent` without a `Previewer`,
    /// a file on disk or a timing race.
    pub fn from_channels(tx: Sender<Request>, rx: Receiver<Response>) -> Self {
        // Nothing on the far end of this one: with no `Previewer` behind the
        // handle there is no theme to change, and a send into a dropped
        // receiver is the same silent no-op a dead worker already gives.
        let (themes, _) = mpsc::channel();
        Self { tx, rx, themes }
    }
}

fn run(
    rx: Receiver<Request>,
    tx: Sender<Response>,
    themes: Receiver<Option<String>>,
    ctx: egui::Context,
    opts: PreviewOptions,
    timing: Timing,
) {
    let mut theme: Option<String> = None;
    let mut previewer = Previewer::new();
    timing.log("previewer ready");

    while let Ok(first) = rx.recv() {
        // Coalesce *within a kind*: anything still queued behind a request of
        // the same kind supersedes it, so arrowing fast through a directory
        // does at most one render per settled selection instead of one per
        // keypress. Across kinds nothing is dropped — a browser listing must
        // not cancel the preview queued next to it.
        let mut queue = vec![first];
        while let Ok(newer) = rx.try_recv() {
            match queue.iter_mut().find(|req| req.kind == newer.kind) {
                Some(slot) => {
                    let superseded = std::mem::replace(slot, newer);
                    superseded.cancel.cancel();
                }
                None => queue.push(newer),
            }
        }
        // Listings are cheap and are what the user is steering right now, so
        // they go ahead of a preview that may take a while. Stable, so the
        // relative order within a kind is untouched.
        queue.sort_by_key(|req| match req.kind {
            Kind::Browse => 0,
            Kind::Preview => 1,
        });

        // Before anything is rendered, not after: the colours are baked into
        // the IR, so a request served with the old theme would have to be
        // thrown away and re-run. The UI sends the theme before the request it
        // wants painted in it, so by the time a request arrives the change is
        // already sitting in this channel.
        if let Some(wanted) = latest(&themes) {
            if wanted != theme {
                match rebuild(wanted.as_deref()) {
                    Some(rebuilt) => {
                        previewer = rebuilt;
                        theme = wanted;
                        timing.log("previewer rebuilt for the new theme");
                    }
                    // An unknown theme name is not a reason to lose the
                    // previewer we have; keep highlighting with it and say so.
                    None => timing.log("unknown syntax theme; keeping the current one"),
                }
            }
        }

        for req in queue {
            if !serve(&previewer, &opts, &tx, &ctx, timing, req) {
                return; // UI is gone.
            }
        }
    }
}

/// The newest theme in the channel, or `None` when nobody asked for one. Only
/// the last matters: rebuilding for a theme that has already been superseded
/// would cost a syntax-set load for a palette nothing is painted in.
fn latest(themes: &Receiver<Option<String>>) -> Option<Option<String>> {
    let mut newest = None;
    while let Ok(name) = themes.try_recv() {
        newest = Some(name);
    }
    newest
}

/// A previewer for the named theme, or `None` if syntect has no such theme.
/// Never panics on a name that came out of a config file.
fn rebuild(name: Option<&str>) -> Option<Previewer> {
    match name {
        None => Some(Previewer::new()),
        Some(name) => Previewer::with_theme(name),
    }
}

/// Render one request and send the result. `false` means the UI has hung up.
fn serve(
    previewer: &Previewer,
    opts: &PreviewOptions,
    tx: &Sender<Response>,
    ctx: &egui::Context,
    timing: Timing,
    req: Request,
) -> bool {
    if req.cancel.is_cancelled() {
        return true;
    }

    let started = std::time::Instant::now();
    // Everything but the width is fixed for the life of the process; the width
    // is whatever the text area was when the request went out.
    let opts = PreviewOptions {
        text_width: req.text_width,
        ..opts.clone()
    };
    let outcome = match previewer.preview(&req.path, &opts, &req.cancel) {
        Ok(preview) => {
            let image = egui_image(&preview.content);
            Outcome::Ready(Box::new(Loaded { preview, image }))
        }
        // Cancellation is expected control flow, never an error banner.
        Err(PreviewError::Cancelled) => Outcome::Cancelled,
        Err(err) => Outcome::Failed(err.to_string()),
    };
    let elapsed = started.elapsed();
    timing.log(&format!(
        "preview ready ({}, {:.1} ms of work)",
        req.path.display(),
        elapsed.as_secs_f64() * 1000.0
    ));

    if tx
        .send(Response {
            id: req.id,
            path: req.path,
            outcome,
            elapsed,
            kind: req.kind,
        })
        .is_err()
    {
        return false;
    }
    ctx.request_repaint();
    true
}

/// Pull the bitmap out of a preview (image body or metadata thumbnail) and
/// convert it to `egui::ColorImage` once, here on the worker thread.
pub fn egui_image(content: &PreviewContent) -> Option<egui::ColorImage> {
    let rgba = match content {
        PreviewContent::Image { image, .. } => Some(image),
        PreviewContent::Metadata { thumbnail, .. } => thumbnail.as_ref(),
        _ => None,
    }?;
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole light mode rests on this name existing in syntect's bundled
    /// set. If it ever stops doing so, `rebuild` degrades to keeping the dark
    /// previewer — which is the failure this feature was built to avoid, so it
    /// is worth one test rather than a silent fallback nobody notices.
    #[test]
    fn the_light_syntax_theme_this_app_asks_for_exists() {
        let name = crate::style::LIGHT_SYNTECT_THEME;
        assert!(
            Previewer::theme_names().iter().any(|known| known == name),
            "syntect no longer ships {name:?}"
        );
        assert!(rebuild(Some(name)).is_some());
    }

    #[test]
    fn an_unknown_theme_name_degrades_instead_of_panicking() {
        assert!(rebuild(Some("no-such-theme-anywhere")).is_none());
        // …and "no theme" is core's own default, which always builds.
        assert!(rebuild(None).is_some());
    }

    #[test]
    fn only_the_newest_theme_is_acted_on() {
        let (tx, rx) = mpsc::channel();
        assert_eq!(latest(&rx), None, "an empty channel asks for no rebuild");
        tx.send(Some("a".to_owned())).expect("send");
        tx.send(None).expect("send");
        tx.send(Some("b".to_owned())).expect("send");
        assert_eq!(latest(&rx), Some(Some("b".to_owned())));
        assert_eq!(latest(&rx), None);
    }

    /// A handle with no worker behind it must swallow a theme change the same
    /// way it swallows a request, or the headless tests would panic on one.
    #[test]
    fn setting_a_theme_on_a_detached_handle_is_a_no_op() {
        let (tx, _requests) = mpsc::channel();
        let (_responses, rx) = mpsc::channel();
        let worker = Worker::from_channels(tx, rx);
        worker.set_theme(Some("base16-ocean.light"));
        worker.set_theme(None);
    }
}
