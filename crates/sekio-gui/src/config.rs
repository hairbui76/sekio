//! The config file: `$XDG_CONFIG_HOME/sekio/gui.toml` (or
//! `%APPDATA%\sekio\gui.toml` on Windows), plus the precedence rules that merge
//! it with the command line.
//!
//! The GUI needs a file where the TUI merely benefits from one. Its headline
//! setting is a *global* hotkey, and a global hotkey that lives only in
//! `--hotkey` has to be repeated in every autostart entry, `.desktop` file and
//! Start Menu shortcut that launches the daemon — and cannot be changed from
//! the tray at all, because the tray has no command line to edit. So this
//! module reads the file *and* writes it: [`save_hotkey`] is what
//! `tray::Event::SetHotkey` persists through.
//!
//! `gui.toml`, not `config.toml`: both frontends resolve the same `sekio/`
//! directory, and two programs with different schemas sharing one file name
//! would mean `deny_unknown_fields` rejecting the other one's keys.
//!
//! Four rules the rest of the crate depends on, three of them lifted wholesale
//! from `sekio-tui`'s `config.rs` because they were hard-won there:
//!
//! 1. **Flags beat config beats defaults.** Every overridable CLI argument is an
//!    `Option<T>` with *no* clap `default_value`, so "the user typed `--lines
//!    500`" and "clap filled in 500" are distinguishable — with `default_value_t`
//!    they are not, and a config value would be silently clobbered by a default
//!    nobody typed. [`resolve`] is the single place the three layers meet, and it
//!    is a pure function so the precedence can be unit tested.
//! 2. **A bad config never blocks startup.** Loading returns warnings instead of
//!    errors; the caller prints them to stderr and carries on with defaults. A
//!    missing file at the *default* location is normal and silent; a missing
//!    file the user named explicitly is worth a warning.
//! 3. **Defaults live where the thing does.** `lines` comes from
//!    `PreviewOptions::default()` and the hotkey from [`hotkey::DEFAULT_SPEC`],
//!    so this file cannot drift from them.
//! 4. **A value the file may contain but the program cannot use is a warning,
//!    not a failure.** [`validate`] drops such a field so the layer *below* it
//!    applies — an unparseable `hotkey` falls back to the default combination
//!    rather than leaving the daemon with no hotkey at all. The command line is
//!    held to a stricter standard on purpose: a `--hotkey` the user just typed
//!    should be a startup error, because they are watching and can fix it.

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use sekio_core::PreviewOptions;
use serde::Deserialize;

use crate::hotkey::{self, HotKey};
use crate::state::OnClose;
use crate::style::Theme;

/// The file's name inside the platform's `sekio` config directory.
pub const FILE: &str = "gui.toml";

/// Show the tray icon when resident, unless told otherwise. The tray is how a
/// daemon with a hidden window proves it is running, so it is on by default;
/// [`crate::tray::spawn`] returning `None` (no tray host in this session) is
/// still an ordinary outcome, not a failure.
pub const DEFAULT_TRAY: bool = true;

/// Ask GitHub for a newer sekio once, at startup, unless told otherwise.
///
/// On by default because an update nobody hears about is an update nobody
/// installs, and off in one line for anyone who would rather their previewer
/// never spoke to the network. The check runs once per process, never on a
/// timer, and says nothing at all when the answer is "you already have it".
pub const DEFAULT_UPDATES: bool = true;

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

/// The whole config file. Every field is optional so a partial config works, and
/// unknown keys are rejected rather than silently ignored — a typo'd key is
/// almost always a mistake the user wants to hear about.
///
/// Deliberately *small*. Only genuinely persistent preferences belong here:
/// `--borderless`, `--probe`, `--timing` and `--doctor` all describe one
/// invocation ("measure this run", "tell me what you can see") rather than how
/// the user wants sekio to behave, and a file that could turn `--probe` on
/// permanently would be a way to make the app never open a window again.
/// `--daemon` / `--no-daemon` are likewise per-launch: which process serves a
/// path is decided by whether one is already resident, not by a preference.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Global hotkey the daemon answers to, e.g. `"Ctrl+Shift+Space"`.
    pub hotkey: Option<String>,
    /// Show the tray icon while resident.
    pub tray: Option<bool>,
    /// Check for a newer release at startup.
    pub updates: Option<bool>,
    /// Max lines of text per preview.
    pub lines: Option<usize>,
    /// Wrap around at the ends when arrowing through a directory.
    pub wrap: Option<bool>,
    /// `"dark"`, `"light"` or `"system"`. A string rather than the enum so an
    /// unusable value is a *warning* (rule 4) instead of a parse error that
    /// throws away the rest of the file — the same treatment `hotkey` gets.
    pub theme: Option<String>,
    /// `"ask"`, `"tray"` or `"quit"`: what the window's close button does when
    /// there is more than one sensible answer. A string for the same reason
    /// `theme` is one. Written back by the close dialog's "don't ask again".
    pub on_close: Option<String>,
}

/// Parse the file's text. Separate from [`load_file`] so the parser is testable
/// without touching a filesystem.
pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
}

/// Drop any field the parser accepted but the program cannot act on, returning
/// one warning per field dropped.
///
/// Pure, and the only reason a *valid* TOML file produces warnings. Dropping to
/// `None` rather than substituting the default is what makes the fallback
/// automatic: [`resolve`] then reaches the layer below, so a bad `hotkey` in the
/// file behaves exactly like no `hotkey` in the file.
pub fn validate(config: &mut Config) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Some(spec) = &config.hotkey {
        if let Err(err) = hotkey::parse(spec) {
            warnings.push(format!(
                "warning: invalid hotkey {spec:?}: {err}\n\
                 warning: using {default:?} instead",
                default = hotkey::DEFAULT_SPEC,
            ));
            config.hotkey = None;
        }
    }

    if let Some(name) = &config.theme {
        if Theme::parse(name).is_none() {
            warnings.push(format!(
                "warning: unknown theme {name:?} (expected {options}); \
                 using {default:?} instead",
                options = Theme::NAMES.join(", "),
                default = Theme::default().as_str(),
            ));
            config.theme = None;
        }
    }

    if let Some(name) = &config.on_close {
        if OnClose::parse(name).is_none() {
            warnings.push(format!(
                "warning: unknown on_close {name:?} (expected {options}); \
                 using {default:?} instead",
                options = OnClose::NAMES.join(", "),
                default = OnClose::default().as_str(),
            ));
            config.on_close = None;
        }
    }

    // A zero line limit is syntactically fine and renders every text file as an
    // empty pane, which reads as a broken program rather than as a setting.
    if config.lines == Some(0) {
        warnings.push(format!(
            "warning: lines = 0 would render every text preview empty; \
             using {default} instead",
            default = PreviewOptions::default().max_lines,
        ));
        config.lines = None;
    }

    warnings
}

// ---------------------------------------------------------------------------
// Merging the layers
// ---------------------------------------------------------------------------

/// What the user actually typed on the command line. `None` means "not typed",
/// which is the whole point — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `--hotkey SPEC`.
    pub hotkey: Option<String>,
    /// `--no-hotkey`: grab nothing, whatever the file says. Not an
    /// `Option<bool>` because there is no way to type "do use a hotkey" and
    /// nothing for it to override — its absence is not a preference.
    pub no_hotkey: bool,
    /// A future `--tray` / `--no-tray`; wired here so adding the flag is one
    /// clap field and no change to the precedence rules.
    pub tray: Option<bool>,
    /// `--no-update-check` sets `Some(false)`.
    pub updates: Option<bool>,
    /// `--lines N`.
    pub lines: Option<usize>,
    /// `--wrap` — `Some(true)` when passed. `Some(false)` is reserved for a
    /// future `--no-wrap`, and already beats `wrap = true` in the file.
    pub wrap: Option<bool>,
    /// `--theme dark|light|system`. Typed as the enum, not a string, because
    /// clap can reject a bad value while the user is watching — which is the
    /// stricter standard the module docs describe for the command line.
    pub theme: Option<Theme>,
}

/// Everything the app needs, with all three layers already merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The hotkey spec to grab, or `None` for `--no-hotkey`.
    pub hotkey: Option<String>,
    pub tray: bool,
    pub lines: usize,
    pub wrap: bool,
    /// Which palette to paint in, before the desktop has been asked. Resolving
    /// `System` is `style::Theme::resolve`'s job, and happens every frame.
    pub theme: Theme,
    /// Look for a newer release once, when the window or the daemon starts.
    pub updates: bool,
    /// What the close button does. Only ever consulted by a resident daemon —
    /// see [`crate::state::close_intent`].
    pub on_close: OnClose,
}

impl Default for Settings {
    fn default() -> Self {
        resolve(&Overrides::default(), &Config::default())
    }
}

impl Settings {
    /// The preview limits this frontend asks core for.
    ///
    /// `max_lines` is the only one the GUI exposes: it is the setting whose cost
    /// the user can feel (syntax highlighting dominates preview latency), and
    /// the rest of `PreviewOptions` has no UI to disagree with.
    pub fn preview_options(&self) -> PreviewOptions {
        PreviewOptions {
            max_lines: self.lines,
            ..Default::default()
        }
    }

    /// The resolved spec parsed into a combination, in the shape `main.rs`
    /// already uses: the spec is kept alongside it because that is what the
    /// tray menu and `--doctor` print.
    ///
    /// Only a spec that came from the command line can fail here — one from the
    /// file was already dropped by [`validate`], and [`hotkey::DEFAULT_SPEC`]
    /// parses by construction (there is a test) — so the caller is right to
    /// report the error as a bad `--hotkey`.
    pub fn binding(&self) -> Result<Option<(String, HotKey)>, hotkey::ParseError> {
        match &self.hotkey {
            None => Ok(None),
            Some(spec) => Ok(Some((spec.clone(), hotkey::parse(spec)?))),
        }
    }
}

/// Merge the three layers: command line, then config file, then built-in
/// defaults.
///
/// Pure on purpose — the precedence is the easiest thing in this feature to get
/// subtly wrong, so it is testable without a filesystem or an `ArgMatches`.
pub fn resolve(cli: &Overrides, cfg: &Config) -> Settings {
    Settings {
        hotkey: if cli.no_hotkey {
            None
        } else {
            Some(
                cli.hotkey
                    .clone()
                    .or_else(|| cfg.hotkey.clone())
                    .unwrap_or_else(|| hotkey::DEFAULT_SPEC.to_owned()),
            )
        },
        tray: cli.tray.or(cfg.tray).unwrap_or(DEFAULT_TRAY),
        updates: cli.updates.or(cfg.updates).unwrap_or(DEFAULT_UPDATES),
        lines: cli
            .lines
            .or(cfg.lines)
            .unwrap_or(PreviewOptions::default().max_lines),
        wrap: cli.wrap.or(cfg.wrap).unwrap_or(false),
        // An unparseable name in the file has normally been dropped by
        // `validate` already; parsing again here means `resolve` stays a total
        // function on any `Config`, and an unusable value still falls through
        // to the default rather than to nothing.
        theme: cli
            .theme
            .or_else(|| cfg.theme.as_deref().and_then(Theme::parse))
            .unwrap_or_default(),
        // No command-line layer: this is a preference the user sets by
        // answering the dialog, and a flag that could pre-answer it would only
        // ever be typed once, in an autostart entry nobody edits again.
        on_close: cfg
            .on_close
            .as_deref()
            .and_then(OnClose::parse)
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Where the file lives
// ---------------------------------------------------------------------------

/// Which platform's config-path convention to follow. Explicit rather than
/// `#[cfg]`-only so both shapes can be tested from either host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Unix,
    Windows,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Resolve the default config path from environment variables.
///
/// The lookup is injected so tests never touch the real environment (and so
/// this is deterministic when tests run in parallel).
///
/// * Unix: `$XDG_CONFIG_HOME/sekio/gui.toml`, else `$HOME/.config/sekio/gui.toml`.
///   A relative `XDG_CONFIG_HOME` is ignored, as the XDG spec requires.
/// * Windows: `%APPDATA%\sekio\gui.toml` — the *roaming* directory, unlike the
///   recent-files list in `recent.rs`, which is local per-machine state.
pub fn default_config_path(
    platform: Platform,
    env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let var = |key: &str| -> Option<PathBuf> {
        let value = env(key)?;
        if value.is_empty() {
            return None;
        }
        Some(PathBuf::from(value))
    };
    let dir = match platform {
        Platform::Unix => var("XDG_CONFIG_HOME")
            .filter(|p| is_absolute_on(Platform::Unix, p))
            .or_else(|| Some(var("HOME")?.join(".config")))?,
        Platform::Windows => var("APPDATA")?,
    };
    Some(dir.join("sekio").join(FILE))
}

/// This machine's config path, or `None` when the environment says nothing
/// useful (a service account with no `$HOME`) — in which case there is no file
/// to read and nowhere to persist a hotkey to.
pub fn config_path() -> Option<PathBuf> {
    default_config_path(Platform::current(), |key| std::env::var_os(key))
}

/// Absoluteness judged by the *emulated* platform rather than the host.
///
/// `Path::is_absolute` answers for whichever OS the binary is running on, so
/// using it here would make `default_config_path(Platform::Unix, ..)` behave
/// like Windows whenever the tests run on Windows — `/xdg` is not absolute
/// under Windows rules, so an absolute XDG dir would be silently discarded.
/// That would defeat the point of taking `platform` as a parameter, and is the
/// exact trap CLAUDE.md records breaking CI three times.
fn is_absolute_on(platform: Platform, path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    match platform {
        Platform::Unix => bytes.first() == Some(&b'/'),
        // A UNC path, or a drive letter followed by a separator.
        Platform::Windows => {
            bytes.starts_with(br"\\")
                || (bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && (bytes[2] == b'\\' || bytes[2] == b'/'))
        }
    }
}

/// Which file (if any) to load, and how loudly to complain when it isn't there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// Load nothing at all (a future `--no-config`).
    Disabled,
    /// The user named it (a future `--config PATH`), so a missing file is worth
    /// saying out loud.
    Explicit(PathBuf),
    /// The platform default, if one could be resolved. Absent is normal.
    Default(Option<PathBuf>),
}

impl Location {
    pub fn resolve(
        explicit: Option<PathBuf>,
        no_config: bool,
        platform: Platform,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        if no_config {
            return Self::Disabled;
        }
        match explicit {
            Some(path) => Self::Explicit(path),
            None => Self::Default(default_config_path(platform, env)),
        }
    }

    /// This machine's location, from the real environment.
    pub fn current() -> Self {
        Self::Default(config_path())
    }

    /// Where a setting changed at runtime should be written, or `None` when
    /// there is nowhere to write it — `--no-config`, or an environment with no
    /// config directory. The tray must treat `None` as "this change cannot
    /// persist", not as an error.
    pub fn write_target(&self) -> Option<&Path> {
        match self {
            Self::Disabled | Self::Default(None) => None,
            Self::Explicit(path) | Self::Default(Some(path)) => Some(path),
        }
    }
}

/// A config plus whatever went wrong getting it. Never an `Err`: a broken config
/// degrades to defaults, it does not stop the app from starting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

pub fn load(location: &Location) -> Loaded {
    match location {
        Location::Disabled => Loaded::default(),
        Location::Default(None) => Loaded::default(),
        Location::Explicit(path) => load_file(path, true),
        Location::Default(Some(path)) => load_file(path, false),
    }
}

/// `required` distinguishes "the user pointed at this file" from "this is just
/// where a config would live if there were one".
pub fn load_file(path: &Path, required: bool) -> Loaded {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse(&text) {
            Ok(mut config) => {
                let warnings = validate(&mut config);
                Loaded { config, warnings }
            }
            Err(err) => degraded(path, &err.to_string()),
        },
        Err(err) if err.kind() == ErrorKind::NotFound && !required => Loaded::default(),
        Err(err) => degraded(path, &err.to_string()),
    }
}

fn degraded(path: &Path, problem: &str) -> Loaded {
    Loaded {
        config: Config::default(),
        warnings: vec![format!(
            "warning: {}: {}\nwarning: continuing with built-in defaults",
            path.display(),
            problem.trim_end(),
        )],
    }
}

// ---------------------------------------------------------------------------
// Persisting a change made at runtime
// ---------------------------------------------------------------------------

/// Why a hotkey could not be persisted. Never a panic and never fatal: the
/// caller registers the new hotkey anyway and warns that it will not survive a
/// restart.
#[derive(Debug)]
pub enum SaveError {
    /// The spec does not parse, so writing it would produce a config file that
    /// warns on every subsequent start. Refused before anything is written.
    Hotkey {
        spec: String,
        source: hotkey::ParseError,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hotkey { spec, source } => {
                write!(f, "refusing to save the invalid hotkey {spec:?}: {source}")
            }
            Self::Io { path, source } => {
                write!(f, "cannot save {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Hotkey { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
        }
    }
}

/// Persist `spec` as the `hotkey` setting in `path`, creating the file and its
/// parent directory if they do not exist.
///
/// This is the write half of the module, and what `tray::Event::SetHotkey`
/// needs: the tray offers a new combination, the daemon grabs it, and this makes
/// it survive a restart. The spec is parsed first, so a bad one is refused
/// rather than written for the *next* start to warn about.
pub fn save_hotkey(path: &Path, spec: &str) -> Result<(), SaveError> {
    if let Err(source) = hotkey::parse(spec) {
        return Err(SaveError::Hotkey {
            spec: spec.to_owned(),
            source,
        });
    }

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|source| io_error(path, source))?;
        }
    }

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        // Unreadable but present: rewriting it from scratch would throw away a
        // file we cannot see, so stop instead.
        Err(err) => return Err(io_error(path, err)),
    };

    let updated = with_hotkey(&existing, spec);
    write_atomically(path, &updated).map_err(|source| io_error(path, source))
}

/// Persist `theme` as the `theme` setting in `path`.
///
/// The window's own theme switcher writes through this, so a mode chosen from
/// the menu is still there next time. No validation step like `save_hotkey`'s:
/// the caller hands over a [`Theme`], which cannot be a value the reader would
/// reject.
pub fn save_theme(path: &Path, theme: Theme) -> Result<(), SaveError> {
    let existing = read_for_update(path)?;
    let updated = with_setting(&existing, "theme", theme.as_str());
    write_atomically(path, &updated).map_err(|source| io_error(path, source))
}

/// Persist `on_close` as the `on_close` setting in `path`.
///
/// What "don't ask again" writes through. Same shape as [`save_theme`], and
/// the same reason it needs no validation: the caller hands over an
/// [`OnClose`], which cannot spell itself in a way the reader would reject.
pub fn save_on_close(path: &Path, on_close: OnClose) -> Result<(), SaveError> {
    let existing = read_for_update(path)?;
    let updated = with_setting(&existing, "on_close", on_close.as_str());
    write_atomically(path, &updated).map_err(|source| io_error(path, source))
}

/// Read a file that is about to be rewritten, creating its directory first.
///
/// A missing file is an empty one — the settings file is optional and this is
/// how it comes into existence. A file that exists but cannot be *read* is a
/// hard stop: rewriting it from scratch would throw away content we cannot
/// see.
fn read_for_update(path: &Path) -> Result<String, SaveError> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|source| io_error(path, source))?;
        }
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(io_error(path, err)),
    }
}

fn io_error(path: &Path, source: io::Error) -> SaveError {
    SaveError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Rewrite `text` so the top-level `hotkey` key is set to `spec`.
///
/// Comment-preserving, and line-oriented for exactly that reason: serialising a
/// `Config` and writing it back would produce a correct file that has lost every
/// comment the user wrote, every key they left commented out, and their key
/// order — for a tray click that changed one string. So an existing `hotkey =`
/// line keeps its indentation and its trailing comment and only the value
/// changes; every other byte of the file is copied through untouched.
///
/// With no such line the assignment is appended to the end of the top-level
/// section — before the first `[table]` header if the file has grown one, since
/// a bare key after a header belongs to that table and would quietly mean
/// something else. A commented-out `# hotkey = …` line is left alone rather than
/// uncommented in place: it is documentation of the default, and the live line
/// added below it wins in TOML anyway.
pub fn with_hotkey(text: &str, spec: &str) -> String {
    with_setting(text, "hotkey", spec)
}

/// The general form of [`with_hotkey`]: set top-level `key` to the string
/// `value`, keeping every other byte of `text` exactly as it was.
///
/// Two settings are written back from the UI now — the tray's hotkey and the
/// window's theme — and they must not have two different ideas about what
/// happens to a comment.
pub fn with_setting(text: &str, key: &str, value: &str) -> String {
    let value = toml_string(value);
    let segments: Vec<&str> = text.split_inclusive('\n').collect();

    // The top-level section is everything before the first table header; only
    // there does a bare `hotkey` key mean the setting we are looking for.
    let first_table = segments
        .iter()
        .position(|segment| line_body(segment).0.trim_start().starts_with('['))
        .unwrap_or(segments.len());

    let mut out = String::with_capacity(text.len() + value.len() + 16);

    for (index, segment) in segments.iter().enumerate() {
        let (body, eol) = line_body(segment);
        match assignment(body, key).filter(|_| index < first_table) {
            Some((value_start, value_end)) => {
                out.push_str(&body[..value_start]);
                out.push_str(&value);
                out.push_str(&body[value_end..]);
                out.push_str(eol);
                // The remaining lines are copied verbatim: a second top-level
                // `hotkey` is not valid TOML, so there is nothing to keep in
                // step with.
                out.push_str(&segments[index + 1..].concat());
                return out;
            }
            None => {
                if index == first_table {
                    push_assignment(&mut out, text, key, &value);
                }
                out.push_str(segment);
            }
        }
    }

    if first_table == segments.len() {
        push_assignment(&mut out, text, key, &value);
    }
    out
}

/// Append `<key> = <value>` as its own line, starting one if the file's last
/// line was unterminated.
fn push_assignment(out: &mut String, text: &str, key: &str, value: &str) {
    // Windows is a first-class target, so a config written by an editor there
    // has CRLF endings and a lone `\n` would leave one line looking different
    // from every other.
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(value);
    out.push_str(eol);
}

/// One `split_inclusive('\n')` segment split into its content and its line
/// ending. `lines()` would do the first half and throw away the second, which
/// is how a CRLF file gets silently rewritten with mixed endings.
fn line_body(segment: &str) -> (&str, &str) {
    match segment.strip_suffix('\n') {
        Some(body) => match body.strip_suffix('\r') {
            Some(body) => (body, "\r\n"),
            None => (body, "\n"),
        },
        None => (segment, ""),
    }
}

/// The byte range of the *value* in a `hotkey = …` line, or `None` when this
/// line is not that assignment.
///
/// Returning a range rather than a rewritten line is what preserves the user's
/// spacing and any trailing comment: everything outside it is copied through.
/// A commented line never matches, because the `#` is part of the trimmed
/// content.
fn assignment(body: &str, key: &str) -> Option<(usize, usize)> {
    let indent = body.len() - body.trim_start().len();
    let after_key = body[indent..].strip_prefix(key)?;
    let gap = after_key.len() - after_key.trim_start().len();
    // Anything but `=` here means this was a different key that merely starts
    // with the one we want (`hotkey` vs `hotkeys`, `theme` vs `theme_name`).
    let after_eq = after_key[gap..].strip_prefix('=')?;
    let space = after_eq.len() - after_eq.trim_start().len();
    let start = indent + key.len() + gap + 1 + space;
    Some((start, start + value_len(&after_eq[space..])))
}

/// How many bytes of `value` the TOML value occupies, so a trailing comment is
/// not mistaken for part of it (`hotkey = "Alt+F1"  # was Ctrl+Space`).
fn value_len(value: &str) -> usize {
    let bytes = value.as_bytes();
    match bytes.first() {
        // A basic string, where `\"` does not end it.
        Some(b'"') => {
            let mut i = 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => return i + 1,
                    _ => i += 1,
                }
            }
            value.len()
        }
        // A literal string: no escapes at all, by definition.
        Some(b'\'') => match value[1..].find('\'') {
            Some(end) => end + 2,
            None => value.len(),
        },
        // Not a string (a malformed file, or a bare word); stop at whitespace
        // or the start of a comment.
        _ => value
            .find(|c: char| c.is_whitespace() || c == '#')
            .unwrap_or(value.len()),
    }
}

/// Quote a string as a TOML basic string.
///
/// The escaping matters more than it looks: `\` and `'` are both key names this
/// crate's hotkey parser accepts (`Ctrl+\` is a real combination), and an
/// unescaped backslash would produce a file that no longer parses — or, worse,
/// one that parses as a different hotkey.
fn toml_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                // Unreachable for a spec that parsed, but a quoting function
                // that can emit a control character raw is a quoting function
                // that produces an unparseable file one day.
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write `contents` to `path` via a temp file in the same directory, then
/// rename.
///
/// A hotkey is changed from a tray menu, which means it can be changed while the
/// machine is shutting down. Writing in place would make a truncated or
/// half-written `gui.toml` possible, and that file is read at every start — so
/// one interrupted write would cost the user their config. `sync_all` before the
/// rename is the other half: without it the rename can land while the new
/// contents are still only in the page cache.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let temp = temp_path(path);
    let written = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(err);
    }
    Ok(())
}

/// A sibling of `path`, so the rename stays within one filesystem, and stamped
/// with the pid so two sekio processes cannot collide on it.
fn temp_path(path: &Path) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// An env lookup backed by a fixed table — never the real environment, so
    /// these tests can't be perturbed by the machine they run on, and never
    /// read the developer's own `~/.config`.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    /// A unique directory under the system temp dir; no `tempfile` dependency
    /// for a handful of tests' worth of I/O.
    fn scratch(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("sekio-gui-cfg-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn temp_file(tag: &str, contents: &str) -> PathBuf {
        let path = scratch(tag).join(FILE);
        std::fs::write(&path, contents).expect("write temp config");
        path
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read back")
    }

    // ---- parsing ----

    #[test]
    fn a_full_config_parses() {
        let config = parse(
            r#"
            hotkey = "Super+P"
            tray = false
            lines = 250
            wrap = true
            theme = "light"
            on_close = "tray"
            "#,
        )
        .expect("full config should parse");

        assert_eq!(config.hotkey.as_deref(), Some("Super+P"));
        assert_eq!(config.tray, Some(false));
        assert_eq!(config.lines, Some(250));
        assert_eq!(config.wrap, Some(true));
        assert_eq!(config.theme.as_deref(), Some("light"));
        assert_eq!(config.on_close.as_deref(), Some("tray"));
    }

    #[test]
    fn a_partial_config_leaves_the_rest_unset() {
        let config = parse("lines = 10\n").expect("partial config should parse");
        assert_eq!(config.lines, Some(10));
        assert_eq!(config.hotkey, None);
        assert_eq!(config.tray, None);
        assert_eq!(config.wrap, None);
        assert_eq!(config.theme, None);

        let only_hotkey = parse("hotkey = \"Alt+F1\"\n").expect("hotkey-only config");
        assert_eq!(only_hotkey.hotkey.as_deref(), Some("Alt+F1"));
        assert_eq!(only_hotkey.lines, None);

        // …and an empty file is a valid config that changes nothing.
        assert_eq!(parse("").expect("empty config"), Config::default());
        assert_eq!(
            resolve(&Overrides::default(), &parse("").expect("empty")),
            Settings::default()
        );
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_useful_message() {
        let err = parse("linez = 10\n").expect_err("a typo'd key must not be ignored");
        let msg = err.to_string();
        assert!(msg.contains("linez"), "{msg}");
        assert!(msg.contains("unknown field"), "{msg}");
        // The valid keys are listed, so the user can see the fix.
        assert!(msg.contains("lines"), "{msg}");
        assert!(msg.contains("hotkey"), "{msg}");
        assert!(msg.contains("theme"), "{msg}");

        // Settings that deliberately are *not* persistent must be rejected too,
        // rather than looking like they work.
        for line in [
            "borderless = true",
            "probe = true",
            "timing = true",
            "doctor = true",
            "daemon = true",
        ] {
            match parse(&format!("{line}\n")) {
                Ok(cfg) => panic!("a per-invocation flag must not be a config key: {cfg:?}"),
                Err(err) => assert!(err.to_string().contains("unknown field"), "{line}: {err}"),
            }
        }
    }

    /// The shipped example must stay valid — including its commented-out lines,
    /// which are what users uncomment. Every key is exercised by uncommenting
    /// the whole file and parsing it.
    #[test]
    fn the_example_config_is_valid_and_documents_every_key() {
        let example = include_str!("../gui.example.toml");
        parse(example).expect("the example config must parse as shipped");

        let uncommented: String = example
            .lines()
            .map(|line| line.strip_prefix("# ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let mut config =
            parse(&uncommented).expect("the example's defaults must parse when enabled");

        assert!(
            validate(&mut config).is_empty(),
            "the documented defaults must all be usable"
        );
        assert_eq!(config.hotkey.as_deref(), Some(hotkey::DEFAULT_SPEC));
        assert_eq!(config.tray, Some(DEFAULT_TRAY));
        assert_eq!(config.lines, Some(PreviewOptions::default().max_lines));
        assert_eq!(config.wrap, Some(false));
        assert_eq!(config.theme.as_deref(), Some(Theme::default().as_str()));
        assert_eq!(
            config.on_close.as_deref(),
            Some(OnClose::default().as_str())
        );
        // Uncommented, the example is exactly the built-in behaviour.
        assert_eq!(resolve(&Overrides::default(), &config), Settings::default());
    }

    /// Every field of `Config` has to appear in the example, or a key ships
    /// undocumented. Cheap structural check: the derive's own error message
    /// lists the field names, so it cannot drift.
    #[test]
    fn the_example_mentions_every_key() {
        let example = include_str!("../gui.example.toml");
        let listing = parse("nope = 1\n").expect_err("unknown key").to_string();
        let mut checked = 0;
        for key in [
            "hotkey", "tray", "lines", "wrap", "theme", "updates", "on_close",
        ] {
            assert!(
                listing.contains(key),
                "`{key}` is not a field of Config any more; update this test"
            );
            assert!(
                example.contains(&format!("# {key} = ")),
                "`{key}` is undocumented in gui.example.toml"
            );
            checked += 1;
        }
        assert_eq!(checked, 7);
    }

    // ---- degrading, never failing ----

    #[test]
    fn malformed_toml_degrades_to_defaults_with_a_warning() {
        let path = temp_file("malformed", "lines = = 3\n[theme\n");
        let loaded = load_file(&path, true);
        assert_eq!(
            loaded.config,
            Config::default(),
            "a broken config must fall back to defaults, not half-apply"
        );
        assert_eq!(loaded.warnings.len(), 1);
        let warning = &loaded.warnings[0];
        assert!(
            warning.contains(&path.display().to_string()),
            "the warning must name the file: {warning}"
        );
        assert!(warning.contains("defaults"), "{warning}");
        // And the resolved settings are exactly the built-in ones.
        assert_eq!(
            resolve(&Overrides::default(), &loaded.config),
            Settings::default()
        );
    }

    #[test]
    fn an_unknown_key_in_a_real_file_warns_but_starts() {
        let path = temp_file("unknown", "lines = 7\nlinez = 9\n");
        let loaded = load_file(&path, false);
        assert_eq!(loaded.config, Config::default());
        assert!(
            loaded.warnings[0].contains("linez"),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn an_invalid_hotkey_warns_and_falls_back() {
        let path = temp_file("bad-hotkey", "hotkey = \"Ctrl+Nope\"\nlines = 7\n");
        let loaded = load_file(&path, false);

        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        let warning = &loaded.warnings[0];
        assert!(warning.contains("Ctrl+Nope"), "{warning}");
        assert!(
            warning.contains(hotkey::DEFAULT_SPEC),
            "the warning must name the fallback: {warning}"
        );

        // The bad field is dropped, so the layer below applies — and the *rest*
        // of the file still takes effect, which is the difference between "warn
        // and fall back" and "throw the file away".
        assert_eq!(loaded.config.hotkey, None);
        assert_eq!(loaded.config.lines, Some(7));
        let settings = resolve(&Overrides::default(), &loaded.config);
        assert_eq!(settings.hotkey.as_deref(), Some(hotkey::DEFAULT_SPEC));
        assert_eq!(settings.lines, 7);
    }

    /// Rule 4 again, for `on_close`. The value is written by the app itself,
    /// so a bad one means the user hand-edited it — and being told the three
    /// words that work beats being told nothing.
    #[test]
    fn an_unknown_on_close_warns_and_falls_back() {
        let path = temp_file("bad-on-close", "on_close = \"maybe\"\nlines = 7\n");
        let loaded = load_file(&path, false);

        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        let warning = &loaded.warnings[0];
        assert!(warning.contains("maybe"), "{warning}");
        for name in OnClose::NAMES {
            assert!(warning.contains(name), "{name} missing from: {warning}");
        }
        assert_eq!(loaded.config.on_close, None);
        assert_eq!(
            loaded.config.lines,
            Some(7),
            "the rest of the file survives"
        );
        assert_eq!(
            resolve(&Overrides::default(), &loaded.config).on_close,
            OnClose::Ask,
            "dropping the field is what makes the default apply"
        );
    }

    /// Asking is the default, and the file is the only layer that can change
    /// it: there is deliberately no `--on-close`.
    #[test]
    fn on_close_comes_from_the_file_or_the_default() {
        assert_eq!(
            resolve(&Overrides::default(), &Config::default()).on_close,
            OnClose::Ask
        );
        let cfg = Config {
            on_close: Some("quit".to_owned()),
            ..Config::default()
        };
        assert_eq!(resolve(&Overrides::default(), &cfg).on_close, OnClose::Quit);
    }

    /// Rule 4, for the newest key: an unusable `theme` is a warning and the
    /// layer below applies, not a startup failure and not a dropped file.
    #[test]
    fn an_unknown_theme_warns_and_falls_back() {
        let path = temp_file("bad-theme", "theme = \"solarized\"\nlines = 7\n");
        let loaded = load_file(&path, false);

        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        let warning = &loaded.warnings[0];
        assert!(warning.contains("solarized"), "{warning}");
        assert!(
            warning.contains(Theme::default().as_str()),
            "the warning must name the fallback: {warning}"
        );
        // Every value the user could have meant is listed.
        for name in Theme::NAMES {
            assert!(warning.contains(name), "{name} missing from: {warning}");
        }

        assert_eq!(loaded.config.theme, None);
        assert_eq!(
            loaded.config.lines,
            Some(7),
            "the rest of the file survives"
        );
        let settings = resolve(&Overrides::default(), &loaded.config);
        assert_eq!(settings.theme, Theme::default());

        // …and a value it *can* use produces no warning at all.
        for name in Theme::NAMES {
            let mut config = Config {
                theme: Some(name.to_owned()),
                ..Config::default()
            };
            assert!(validate(&mut config).is_empty(), "{name}");
            assert_eq!(config.theme.as_deref(), Some(name));
        }
    }

    #[test]
    fn a_zero_line_limit_warns_and_falls_back() {
        let mut config = Config {
            lines: Some(0),
            ..Config::default()
        };
        let warnings = validate(&mut config);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("lines = 0"), "{:?}", warnings[0]);
        assert_eq!(config.lines, None);
        assert_eq!(
            resolve(&Overrides::default(), &config).lines,
            PreviewOptions::default().max_lines
        );
    }

    #[test]
    fn a_good_file_loads_with_no_warnings() {
        let path = temp_file(
            "good",
            "hotkey = \"Super+P\"\ntray = false\nwrap = true\ntheme = \"light\"\n",
        );
        let loaded = load_file(&path, false);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let settings = resolve(&Overrides::default(), &loaded.config);
        assert_eq!(settings.hotkey.as_deref(), Some("Super+P"));
        assert!(!settings.tray);
        assert!(settings.wrap);
        assert_eq!(settings.theme, Theme::Light);
    }

    #[test]
    fn a_missing_file_is_silent_by_default_and_loud_when_asked_for() {
        let missing = std::env::temp_dir()
            .join("sekio-gui-does-not-exist-4b7a")
            .join(FILE);

        let quiet = load_file(&missing, false);
        assert!(quiet.warnings.is_empty(), "an absent config is normal");
        assert_eq!(quiet.config, Config::default());

        let loud = load_file(&missing, true);
        assert_eq!(loud.warnings.len(), 1, "an explicit path should warn");
        assert!(loud.warnings[0].contains(FILE), "{:?}", loud.warnings[0]);
        assert_eq!(loud.config, Config::default());
    }

    #[test]
    fn a_disabled_or_unresolvable_location_loads_nothing() {
        let path = temp_file("disabled", "lines = 7\n");

        let disabled = Location::resolve(Some(path.clone()), true, Platform::Unix, env_of(&[]));
        assert_eq!(disabled, Location::Disabled);
        assert_eq!(load(&disabled), Loaded::default());
        assert_eq!(disabled.write_target(), None);

        // Sanity: without the opt-out the same file would have been read.
        let explicit = Location::resolve(Some(path.clone()), false, Platform::Unix, env_of(&[]));
        assert_eq!(load(&explicit).config.lines, Some(7));
        assert_eq!(explicit.write_target(), Some(path.as_path()));

        let nowhere = Location::resolve(None, false, Platform::Unix, env_of(&[]));
        assert_eq!(nowhere, Location::Default(None));
        assert_eq!(load(&nowhere), Loaded::default());
        assert_eq!(
            nowhere.write_target(),
            None,
            "with nowhere to write, a tray change simply does not persist"
        );
    }

    // ---- path resolution ----

    #[test]
    fn unix_config_path_prefers_xdg_then_home() {
        let xdg = default_config_path(
            Platform::Unix,
            env_of(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")]),
        );
        assert_eq!(xdg, Some(PathBuf::from("/xdg").join("sekio").join(FILE)));

        let home = default_config_path(Platform::Unix, env_of(&[("HOME", "/home/u")]));
        assert_eq!(
            home,
            Some(
                PathBuf::from("/home/u")
                    .join(".config")
                    .join("sekio")
                    .join(FILE)
            )
        );

        // An empty or relative XDG_CONFIG_HOME is ignored, per the XDG spec.
        assert_eq!(
            default_config_path(
                Platform::Unix,
                env_of(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/u")])
            ),
            home
        );
        assert_eq!(
            default_config_path(
                Platform::Unix,
                env_of(&[("XDG_CONFIG_HOME", "relative/dir"), ("HOME", "/home/u")])
            ),
            home
        );

        // Nothing in the environment: no default path, and that is not an error.
        assert_eq!(default_config_path(Platform::Unix, env_of(&[])), None);
    }

    #[test]
    fn windows_config_path_uses_appdata() {
        let appdata = r"C:\Users\u\AppData\Roaming";
        assert_eq!(
            default_config_path(Platform::Windows, env_of(&[("APPDATA", appdata)])),
            Some(PathBuf::from(appdata).join("sekio").join(FILE))
        );
        // HOME/XDG are irrelevant on Windows.
        assert_eq!(
            default_config_path(
                Platform::Windows,
                env_of(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/xdg")])
            ),
            None
        );
        assert_eq!(
            default_config_path(Platform::Windows, env_of(&[("APPDATA", "")])),
            None
        );
    }

    #[test]
    fn the_two_platforms_disagree_about_the_same_environment() {
        // The point of the parameter: one environment, two answers, and neither
        // depends on the host this test runs on.
        let env = env_of(&[
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
            ("APPDATA", r"C:\Users\u\AppData\Roaming"),
        ]);
        let unix = default_config_path(Platform::Unix, &env).expect("unix path");
        let windows = default_config_path(Platform::Windows, &env).expect("windows path");
        assert_ne!(unix, windows);
        // Asserted as string rewrites, never via `Path::file_name`, which
        // answers for the host OS (CLAUDE.md).
        assert_eq!(
            unix.to_string_lossy().replace('\\', "/"),
            format!("/xdg/sekio/{FILE}")
        );
        assert_eq!(
            windows.to_string_lossy().replace('/', "\\"),
            format!(r"C:\Users\u\AppData\Roaming\sekio\{FILE}")
        );
    }

    #[test]
    fn absoluteness_follows_the_emulated_platform_not_the_host() {
        // These must hold identically on a Linux and a Windows runner: the
        // whole point of the `platform` parameter is that the answer does not
        // depend on where the test happens to run.
        assert!(is_absolute_on(Platform::Unix, Path::new("/xdg")));
        assert!(!is_absolute_on(Platform::Unix, Path::new("relative/xdg")));
        assert!(!is_absolute_on(Platform::Unix, Path::new(r"C:\Users")));

        assert!(is_absolute_on(Platform::Windows, Path::new(r"C:\Users")));
        assert!(is_absolute_on(Platform::Windows, Path::new("C:/Users")));
        assert!(is_absolute_on(
            Platform::Windows,
            Path::new(r"\\server\share")
        ));
        assert!(!is_absolute_on(Platform::Windows, Path::new("/xdg")));
        assert!(!is_absolute_on(Platform::Windows, Path::new("C:")));
    }

    #[test]
    fn current_platform_matches_the_build_target() {
        let expected = if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Unix
        };
        assert_eq!(Platform::current(), expected);
    }

    // ---- precedence: flag > config > default ----

    fn full_config() -> Config {
        Config {
            hotkey: Some("Alt+F1".to_owned()),
            tray: Some(false),
            lines: Some(100),
            wrap: Some(true),
            theme: Some("light".to_owned()),
            updates: Some(false),
            on_close: Some("quit".to_owned()),
        }
    }

    fn full_overrides() -> Overrides {
        Overrides {
            hotkey: Some("Super+P".to_owned()),
            no_hotkey: false,
            tray: Some(true),
            lines: Some(1),
            wrap: Some(false),
            theme: Some(Theme::Dark),
            updates: Some(true),
        }
    }

    #[test]
    fn defaults_apply_when_neither_layer_says_anything() {
        let settings = resolve(&Overrides::default(), &Config::default());
        assert_eq!(settings.hotkey.as_deref(), Some(hotkey::DEFAULT_SPEC));
        assert_eq!(settings.tray, DEFAULT_TRAY);
        assert_eq!(settings.lines, PreviewOptions::default().max_lines);
        assert!(!settings.wrap);
        assert_eq!(
            settings.theme,
            Theme::System,
            "with nothing said, sekio matches the desktop"
        );
    }

    #[test]
    fn config_beats_defaults() {
        let settings = resolve(&Overrides::default(), &full_config());
        assert_eq!(settings.hotkey.as_deref(), Some("Alt+F1"));
        assert!(!settings.tray);
        assert_eq!(settings.lines, 100);
        assert!(settings.wrap);
        assert_eq!(settings.theme, Theme::Light);
    }

    #[test]
    fn flags_beat_config() {
        let settings = resolve(&full_overrides(), &full_config());
        assert_eq!(settings.hotkey.as_deref(), Some("Super+P"));
        assert!(settings.tray, "`--tray` must beat `tray = false`");
        assert_eq!(settings.lines, 1);
        assert!(
            !settings.wrap,
            "an explicit `--no-wrap` must beat `wrap = true` in the config"
        );
        assert_eq!(
            settings.theme,
            Theme::Dark,
            "`--theme dark` must beat `theme = \"light\"` in the config"
        );
    }

    #[test]
    fn flags_beat_defaults_with_no_config_file() {
        let settings = resolve(&full_overrides(), &Config::default());
        assert_eq!(settings.hotkey.as_deref(), Some("Super+P"));
        assert!(settings.tray);
        assert_eq!(settings.lines, 1);
        assert!(!settings.wrap);
        assert_eq!(settings.theme, Theme::Dark);
    }

    /// The bug this whole `Option<T>` design exists to prevent: with clap's
    /// `default_value_t`, an untyped flag looks identical to a typed one, so the
    /// config's value gets clobbered by a default the user never asked for.
    #[test]
    fn an_untyped_flag_does_not_clobber_the_config() {
        let cli = Overrides {
            // Only `--lines` was actually typed.
            lines: Some(9),
            ..Overrides::default()
        };
        let settings = resolve(&cli, &full_config());
        assert_eq!(settings.lines, 9, "the typed flag wins");
        assert_eq!(
            settings.hotkey.as_deref(),
            Some("Alt+F1"),
            "the untyped one leaves config alone"
        );
        assert!(!settings.tray, "config's tray must survive");
        assert!(settings.wrap, "config's wrap must survive");
        assert_eq!(
            settings.theme,
            Theme::Light,
            "config's theme must survive an untyped --theme"
        );
    }

    /// The case the `Option` design exists for and the one a `default_value_t`
    /// build gets wrong in *both* directions: a flag typed with exactly the
    /// built-in default value is indistinguishable from an absent flag, so the
    /// config wins when the user's explicit instruction should.
    #[test]
    fn the_update_check_is_on_unless_something_turns_it_off() {
        assert!(Settings::default().updates, "on by default");

        // The file can turn it off...
        let off = Config {
            updates: Some(false),
            ..Config::default()
        };
        assert!(!resolve(&Overrides::default(), &off).updates);

        // ...and `--no-update-check` can, over a file that says otherwise.
        let on = Config {
            updates: Some(true),
            ..Config::default()
        };
        let flag = Overrides {
            updates: Some(false),
            ..Overrides::default()
        };
        assert!(!resolve(&flag, &on).updates);
    }

    #[test]
    fn a_flag_typed_with_the_default_value_still_wins() {
        let default_lines = PreviewOptions::default().max_lines;
        let cli = Overrides {
            lines: Some(default_lines),
            hotkey: Some(hotkey::DEFAULT_SPEC.to_owned()),
            wrap: Some(false),
            tray: Some(DEFAULT_TRAY),
            no_hotkey: false,
            theme: Some(Theme::default()),
            updates: Some(DEFAULT_UPDATES),
        };
        let settings = resolve(&cli, &full_config());
        assert_eq!(settings.lines, default_lines);
        assert_eq!(settings.hotkey.as_deref(), Some(hotkey::DEFAULT_SPEC));
        assert!(!settings.wrap);
        assert_eq!(settings.tray, DEFAULT_TRAY);
        assert_eq!(settings.theme, Theme::default());
        // Nothing *overridable* from the config survived, even though every
        // value the user typed happens to equal a default. `on_close` is the
        // one key with no command-line layer, so the file still owns it —
        // which is the point, not an exception to the rule.
        assert_eq!(settings.on_close, OnClose::Quit);
        assert_eq!(
            settings,
            Settings {
                on_close: OnClose::Quit,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn no_hotkey_beats_every_layer() {
        let cli = Overrides {
            no_hotkey: true,
            ..full_overrides()
        };
        assert_eq!(resolve(&cli, &full_config()).hotkey, None);
        assert_eq!(
            resolve(
                &Overrides {
                    no_hotkey: true,
                    ..Overrides::default()
                },
                &full_config()
            )
            .hotkey,
            None
        );
    }

    #[test]
    fn the_resolved_settings_reach_core_and_the_hotkey_layer() {
        let settings = resolve(
            &Overrides {
                lines: Some(42),
                ..Overrides::default()
            },
            &Config::default(),
        );
        assert_eq!(settings.preview_options().max_lines, 42);
        // Everything else is core's own default, untouched by this frontend.
        assert_eq!(
            settings.preview_options().max_bytes,
            PreviewOptions::default().max_bytes
        );

        let (spec, key) = settings
            .binding()
            .expect("the default spec must parse")
            .expect("a hotkey is bound by default");
        assert_eq!(spec, hotkey::DEFAULT_SPEC);
        assert_eq!(hotkey::describe(&key), hotkey::DEFAULT_SPEC);

        // `--no-hotkey` resolves to no binding, and that is not an error.
        let none = resolve(
            &Overrides {
                no_hotkey: true,
                ..Overrides::default()
            },
            &Config::default(),
        );
        assert_eq!(none.binding().expect("no spec to fail on"), None);

        // Only a spec from the command line can still be bad here, and it is an
        // error rather than a silent fallback.
        let typed = resolve(
            &Overrides {
                hotkey: Some("Ctrl+Nope".to_owned()),
                ..Overrides::default()
            },
            &Config::default(),
        );
        assert!(typed.binding().is_err());
    }

    // ---- the write path ----

    #[test]
    fn saving_a_hotkey_round_trips() {
        let path = scratch("round-trip").join(FILE);
        save_hotkey(&path, "Super+P").expect("save into a fresh file");

        let loaded = load_file(&path, true);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.hotkey.as_deref(), Some("Super+P"));
        assert_eq!(
            resolve(&Overrides::default(), &loaded.config)
                .hotkey
                .as_deref(),
            Some("Super+P")
        );

        // Saving again replaces the value rather than appending a second key,
        // which would not even be valid TOML.
        save_hotkey(&path, "Alt+F1").expect("save over an existing value");
        let text = read(&path);
        assert_eq!(text.matches("hotkey").count(), 1, "{text}");
        assert_eq!(
            load_file(&path, true).config.hotkey.as_deref(),
            Some("Alt+F1")
        );
    }

    #[test]
    fn saving_creates_the_directory_and_leaves_no_temp_file() {
        let dir = scratch("mkdir").join("nested").join("deeper");
        let path = dir.join(FILE);
        save_hotkey(&path, "Ctrl+Alt+P").expect("create the directory too");
        assert_eq!(
            load_file(&path, true).config.hotkey.as_deref(),
            Some("Ctrl+Alt+P")
        );

        let leftovers: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn saving_keeps_comments_and_unrelated_keys() {
        let original = "\
## sekio-gui configuration.

## The global hotkey.
hotkey = \"Ctrl+Shift+Space\"  # changed from Super+P

## Max lines of text per preview.
lines = 250
tray = false
# wrap = false
";
        let path = temp_file("comments", original);
        save_hotkey(&path, "Alt+F1").expect("save");
        let text = read(&path);

        assert!(text.contains("## sekio-gui configuration."), "{text}");
        assert!(text.contains("## The global hotkey."), "{text}");
        assert!(text.contains("## Max lines of text per preview."), "{text}");
        assert!(
            text.contains("# changed from Super+P"),
            "the trailing comment must survive: {text}"
        );
        assert!(
            text.contains("# wrap = false"),
            "a commented-out key must stay commented: {text}"
        );
        assert!(text.contains("hotkey = \"Alt+F1\""), "{text}");
        assert!(!text.contains("Ctrl+Shift+Space"), "{text}");

        // The unrelated settings are unchanged, not just present.
        let loaded = load_file(&path, true);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.lines, Some(250));
        assert_eq!(loaded.config.tray, Some(false));
        assert_eq!(loaded.config.wrap, None);
        assert_eq!(loaded.config.hotkey.as_deref(), Some("Alt+F1"));
    }

    #[test]
    fn saving_into_the_shipped_example_keeps_it_documented() {
        // The realistic first write: the user copied the example, so every key
        // is commented out. The tray's change must become live without
        // uncommenting — or destroying — the documentation.
        let path = temp_file("example", include_str!("../gui.example.toml"));
        save_hotkey(&path, "Super+Space").expect("save");
        let text = read(&path);

        assert!(
            text.contains(&format!("# hotkey = \"{}\"", hotkey::DEFAULT_SPEC)),
            "the documented default stays commented: {text}"
        );
        assert!(text.contains("hotkey = \"Super+Space\""), "{text}");
        let loaded = load_file(&path, true);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.hotkey.as_deref(), Some("Super+Space"));
        assert_eq!(loaded.config.lines, None, "the rest stays commented out");
    }

    #[test]
    fn saving_refuses_an_invalid_hotkey_without_touching_the_file() {
        let path = temp_file("refuse", "hotkey = \"Super+P\"\n");
        let err = save_hotkey(&path, "Ctrl+Nope").expect_err("an unparseable spec");
        match &err {
            SaveError::Hotkey { spec, .. } => assert_eq!(spec, "Ctrl+Nope"),
            other => panic!("expected a hotkey error, got {other:?}"),
        }
        assert!(err.to_string().contains("Ctrl+Nope"), "{err}");
        assert_eq!(
            read(&path),
            "hotkey = \"Super+P\"\n",
            "a refused save must not have written anything"
        );
    }

    #[test]
    fn saving_reports_an_unwritable_path_instead_of_panicking() {
        // A directory where the file should be: every filesystem call fails,
        // and the error names the path rather than unwinding.
        let path = scratch("unwritable").join(FILE);
        std::fs::create_dir_all(&path).expect("a directory in the file's place");
        let err = save_hotkey(&path, "Super+P").expect_err("cannot write over a directory");
        assert!(matches!(err, SaveError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains(FILE), "{err}");
    }

    /// What "don't ask again" writes. The point of the round trip through
    /// `load_file` is that the value the dialog wrote is a value the *reader*
    /// accepts without a warning — the two spellings cannot drift apart.
    #[test]
    fn saving_an_answer_to_the_close_dialog_round_trips() {
        let path = temp_file(
            "on-close-roundtrip",
            "# mine\nhotkey = \"Super+P\"  # keep me\n",
        );

        save_on_close(&path, OnClose::Tray).expect("an answer should save");
        let loaded = load_file(&path, true);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(
            resolve(&Overrides::default(), &loaded.config).on_close,
            OnClose::Tray
        );
        let text = read(&path);
        assert!(text.contains("# mine"), "the comment survives: {text}");
        assert!(text.contains("# keep me"), "{text}");

        // And changing the answer replaces it rather than appending a second.
        save_on_close(&path, OnClose::Quit).expect("a second answer should save");
        let text = read(&path);
        assert_eq!(text.matches("on_close").count(), 1, "{text}");
        assert_eq!(
            resolve(&Overrides::default(), &load_file(&path, true).config).on_close,
            OnClose::Quit
        );
    }

    // ---- the comment-preserving rewrite, as a pure function ----

    #[test]
    fn saving_a_theme_round_trips_and_leaves_the_rest_of_the_file_alone() {
        let dir = std::env::temp_dir().join("sekio-config-theme-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("gui.toml");

        // Into a file that does not exist yet, directory and all.
        save_theme(&path, Theme::Light).expect("a theme should save");
        let loaded = load_file(&path, true);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.theme.as_deref(), Some("light"));

        // Over an existing value, with a neighbour and a comment to preserve.
        std::fs::write(
            &path,
            "# mine\nhotkey = \"Super+P\"  # keep me\ntheme = \"light\"\nlines = 42\n",
        )
        .unwrap();
        save_theme(&path, Theme::Dark).expect("a theme should save");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nhotkey = \"Super+P\"  # keep me\ntheme = \"dark\"\nlines = 42\n",
            "only the theme value may change"
        );

        // And the two writers do not tread on each other.
        save_hotkey(&path, "Alt+F1").expect("a hotkey should save");
        let loaded = load_file(&path, true);
        assert_eq!(loaded.config.hotkey.as_deref(), Some("Alt+F1"));
        assert_eq!(loaded.config.theme.as_deref(), Some("dark"));
        assert_eq!(loaded.config.lines, Some(42));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_that_merely_starts_with_the_one_we_want_is_not_it() {
        // `theme_name` is not `theme`, so the setting is appended rather than
        // the wrong line being rewritten.
        assert_eq!(
            with_setting("theme_name = \"x\"\n", "theme", "dark"),
            "theme_name = \"x\"\ntheme = \"dark\"\n"
        );
    }

    #[test]
    fn with_hotkey_appends_when_the_key_is_absent() {
        assert_eq!(with_hotkey("", "Super+P"), "hotkey = \"Super+P\"\n");
        assert_eq!(
            with_hotkey("lines = 7\n", "Super+P"),
            "lines = 7\nhotkey = \"Super+P\"\n"
        );
        // A file whose last line has no newline must not get the assignment
        // glued onto it.
        assert_eq!(
            with_hotkey("lines = 7", "Super+P"),
            "lines = 7\nhotkey = \"Super+P\"\n"
        );
        // Only commented-out mentions: the live key is added, the comment kept.
        assert_eq!(
            with_hotkey("# hotkey = \"Alt+F1\"\n", "Super+P"),
            "# hotkey = \"Alt+F1\"\nhotkey = \"Super+P\"\n"
        );
        // A key that merely starts with "hotkey" is not it.
        assert_eq!(
            with_hotkey("hotkeys = 2\n", "Super+P"),
            "hotkeys = 2\nhotkey = \"Super+P\"\n"
        );
    }

    #[test]
    fn with_hotkey_edits_in_place_and_keeps_the_line_around_it() {
        assert_eq!(
            with_hotkey("hotkey = \"Alt+F1\"\n", "Super+P"),
            "hotkey = \"Super+P\"\n"
        );
        // Indentation, spacing style and a trailing comment all survive.
        assert_eq!(
            with_hotkey("  hotkey='Alt+F1'   # why\nlines = 7\n", "Super+P"),
            "  hotkey=\"Super+P\"   # why\nlines = 7\n"
        );
        // Lines before and after are copied verbatim.
        assert_eq!(
            with_hotkey(
                "## doc\nlines = 7\nhotkey = \"Alt+F1\"\nwrap = true\n",
                "Super+P"
            ),
            "## doc\nlines = 7\nhotkey = \"Super+P\"\nwrap = true\n"
        );
    }

    #[test]
    fn with_hotkey_never_writes_a_bare_key_into_a_table() {
        // No table exists in today's schema, but a file that grows one must not
        // have `hotkey` land inside it — where it would mean `table.hotkey`.
        let text = "lines = 7\n\n[future]\nx = 1\n";
        let out = with_hotkey(text, "Super+P");
        assert_eq!(out, "lines = 7\n\nhotkey = \"Super+P\"\n[future]\nx = 1\n");
        // And a `hotkey` *inside* a table is a different key, so it is left
        // alone and a top-level one is added.
        let text = "[future]\nhotkey = \"Alt+F1\"\n";
        assert_eq!(
            with_hotkey(text, "Super+P"),
            "hotkey = \"Super+P\"\n[future]\nhotkey = \"Alt+F1\"\n"
        );
    }

    #[test]
    fn with_hotkey_keeps_windows_line_endings() {
        assert_eq!(
            with_hotkey("lines = 7\r\nhotkey = \"Alt+F1\"\r\n", "Super+P"),
            "lines = 7\r\nhotkey = \"Super+P\"\r\n"
        );
        assert_eq!(
            with_hotkey("lines = 7\r\n", "Super+P"),
            "lines = 7\r\nhotkey = \"Super+P\"\r\n"
        );
    }

    #[test]
    fn a_spec_with_toml_metacharacters_round_trips() {
        // `Ctrl+\` and `Ctrl+'` are real combinations this crate's parser
        // accepts, and a backslash written raw would produce a file that either
        // fails to parse or parses as a different key.
        for spec in ["Ctrl+\\", "Ctrl+'", "Ctrl+Alt+\\"] {
            assert!(
                hotkey::parse(spec).is_ok(),
                "{spec} should be a valid spec for this test to mean anything"
            );
            let text = with_hotkey("", spec);
            let config = parse(&text).unwrap_or_else(|e| panic!("{spec}: {text:?}: {e}"));
            assert_eq!(config.hotkey.as_deref(), Some(spec), "{text:?}");
        }
        assert_eq!(toml_string("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(toml_string("a\tb"), r#""a\tb""#);
        assert_eq!(toml_string("\u{1}"), r#""\u0001""#);
    }

    #[test]
    fn a_value_ends_where_the_comment_begins() {
        assert_eq!(value_len("\"Alt+F1\"  # note"), 8);
        assert_eq!(value_len("'Alt+F1'  # note"), 8);
        assert_eq!(value_len("\"a\\\"b\" rest"), 6);
        assert_eq!(value_len("true # note"), 4);
        // Unterminated strings do not run off the end.
        assert_eq!(value_len("\"unterminated"), 13);
        assert_eq!(value_len("'unterminated"), 13);
    }
}
