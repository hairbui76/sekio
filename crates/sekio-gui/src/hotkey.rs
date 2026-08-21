//! The global hotkey that summons the daemon (ROADMAP Phase 3).
//!
//! `sekio-gui --daemon` is only worth staying resident for if it can be
//! summoned without going through a file manager, so it grabs one system-wide
//! key combination. Pressing it asks the platform [`Source`] what file is
//! selected right now and previews it, through exactly the same code path a
//! socket handoff takes.
//!
//! Three rules shape this module:
//!
//! * **A hotkey is a nicety, never a requirement.** Registration fails on a
//!   headless box, on Wayland (this crate grabs through X11 only), and when
//!   another application already owns the combination. All of those produce a
//!   [`Status::Unavailable`] *value* carrying a warning — never an error that
//!   aborts the daemon. The socket keeps working, which is what `sekio-gui
//!   <path>` actually depends on.
//! * **Nothing here runs on the UI thread.** Reading the selection can take
//!   ~200 ms (an IPC round trip to a file manager), so it happens on the hotkey
//!   thread and only the resolved path crosses the channel.
//! * **No platform-specific code.** `global-hotkey` hides Win32
//!   `RegisterHotKey` and X11 `XGrabKey` behind one API; the only concession is
//!   the `cfg!(unix)` *expression* in [`display_server`], which compiles
//!   identically on every target.
//!
//! Windows caveat, deliberately not `#[cfg]`-ed away: Win32 delivers
//! `WM_HOTKEY` to the thread that created the manager, so the pump thread would
//! also need a win32 message loop there. That lands with Windows daemon
//! support — there is no daemon on Windows yet, so today the Windows path of
//! this module is exercised by `--doctor`, which reports registration
//! synchronously and needs no pump.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

pub use global_hotkey::hotkey::HotKey;
use global_hotkey::hotkey::{Code, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::selection::{self, Source};

/// The combination bound when `--hotkey` is not given.
///
/// Ctrl+Shift+Space is chosen because it is unclaimed on stock GNOME/KDE and
/// Windows, needs no function row, and — the part that matters — carries two
/// modifiers. A bare `Space` (or any unmodified typing key) *can* be grabbed,
/// and doing so steals it from every other application on the desktop: the
/// spacebar stops working in the browser, the editor and the terminal. The
/// file-manager spacebar flow is served by the socket handoff instead, which
/// costs nothing globally.
pub const DEFAULT_SPEC: &str = "Ctrl+Shift+Space";

/// How long we will wait for the platform to answer "registered or not".
/// X11 registration is a round trip to the server; if that server never
/// answers, startup must continue without a hotkey rather than hang.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Why a `--hotkey` spec could not be turned into a key combination.
///
/// Every variant names the offending token and the `Display` impl ends with a
/// working example: an unparseable spec is a startup error the user can act
/// on, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Nothing but whitespace.
    Empty,
    /// A `+` with nothing on one side of it (`"Ctrl++Space"`, `"Ctrl+"`).
    EmptyToken,
    /// A token before the final one that is not a modifier.
    UnknownModifier(String),
    /// The final token is not a key we can name.
    UnknownKey(String),
    /// Modifiers only, with no key to press (`"Ctrl+Shift"`).
    NoKey,
}

/// The modifier names accepted, with their aliases, for error messages.
const MODIFIER_HELP: &str =
    "modifiers are Ctrl (Control), Alt (Option), Shift and Super (Meta, Win, Cmd)";

const EXAMPLE: &str = r#"for example "Ctrl+Shift+Space", "Super+P" or "Alt+F1""#;

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the hotkey is empty; {EXAMPLE}"),
            Self::EmptyToken => write!(
                f,
                "the hotkey has an empty part between '+' separators; {EXAMPLE}"
            ),
            Self::UnknownModifier(token) => write!(
                f,
                "{token:?} is not a modifier ({MODIFIER_HELP}), and only the \
                 last part may be a key; {EXAMPLE}"
            ),
            Self::UnknownKey(token) => write!(
                f,
                "{token:?} is not a key sekio knows; keys are letters, digits, \
                 F1-F24, Space, Enter, Tab, Escape, the arrows, Home/End/PageUp/\
                 PageDown, Insert/Delete, the numpad keys and punctuation; {EXAMPLE}"
            ),
            Self::NoKey => write!(
                f,
                "the hotkey is modifiers only and has no key to press; {EXAMPLE}"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a spec like `"Ctrl+Shift+Space"`, `"super+p"` or `"Alt+F1"`.
///
/// Case-insensitive, `+`-separated, whitespace around each part ignored.
/// Modifiers come first and exactly one key comes last, which is also what the
/// platforms underneath accept.
pub fn parse(spec: &str) -> Result<HotKey, ParseError> {
    if spec.trim().is_empty() {
        return Err(ParseError::Empty);
    }

    let tokens: Vec<&str> = spec.split('+').map(str::trim).collect();
    if tokens.iter().any(|t| t.is_empty()) {
        return Err(ParseError::EmptyToken);
    }
    // `split` always yields at least one element, and none of them are empty.
    let (last, leading) = match tokens.split_last() {
        Some(parts) => parts,
        None => return Err(ParseError::Empty),
    };

    let mut mods = Modifiers::empty();
    for token in leading {
        match modifier(token) {
            Some(m) => mods |= m,
            None => return Err(ParseError::UnknownModifier((*token).to_string())),
        }
    }

    // No modifier name is also a key name, so "Ctrl+Shift" reaches here having
    // parsed all its tokens and with nothing left to press.
    let key = code(last).ok_or_else(|| {
        if modifier(last).is_some() {
            ParseError::NoKey
        } else {
            ParseError::UnknownKey((*last).to_string())
        }
    })?;

    Ok(HotKey::new(Some(mods), key))
}

fn modifier(token: &str) -> Option<Modifiers> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Modifiers::CONTROL),
        "alt" | "option" => Some(Modifiers::ALT),
        "shift" => Some(Modifiers::SHIFT),
        // `HotKey::new` folds META into SUPER, so all four spellings end up as
        // the same physical key (Windows logo / Command / Meta).
        "super" | "meta" | "win" | "windows" | "cmd" | "command" => Some(Modifiers::SUPER),
        _ => None,
    }
}

/// Keys that are not letters or digits, matched case-insensitively against
/// their canonical `Code` names plus the friendly aliases below.
const NAMED: &[Code] = &[
    Code::Space,
    Code::Enter,
    Code::Tab,
    Code::Escape,
    Code::Backspace,
    Code::Delete,
    Code::Insert,
    Code::Home,
    Code::End,
    Code::PageUp,
    Code::PageDown,
    Code::ArrowUp,
    Code::ArrowDown,
    Code::ArrowLeft,
    Code::ArrowRight,
    Code::CapsLock,
    Code::NumLock,
    Code::ScrollLock,
    Code::PrintScreen,
    Code::Pause,
    Code::Backquote,
    Code::Backslash,
    Code::BracketLeft,
    Code::BracketRight,
    Code::Comma,
    Code::Equal,
    Code::Minus,
    Code::Period,
    Code::Quote,
    Code::Semicolon,
    Code::Slash,
    Code::F1,
    Code::F2,
    Code::F3,
    Code::F4,
    Code::F5,
    Code::F6,
    Code::F7,
    Code::F8,
    Code::F9,
    Code::F10,
    Code::F11,
    Code::F12,
    Code::F13,
    Code::F14,
    Code::F15,
    Code::F16,
    Code::F17,
    Code::F18,
    Code::F19,
    Code::F20,
    Code::F21,
    Code::F22,
    Code::F23,
    Code::F24,
    Code::Numpad0,
    Code::Numpad1,
    Code::Numpad2,
    Code::Numpad3,
    Code::Numpad4,
    Code::Numpad5,
    Code::Numpad6,
    Code::Numpad7,
    Code::Numpad8,
    Code::Numpad9,
    Code::NumpadAdd,
    Code::NumpadSubtract,
    Code::NumpadMultiply,
    Code::NumpadDivide,
    Code::NumpadDecimal,
    Code::NumpadEnter,
    Code::AudioVolumeUp,
    Code::AudioVolumeDown,
    Code::AudioVolumeMute,
    Code::MediaPlayPause,
    Code::MediaStop,
    Code::MediaTrackNext,
    Code::MediaTrackPrevious,
];

/// What people actually type, mapped to the W3C code names.
const ALIASES: &[(&str, Code)] = &[
    ("esc", Code::Escape),
    ("return", Code::Enter),
    ("up", Code::ArrowUp),
    ("down", Code::ArrowDown),
    ("left", Code::ArrowLeft),
    ("right", Code::ArrowRight),
    ("pgup", Code::PageUp),
    ("pgdn", Code::PageDown),
    ("ins", Code::Insert),
    ("del", Code::Delete),
    ("spacebar", Code::Space),
    ("`", Code::Backquote),
    ("\\", Code::Backslash),
    ("[", Code::BracketLeft),
    ("]", Code::BracketRight),
    (",", Code::Comma),
    ("=", Code::Equal),
    ("-", Code::Minus),
    (".", Code::Period),
    ("'", Code::Quote),
    (";", Code::Semicolon),
    ("/", Code::Slash),
];

fn code(token: &str) -> Option<Code> {
    let lower = token.to_ascii_lowercase();

    // Single characters: `p` is KeyP, `5` is Digit5. This is the form users
    // reach for, and the one the W3C names hide behind a prefix.
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return named(&format!("key{c}"));
        }
        if c.is_ascii_digit() {
            return named(&format!("digit{c}"));
        }
    }
    // Canonical names ("KeyP", "Digit5", "ArrowUp"), case-insensitively.
    named(&lower).or_else(|| ALIASES.iter().find(|(a, _)| *a == lower).map(|(_, c)| *c))
}

/// Match a lowercased token against the canonical `Code` spelling.
fn named(lower: &str) -> Option<Code> {
    if let Some(letter) = lower.strip_prefix("key") {
        let mut chars = letter.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_alphabetic() {
                return letter_code(c);
            }
        }
        return None;
    }
    if let Some(digit) = lower.strip_prefix("digit") {
        return digit_code(digit);
    }
    NAMED
        .iter()
        .find(|c| c.to_string().eq_ignore_ascii_case(lower))
        .copied()
}

fn letter_code(c: char) -> Option<Code> {
    let upper = c.to_ascii_uppercase();
    const LETTERS: &[Code] = &[
        Code::KeyA,
        Code::KeyB,
        Code::KeyC,
        Code::KeyD,
        Code::KeyE,
        Code::KeyF,
        Code::KeyG,
        Code::KeyH,
        Code::KeyI,
        Code::KeyJ,
        Code::KeyK,
        Code::KeyL,
        Code::KeyM,
        Code::KeyN,
        Code::KeyO,
        Code::KeyP,
        Code::KeyQ,
        Code::KeyR,
        Code::KeyS,
        Code::KeyT,
        Code::KeyU,
        Code::KeyV,
        Code::KeyW,
        Code::KeyX,
        Code::KeyY,
        Code::KeyZ,
    ];
    LETTERS
        .get((upper as usize).checked_sub('A' as usize)?)
        .copied()
}

fn digit_code(digit: &str) -> Option<Code> {
    const DIGITS: &[Code] = &[
        Code::Digit0,
        Code::Digit1,
        Code::Digit2,
        Code::Digit3,
        Code::Digit4,
        Code::Digit5,
        Code::Digit6,
        Code::Digit7,
        Code::Digit8,
        Code::Digit9,
    ];
    let mut chars = digit.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => DIGITS.get(c.to_digit(10)? as usize).copied(),
        _ => None,
    }
}

/// Render a hotkey the way it is written on the command line
/// (`"Ctrl+Shift+Space"`), rather than the crate's lowercase form.
pub fn describe(hotkey: &HotKey) -> String {
    let mut out = String::new();
    for (flag, name) in [
        (Modifiers::CONTROL, "Ctrl"),
        (Modifiers::ALT, "Alt"),
        (Modifiers::SHIFT, "Shift"),
        (Modifiers::SUPER, "Super"),
    ] {
        if hotkey.mods.contains(flag) {
            out.push_str(name);
            out.push('+');
        }
    }
    // W3C code names back to what a user types: "KeyP" is "P", "Digit5" is
    // "5", everything else already reads correctly ("Space", "F1", "ArrowUp").
    let key = hotkey.key.to_string();
    let short = key
        .strip_prefix("Key")
        .or_else(|| key.strip_prefix("Digit"))
        .filter(|rest| rest.len() == 1)
        .unwrap_or(&key);
    out.push_str(short);
    out
}

/// A warning for combinations that will take a key away from every other
/// application on the desktop. Unmodified typing keys are grabbable and almost
/// never what someone means.
pub fn risky(hotkey: &HotKey) -> Option<String> {
    if !hotkey.mods.is_empty() {
        return None;
    }
    let typing = matches!(hotkey.key, Code::Space | Code::Enter | Code::Tab)
        || named_is_letter_or_digit(hotkey.key);
    typing.then(|| {
        format!(
            "{} has no modifier: grabbing it globally stops that key from \
             reaching every other application",
            describe(hotkey)
        )
    })
}

fn named_is_letter_or_digit(key: Code) -> bool {
    let name = key.to_string();
    name.starts_with("Key") || name.starts_with("Digit")
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// What we can tell about the windowing system before touching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayServer {
    /// Something `global-hotkey` can plausibly grab a key on.
    Present(String),
    /// Nothing to grab on, and why.
    Missing(String),
}

impl DisplayServer {
    pub fn label(&self) -> &str {
        match self {
            Self::Present(what) | Self::Missing(what) => what,
        }
    }
}

/// Look for a display server *before* asking `global-hotkey` for a grab.
///
/// This is not belt-and-braces: on Linux the crate answers registration from a
/// worker thread that has already died when the X connection failed, so
/// `register()` returns `Ok(())` on a headless machine and nothing ever fires.
/// A `--doctor` that reported "registered" there would be worse than useless,
/// so the environment is checked first and a missing display is reported as
/// exactly that.
///
/// `cfg!` rather than `#[cfg]` on purpose: the same code compiles on every
/// target, which is what keeps `cargo check --target x86_64-pc-windows-msvc`
/// honest about this file.
pub fn display_server() -> DisplayServer {
    if !cfg!(unix) {
        // Windows always has a window station to register against.
        return DisplayServer::Present("the desktop session".to_string());
    }
    let x11 = std::env::var("DISPLAY").ok().filter(|v| !v.is_empty());
    let wayland = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|v| !v.is_empty());
    match (x11, wayland) {
        (Some(display), _) => DisplayServer::Present(format!("X11 (DISPLAY={display})")),
        (None, Some(wayland)) => DisplayServer::Missing(format!(
            "a Wayland session (WAYLAND_DISPLAY={wayland}) with no X11 display; \
             global hotkeys are grabbed through X11 only"
        )),
        (None, None) => DisplayServer::Missing(
            "no display server ($DISPLAY and $WAYLAND_DISPLAY are both unset)".to_string(),
        ),
    }
}

/// The outcome of asking for the grab. Never an `Err`: the daemon must start
/// either way, so the failure is a value it can log and move past.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The key is grabbed and presses will arrive.
    Registered { spec: String },
    /// No grab. `reason` is user-facing.
    Unavailable { spec: String, reason: String },
}

impl Status {
    fn unavailable(spec: &str, reason: impl Into<String>) -> Self {
        Self::Unavailable {
            spec: spec.to_string(),
            reason: reason.into(),
        }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn is_registered(&self) -> bool {
        matches!(self, Self::Registered { .. })
    }

    /// The line to print at startup, or `None` when all is well. Phrased so a
    /// user reading their terminal knows the daemon is still up.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Registered { .. } => None,
            Self::Unavailable { spec, reason } => Some(format!(
                "sekio-gui: the hotkey {spec} was not registered ({reason}); \
                 the daemon is running anyway — `sekio-gui <path>` still works. \
                 Run `sekio-gui --doctor` for details, or --no-hotkey to stop asking."
            )),
        }
    }
}

/// Decide what a grab attempt amounted to, given what we know about the
/// display and what the platform said.
///
/// Split out so both branches are unit-testable without ever grabbing a real
/// key: `register` is not even called when there is no display to grab on.
fn attempt(
    spec: &str,
    display: &DisplayServer,
    register: impl FnOnce() -> Result<(), String>,
) -> Status {
    match display {
        DisplayServer::Missing(reason) => Status::unavailable(spec, reason.clone()),
        DisplayServer::Present(_) => match register() {
            Ok(()) => Status::Registered {
                spec: spec.to_string(),
            },
            Err(reason) => Status::unavailable(spec, reason),
        },
    }
}

/// Grab `hotkey`, report whether it worked, and release it again.
///
/// Only `--doctor` uses this: it answers "would the daemon get this key?"
/// without staying resident. A daemon that is already running owns the grab,
/// so a failure here can also mean "your own daemon has it" — the caller says
/// so in its report.
pub fn probe(hotkey: &HotKey, spec: &str) -> Status {
    attempt(spec, &display_server(), || {
        let manager = GlobalHotKeyManager::new().map_err(|err| err.to_string())?;
        manager.register(*hotkey).map_err(|err| err.to_string())?;
        let _ = manager.unregister(*hotkey);
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// The hotkey thread
// ---------------------------------------------------------------------------
//
// Everything below is what the *daemon* uses, and the daemon is Unix-only
// today (`--daemon` needs Unix domain sockets). The code is platform-neutral
// and compiles everywhere on purpose — Windows daemon support will call it
// unchanged — so the only concession is silencing dead-code there rather than
// hiding the code behind a `cfg`.

/// What the daemon gets back from [`listen`].
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Hotkeys {
    /// Registered, or the warning to print.
    pub status: Status,
    /// Paths resolved on the hotkey thread, ready for `SekioApp::show`. Empty
    /// forever when nothing was registered.
    pub presses: Receiver<PathBuf>,
}

/// Register `hotkey` and pump its presses into a channel.
///
/// The manager is created, registered *and kept alive* on the hotkey thread:
/// Win32 ties the registration to the thread that made it, and X11's backend
/// tears its worker down when the manager drops. The thread then blocks on the
/// crate's global event receiver — one press at a time, no polling.
///
/// The selection lookup happens on that thread too, which is the whole point:
/// asking a file manager what is selected is an IPC round trip and the UI
/// thread must never wait for it.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn listen(
    hotkey: HotKey,
    spec: &str,
    source: Box<dyn Source>,
    wake: impl Fn() + Send + 'static,
) -> Hotkeys {
    let (path_tx, presses) = mpsc::channel::<PathBuf>();
    let (status_tx, status_rx) = mpsc::channel::<Status>();
    let spec = spec.to_string();
    let thread_spec = spec.clone();

    let spawned = std::thread::Builder::new()
        .name("sekio-hotkey".to_owned())
        .spawn(move || {
            // Held for the lifetime of the thread; dropping it un-grabs.
            let mut manager = None;
            let status = attempt(&thread_spec, &display_server(), || {
                let created = GlobalHotKeyManager::new().map_err(|err| err.to_string())?;
                created.register(hotkey).map_err(|err| err.to_string())?;
                manager = Some(created);
                Ok(())
            });
            let registered = status.is_registered();
            // A receiver that went away means startup gave up waiting; the
            // grab is still ours, so carry on pumping regardless.
            let _ = status_tx.send(status);
            if !registered {
                return;
            }
            pump(hotkey.id(), source.as_ref(), &path_tx, &wake);
            drop(manager); // Also what keeps the grab alive until here.
        });

    let status = match spawned {
        Ok(_handle) => status_rx
            .recv_timeout(REGISTER_TIMEOUT)
            .unwrap_or_else(|_| Status::unavailable(&spec, "the platform never answered")),
        Err(err) => Status::unavailable(&spec, format!("cannot spawn the hotkey thread: {err}")),
    };
    Hotkeys { status, presses }
}

/// Forward presses forever. Returns when the UI drops the channel.
#[cfg_attr(not(unix), allow(dead_code))]
fn pump(id: u32, source: &dyn Source, tx: &mpsc::Sender<PathBuf>, wake: &dyn Fn()) {
    let events = GlobalHotKeyEvent::receiver();
    while let Ok(event) = events.recv() {
        // The process-wide channel carries every hotkey any manager registered,
        // and both edges of each press.
        if event.id != id || event.state != HotKeyState::Pressed {
            continue;
        }
        // Up to ~200 ms, on this thread, never on the UI thread.
        let Some(selection) = source.current() else {
            continue; // Nothing selected is normal: do nothing at all.
        };
        if !selection::usable(&selection.path) {
            continue;
        }
        if tx.send(selection.path).is_err() {
            break; // UI is gone.
        }
        wake();
    }
}

// ---------------------------------------------------------------------------
// What a press means
// ---------------------------------------------------------------------------

/// What the UI should do about a press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Preview this path (and un-hide the window).
    Show(PathBuf),
    /// Same file, already on screen: the press is a dismiss, the way a second
    /// spacebar closes Quick Look.
    Dismiss,
    /// Nothing to do, and nothing to say about it.
    Ignore,
}

/// Decide what a press means. Pure, so the toggle rule is tested without a
/// window.
///
/// `selection` is what the hotkey thread resolved (`None` when it resolved
/// nothing — that thread sends no message at all in that case, and this
/// function agrees: no error dialog, no empty window). `showing` is the path
/// the window is *visibly* displaying, or `None` when it is hidden.
pub fn action(selection: Option<PathBuf>, showing: Option<&Path>) -> Action {
    match selection {
        None => Action::Ignore,
        Some(path) if showing == Some(path.as_path()) => Action::Dismiss,
        Some(path) => Action::Show(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(spec: &str) -> HotKey {
        parse(spec).unwrap_or_else(|err| panic!("{spec:?} should parse: {err}"))
    }

    #[test]
    fn the_default_spec_parses_and_carries_modifiers() {
        let hotkey = parsed(DEFAULT_SPEC);
        assert_eq!(hotkey.key, Code::Space);
        assert!(hotkey.mods.contains(Modifiers::CONTROL));
        assert!(hotkey.mods.contains(Modifiers::SHIFT));
        assert_eq!(describe(&hotkey), "Ctrl+Shift+Space");
        // Written the way it was typed, not as a W3C code name.
        assert_eq!(describe(&parsed("super+p")), "Super+P");
        assert_eq!(describe(&parsed("ctrl+5")), "Ctrl+5");
        assert_eq!(describe(&parsed("alt+f1")), "Alt+F1");
        assert_eq!(describe(&parsed("ctrl+up")), "Ctrl+ArrowUp");
        assert!(
            risky(&hotkey).is_none(),
            "the default must not steal a typing key"
        );
    }

    #[test]
    fn every_modifier_and_its_aliases_parse() {
        assert_eq!(parsed("Ctrl+P").mods, Modifiers::CONTROL);
        assert_eq!(parsed("Control+P").mods, Modifiers::CONTROL);
        assert_eq!(parsed("Alt+P").mods, Modifiers::ALT);
        assert_eq!(parsed("Option+P").mods, Modifiers::ALT);
        assert_eq!(parsed("Shift+P").mods, Modifiers::SHIFT);
        // Meta/Win/Cmd are all the same physical key as Super.
        for spec in [
            "Super+P",
            "Meta+P",
            "Win+P",
            "Windows+P",
            "Cmd+P",
            "Command+P",
        ] {
            assert_eq!(parsed(spec).mods, Modifiers::SUPER, "{spec}");
        }
        assert_eq!(
            parsed("Ctrl+Alt+Shift+Super+F1").mods,
            Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT | Modifiers::SUPER
        );
    }

    #[test]
    fn parsing_is_case_insensitive_and_ignores_spaces() {
        let want = parsed("Ctrl+Shift+Space");
        for spec in [
            "ctrl+shift+space",
            "CTRL+SHIFT+SPACE",
            "cTrL+ShIfT+sPaCe",
            " Ctrl + Shift + Space ",
        ] {
            assert_eq!(parsed(spec), want, "{spec}");
        }
    }

    #[test]
    fn keys_accept_both_the_short_and_the_w3c_spelling() {
        assert_eq!(parsed("Super+P").key, Code::KeyP);
        assert_eq!(parsed("Super+KeyP").key, Code::KeyP);
        assert_eq!(parsed("Ctrl+5").key, Code::Digit5);
        assert_eq!(parsed("Ctrl+Digit5").key, Code::Digit5);
        assert_eq!(parsed("Alt+F1").key, Code::F1);
        assert_eq!(parsed("Alt+f24").key, Code::F24);
        assert_eq!(parsed("Ctrl+Up").key, Code::ArrowUp);
        assert_eq!(parsed("Ctrl+arrowup").key, Code::ArrowUp);
        assert_eq!(parsed("Ctrl+Esc").key, Code::Escape);
        assert_eq!(parsed("Ctrl+/").key, Code::Slash);
        assert_eq!(parsed("Ctrl+PageDown").key, Code::PageDown);
        assert_eq!(parsed("Ctrl+numpad7").key, Code::Numpad7);
    }

    #[test]
    fn a_bare_key_is_allowed_but_flagged_as_risky() {
        let space = parsed("Space");
        assert_eq!(space.mods, Modifiers::empty());
        let warning = risky(&space).expect("an unmodified Space steals the spacebar");
        assert!(warning.contains("Space"), "{warning}");
        assert!(risky(&parsed("A")).is_some());
        assert!(risky(&parsed("F13")).is_none(), "F13 is not a typing key");
    }

    #[test]
    fn an_unknown_modifier_says_which_one_and_shows_an_example() {
        let err = parse("Hyper+P").expect_err("Hyper is not a modifier we map");
        assert_eq!(err, ParseError::UnknownModifier("Hyper".to_string()));
        let text = err.to_string();
        assert!(text.contains("Hyper"), "must name the token: {text}");
        assert!(text.contains("Ctrl"), "must list the modifiers: {text}");
        assert!(text.contains("Ctrl+Shift+Space"), "must show how: {text}");
    }

    #[test]
    fn an_unknown_key_says_which_one_and_shows_an_example() {
        let err = parse("Ctrl+Banana").expect_err("Banana is not a key");
        assert_eq!(err, ParseError::UnknownKey("Banana".to_string()));
        let text = err.to_string();
        assert!(text.contains("Banana"), "must name the token: {text}");
        assert!(text.contains("Ctrl+Shift+Space"), "must show how: {text}");
        // A lone unknown token is a key, not a modifier.
        assert_eq!(
            parse("Banana"),
            Err(ParseError::UnknownKey("Banana".to_string()))
        );
    }

    #[test]
    fn empty_and_modifier_only_specs_are_rejected() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
        assert_eq!(parse("Ctrl+"), Err(ParseError::EmptyToken));
        assert_eq!(parse("Ctrl++Space"), Err(ParseError::EmptyToken));
        assert_eq!(parse("+Space"), Err(ParseError::EmptyToken));
        assert_eq!(parse("Ctrl+Shift"), Err(ParseError::NoKey));
        assert_eq!(parse("Ctrl+Shift+Alt"), Err(ParseError::NoKey));
        for spec in ["", "Ctrl+", "Ctrl+Shift"] {
            let text = parse(spec).expect_err("must fail").to_string();
            assert!(text.contains("Ctrl+Shift+Space"), "{spec}: {text}");
        }
    }

    #[test]
    fn a_refused_grab_is_a_warning_value_not_a_startup_error() {
        let display = DisplayServer::Present("X11 (DISPLAY=:0)".to_string());
        let status = attempt("Ctrl+Shift+Space", &display, || {
            Err("HotKey already registered".to_string())
        });
        assert!(!status.is_registered());
        let warning = status.warning().expect("a refusal must be reported");
        assert!(warning.contains("Ctrl+Shift+Space"), "{warning}");
        assert!(warning.contains("already registered"), "{warning}");
        assert!(
            warning.contains("daemon is running anyway"),
            "the daemon must be described as usable: {warning}"
        );
        // And the happy path stays quiet.
        let ok = attempt("Ctrl+Shift+Space", &display, || Ok(()));
        assert!(ok.is_registered());
        assert_eq!(ok.warning(), None);
    }

    #[test]
    fn a_missing_display_never_reaches_the_platform() {
        // The X11 backend answers `Ok` from a dead worker thread on a headless
        // box, so we must not ask it at all.
        let display = DisplayServer::Missing("no display server".to_string());
        let mut asked = false;
        let status = attempt("Alt+F1", &display, || {
            asked = true;
            Ok(())
        });
        assert!(!asked, "must not try to grab without a display");
        assert!(!status.is_registered());
        assert!(status.warning().is_some_and(|w| w.contains("Alt+F1")));
    }

    #[test]
    fn the_display_probe_reports_something_either_way() {
        // Whatever this machine is, the label must be non-empty and the two
        // variants must be distinguishable.
        let server = display_server();
        assert!(!server.label().is_empty());
    }

    #[test]
    fn a_press_on_the_file_already_showing_dismisses_it() {
        let file = PathBuf::from("/tmp/a.txt");
        assert_eq!(
            action(Some(file.clone()), Some(&file)),
            Action::Dismiss,
            "a second press is a dismiss, like Quick Look"
        );
    }

    #[test]
    fn a_press_on_another_file_shows_it() {
        let showing = PathBuf::from("/tmp/a.txt");
        let picked = PathBuf::from("/tmp/b.txt");
        assert_eq!(
            action(Some(picked.clone()), Some(&showing)),
            Action::Show(picked.clone())
        );
        // Hidden window: always a show, even for the last file previewed.
        assert_eq!(
            action(Some(showing.clone()), None),
            Action::Show(showing.clone())
        );
    }

    #[test]
    fn no_selection_does_nothing_at_all() {
        assert_eq!(action(None, None), Action::Ignore);
        assert_eq!(
            action(None, Some(Path::new("/tmp/a.txt"))),
            Action::Ignore,
            "an unresolvable press must not close what is on screen"
        );
    }
}
