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
}

impl Worker {
    /// Spawn the worker. `ctx` is used only to wake the UI when a result is
    /// ready — no busy-polling loop on the UI side.
    pub fn spawn(ctx: egui::Context, opts: PreviewOptions, timing: Timing) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, res_rx) = mpsc::channel::<Response>();

        // `Previewer::new()` runs *here*, not before the window opens, so the
        // first frame can be painted while the syntax sets are still loading.
        std::thread::spawn(move || run(req_rx, res_tx, ctx, opts, timing));

        Self {
            tx: req_tx,
            rx: res_rx,
        }
    }

    /// Queue a preview. Fails silently if the worker died — the UI keeps its
    /// "loading…" state rather than panicking.
    pub fn request(&self, req: Request) {
        let _ = self.tx.send(req);
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
        Self { tx, rx }
    }
}

fn run(
    rx: Receiver<Request>,
    tx: Sender<Response>,
    ctx: egui::Context,
    opts: PreviewOptions,
    timing: Timing,
) {
    let previewer = Previewer::new();
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

        for req in queue {
            if !serve(&previewer, &opts, &tx, &ctx, timing, req) {
                return; // UI is gone.
            }
        }
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
    let outcome = match previewer.preview(&req.path, opts, &req.cancel) {
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
