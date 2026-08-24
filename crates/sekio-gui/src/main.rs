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
//!   paths on a Unix socket (Linux) or a named pipe (Windows) — see `daemon/`.
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
//!
//! Windows note: this is a *windowed* binary, so clicking it opens the app and
//! nothing else — no console window beside it. The console-shaped entry points
//! above still print, because the first thing `main` does is reattach to the
//! terminal that started the process when there is one. `console.rs` explains
//! both halves; the attribute below is the half that removes the window.

// Declared unconditionally, and not as the usual
// `cfg_attr(not(debug_assertions), ...)`: users run release builds, so the
// conditional form would leave the stray console exactly where it is for
// everyone who is not building this themselves. Nothing on Linux reads it.
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use clap::Parser;
use sekio_core::paths;

use sekio_gui::app::{SekioApp, Startup};
use sekio_gui::config::{self, Settings};
use sekio_gui::daemon;
use sekio_gui::state::{Mode, RequestTracker};
use sekio_gui::style::{self, Theme};
use sekio_gui::timing::Timing;
use sekio_gui::worker::Worker;
use sekio_gui::{console, dialog, hotkey, icon, recent, selection, tray, worker};

/// sekio — instant preview popup for any file.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// File or directory to preview [default: open the home screen]
    path: Option<PathBuf>,

    /// Max lines of text to render [default: 500]
    ///
    /// Deliberately without a clap default: with one, "the user typed --lines
    /// 500" and "clap filled in 500" are the same value, and a `lines` in the
    /// config file would be silently clobbered by a default nobody typed. The
    /// three layers meet in `config::resolve` instead.
    #[arg(long)]
    lines: Option<usize>,

    /// Wrap around at the ends when arrowing through the directory
    #[arg(long)]
    wrap: bool,

    /// Colour theme: follow the desktop, or force one [default: system]
    #[arg(long, value_enum)]
    theme: Option<Theme>,

    /// Run without a tray icon
    #[arg(long)]
    no_tray: bool,

    /// Read this config file instead of the default location
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    config: Option<PathBuf>,

    /// Ignore any config file
    #[arg(long)]
    no_config: bool,

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
    /// socket (Linux) or named pipe (Windows) and keep the window hidden until
    /// a path arrives
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
    // Before anything that can print, which on Windows means before
    // `Args::parse()`: `--help`, `--version` and clap's usage errors are all
    // written from inside it, and until this runs a windows-subsystem process
    // has no standard handles to write them to. A no-op on Linux.
    let output = console::attach();
    let args = Args::parse();
    let timing = Timing::start(args.timing);
    // Warnings, never errors: a config file the user cannot currently fix must
    // not stop the program starting (see `config`'s module docs, rule 2).
    let location = config::Location::resolve(
        args.config.clone(),
        args.no_config,
        config::Platform::current(),
        |key| std::env::var_os(key),
    );
    let loaded = config::load(&location);
    for warning in &loaded.warnings {
        eprintln!("sekio-gui: {warning}");
    }
    let settings = config::resolve(&overrides(&args), &loaded.config);
    timing.log("config resolved");
    // A `--hotkey` that cannot be parsed is a startup error in every mode,
    // reported before anything is bound, spawned or drawn — and never a panic.
    // Only the command line can reach here: `validate` already dropped an
    // unusable spec from the file.
    let binding = settings
        .binding()
        .map_err(|err| anyhow!("invalid --hotkey {:?}: {err}", settings.hotkey.as_deref()))?;

    if args.doctor {
        doctor(
            &args,
            &settings,
            &location,
            loaded.config.hotkey.is_some(),
            binding.as_ref(),
            output,
        )
    } else if args.daemon {
        run_daemon(args, settings, location, binding, timing)
    } else {
        run_once(args, settings, timing)
    }
}

/// What the user actually typed, in the shape `config::resolve` merges.
///
/// The two negative flags are asymmetric on purpose: `--no-hotkey` has no
/// positive counterpart (its absence is not a preference), while `--no-tray`
/// does have one in the file, so it is an `Option<bool>` that can say "false"
/// over a `tray = true`.
fn overrides(args: &Args) -> config::Overrides {
    config::Overrides {
        hotkey: args.hotkey.clone(),
        no_hotkey: args.no_hotkey,
        tray: if args.no_tray { Some(false) } else { None },
        lines: args.lines,
        // A flag can only be present or absent; absent must defer to the file
        // rather than override it with `false`.
        wrap: if args.wrap { Some(true) } else { None },
        theme: args.theme,
    }
}

/// One window, with or without a path.
///
/// With a path this is the Quick Look popup it has always been (after one
/// attempt to let a warm daemon do it instead). Without one — a launcher, a
/// dock icon, a Start Menu entry — it opens on the home screen, which is the
/// difference between an application and a command that fails when you click
/// its icon.
fn run_once(args: Args, settings: Settings, timing: Timing) -> Result<()> {
    // Canonicalized *here*, in the client: the daemon's cwd is not ours, so a
    // relative path would mean something different on the other side of the
    // socket. It also fails early, with this process's error message, if the
    // path does not exist.
    let path = match args.path.clone() {
        Some(requested) => Some(
            paths::canonical(&requested)
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
    // Before the worker: the theme decides which syntect theme core highlights
    // with, and the first request is fired inside `start_preview`.
    style::install(&ctx, settings.theme);
    let (worker, tracker) = start_preview(&ctx, &settings, timing, path.as_deref());

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
            wrap: settings.wrap,
            borderless: args.borderless,
            timing,
            incoming: None,
            // A one-shot window is gone in a moment; grabbing a system-wide
            // key for its lifetime would be rude and pointless.
            presses: None,
            // Likewise the tray: an icon that appears for the second a preview
            // is on screen is worse than none. It belongs to the daemon.
            tray: None,
            hotkey_spec: None,
            config_path: None,
            theme: settings.theme,
        },
    )
}

/// The context is created before eframe starts so the worker can be spawned
/// (and `Previewer::new()` begun) while the window is still being created, and
/// the first preview is fired immediately so it overlaps GL context creation.
/// Nothing expensive happens on this thread before the first frame.
fn start_preview(
    ctx: &egui::Context,
    settings: &Settings,
    timing: Timing,
    path: Option<&Path>,
) -> (Worker, RequestTracker) {
    let worker = Worker::spawn(ctx.clone(), settings.preview_options(), timing);
    // Light mode needs core highlighting with a light syntect theme, and the
    // first request goes out below — so the worker is told before, not after.
    worker.set_theme(style::Palette::for_theme(ctx.theme()).syntect_theme());
    let mut tracker = RequestTracker::new();
    if let Some(path) = path {
        let (id, cancel) = tracker.begin();
        worker.request(worker::Request {
            id,
            path: path.to_path_buf(),
            cancel,
            kind: worker::Kind::Preview,
            // Fired before the window exists, so there is no width to measure
            // yet: core lays out for `DEFAULT_TEXT_WIDTH`, and the first frame
            // re-requests if the real text area turns out to differ.
            text_width: None,
            sheet: 0,
            page: 0,
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
///
/// Written with `writeln!` rather than `println!` because this now prints more
/// than one line, and `sekio-gui --probe | head -1` is exactly the sort of
/// thing a benchmark script does: a closed pipe must end the report, never
/// panic the process.
fn probe(worker: &Worker, mut tracker: RequestTracker, timing: Timing, has_path: bool) {
    use std::io::Write as _;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let first = if has_path {
        let outcome = match worker.wait() {
            Some(response) if tracker.accept(response.id) => outcome_label(response.outcome),
            _ => "worker stopped".to_string(),
        };
        format!(
            "first preview available after {:.1} ms ({outcome})",
            timing.elapsed_ms()
        )
    } else {
        format!(
            "home screen ready after {:.1} ms (no path given)",
            timing.elapsed_ms()
        )
    };
    let _ = writeln!(out, "{first}");
    let _ = writeln!(
        out,
        "open dialog: {}",
        dialog::describe(dialog::availability())
    );
    let _ = writeln!(out, "recent files: {}", recent_label());
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

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(title)
        // Wayland needs an app id to match the window to its .desktop file.
        .with_app_id("sekio")
        .with_inner_size([880.0, 640.0])
        .with_min_inner_size([320.0, 200.0])
        .with_decorations(!borderless)
        .with_visible(visible)
        .with_active(true);

    // Title bar, taskbar and Alt-Tab. Left unset, eframe substitutes its own
    // bundled egui logo. A 64x64 PNG compiled into the binary, so this costs a
    // decode and no file IO — `icon.rs` explains the size choice; the timing
    // milestone is here because this runs on the cold-start path, before the
    // window exists. `None` means the decode failed, which is a window without
    // an icon and never a reason to stop: this process may be a daemon.
    match icon::load() {
        Some(icon) => {
            viewport = viewport.with_icon(icon);
            timing.log("window icon decoded");
        }
        None => timing.log("window icon could not be decoded; opening without one"),
    }

    let native_options = eframe::NativeOptions {
        viewport,
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

/// Run as the single instance for this session.
///
/// Binding comes first and decides everything: if another daemon owns the
/// socket we do not fight it, we become a client (handing over `path` if we
/// were given one) and exit.
fn run_daemon(
    args: Args,
    settings: Settings,
    location: config::Location,
    binding: Binding,
    timing: Timing,
) -> Result<()> {
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
                let path = paths::canonical(path)
                    .with_context(|| format!("cannot open {}", path.display()))?;
                if !hand_off(&path, timing) {
                    // The winner of the race stopped answering in the
                    // meantime: show the file ourselves rather than silently
                    // doing nothing.
                    return run_once(args, settings, timing);
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
            paths::canonical(path).with_context(|| format!("cannot open {}", path.display()))?,
        ),
        None => None,
    };

    let ctx = egui::Context::default();
    style::install(&ctx, settings.theme);
    let (worker, tracker) = start_preview(&ctx, &settings, timing, path.as_deref());

    // Deliberately after the socket is bound and before anything can fail:
    // whatever the hotkey does, this daemon is already serving. A refused
    // grab prints one line and changes nothing else.
    // The spec comes back alongside the channel because the tray menu ticks
    // what was *granted*, not what was asked for: a refused grab must not show
    // as the live combination.
    let (presses, hotkey_spec) = start_hotkey(binding, &ctx, timing);

    if args.probe {
        // Nothing drains the channel headlessly; dropping it lets the hotkey
        // thread retire on the first press it can never deliver.
        drop(presses);
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stdout(),
                "open dialog: {}",
                dialog::describe(dialog::availability())
            );
        }
        return probe_daemon(&listener, &guard, &worker, tracker, timing);
    }

    // After the hotkey, so the menu can tick the combination that was actually
    // grabbed rather than the one that was asked for. `None` is ordinary: no
    // tray host in this session, or `tray = false`.
    let tray = start_tray(&settings, hotkey_spec.as_deref(), timing);

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
            wrap: settings.wrap,
            borderless: args.borderless,
            timing,
            incoming: Some(rx),
            presses,
            tray,
            hotkey_spec,
            // Where the tray writes a hotkey chosen from its menu. `None` for
            // `--no-config` or an environment with no config directory, in
            // which case a choice lasts as long as the process.
            config_path: location.write_target().map(Path::to_path_buf),
            theme: settings.theme,
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
fn probe_daemon(
    listener: &daemon::Listener,
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
                    // `--daemon` with no window attached: nothing to measure.
                    text_width: None,
                    sheet: 0,
                    page: 0,
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

/// Start the tray icon, if this session wants one and can host one.
///
/// Every outcome but "it appeared" is silent-but-for-a-timing-line: a daemon
/// whose icon could not be placed is still serving its socket and still
/// answering its hotkey, and saying so on stderr would be noise on the many
/// desktops that simply have no tray. `--doctor` is where the answer belongs.
fn start_tray(
    settings: &Settings,
    hotkey_spec: Option<&str>,
    timing: Timing,
) -> Option<(Box<dyn tray::Tray>, std::sync::mpsc::Receiver<tray::Event>)> {
    if !settings.tray {
        timing.log("tray disabled");
        return None;
    }
    let menu = tray::Menu {
        hotkey: hotkey_spec.map(str::to_owned),
        hotkey_choices: tray::hotkey_choices(),
        // Filled in on the first frame, once the recent-files thread has read
        // the list — the same list the home screen shows.
        recent: Vec::new(),
    };
    match tray::spawn(icon::PNG, menu) {
        Some(started) => {
            timing.log("tray icon shown");
            Some(started)
        }
        None => {
            timing.log("no tray host in this session");
            None
        }
    }
}

/// Grab the global hotkey for this daemon.
///
/// Returns the channel of resolved paths, or `None` when there is no hotkey —
/// because it was declined with `--no-hotkey`, or because the platform refused
/// the grab. **A refusal is a printed warning and nothing more**: the daemon
/// has already bound its socket by the time this runs, and it goes on serving
/// it whatever happens here. A headless box, a Wayland-only session and a
/// combination another application already owns all land in that branch.
fn start_hotkey(
    binding: Binding,
    ctx: &egui::Context,
    timing: Timing,
) -> (Option<std::sync::mpsc::Receiver<PathBuf>>, Option<String>) {
    let Some((spec, key)) = binding else {
        return (None, None);
    };
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
            (None, None)
        }
        None => {
            timing.log("hotkey registered");
            (Some(hotkeys.presses), Some(spec))
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
fn doctor(
    args: &Args,
    settings: &Settings,
    location: &config::Location,
    loaded_hotkey: bool,
    binding: Option<&(String, hotkey::HotKey)>,
    output: console::Attached,
) -> Result<()> {
    println!("sekio-gui {} — doctor", env!("CARGO_PKG_VERSION"));
    println!();
    doctor_console(output);
    doctor_selection();
    println!();
    // Asked first: a daemon that is running is the most likely owner of the
    // hotkey, and that changes what a failed grab below means.
    let running = daemon_running();
    doctor_hotkey(hotkey_source(args, loaded_hotkey), binding, running);
    println!();
    doctor_dialog();
    println!();
    doctor_daemon(running);
    println!();
    doctor_tray(settings);
    println!();
    doctor_config(location, settings);
    Ok(())
}

/// Whether a resident daemon would get an icon, and where it would go.
///
/// Started here rather than reported from a running daemon because `--doctor`
/// is a separate process: it can only say what *this* session can host. An
/// icon appearing for the moment this runs is the honest test, and it is
/// removed again as the tray drops at the end of the function.
fn doctor_tray(settings: &Settings) {
    println!("tray");
    if !settings.tray {
        row("icon", "off (tray = false / --no-tray)");
        return;
    }
    let probe = tray::spawn(
        icon::PNG,
        tray::Menu {
            hotkey: settings.hotkey.clone(),
            hotkey_choices: tray::hotkey_choices(),
            recent: Vec::new(),
        },
    );
    match probe {
        Some((tray, _events)) => row("icon", format!("yes — {}", tray.describe())),
        None => {
            row("icon", "no — nothing in this session hosts one");
            hint(&[
                "the daemon runs, serves its socket and answers its hotkey",
                "regardless; it just has nowhere to put an icon. On GNOME,",
                "install the AppIndicator and KStatusNotifierItem extension.",
            ]);
        }
    }
}

/// Which config file was read, and what came out of the three layers.
///
/// Worth a section of its own because the file is where the hotkey and the
/// theme really live — a `--hotkey` has to be repeated in every autostart
/// entry, and this is the row that says whether the file was found at all.
fn doctor_config(location: &config::Location, settings: &Settings) {
    println!("config");
    match location {
        config::Location::Disabled => row("file", "ignored (--no-config)"),
        config::Location::Explicit(path) => row("file", format!("{} (--config)", path.display())),
        config::Location::Default(Some(path)) => {
            let state = if path.exists() { "read" } else { "not present" };
            row("file", format!("{} ({state})", path.display()));
        }
        config::Location::Default(None) => {
            row("file", "none — no config directory in this environment");
            hint(&["a hotkey chosen from the tray cannot be remembered here."]);
        }
    }
    row("theme", settings.theme.as_str());
    row("lines", settings.lines);
    row("wrap", settings.wrap);
}

/// Where this report is going — a question only Windows can get wrong.
///
/// A windowed binary has no console of its own, so `--doctor` printing nothing
/// is a real thing that can happen there, and this row says whether it did.
/// Prints nothing on any other platform, where the answer has always been "the
/// terminal you ran it in" and a row saying so would be noise.
#[cfg(windows)]
fn doctor_console(output: console::Attached) {
    println!("console");
    row("output", console::describe(output));
    if output != console::Attached::Parent {
        hint(&[
            "nothing is reading this. Run `sekio-gui --doctor` from",
            "PowerShell, cmd or Windows Terminal — started from there, the",
            "app reattaches to that window and prints into it.",
        ]);
    }
    println!();
}

#[cfg(not(windows))]
fn doctor_console(_output: console::Attached) {}

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
/// Which of the three layers the hotkey on screen actually came from.
///
/// Worth printing: "it is not the combination I set" is the commonest hotkey
/// complaint, and the answer is nearly always that a flag or a stale config
/// file is winning over the one the user just edited.
fn hotkey_source(args: &Args, from_file: bool) -> &'static str {
    if args.hotkey.is_some() {
        "--hotkey"
    } else if from_file {
        "config file"
    } else {
        "default"
    }
}

fn doctor_hotkey(source: &str, binding: Option<&(String, hotkey::HotKey)>, running: Option<bool>) {
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
fn daemon_running() -> Option<bool> {
    Some(daemon::is_running(&daemon::socket_path()))
}

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
