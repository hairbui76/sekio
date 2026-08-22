//! The config file: `$XDG_CONFIG_HOME/sekio/config.toml` (or
//! `%APPDATA%\sekio\config.toml` on Windows), plus the precedence rules that
//! merge it with the command line.
//!
//! Three rules the rest of the crate depends on:
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
//!    file the user named with `--config` is worth a warning.
//! 3. **Defaults live in core.** The numeric defaults are read from
//!    `PreviewOptions::default()` and the syntax theme from
//!    `Previewer::DEFAULT_THEME`, so this file never drifts from them.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use sekio_core::{PreviewOptions, Previewer};
use serde::{Deserialize, Deserializer};

/// The whole config file. Every field is optional so a partial config works, and
/// unknown keys are rejected rather than silently ignored — a typo'd key is
/// almost always a mistake the user wants to hear about.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Max lines of text per preview.
    pub lines: Option<usize>,
    /// Max bytes read from any one file.
    pub max_bytes: Option<usize>,
    /// Max entries listed for a directory or archive.
    pub max_entries: Option<usize>,
    /// Longest edge an image is downscaled to.
    pub image_max_dim: Option<u32>,
    /// Always use halfblocks instead of querying the terminal for graphics.
    pub halfblocks: Option<bool>,
    #[serde(default)]
    pub theme: ThemeConfig,
}

/// The `[theme]` table. `syntax` picks the syntect theme core highlights with;
/// the rest are the TUI's own chrome colors.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeConfig {
    /// Syntect theme name, e.g. `base16-ocean.dark`.
    pub syntax: Option<String>,
    /// Selected row in the file list.
    pub accent: Option<ThemeColor>,
    /// Pane borders.
    pub border: Option<ThemeColor>,
    /// Directory entries.
    pub directory: Option<ThemeColor>,
    /// Metadata keys and the hexdump's ASCII gutter.
    pub key: Option<ThemeColor>,
    /// Titles, status bar, hexdump offsets, file sizes.
    pub dim: Option<ThemeColor>,
    /// "truncated" markers.
    pub warning: Option<ThemeColor>,
    /// Error text.
    pub error: Option<ThemeColor>,
}

/// A color written as `#rrggbb`, a 0–255 palette index, or an ANSI name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColor(pub Color);

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_color(&raw)
            .map(ThemeColor)
            .map_err(serde::de::Error::custom)
    }
}

/// Resolved chrome colors. Defaults reproduce the hardcoded palette the TUI
/// shipped with, so an absent `[theme]` table changes nothing on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub border: Color,
    pub directory: Color,
    pub key: Color,
    pub dim: Color,
    pub warning: Color,
    pub error: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // `Reset` + the REVERSED modifier is exactly the plain reversed
            // highlight the list used before themes existed.
            accent: Color::Reset,
            border: Color::Reset,
            directory: Color::Rgb(138, 190, 183),
            key: Color::Rgb(143, 161, 179),
            dim: Color::Rgb(101, 115, 126),
            warning: Color::Yellow,
            error: Color::Red,
        }
    }
}

impl Theme {
    fn from_config(cfg: &ThemeConfig) -> Self {
        let base = Self::default();
        let pick = |set: Option<ThemeColor>, fallback: Color| set.map(|c| c.0).unwrap_or(fallback);
        Self {
            accent: pick(cfg.accent, base.accent),
            border: pick(cfg.border, base.border),
            directory: pick(cfg.directory, base.directory),
            key: pick(cfg.key, base.key),
            dim: pick(cfg.dim, base.dim),
            warning: pick(cfg.warning, base.warning),
            error: pick(cfg.error, base.error),
        }
    }
}

/// What the user actually typed on the command line. `None` means "not typed",
/// which is the whole point — see the module docs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub lines: Option<usize>,
    pub max_bytes: Option<usize>,
    pub max_entries: Option<usize>,
    pub image_max_dim: Option<u32>,
    pub halfblocks: Option<bool>,
}

/// Everything the app needs, with all three layers already merged.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub lines: usize,
    pub max_bytes: usize,
    pub max_entries: usize,
    pub image_max_dim: u32,
    pub halfblocks: bool,
    pub syntax_theme: String,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        resolve(&Overrides::default(), &Config::default())
    }
}

impl Settings {
    pub fn preview_options(&self) -> PreviewOptions {
        PreviewOptions {
            max_bytes: self.max_bytes,
            max_lines: self.lines,
            image_max_dim: self.image_max_dim,
            max_entries: self.max_entries,
            // Not a setting: the preview pane's width is whatever the terminal
            // is right now, so it rides on each `Request` instead (see
            // `worker::Request::text_width`).
            text_width: None,
        }
    }
}

/// Merge the three layers: command line, then config file, then core's defaults.
///
/// Pure on purpose — the precedence is the easiest thing in this feature to get
/// subtly wrong, so it is testable without a filesystem or an `ArgMatches`.
pub fn resolve(cli: &Overrides, cfg: &Config) -> Settings {
    let defaults = PreviewOptions::default();
    Settings {
        lines: cli.lines.or(cfg.lines).unwrap_or(defaults.max_lines),
        max_bytes: cli
            .max_bytes
            .or(cfg.max_bytes)
            .unwrap_or(defaults.max_bytes),
        max_entries: cli
            .max_entries
            .or(cfg.max_entries)
            .unwrap_or(defaults.max_entries),
        image_max_dim: cli
            .image_max_dim
            .or(cfg.image_max_dim)
            .unwrap_or(defaults.image_max_dim),
        halfblocks: cli.halfblocks.or(cfg.halfblocks).unwrap_or(false),
        syntax_theme: cfg
            .theme
            .syntax
            .clone()
            .unwrap_or_else(|| Previewer::DEFAULT_THEME.to_owned()),
        theme: Theme::from_config(&cfg.theme),
    }
}

/// A syntect theme name core doesn't know about is a warning, not a failure:
/// returns the message to print, or `None` when the name is fine.
pub fn check_syntax_theme(name: &str) -> Option<String> {
    let names = Previewer::theme_names();
    if names.iter().any(|known| known == name) {
        return None;
    }
    Some(format!(
        "warning: unknown theme.syntax {name:?}; using {default:?}\n\
         warning: available themes: {list}",
        default = Previewer::DEFAULT_THEME,
        list = names.join(", "),
    ))
}

// ---- where the file lives ------------------------------------------------

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
/// * Unix: `$XDG_CONFIG_HOME/sekio/config.toml`, else `$HOME/.config/sekio/config.toml`.
///   A relative `XDG_CONFIG_HOME` is ignored, as the XDG spec requires.
/// * Windows: `%APPDATA%\sekio\config.toml`.
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
    Some(dir.join("sekio").join("config.toml"))
}

/// Absoluteness judged by the *emulated* platform rather than the host.
///
/// `Path::is_absolute` answers for whichever OS the binary is running on, so
/// using it here would make `default_config_path(Platform::Unix, ..)` behave
/// like Windows whenever the tests run on Windows — `/xdg` is not absolute
/// under Windows rules, so an absolute XDG dir would be silently discarded.
/// That would defeat the point of taking `platform` as a parameter.
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
    /// `--no-config`: load nothing at all.
    Disabled,
    /// `--config PATH`: the user named it, so a missing file is worth saying.
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
}

/// A config plus whatever went wrong getting it. Never an `Err`: a broken config
/// degrades to defaults, it does not stop the browser from starting.
#[derive(Debug, Clone, Default, PartialEq)]
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
            Ok(config) => Loaded {
                config,
                warnings: Vec::new(),
            },
            Err(err) => degraded(path, &err.to_string()),
        },
        Err(err) if err.kind() == ErrorKind::NotFound && !required => Loaded::default(),
        Err(err) => degraded(path, &err.to_string()),
    }
}

pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
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

// ---- colors --------------------------------------------------------------

/// Accepts `#rrggbb`, `#rgb`, a bare `rrggbb`, a 0–255 palette index, or an ANSI
/// color name (`cyan`, `light-blue`, `reset`). Names ignore case, `-` and `_`.
pub fn parse_color(raw: &str) -> Result<Color, String> {
    let text = raw.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex).ok_or_else(|| bad_color(raw));
    }
    if let Ok(index) = text.parse::<u8>() {
        return Ok(Color::Indexed(index));
    }

    let name: String = text
        .chars()
        .filter(|c| *c != '-' && *c != '_' && *c != ' ')
        .flat_map(char::to_lowercase)
        .collect();
    let color = match name.as_str() {
        "reset" | "default" | "none" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        // Last chance: `rrggbb` without the `#`, which people write constantly.
        // Only the 6-digit form — a bare `abc` would otherwise collide with the
        // palette-index spelling of a three-digit number.
        _ if text.len() == 6 => return parse_hex(text).ok_or_else(|| bad_color(raw)),
        _ => return Err(bad_color(raw)),
    };
    Ok(color)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let bytes = hex.as_bytes();
    let nibble = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    match bytes.len() {
        // `#abc` is the CSS shorthand for `#aabbcc`.
        3 => {
            let (r, g, b) = (nibble(bytes[0])?, nibble(bytes[1])?, nibble(bytes[2])?);
            Some(Color::Rgb(r * 17, g * 17, b * 17))
        }
        6 => {
            let pair = |i: usize| Some(nibble(bytes[i])? * 16 + nibble(bytes[i + 1])?);
            Some(Color::Rgb(pair(0)?, pair(2)?, pair(4)?))
        }
        _ => None,
    }
}

fn bad_color(raw: &str) -> String {
    format!(
        "invalid color {raw:?}: expected `#rrggbb`, a 0-255 palette index, \
         or a name such as `cyan`, `light-blue` or `reset`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// An env lookup backed by a fixed table — never the real environment, so
    /// these tests can't be perturbed by the machine they run on.
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

    /// A unique path under the system temp dir; no `tempfile` dependency for
    /// four tests' worth of I/O.
    fn temp_file(name: &str, contents: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sekio-tui-cfg-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write temp config");
        path
    }

    // ---- parsing ----

    #[test]
    fn a_full_config_parses() {
        let config = parse(
            r##"
            lines = 250
            max_bytes = 65536
            max_entries = 42
            image_max_dim = 512
            halfblocks = true

            [theme]
            syntax = "Solarized (dark)"
            accent = "#ff0000"
            border = "cyan"
            directory = "#8abeb7"
            key = "12"
            dim = "dark-gray"
            warning = "yellow"
            error = "#f00"
            "##,
        )
        .expect("full config should parse");

        assert_eq!(config.lines, Some(250));
        assert_eq!(config.max_bytes, Some(65536));
        assert_eq!(config.max_entries, Some(42));
        assert_eq!(config.image_max_dim, Some(512));
        assert_eq!(config.halfblocks, Some(true));
        assert_eq!(config.theme.syntax.as_deref(), Some("Solarized (dark)"));
        assert_eq!(config.theme.accent, Some(ThemeColor(Color::Rgb(255, 0, 0))));
        assert_eq!(config.theme.border, Some(ThemeColor(Color::Cyan)));
        assert_eq!(config.theme.key, Some(ThemeColor(Color::Indexed(12))));
        assert_eq!(config.theme.dim, Some(ThemeColor(Color::DarkGray)));
        assert_eq!(config.theme.error, Some(ThemeColor(Color::Rgb(255, 0, 0))));
    }

    /// The shipped example must stay valid — including its commented-out lines,
    /// which are what users uncomment. Every key is exercised by uncommenting
    /// the whole file and parsing it.
    #[test]
    fn the_example_config_is_valid_and_documents_every_key() {
        let example = include_str!("../config.example.toml");
        parse(example).expect("the example config must parse as shipped");

        let uncommented: String = example
            .lines()
            .map(|line| line.strip_prefix("# ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let config = parse(&uncommented).expect("the example's defaults must parse when enabled");

        assert_eq!(config.lines, Some(PreviewOptions::default().max_lines));
        assert_eq!(config.max_bytes, Some(PreviewOptions::default().max_bytes));
        assert_eq!(
            config.max_entries,
            Some(PreviewOptions::default().max_entries)
        );
        assert_eq!(
            config.image_max_dim,
            Some(PreviewOptions::default().image_max_dim)
        );
        assert_eq!(config.halfblocks, Some(false));
        // Uncommented, the example is exactly the built-in behaviour.
        assert_eq!(resolve(&Overrides::default(), &config), Settings::default());
    }

    #[test]
    fn a_partial_config_leaves_the_rest_unset() {
        let config = parse("lines = 10\n").expect("partial config should parse");
        assert_eq!(config.lines, Some(10));
        assert_eq!(config.max_bytes, None);
        assert_eq!(config.theme, ThemeConfig::default());

        let only_theme = parse("[theme]\naccent = \"red\"\n").expect("theme-only config");
        assert_eq!(only_theme.lines, None);
        assert_eq!(only_theme.theme.accent, Some(ThemeColor(Color::Red)));
        assert_eq!(only_theme.theme.dim, None);

        // …and an empty file is a valid config that changes nothing.
        assert_eq!(parse("").expect("empty config"), Config::default());
    }

    #[test]
    fn an_unknown_key_is_rejected_with_a_useful_message() {
        let err = parse("linez = 10\n").expect_err("a typo'd key must not be ignored");
        let msg = err.to_string();
        assert!(msg.contains("linez"), "{msg}");
        assert!(msg.contains("unknown field"), "{msg}");
        // The valid keys are listed, so the user can see the fix.
        assert!(msg.contains("lines"), "{msg}");

        let err = parse("[theme]\naccnt = \"red\"\n").expect_err("typo inside [theme]");
        assert!(err.to_string().contains("accnt"), "{err}");
    }

    #[test]
    fn an_invalid_color_names_the_value() {
        let err = parse("[theme]\naccent = \"burnt sienna\"\n").expect_err("bad color");
        let msg = err.to_string();
        assert!(msg.contains("burnt sienna"), "{msg}");
        assert!(msg.contains("#rrggbb"), "{msg}");
    }

    #[test]
    fn color_spellings() {
        assert_eq!(parse_color("#8ABEB7"), Ok(Color::Rgb(138, 190, 183)));
        assert_eq!(parse_color("8abeb7"), Ok(Color::Rgb(138, 190, 183)));
        assert_eq!(parse_color("#abc"), Ok(Color::Rgb(170, 187, 204)));
        assert_eq!(parse_color("  cyan "), Ok(Color::Cyan));
        assert_eq!(parse_color("Light_Blue"), Ok(Color::LightBlue));
        assert_eq!(parse_color("0"), Ok(Color::Indexed(0)));
        assert_eq!(parse_color("255"), Ok(Color::Indexed(255)));
        assert_eq!(parse_color("reset"), Ok(Color::Reset));
        assert!(parse_color("#12345").is_err());
        assert!(parse_color("#gggggg").is_err());
        assert!(parse_color("256").is_err());
        assert!(parse_color("").is_err());
    }

    // ---- degrading, never failing ----

    #[test]
    fn malformed_toml_degrades_to_defaults_with_a_warning() {
        let path = temp_file("config.toml", "lines = = 3\n[theme\n");
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
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_key_in_a_real_file_warns_but_starts() {
        let path = temp_file("config.toml", "lines = 7\nlinez = 9\n");
        let loaded = load_file(&path, false);
        assert_eq!(loaded.config, Config::default());
        assert!(
            loaded.warnings[0].contains("linez"),
            "{:?}",
            loaded.warnings
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_good_file_loads_with_no_warnings() {
        let path = temp_file("config.toml", "lines = 7\n\n[theme]\ndim = \"red\"\n");
        let loaded = load_file(&path, false);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.lines, Some(7));
        assert_eq!(
            resolve(&Overrides::default(), &loaded.config).theme.dim,
            Color::Red
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_silent_by_default_and_loud_when_asked_for() {
        let missing = std::env::temp_dir().join("sekio-tui-does-not-exist-9c1f/config.toml");

        let quiet = load_file(&missing, false);
        assert!(quiet.warnings.is_empty(), "an absent config is normal");
        assert_eq!(quiet.config, Config::default());

        let loud = load_file(&missing, true);
        assert_eq!(loud.warnings.len(), 1, "--config <missing> should warn");
        assert!(loud.warnings[0].contains("config.toml"));
        assert_eq!(loud.config, Config::default());
    }

    #[test]
    fn no_config_skips_the_file_entirely() {
        let path = temp_file("config.toml", "lines = 7\n");
        let location = Location::resolve(Some(path.clone()), true, Platform::Unix, env_of(&[]));
        assert_eq!(location, Location::Disabled);
        assert_eq!(load(&location), Loaded::default());
        // Sanity: without --no-config the same file would have been read.
        let location = Location::resolve(Some(path.clone()), false, Platform::Unix, env_of(&[]));
        assert_eq!(load(&location).config.lines, Some(7));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unresolvable_default_location_loads_nothing() {
        let location = Location::resolve(None, false, Platform::Unix, env_of(&[]));
        assert_eq!(location, Location::Default(None));
        assert_eq!(load(&location), Loaded::default());
    }

    // ---- path resolution ----

    #[test]
    fn unix_config_path_prefers_xdg_then_home() {
        let xdg = default_config_path(
            Platform::Unix,
            env_of(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/home/u")]),
        );
        assert_eq!(
            xdg,
            Some(PathBuf::from("/xdg").join("sekio").join("config.toml"))
        );

        let home = default_config_path(Platform::Unix, env_of(&[("HOME", "/home/u")]));
        assert_eq!(
            home,
            Some(
                PathBuf::from("/home/u")
                    .join(".config")
                    .join("sekio")
                    .join("config.toml")
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
            Some(PathBuf::from(appdata).join("sekio").join("config.toml"))
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
            lines: Some(100),
            max_bytes: Some(200),
            max_entries: Some(300),
            image_max_dim: Some(400),
            halfblocks: Some(true),
            theme: ThemeConfig::default(),
        }
    }

    fn full_overrides() -> Overrides {
        Overrides {
            lines: Some(1),
            max_bytes: Some(2),
            max_entries: Some(3),
            image_max_dim: Some(4),
            halfblocks: Some(false),
        }
    }

    #[test]
    fn defaults_apply_when_neither_layer_says_anything() {
        let settings = resolve(&Overrides::default(), &Config::default());
        let core = PreviewOptions::default();
        assert_eq!(settings.lines, core.max_lines);
        assert_eq!(settings.max_bytes, core.max_bytes);
        assert_eq!(settings.max_entries, core.max_entries);
        assert_eq!(settings.image_max_dim, core.image_max_dim);
        assert!(!settings.halfblocks);
        assert_eq!(settings.syntax_theme, Previewer::DEFAULT_THEME);
        assert_eq!(settings.theme, Theme::default());
    }

    #[test]
    fn config_beats_defaults() {
        let settings = resolve(&Overrides::default(), &full_config());
        assert_eq!(settings.lines, 100);
        assert_eq!(settings.max_bytes, 200);
        assert_eq!(settings.max_entries, 300);
        assert_eq!(settings.image_max_dim, 400);
        assert!(settings.halfblocks);
    }

    #[test]
    fn flags_beat_config() {
        let settings = resolve(&full_overrides(), &full_config());
        assert_eq!(settings.lines, 1);
        assert_eq!(settings.max_bytes, 2);
        assert_eq!(settings.max_entries, 3);
        assert_eq!(settings.image_max_dim, 4);
        assert!(
            !settings.halfblocks,
            "`--halfblocks false` must beat `halfblocks = true` in the config"
        );
    }

    #[test]
    fn flags_beat_defaults_with_no_config_file() {
        let settings = resolve(&full_overrides(), &Config::default());
        assert_eq!(settings.lines, 1);
        assert_eq!(settings.max_bytes, 2);
        assert_eq!(settings.max_entries, 3);
        assert_eq!(settings.image_max_dim, 4);
        assert!(!settings.halfblocks);
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
            settings.max_bytes, 200,
            "the untyped one leaves config alone"
        );
        assert_eq!(settings.max_entries, 300);
        assert_eq!(settings.image_max_dim, 400);
        assert!(settings.halfblocks, "config's halfblocks must survive");

        // And a flag typed with the *same value as the default* still wins,
        // which is indistinguishable from "not typed" under `default_value_t`.
        let cli = Overrides {
            lines: Some(PreviewOptions::default().max_lines),
            ..Overrides::default()
        };
        assert_eq!(
            resolve(&cli, &full_config()).lines,
            PreviewOptions::default().max_lines
        );
    }

    #[test]
    fn theme_settings_flow_through_resolve() {
        let config = Config {
            theme: ThemeConfig {
                syntax: Some("InspiredGitHub".to_owned()),
                accent: Some(ThemeColor(Color::Rgb(1, 2, 3))),
                dim: Some(ThemeColor(Color::Indexed(8))),
                ..ThemeConfig::default()
            },
            ..Config::default()
        };
        let settings = resolve(&Overrides::default(), &config);
        assert_eq!(settings.syntax_theme, "InspiredGitHub");
        assert_eq!(settings.theme.accent, Color::Rgb(1, 2, 3));
        assert_eq!(settings.theme.dim, Color::Indexed(8));
        // Unset colors keep the shipped palette.
        assert_eq!(settings.theme.directory, Theme::default().directory);
    }

    #[test]
    fn syntax_theme_names_are_validated_against_core() {
        assert_eq!(check_syntax_theme(Previewer::DEFAULT_THEME), None);
        let warning = check_syntax_theme("no-such-theme").expect("unknown theme must warn");
        assert!(warning.contains("no-such-theme"), "{warning}");
        assert!(
            warning.contains(Previewer::DEFAULT_THEME),
            "the warning should name the fallback: {warning}"
        );
    }
}
