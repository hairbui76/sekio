//! sekio-gui — a Quick Look-style popup previewer built on eframe/egui.
//!
//! It paints `sekio_core::PreviewContent` and nothing else: all detection and
//! IO happens in core, on a worker thread (see `worker.rs`). The UI thread only
//! polls for results, so arrowing through a directory of huge files never
//! freezes the window.
//!
//! Three entry points, one code path:
//!
//! * `sekio-gui <path>` — open a window on that path.
//! * `sekio-gui --daemon` — stay resident with the window hidden, waiting for
//!   paths on a Unix socket (see `daemon.rs`). Linux/Unix only.
//! * `sekio-gui <path>` *with a daemon running* — hand the path over the socket
//!   and exit immediately; the daemon raises its window on that path. If the
//!   handoff fails for any reason at all, this falls back to opening its own
//!   window, so the daemon is only ever an optimisation.
//!
//! Platform integration hook (ROADMAP Phase 3): the Nautilus / Explorer
//! "spacebar" flows call `sekio-gui <path>`, which is the fast path above on
//! Linux and a plain process spawn on Windows, where a shell hook resolves the
//! selection instead.

mod app;
#[cfg(unix)]
mod daemon;
mod state;
mod style;
mod timing;
mod worker;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use clap::Parser;
use sekio_core::PreviewOptions;

use crate::app::{SekioApp, Startup};
use crate::state::RequestTracker;
use crate::timing::Timing;
use crate::worker::Worker;

/// sekio — instant preview popup for any file.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// File or directory to preview
    #[arg(required_unless_present = "daemon")]
    path: Option<PathBuf>,

    /// Max lines of text to render
    #[arg(long, default_value_t = 500)]
    lines: usize,

    /// Wrap around at the ends when arrowing through the directory
    #[arg(long)]
    wrap: bool,

    /// Borderless popup window (drag the header to move it)
    #[arg(long)]
    borderless: bool,

    /// Print cold-start milestones to stderr
    #[arg(long)]
    timing: bool,

    /// Run the startup path and the first preview without opening a window,
    /// then exit. Lets the cold-start budget be measured on headless machines
    /// and in CI (see ROADMAP "benchmarks"), with no GUI-only code skipped.
    /// With `--daemon`, serves the socket headlessly instead, printing each
    /// path it is handed.
    #[arg(long)]
    probe: bool,

    /// Stay resident as the single instance for this session: listen on a Unix
    /// socket and keep the window hidden until a path arrives. Linux/Unix only.
    #[arg(long)]
    daemon: bool,

    /// Never hand off to a running daemon; open a window in this process
    #[arg(long, conflicts_with = "daemon")]
    no_daemon: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let timing = Timing::start(args.timing);

    if args.daemon {
        run_daemon(args, timing)
    } else {
        run_once(args, timing)
    }
}

/// One path, one window (after one attempt to let a warm daemon do it).
fn run_once(args: Args, timing: Timing) -> Result<()> {
    let requested = args
        .path
        .clone()
        .ok_or_else(|| anyhow!("a path is required"))?;
    // Canonicalized *here*, in the client: the daemon's cwd is not ours, so a
    // relative path would mean something different on the other side of the
    // socket. It also fails early, with this process's error message, if the
    // path does not exist.
    let path = requested
        .canonicalize()
        .with_context(|| format!("cannot open {}", requested.display()))?;

    // `--probe` measures *this* process, so it never hands off.
    if !args.no_daemon && !args.probe && hand_off(&path, timing) {
        return Ok(());
    }

    let ctx = egui::Context::default();
    let (worker, tracker) = start_preview(&ctx, &args, timing, Some(&path));

    if args.probe {
        probe(&worker, tracker, timing);
        return Ok(());
    }

    open_window(
        ctx,
        Startup {
            worker,
            tracker,
            path: Some(path),
            wrap: args.wrap,
            borderless: args.borderless,
            timing,
            incoming: None,
        },
    )
}

/// The context is created before eframe starts so the worker can be spawned
/// (and `Previewer::new()` begun) while the window is still being created, and
/// the first preview is fired immediately so it overlaps GL context creation.
/// Nothing expensive happens on this thread before the first frame.
fn start_preview(
    ctx: &egui::Context,
    args: &Args,
    timing: Timing,
    path: Option<&Path>,
) -> (Worker, RequestTracker) {
    let opts = PreviewOptions {
        max_lines: args.lines,
        ..Default::default()
    };
    let worker = Worker::spawn(ctx.clone(), opts, timing);
    let mut tracker = RequestTracker::new();
    if let Some(path) = path {
        let (id, cancel) = tracker.begin();
        worker.request(worker::Request {
            id,
            path: path.to_path_buf(),
            cancel,
        });
        timing.log("first request queued");
    }
    (worker, tracker)
}

/// Wait for the first preview and report it, without ever opening a window.
fn probe(worker: &Worker, mut tracker: RequestTracker, timing: Timing) {
    let outcome = match worker.wait() {
        Some(response) if tracker.accept(response.id) => outcome_label(response.outcome),
        _ => "worker stopped".to_string(),
    };
    println!(
        "first preview available after {:.1} ms ({outcome})",
        timing.elapsed_ms()
    );
}

fn outcome_label(outcome: worker::Outcome) -> String {
    match outcome {
        worker::Outcome::Ready(_) => "ok".to_string(),
        worker::Outcome::Failed(message) => format!("error: {message}"),
        worker::Outcome::Cancelled => "cancelled".to_string(),
    }
}

fn open_window(ctx: egui::Context, startup: Startup) -> Result<()> {
    let title = match &startup.path {
        Some(path) => format!("sekio — {}", path.display()),
        None => "sekio".to_string(),
    };
    // A daemon with no path yet must not flash an empty window on screen; it
    // un-hides itself when the first handoff arrives.
    let visible = startup.path.is_some();
    let borderless = startup.borderless;
    let timing = startup.timing;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            // Wayland needs an app id to match the window to its .desktop file.
            .with_app_id("sekio")
            .with_inner_size([880.0, 640.0])
            .with_min_inner_size([320.0, 200.0])
            .with_decorations(!borderless)
            .with_visible(visible)
            .with_active(true),
        ..Default::default()
    };

    eframe::run_native_ext(
        "sekio",
        native_options,
        Some(ctx),
        Box::new(move |cc| {
            timing.log("window created");
            Ok(Box::new(SekioApp::new(&cc.egui_ctx, startup)))
        }),
    )
    .map_err(|e| anyhow!("failed to open window: {e}"))
}

/// Try to let a resident daemon show `path`. `true` means it did, and this
/// process is done.
#[cfg(unix)]
fn hand_off(path: &Path, timing: Timing) -> bool {
    match daemon::try_handoff(path) {
        daemon::Handoff::Delivered => {
            timing.log("handed off to the daemon");
            true
        }
        daemon::Handoff::Unavailable(reason) => {
            timing.log(&format!("no daemon ({reason}); opening a window here"));
            false
        }
    }
}

/// Non-Unix platforms have no daemon: every invocation opens its own window.
#[cfg(not(unix))]
fn hand_off(_path: &Path, _timing: Timing) -> bool {
    false
}

/// Run as the single instance for this session.
///
/// Binding comes first and decides everything: if another daemon owns the
/// socket we do not fight it, we become a client (handing over `path` if we
/// were given one) and exit.
#[cfg(unix)]
fn run_daemon(args: Args, timing: Timing) -> Result<()> {
    use std::sync::mpsc;

    let socket = daemon::socket_path();
    let bound =
        daemon::bind(&socket).with_context(|| format!("cannot listen on {}", socket.display()))?;
    let (listener, guard) = match bound {
        daemon::Bind::Bound(listener, guard) => (listener, guard),
        daemon::Bind::AlreadyRunning => {
            eprintln!(
                "sekio-gui: a daemon is already listening on {}",
                socket.display()
            );
            if let Some(path) = args.path.as_ref() {
                let path = path
                    .canonicalize()
                    .with_context(|| format!("cannot open {}", path.display()))?;
                if !hand_off(&path, timing) {
                    // The winner of the race stopped answering in the
                    // meantime: show the file ourselves rather than silently
                    // doing nothing.
                    return run_once(args, timing);
                }
            }
            return Ok(());
        }
    };
    // Best-effort: without it a killed daemon leaves a stale socket, which the
    // next client detects and removes anyway.
    if let Err(err) = daemon::install_signal_cleanup(&guard) {
        eprintln!("sekio-gui: no signal cleanup ({err}); a stale socket may be left behind");
    }
    timing.log("daemon listening");

    let path = match args.path.as_ref() {
        Some(path) => Some(
            path.canonicalize()
                .with_context(|| format!("cannot open {}", path.display()))?,
        ),
        None => None,
    };

    let ctx = egui::Context::default();
    let (worker, tracker) = start_preview(&ctx, &args, timing, path.as_deref());

    if args.probe {
        return probe_daemon(&listener, &guard, &worker, tracker, timing);
    }

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let wake = ctx.clone();
    // Accepting happens here, never on the UI thread: a client that connects
    // and stalls must not be able to freeze the window.
    std::thread::Builder::new()
        .name("sekio-socket".to_owned())
        .spawn(move || daemon::serve(listener, tx, move || wake.request_repaint()))
        .context("cannot spawn the socket thread")?;

    let result = open_window(
        ctx,
        Startup {
            worker,
            tracker,
            path,
            wrap: args.wrap,
            borderless: args.borderless,
            timing,
            incoming: Some(rx),
        },
    );
    // Explicit, so the socket is gone before the process is.
    drop(guard);
    result
}

/// Headless daemon: serve the socket and preview what arrives, printing one
/// line per request instead of opening a window. This is how the daemon path
/// is exercised on a machine with no display server. Runs until killed —
/// SIGTERM/SIGINT unlink the socket on the way out.
#[cfg(unix)]
fn probe_daemon(
    listener: &std::os::unix::net::UnixListener,
    guard: &daemon::SocketGuard,
    worker: &Worker,
    mut tracker: RequestTracker,
    timing: Timing,
) -> Result<()> {
    use std::io::Write as _;

    println!("daemon listening on {}", guard.path().display());
    let _ = std::io::stdout().flush();

    loop {
        match daemon::accept_one(listener) {
            Ok(daemon::Accepted::Path(path)) => {
                let started = std::time::Instant::now();
                // Same generation counter as the UI: a handoff cancels the
                // preview in flight and stale results are dropped.
                let (id, cancel) = tracker.begin();
                worker.request(worker::Request {
                    id,
                    path: path.clone(),
                    cancel,
                });
                let outcome = match worker.wait() {
                    Some(response) if tracker.accept(response.id) => {
                        outcome_label(response.outcome)
                    }
                    Some(_) => "stale".to_string(),
                    None => "worker stopped".to_string(),
                };
                println!(
                    "received {} ({outcome}, {:.1} ms)",
                    path.display(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
            Ok(daemon::Accepted::Rejected) => println!("rejected a malformed request"),
            // Another daemon checking whether this socket is alive.
            Ok(daemon::Accepted::Probe) => println!("answered a liveness probe"),
            Err(err) => {
                eprintln!("sekio-gui: accept failed: {err}");
                break;
            }
        }
        let _ = std::io::stdout().flush();
    }
    timing.log("daemon stopping");
    Ok(())
}

/// `--daemon` is a Unix-socket feature; there is nothing to run elsewhere.
#[cfg(not(unix))]
fn run_daemon(_args: Args, _timing: Timing) -> Result<()> {
    Err(anyhow!(
        "--daemon is not supported on this platform (it needs Unix domain sockets); \
         run `sekio-gui <path>` instead — it opens a window directly"
    ))
}
