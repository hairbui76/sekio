//! sekio-tui — a two-pane terminal file browser on top of `sekio-core`.
//!
//! The left pane lists the current directory, the right pane paints the
//! `PreviewContent` IR for the entry under the cursor. Every preview (the
//! directory listing included) is produced on a worker thread; the UI loop only
//! polls. See `worker.rs` for the cancellation contract and `app.rs` for the
//! state machine.

mod app;
mod config;
mod table;
mod ui;
mod worker;

use std::io::{stdout, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use ratatui_image::picker::Picker;

use crate::app::{start_location, App};
use crate::config::{Location, Overrides, Platform, Settings};
use crate::ui::Ui;
use crate::worker::Worker;

/// How long a frame waits for a key before checking the worker again. Small
/// enough that a finished preview appears immediately, large enough that an
/// idle browser costs nothing.
const POLL: Duration = Duration::from_millis(30);

// Every overridable setting below is an `Option<T>` with no clap
// `default_value`. That is what makes "the user typed `--lines 500`"
// distinguishable from "clap supplied 500", so a config-file value is never
// clobbered by a default nobody asked for. The `[default: …]` notes in the help
// text are written by hand for the same reason. See `config.rs`.
/// sekio-tui — browse a directory and preview whatever is under the cursor.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Directory to browse, or a file to start on
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Read this config file instead of the default location
    #[arg(long, value_name = "PATH", conflicts_with = "no_config")]
    config: Option<PathBuf>,

    /// Ignore the config file entirely
    #[arg(long)]
    no_config: bool,

    /// Max lines of text per preview [default: 500]
    #[arg(long, value_name = "N")]
    lines: Option<usize>,

    /// Max bytes read from any one file [default: 524288]
    #[arg(long, value_name = "N")]
    max_bytes: Option<usize>,

    /// Max entries listed for a directory or archive [default: 1000]
    #[arg(long, value_name = "N")]
    max_entries: Option<usize>,

    /// Longest edge an image is downscaled to [default: 1024]
    #[arg(long, value_name = "N")]
    image_max_dim: Option<u32>,

    /// Skip the terminal graphics-capability query and always use halfblocks
    /// [default: false]
    ///
    /// `--halfblocks` means true; `--halfblocks=false` turns it back off when
    /// the config file enabled it. The value must be attached with `=` so a bare
    /// `--halfblocks` never swallows the path argument.
    #[arg(
        long,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_name = "BOOL"
    )]
    halfblocks: Option<bool>,
}

impl Args {
    fn overrides(&self) -> Overrides {
        Overrides {
            lines: self.lines,
            max_bytes: self.max_bytes,
            max_entries: self.max_entries,
            image_max_dim: self.image_max_dim,
            halfblocks: self.halfblocks,
        }
    }
}

/// Load the config, merge it under the command line, and report anything that
/// went wrong on stderr. Never fails: a broken config degrades to defaults.
fn settings(args: &Args) -> Settings {
    let location = Location::resolve(
        args.config.clone(),
        args.no_config,
        Platform::current(),
        |key| std::env::var_os(key),
    );
    let loaded = config::load(&location);
    let mut settings = config::resolve(&args.overrides(), &loaded.config);

    let mut warnings = loaded.warnings;
    if let Some(warning) = config::check_syntax_theme(&settings.syntax_theme) {
        warnings.push(warning);
        settings.syntax_theme = sekio_core::Previewer::DEFAULT_THEME.to_owned();
    }
    // Prefix every line, including the continuation lines of a multi-line TOML
    // parse error, so the whole thing is greppable and obviously ours.
    for line in warnings.iter().flat_map(|w| w.lines()) {
        eprintln!("sekio-tui: {line}");
    }
    settings
}

fn main() -> Result<()> {
    let args = Args::parse();
    let settings = settings(&args);

    let (dir, select) = start_location(&args.path)
        .with_context(|| format!("cannot open {}", args.path.display()))?;

    // A TUI needs a real terminal on both ends: stdout to draw on, stdin to
    // read keys from. Bail with a clear message instead of hanging on a
    // capability query that will never get an answer.
    if !stdout().is_terminal() {
        bail!(
            "sekio-tui needs an interactive terminal (stdout is not a tty).\n\
             For pipes and preview panes use `sekio` (the CLI) instead."
        );
    }
    if !std::io::stdin().is_terminal() {
        bail!("sekio-tui needs an interactive terminal (stdin is not a tty).");
    }

    // Must happen BEFORE the alternate screen and before we start reading
    // events: this writes escape sequences to stdout and reads the terminal's
    // replies off stdin. A terminal that doesn't answer just means halfblocks.
    //
    // The query ends with a Device Status Report, which every real terminal
    // answers. One that doesn't leaves ratatui-image's reader thread parked on
    // stdin, where it swallows keystrokes — `--halfblocks` skips the query
    // entirely for those (and for scripted runs that want no stdin traffic).
    let picker = if settings.halfblocks {
        Picker::halfblocks()
    } else {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    };

    let worker = Worker::spawn(settings.preview_options(), settings.syntax_theme.clone())
        .context("failed to start the preview worker")?;

    install_panic_hook();
    let _guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(stdout());
    // A pty whose window size was never set (e.g. `script` with its output
    // redirected to a file, or some CI runners) reports 0×0 — the ioctl
    // succeeds, so nothing errors, we just draw an empty screen forever. Pin a
    // usable viewport in that case instead of silently rendering nothing.
    let reported = crossterm::terminal::size().ok();
    let mut terminal = match fallback_viewport(reported, env_size()) {
        Some((columns, rows)) => Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, columns, rows)),
            },
        ),
        None => Terminal::new(backend),
    }
    .context("failed to size the terminal")?;
    terminal.clear().ok();

    // `_guard` restores the terminal on the way out of this function, on the
    // error path as much as the happy one.
    run(
        &mut terminal,
        &worker,
        App::new(dir, select),
        Ui::new(picker, settings.theme),
    )
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    worker: &Worker,
    mut app: App,
    mut ui: Ui,
) -> Result<()> {
    for request in app.take_requests() {
        worker.send(request);
    }

    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, &mut app, &mut ui))?;
            dirty = false;
        }

        // Never block on the worker: poll for input with a short timeout, then
        // drain whatever results happen to be ready.
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    on_key(&mut app, key);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollDown => {
                        app.scroll_by(1);
                        dirty = true;
                    }
                    MouseEventKind::ScrollUp => {
                        app.scroll_by(-1);
                        dirty = true;
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        while let Some(response) = worker.try_recv() {
            app.on_response(response);
            dirty = true;
        }

        // Every tick, not only on a frame we drew: a resize has to be seen
        // holding still for a moment before it costs a re-render, and the loop
        // stops drawing as soon as it has caught up.
        if app.poll_reflow() {
            dirty = true;
        }

        for request in app.take_requests() {
            worker.send(request);
        }

        if app.is_loading() {
            // Keep the spinner moving while we wait.
            ui.tick = ui.tick.wrapping_add(1);
            dirty = true;
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn on_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c' | 'C') if ctrl => app.should_quit = true,
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,

        // Cursor / navigation — each of these cancels the in-flight preview.
        KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
        KeyCode::Char('n') if ctrl => app.move_cursor(1),
        KeyCode::Char('p') if ctrl => app.move_cursor(-1),
        KeyCode::Home => app.select_first(),
        KeyCode::End => app.select_last(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.enter(),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.go_parent(),
        KeyCode::Char('r') => app.reload(),

        // Preview scrolling.
        KeyCode::PageDown => {
            let page = app.page();
            app.scroll_by(page);
        }
        KeyCode::PageUp => {
            let page = app.page();
            app.scroll_by(-page);
        }
        KeyCode::Char('d') if ctrl => {
            let half = app.half_page();
            app.scroll_by(half);
        }
        KeyCode::Char('u') if ctrl => {
            let half = app.half_page();
            app.scroll_by(-half);
        }
        KeyCode::Char(' ') => {
            let page = app.page();
            app.scroll_by(page);
        }
        KeyCode::Char('g') => app.scroll_to_top(),
        KeyCode::Char('G') => app.scroll_to_bottom(),
        _ => {}
    }
}

/// Decide whether the terminal's self-reported size is usable, and what to pin
/// the viewport to when it isn't.
///
/// Returns `None` when `reported` is a real size (use a fullscreen, auto-resizing
/// viewport), or `Some((columns, rows))` for a fixed fallback viewport.
fn fallback_viewport(reported: Option<(u16, u16)>, env: Option<(u16, u16)>) -> Option<(u16, u16)> {
    const DEFAULT: (u16, u16) = (80, 24);
    match reported {
        Some((columns, rows)) if columns > 0 && rows > 0 => None,
        // Trust $COLUMNS/$LINES next — a scripted run can set them deliberately.
        _ => Some(match env {
            Some((columns, rows)) if columns > 0 && rows > 0 => (columns, rows),
            _ => DEFAULT,
        }),
    }
}

fn env_size() -> Option<(u16, u16)> {
    let parse = |key| std::env::var(key).ok()?.parse::<u16>().ok();
    Some((parse("COLUMNS")?, parse("LINES")?))
}

/// Owns the raw-mode + alternate-screen state. `Drop` runs on every exit path —
/// clean quit, `?` bubbling an error, or an unwinding panic — so the user's
/// shell is never left in raw mode.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        // If entering the alternate screen fails, undo raw mode before
        // returning: there is no guard yet to do it for us.
        if let Err(err) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err).context("failed to enter the alternate screen");
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Best-effort restore. Deliberately ignores errors: it runs from `Drop` and
/// from the panic hook, where there is nothing useful left to do about them.
fn restore_terminal() {
    // Raw mode first — it has the wider blast radius if we only manage one.
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Restore the terminal *before* the default hook prints the panic message,
/// otherwise the backtrace lands on the alternate screen and vanishes — and the
/// shell is left in raw mode.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn app() -> App {
        App::new(PathBuf::from("/tmp/root"), None)
    }

    #[test]
    fn the_clap_definition_is_well_formed() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    /// The clap half of the precedence contract: a flag the user did *not* type
    /// must arrive as `None`, so `config::resolve` can tell it apart from one
    /// they did type — even when they typed the default value.
    #[test]
    fn untyped_flags_stay_none_so_the_config_survives() {
        let bare = Args::parse_from(["sekio-tui", "somedir"]);
        assert_eq!(bare.overrides(), Overrides::default());
        assert_eq!(bare.path, PathBuf::from("somedir"));

        let typed = Args::parse_from(["sekio-tui", "--lines", "500", "somedir"]);
        assert_eq!(
            typed.overrides().lines,
            Some(500),
            "an explicitly typed flag must be Some even at the default value"
        );
        assert_eq!(typed.overrides().max_bytes, None);
        assert_eq!(typed.overrides().halfblocks, None);

        let all = Args::parse_from([
            "sekio-tui",
            "--lines=1",
            "--max-bytes=2",
            "--max-entries=3",
            "--image-max-dim=4",
            "--halfblocks=false",
        ]);
        assert_eq!(
            all.overrides(),
            Overrides {
                lines: Some(1),
                max_bytes: Some(2),
                max_entries: Some(3),
                image_max_dim: Some(4),
                halfblocks: Some(false),
            }
        );
    }

    /// `--halfblocks` takes an optional value, so it must require `=` — else a
    /// bare `sekio-tui --halfblocks DIR` would eat the directory as its value.
    #[test]
    fn bare_halfblocks_does_not_swallow_the_path() {
        let args = Args::parse_from(["sekio-tui", "--halfblocks", "somedir"]);
        assert_eq!(args.halfblocks, Some(true));
        assert_eq!(args.path, PathBuf::from("somedir"));

        assert_eq!(
            Args::parse_from(["sekio-tui", "--halfblocks=false"]).halfblocks,
            Some(false)
        );
    }

    #[test]
    fn config_flags_are_wired_up() {
        let args = Args::parse_from(["sekio-tui", "--config", "/tmp/c.toml"]);
        assert_eq!(args.config, Some(PathBuf::from("/tmp/c.toml")));
        assert!(!args.no_config);
        assert!(Args::parse_from(["sekio-tui", "--no-config"]).no_config);
        // The two are mutually exclusive, so "which wins" never comes up.
        assert!(
            Args::try_parse_from(["sekio-tui", "--no-config", "--config", "/tmp/c.toml"]).is_err()
        );
    }

    #[test]
    fn quit_keys() {
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut a = app();
            on_key(&mut a, press(code, KeyModifiers::NONE));
            assert!(a.should_quit, "{code:?} should quit");
        }
        let mut a = app();
        on_key(&mut a, press(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(a.should_quit);
    }

    #[test]
    fn ctrl_d_scrolls_and_does_not_quit() {
        let mut a = app();
        on_key(&mut a, press(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(!a.should_quit);
        assert_eq!(a.scroll, 0, "nothing to scroll yet");
    }

    #[test]
    fn zero_sized_terminal_falls_back_to_a_fixed_viewport() {
        // The bug: a pty whose winsize was never set reports 0×0, the ioctl
        // succeeds, and every frame paints into nothing.
        assert_eq!(fallback_viewport(Some((0, 0)), None), Some((80, 24)));
        assert_eq!(fallback_viewport(Some((120, 0)), None), Some((80, 24)));
        assert_eq!(fallback_viewport(None, None), Some((80, 24)));
        assert_eq!(
            fallback_viewport(Some((0, 0)), Some((100, 30))),
            Some((100, 30))
        );
        assert_eq!(
            fallback_viewport(Some((0, 0)), Some((0, 30))),
            Some((80, 24))
        );
        // A real size means "leave the auto-resizing fullscreen viewport alone".
        assert_eq!(fallback_viewport(Some((120, 40)), Some((10, 10))), None);
    }

    #[test]
    fn plain_letters_do_not_trigger_ctrl_bindings() {
        let mut a = app();
        // 'n' with no modifier must not move the cursor logic path; with an
        // empty listing nothing happens either way, but it must not panic.
        on_key(&mut a, press(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(!a.should_quit);
    }
}
