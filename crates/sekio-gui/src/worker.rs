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

pub struct Request {
    pub id: u64,
    pub path: PathBuf,
    pub cancel: CancelToken,
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

    while let Ok(mut req) = rx.recv() {
        // Coalesce: anything still queued behind this request supersedes it.
        // Arrowing fast through a directory therefore does at most one render
        // per settled selection instead of one per keypress.
        while let Ok(newer) = rx.try_recv() {
            req.cancel.cancel();
            req = newer;
        }
        if req.cancel.is_cancelled() {
            continue;
        }

        let started = std::time::Instant::now();
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
            })
            .is_err()
        {
            break; // UI is gone.
        }
        ctx.request_repaint();
    }
}

/// Pull the bitmap out of a preview (image body or metadata thumbnail) and
/// convert it to `egui::ColorImage` once, here on the worker thread.
fn egui_image(content: &PreviewContent) -> Option<egui::ColorImage> {
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
