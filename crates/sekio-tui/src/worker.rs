//! The preview worker thread.
//!
//! `Previewer` is expensive to construct (syntect syntax + theme sets) and a
//! single preview can take arbitrarily long on a huge file, so exactly one
//! worker thread owns exactly one `Previewer` and the UI thread never touches
//! it. Requests go out over an `mpsc` channel and results come back over
//! another one that the UI polls with `try_recv` — the UI loop never blocks.
//!
//! Staleness is handled twice, on purpose:
//!
//! 1. The UI cancels the previous request's `CancelToken` the instant the user
//!    moves. A request still sitting in the queue is therefore already
//!    cancelled by the time the worker pops it, and gets skipped without doing
//!    any work; a request already running bails out at its next cancel check.
//! 2. Every request carries a monotonically increasing id, and the UI drops any
//!    result that isn't the newest one it issued (see [`crate::app::RequestTracker`]).
//!    This covers the race where a preview finishes just as the user moves on.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use sekio_core::{CancelToken, Preview, PreviewError, PreviewOptions, Previewer};

/// Which pane a request feeds. The left pane's directory listing and the right
/// pane's preview are tracked independently so a slow preview can never hold up
/// navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The current directory's `Listing`, for the left pane.
    Listing,
    /// The preview of the entry under the cursor, for the right pane.
    Preview,
}

#[derive(Debug)]
pub struct Request {
    pub id: u64,
    pub kind: Kind,
    pub path: PathBuf,
    pub cancel: CancelToken,
    /// Characters the preview pane can paint, as of the moment this request
    /// was issued. Per-request rather than per-worker because it changes every
    /// time the terminal is resized; `None` for a listing, which has no
    /// columns to lay out.
    pub text_width: Option<usize>,
}

#[derive(Debug)]
pub struct Response {
    pub id: u64,
    pub kind: Kind,
    /// `Err` carries an already-formatted message; `PreviewError::Cancelled` is
    /// normal control flow and never reaches the UI at all.
    pub result: Result<Preview, String>,
}

/// Handle to the worker thread. Dropping it closes the request channel, which
/// makes the worker exit once it finishes whatever it is doing.
pub struct Worker {
    tx: Sender<Request>,
    rx: Receiver<Response>,
}

impl Worker {
    /// `syntax_theme` names the syntect theme core highlights text with. It is
    /// validated before we get here; an unknown name still degrades to core's
    /// default rather than failing to start.
    pub fn spawn(opts: PreviewOptions, syntax_theme: String) -> std::io::Result<Self> {
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, res_rx) = mpsc::channel::<Response>();

        thread::Builder::new()
            .name("sekio-preview".to_owned())
            .spawn(move || run(&req_rx, &res_tx, &opts, &syntax_theme))?;

        Ok(Self {
            tx: req_tx,
            rx: res_rx,
        })
    }

    /// Non-blocking. A closed channel means the worker died; the UI degrades to
    /// showing whatever it already has rather than crashing.
    pub fn send(&self, request: Request) {
        let _ = self.tx.send(request);
    }

    /// Non-blocking poll for one finished preview. "Nothing ready" and "worker
    /// gone" look the same to the UI: keep drawing what we already have.
    pub fn try_recv(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }
}

fn run(
    requests: &Receiver<Request>,
    results: &Sender<Response>,
    opts: &PreviewOptions,
    syntax_theme: &str,
) {
    // Building the previewer is the expensive part (syntax + theme sets), which
    // is exactly why it happens once, here, off the UI thread.
    let previewer = Previewer::with_theme(syntax_theme).unwrap_or_default();

    while let Ok(req) = requests.recv() {
        // Already superseded before we even started: the UI cancelled it when
        // the user moved on. Spend nothing on it.
        if req.cancel.is_cancelled() {
            continue;
        }

        // Everything but the width is fixed for the life of the process; the
        // width is whatever the pane was when the request went out.
        let opts = PreviewOptions {
            text_width: req.text_width,
            ..opts.clone()
        };
        let result = match previewer.preview(&req.path, &opts, &req.cancel) {
            Ok(preview) => Ok(preview),
            // Cancellation is how the design is supposed to work, not a
            // failure. Swallow it: the UI is already waiting on a newer id.
            Err(PreviewError::Cancelled) => continue,
            Err(err) => Err(err.to_string()),
        };

        if results
            .send(Response {
                id: req.id,
                kind: req.kind,
                result,
            })
            .is_err()
        {
            // UI is gone.
            break;
        }
    }
}
