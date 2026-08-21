//! sekio-gui — a Quick Look-style popup previewer built on eframe/egui.
//!
//! It paints `sekio_core::PreviewContent` and nothing else: all detection and
//! IO happens in core, on a worker thread (see `worker.rs`). The UI thread only
//! polls for results, so arrowing through a directory of huge files never
//! freezes the window.
//!
//! Entry points, one code path:
//!
//! * `sekio-gui` — open a window on the home screen: the app name, the keys,
//!   the files previewed last time, and an obvious way to open something. This
//!   is what a Start Menu entry, a dock icon or a `.desktop` launcher runs, so
//!   it must never be an error.
//! * `sekio-gui <path>` — open a window on that path.
//! * `sekio-gui --daemon` — stay resident with the window hidden, waiting for
//!   paths on a Unix socket (see `daemon.rs`). Linux/Unix only.
//! * `sekio-gui --daemon --hotkey <SPEC>` — the same, plus a global hotkey that
//!   previews whatever is selected right now (see `hotkey.rs`). A hotkey that
//!   cannot be grabbed is a warning, never a reason not to start.
//! * `sekio-gui --doctor` — print what sekio can see (selection strategy,
//!   hotkey, socket) and exit. The first thing to run when the hotkey seems
//!   dead.
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
mod browser;
#[cfg(unix)]
mod daemon;
mod dialog;
mod hotkey;
mod recent;
mod selection;
mod state;
mod style;
mod timing;
mod worker;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use clap::Parser;
use sekio_core::PreviewOptions;

use crate::app::{SekioApp, Startup};
use crate::state::{Mode, RequestTracker};
use crate::timing::Timing;
use crate::worker::Worker;

/// sekio — instant preview popup for any file.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// File or directory to preview [default: open the home screen]
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

    /// Global hotkey the daemon answers to, e.g. "Ctrl+Shift+Space",
    /// "Super+P", "Alt+F1" [default: Ctrl+Shift+Space]
    #[arg(long, value_name = "SPEC", conflicts_with = "no_hotkey")]
    hotkey: Option<String>,

    /// Run the daemon without grabbing any global hotkey
    #[arg(long)]
    no_hotkey: bool,

    /// Print what sekio can see — selection strategy, hotkey, socket — and
    /// exit. Run this first when the hotkey does nothing.
    #[arg(long)]
    doctor: bool,
}

/// The hotkey to grab, with the spec it was written as, or `None` for
/// `--no-hotkey`.
type Binding = Option<(String, hotkey::HotKey)>;

fn main() -> Result<()> {
    let args = Args::parse();
    let timing = Timing::start(args.timing);
    // A `--hotkey` that cannot be parsed is a startup error in every mode,
    // reported before anything is bound, spawned or drawn — and never a panic.
    let binding = binding(&args)?;

    if args.doctor {
        doctor(&args, binding.as_ref())
    } else if args.daemon {
        run_daemon(args, binding, timing)
    } else {
        run_once(args, timing)
    }
}

/// Resolve `--hotkey` / `--no-hotkey` into the combination to grab.
fn binding(args: &Args) -> Result<Binding> {
    if args.no_hotkey {
        return Ok(None);
    }
    let spec = args.hotkey.as_deref().unwrap_or(hotkey::DEFAULT_SPEC);
    let key = hotkey::parse(spec).map_err(|err| anyhow!("invalid --hotkey {spec:?}: {err}"))?;
    Ok(Some((spec.to_string(), key)))
}

/// One window, with or without a path.
///
/// With a path this is the Quick Look popup it has always been (after one
/// attempt to let a warm daemon do it instead). Without one — a launcher, a
/// dock icon, a Start Menu entry — it opens on the home screen, which is the
/// difference between an application and a command that fails when you click
/// its icon.
fn run_once(args: Args, timing: Timing) -> Result<()> {
    // Canonicalized *here*, in the client: the daemon's cwd is not ours, so a
    // relative path would mean something different on the other side of the
    // socket. It also fails early, with this process's error message, if the
    // path does not exist.
    let path = match args.path.clone() {
        Some(requested) => Some(
            requested
                .canonicalize()
                .with_context(|| format!("cannot open {}", requested.display()))?,
        ),
        None => None,
    };

    // `--probe` measures *this* process, so it never hands off. Neither does a
    // launch with no path: there is nothing to hand over, and the user asked
    // for a window here.
    if let Some(path) = &path {
        if !args.no_daemon && !args.probe && hand_off(path, timing) {
            return Ok(());
        }
    }

    let ctx = egui::Context::default();
    let (worker, tracker) = start_preview(&ctx, &args, timing, path.as_deref());

    if args.probe {
        probe(&worker, tracker, timing, path.is_some());
        return Ok(());
    }

    let mode = if path.is_some() {
        Mode::Popup
    } else {
        Mode::App
    };
    open_window(
        ctx,
        Startup {
            worker,
            tracker,
            path,
            mode,
            wrap: args.wrap,
            borderless: args.borderless,
            timing,
            incoming: None,
            // A one-shot window is gone in a moment; grabbing a system-wide
            // key for its lifetime would be rude and pointless.
            presses: None,
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
            kind: worker::Kind::Preview,
        });
        timing.log("first request queued");
    }
    (worker, tracker)
}

/// Report the startup path without ever opening a window.
///
/// With a path that means waiting for the first preview. Without one there is
/// nothing to wait for — which is the point: the home screen has to be on
/// screen before any of the optional extras (the recent list, the dialog
/// probe) have even been looked at, so this reports the time and then the
/// extras, in that order.
fn probe(worker: &Worker, mut tracker: RequestTracker, timing: Timing, has_path: bool) {
    if has_path {
        let outcome = match worker.wait() {
            Some(response) if tracker.accept(response.id) => outcome_label(response.outcome),
            _ => "worker stopped".to_string(),
        };
        println!(
            "first preview available after {:.1} ms ({outcome})",
            timing.elapsed_ms()
        );
    } else {
        println!(
            "home screen ready after {:.1} ms (no path given)",
            timing.elapsed_ms()
        );
    }
    println!("open dialog: {}", dialog::describe(dialog::availability()));
    println!("recent files: {}", recent_label());
}

/// What the home screen would list, and where it comes from.
fn recent_label() -> String {
    let Some(file) = recent::state_file() else {
        return "not stored (no state directory in this environment)".to_string();
    };
    let recent = recent::Recent::load(&file);
    format!(
        "{} remembered, {} still on disk ({})",
        recent.paths().len(),
        recent.existing().len(),
        file.display()
    )
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
    // un-hides itself when the first handoff arrives. Every other window
    // appears immediately, home screen or not.
    let visible = startup.path.is_some() || startup.mode != Mode::Daemon;
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
fn run_daemon(args: Args, binding: Binding, timing: Timing) -> Result<()> {
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

    // Deliberately after the socket is bound and before anything can fail:
    // whatever the hotkey does, this daemon is already serving. A refused
    // grab prints one line and changes nothing else.
    let presses = start_hotkey(binding, &ctx, timing);

    if args.probe {
        // Nothing drains the channel headlessly; dropping it lets the hotkey
        // thread retire on the first press it can never deliver.
        drop(presses);
        println!("open dialog: {}", dialog::describe(dialog::availability()));
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
            mode: Mode::Daemon,
            wrap: args.wrap,
            borderless: args.borderless,
            timing,
            incoming: Some(rx),
            presses,
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
                    kind: worker::Kind::Preview,
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

/// Grab the global hotkey for this daemon.
///
/// Returns the channel of resolved paths, or `None` when there is no hotkey —
/// because it was declined with `--no-hotkey`, or because the platform refused
/// the grab. **A refusal is a printed warning and nothing more**: the daemon
/// has already bound its socket by the time this runs, and it goes on serving
/// it whatever happens here. A headless box, a Wayland-only session and a
/// combination another application already owns all land in that branch.
#[cfg(unix)]
fn start_hotkey(
    binding: Binding,
    ctx: &egui::Context,
    timing: Timing,
) -> Option<std::sync::mpsc::Receiver<PathBuf>> {
    let (spec, key) = binding?;
    if let Some(warning) = hotkey::risky(&key) {
        eprintln!("sekio-gui: {warning}");
    }
    // The wake is what makes a *hidden* window run its logic again and notice
    // the press, exactly as the socket thread does.
    let wake = ctx.clone();
    let hotkeys = hotkey::listen(key, &spec, selection::for_this_platform(), move || {
        wake.request_repaint()
    });
    match hotkeys.status.warning() {
        Some(warning) => {
            eprintln!("{warning}");
            timing.log("hotkey unavailable");
            None
        }
        None => {
            timing.log("hotkey registered");
            Some(hotkeys.presses)
        }
    }
}

// ---------------------------------------------------------------------------
// --doctor
// ---------------------------------------------------------------------------

/// Width of the label column, and the indent its hints line up under.
const PAD: &str = "              ";

fn row(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<12}{value}");
}

/// "→ do this next", under the row it belongs to.
fn hint(lines: &[&str]) {
    for (i, line) in lines.iter().enumerate() {
        let marker = if i == 0 { "→" } else { " " };
        println!("{PAD}{marker} {line}");
    }
}

/// `--doctor`: everything that decides whether a hotkey press shows a file.
///
/// Exits 0 whatever it finds — this is a report, not a test — and every row
/// that says "no" is followed by the next thing to try.
fn doctor(args: &Args, binding: Option<&(String, hotkey::HotKey)>) -> Result<()> {
    println!("sekio-gui {} — doctor", env!("CARGO_PKG_VERSION"));
    println!();
    doctor_selection();
    println!();
    // Asked first: a daemon that is running is the most likely owner of the
    // hotkey, and that changes what a failed grab below means.
    let running = daemon_running();
    doctor_hotkey(args, binding, running);
    println!();
    doctor_dialog();
    println!();
    doctor_daemon(running);
    Ok(())
}

/// Whether "Open file…" will show a native dialog here, and what it looked at.
///
/// A "no" is not a failure: the built-in browser needs nothing external and is
/// always there. This row exists so the fallback is never a surprise.
fn doctor_dialog() {
    println!("open dialog");
    let availability = dialog::availability();
    row("native", dialog::describe(availability));
    for (label, value) in dialog::evidence() {
        row(label, value);
    }
    if matches!(availability, dialog::Availability::Unavailable(_)) {
        hint(&[
            "Ctrl+O still works: it opens the built-in browser pane instead,",
            "which needs no portal, no GTK and no external process. Install a",
            "desktop portal (xdg-desktop-portal-gtk/-kde) for the native one.",
        ]);
    }
    row("recent", recent_label());
}

/// Which strategy is in use, and what it can see right now.
fn doctor_selection() {
    println!("selection");
    let source = selection::for_this_platform();
    row("strategy", source.describe());

    let started = std::time::Instant::now();
    let current = source.current();
    let took = started.elapsed().as_secs_f64() * 1000.0;
    match current {
        Some(found) => {
            let origin = match found.origin {
                selection::Origin::FileManager => "from the file manager",
                selection::Origin::Clipboard => "from the clipboard",
            };
            row(
                "selected",
                format!("{} ({origin}, {took:.0} ms)", found.path.display()),
            );
            if !selection::usable(&found.path) {
                row("usable", "no — not an existing absolute path");
                hint(&[
                    "a press would find nothing to preview: sekio only opens",
                    "paths that exist. Select the file in a file manager rather",
                    "than copying its name.",
                ]);
            }
        }
        None => {
            row("selected", format!("nothing ({took:.0} ms)"));
            hint(&[
                "select exactly one file in your file manager and run this",
                "again; an absolute path in the clipboard also works.",
                "A press that resolves nothing does nothing at all: no window,",
                "no error — which is what \"the hotkey did nothing\" looks like.",
            ]);
        }
    }
}

/// Whether the spec parsed, and whether this session will hand over the key.
fn doctor_hotkey(args: &Args, binding: Option<&(String, hotkey::HotKey)>, running: Option<bool>) {
    println!("hotkey");
    let Some((spec, key)) = binding else {
        row("hotkey", "none (--no-hotkey)");
        hint(&[format!(
            "drop --no-hotkey to have the daemon answer {}",
            hotkey::DEFAULT_SPEC
        )
        .as_str()]);
        return;
    };

    let source = if args.hotkey.is_some() {
        "--hotkey"
    } else {
        "default"
    };
    row("spec", format!("{spec} ({source})"));
    row("parsed", hotkey::describe(key));
    if let Some(warning) = hotkey::risky(key) {
        row("careful", warning);
    }

    let display = hotkey::display_server();
    row("display", display.label());
    // A real grab, released again immediately: it is the only honest answer to
    // "would the daemon get this key?". It cannot disturb a running daemon —
    // X11 and Win32 both refuse the second grab rather than stealing the first.
    match hotkey::probe(key, spec) {
        hotkey::Status::Registered { .. } => {
            row("registered", "yes — this session hands the key over");
        }
        hotkey::Status::Unavailable { reason, .. } => {
            row("registered", format!("no — {reason}"));
            // Order matters: a missing display explains the failure on its
            // own, and blaming a running daemon for it would send the user
            // hunting the wrong thing.
            if matches!(display, hotkey::DisplayServer::Missing(_)) {
                hint(&[
                    "global hotkeys are grabbed through X11 (XWayland counts).",
                    "On a Wayland-only or headless session, bind a shortcut in",
                    "your desktop settings to `sekio-gui <path>` instead.",
                ]);
            } else if running == Some(true) {
                hint(&[
                    "a daemon is already running and is probably holding this",
                    "key itself, which is exactly what should happen. Press it",
                    "and see; if nothing appears, check the selection above.",
                ]);
            } else {
                hint(&[
                    "another application probably owns this combination; try",
                    "another one, e.g. --hotkey \"Super+P\".",
                ]);
            }
            hint(&[
                "either way the daemon still runs and still serves its socket,",
                "so `sekio-gui <path>` and the file-manager popup keep working.",
            ]);
        }
    }
}

/// Is a daemon answering on this session's socket? `None` where there is no
/// daemon mode at all.
#[cfg(unix)]
fn daemon_running() -> Option<bool> {
    Some(daemon::is_running(&daemon::socket_path()))
}

#[cfg(not(unix))]
fn daemon_running() -> Option<bool> {
    None
}

#[cfg(unix)]
fn doctor_daemon(running: Option<bool>) {
    println!("daemon");
    row("socket", daemon::socket_path().display());
    if running == Some(true) {
        row("running", "yes — it answers on that socket");
    } else {
        row("running", "no");
        hint(&[
            "start one with `sekio-gui --daemon &`. Without it there is",
            "nothing resident for a hotkey to summon, and every popup pays",
            "for a fresh process.",
        ]);
    }
}

/// Windows has no daemon yet; say so rather than printing a socket path that
/// means nothing here.
#[cfg(not(unix))]
fn doctor_daemon(_running: Option<bool>) {
    println!("daemon");
    row("supported", "no — the daemon needs Unix domain sockets");
    hint(&[
        "on this platform every `sekio-gui <path>` opens its own window;",
        "there is nothing resident for a hotkey to summon yet.",
    ]);
}

/// `--daemon` is a Unix-socket feature; there is nothing to run elsewhere.
#[cfg(not(unix))]
fn run_daemon(_args: Args, _binding: Binding, _timing: Timing) -> Result<()> {
    Err(anyhow!(
        "--daemon is not supported on this platform (it needs Unix domain sockets); \
         run `sekio-gui <path>` instead — it opens a window directly"
    ))
}
