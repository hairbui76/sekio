//! Non-visual state for the GUI: request generations (so stale previews are
//! never painted) and sibling-file navigation. Deliberately free of any egui
//! types so it can be unit-tested without a window.

use std::path::{Path, PathBuf};

use sekio_core::CancelToken;

/// Hands out monotonically increasing request ids and owns the `CancelToken`
/// of the request currently in flight.
///
/// The rule the whole GUI hangs on: starting a new request immediately cancels
/// the previous one, and a result whose id is not the current id is dropped on
/// the floor — it belongs to a file the user has already moved past.
#[derive(Debug, Default)]
pub struct RequestTracker {
    next_id: u64,
    in_flight: Option<InFlight>,
}

#[derive(Debug)]
struct InFlight {
    id: u64,
    cancel: CancelToken,
}

impl RequestTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel whatever is in flight and start a new generation.
    /// Returns the id and the token to hand to the worker.
    pub fn begin(&mut self) -> (u64, CancelToken) {
        if let Some(prev) = self.in_flight.take() {
            prev.cancel.cancel();
        }
        self.next_id += 1;
        let id = self.next_id;
        let cancel = CancelToken::new();
        self.in_flight = Some(InFlight {
            id,
            cancel: cancel.clone(),
        });
        (id, cancel)
    }

    /// True while a request is outstanding (paint the "loading…" placeholder).
    pub fn is_pending(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Should a result with this id be displayed? Accepting clears the
    /// in-flight slot, so a duplicate result for the same id is also rejected.
    pub fn accept(&mut self, id: u64) -> bool {
        match &self.in_flight {
            Some(cur) if cur.id == id => {
                self.in_flight = None;
                true
            }
            _ => false,
        }
    }

    /// Cancel the in-flight request without starting a new one (window closing).
    pub fn cancel_all(&mut self) {
        if let Some(prev) = self.in_flight.take() {
            prev.cancel.cancel();
        }
    }
}

/// The files that live next to the previewed path, so Left/Right can flip
/// through a directory the way Quick Look does.
///
/// Only regular files are listed: arrowing into a subdirectory would change
/// what "the directory" means mid-flight, so directories are skipped.
#[derive(Debug, Clone, Default)]
pub struct Siblings {
    files: Vec<PathBuf>,
    /// Index of the current path within `files`, if it is one of them (it is
    /// not when the previewed path is itself a directory).
    index: Option<usize>,
    /// Wrap around at the ends instead of clamping.
    wrap: bool,
}

impl Siblings {
    /// Build the sibling list by listing `path`'s parent directory. Any IO
    /// error just yields an empty list — navigation goes dead, nothing panics.
    pub fn scan(path: &Path, wrap: bool) -> Self {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let mut files: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&parent) {
            for entry in entries.flatten() {
                // `file_type()` avoids a stat syscall on most platforms and
                // never follows symlinks into an unreadable target.
                let is_dir = match entry.file_type() {
                    Ok(ft) if ft.is_symlink() => entry.path().is_dir(),
                    Ok(ft) => ft.is_dir(),
                    Err(_) => continue,
                };
                if !is_dir {
                    files.push(entry.path());
                }
            }
        }
        files.sort();
        Self::from_files(files, path, wrap)
    }

    /// Same as `scan` but with an explicit file list (used by tests).
    pub fn from_files(files: Vec<PathBuf>, current: &Path, wrap: bool) -> Self {
        let index = files.iter().position(|p| p == current);
        Self { files, index, wrap }
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 1-based position of the current file, for the "3 / 17" header.
    pub fn position(&self) -> Option<usize> {
        self.index.map(|i| i + 1)
    }

    /// Step by `delta` (-1 = previous, +1 = next). Returns the new path, or
    /// `None` when there is nowhere to go (clamped at an end, or no siblings).
    /// Also advances the internal cursor so repeated steps keep moving.
    pub fn step(&mut self, delta: isize) -> Option<PathBuf> {
        if self.files.is_empty() {
            return None;
        }
        let len = self.files.len() as isize;
        let next = match self.index {
            Some(i) => {
                let raw = i as isize + delta;
                if raw < 0 || raw >= len {
                    if self.wrap {
                        raw.rem_euclid(len)
                    } else {
                        return None;
                    }
                } else {
                    raw
                }
            }
            // The current path is not a file in this directory (e.g. it is the
            // directory itself): enter the list from whichever end we came in.
            None => {
                if delta >= 0 {
                    0
                } else {
                    len - 1
                }
            }
        };
        let next = next as usize;
        if Some(next) == self.index {
            return None;
        }
        self.index = Some(next);
        self.files.get(next).cloned()
    }
}

// ---------------------------------------------------------------------------
// What "dismiss" means
// ---------------------------------------------------------------------------

/// How this process was started, which is the only thing that decides what
/// closing the preview does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `sekio-gui <path>` — a Quick Look popup over whatever the user was
    /// doing. Dismissing it is the whole point, and it exits.
    Popup,
    /// `sekio-gui` with no path — a window the user opened deliberately, from
    /// a launcher, a dock or a Start Menu entry. It is an application: it must
    /// not vanish when they press Escape.
    App,
    /// `--daemon` — resident. Dismissing hides the window and keeps the
    /// process (and its warm `Previewer`) alive.
    Daemon,
}

impl Mode {
    /// Should a window started this way look for a newer sekio?
    ///
    /// Not a popup: it is on screen for a moment and gone, its window is over
    /// whatever the user was actually doing, and there is nowhere for the
    /// answer to be read. A daemon checks once per login and an application
    /// once per launch, which is as often as anyone needs to be told.
    pub fn checks_updates(self) -> bool {
        matches!(self, Self::App | Self::Daemon)
    }

    /// A popup becomes an application the moment the user opens something
    /// through the app itself — the dialog, the browser, a drop, a recent
    /// entry. They did not ask for a transient popup at that point; they are
    /// using sekio as a viewer, and Escape must not throw the window away.
    /// A daemon stays a daemon, and an app is already there.
    pub fn promoted(self) -> Self {
        match self {
            Self::Popup | Self::App => Self::App,
            Self::Daemon => Self::Daemon,
        }
    }
}

/// What dismissing should actually do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Close {
    /// Close the window; for a one-shot process that ends it.
    Window,
    /// Hide the window, stay resident.
    Hide,
    /// Drop the preview and show the home screen.
    Home,
    /// Nothing at all — there is nothing to dismiss.
    Nothing,
}

/// The Esc/Space rule, in one pure function.
///
/// `showing` is whether anything is loaded (a preview, a "loading…" or an
/// error) as opposed to the home screen.
pub fn close_action(mode: Mode, showing: bool) -> Close {
    match mode {
        Mode::Daemon => Close::Hide,
        Mode::Popup => Close::Window,
        Mode::App if showing => Close::Home,
        Mode::App => Close::Nothing,
    }
}

/// Which rule this window follows *right now*.
///
/// One resident process serves two roles: the popup the hotkey throws up over
/// whatever the user was doing, and the window they launched from a menu and
/// are sitting in. Escape means opposite things in the two — put this away
/// versus go back to the home screen — and the difference is not the process,
/// it is how this particular window got on screen.
///
/// `summoned` is true when a hotkey press or a socket handoff put a file up,
/// false when the user asked for the window itself (a launcher click, the tray
/// icon) or opened something through the app. So a daemon behaves like the
/// application it is being used as, and only reverts to popup manners for the
/// presses that are actually popups.
pub fn posture(mode: Mode, summoned: bool) -> Mode {
    match mode {
        Mode::Daemon if !summoned => Mode::App,
        other => other,
    }
}

/// What closing the window should do, when there is more than one answer.
///
/// Only a resident daemon with a tray icon has two: end the process, or put
/// the window away and keep answering the hotkey. Everything else has exactly
/// one, so this preference never reaches [`close_intent`]'s first two arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnClose {
    /// Ask, once, with a "don't ask again" checkbox. The default: the first
    /// close is the only moment the user is actually thinking about the
    /// question, so it is the only good moment to ask it.
    #[default]
    Ask,
    /// Put the window away and stay resident. Never ask again.
    Tray,
    /// End the process. Never ask again.
    Quit,
}

impl OnClose {
    /// Every value the config file accepts, for the warning `validate` prints.
    pub const NAMES: [&'static str; 3] = ["ask", "tray", "quit"];

    /// Parse the config file's spelling; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "tray" | "minimize" | "hide" => Some(Self::Tray),
            "quit" | "exit" => Some(Self::Quit),
            _ => None,
        }
    }

    /// How it is written back to the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Tray => "tray",
            Self::Quit => "quit",
        }
    }
}

/// What a close request means for *this* process, given what it can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Put the question to the user and do nothing until they answer.
    Ask,
    /// Hide the window, stay resident.
    Tray,
    /// End the process.
    Quit,
}

/// The close-button rule, in one pure function.
///
/// `tray` is whether an icon is actually on screen — not whether one was asked
/// for. Both halves matter: the choice is only real when the process can
/// outlive its window (a daemon) *and* there is something on screen to get it
/// back from. Offering "minimize to the tray" with no tray would be offering
/// the user a way to lose the application.
pub fn close_intent(mode: Mode, tray: bool, pref: OnClose) -> Intent {
    match (mode, tray, pref) {
        // An answer already given is honoured whether or not the icon came
        // back this session. Quit first: a user who asked to be shut down
        // must not be left with an invisible process when the tray fails.
        (Mode::Daemon, _, OnClose::Quit) => Intent::Quit,
        (Mode::Daemon, true, OnClose::Ask) => Intent::Ask,
        // Asked, but with no icon: hide, exactly as a daemon always has. The
        // hotkey and the socket still reach it, so the window is not the only
        // way back, and closing must not kill a process the user meant to keep.
        (Mode::Daemon, _, _) => Intent::Tray,
        // Nothing to stay resident for, so there is nothing to ask about.
        _ => Intent::Quit,
    }
}

/// Is decoding this picture again, at `wanted` pixels, worth what it costs?
///
/// Two things have to hold, and both are about not paying for a decode and a
/// GPU upload that change nothing on screen.
///
/// `wanted > have` — the surface can show more pixels than the texture has.
/// Shrinking a window is free to paint, because the texture just scales down,
/// so a smaller window never triggers a re-decode.
///
/// `have >= asked` — the last request came back at the full size it asked for,
/// which is the only evidence available that the file has more to give. A
/// 200 px icon asked for at 1024 comes back 200 px wide, and it will still be
/// 200 px wide in a window the size of a wall.
pub fn worth_rescaling(wanted: u32, have: u32, asked: u32) -> bool {
    wanted > have && have >= asked
}

/// Human-readable byte count, matching the CLI's formatting.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A popup is on screen for a moment, over whatever the user was doing.
    /// Telling it about a new release would put the news somewhere nobody can
    /// read it, and would ask GitHub once per previewed file.
    #[test]
    fn only_a_lasting_window_looks_for_updates() {
        assert!(Mode::App.checks_updates());
        assert!(Mode::Daemon.checks_updates());
        assert!(!Mode::Popup.checks_updates());
    }

    /// The regression this function exists to prevent. Making the launcher
    /// window resident put it in `Mode::Daemon`, where Escape hides the whole
    /// window — but the key legend on that very screen says "back to this
    /// screen", and a user sitting in the app does not expect it to vanish.
    #[test]
    fn a_daemon_the_user_launched_follows_the_application_rule() {
        assert_eq!(posture(Mode::Daemon, false), Mode::App);
        assert_eq!(
            close_action(posture(Mode::Daemon, false), true),
            Close::Home,
            "Escape on a window somebody opened goes home, as the legend says"
        );
        assert_eq!(
            close_action(posture(Mode::Daemon, false), false),
            Close::Nothing
        );
    }

    /// …and the popup manners survive for the presses that really are popups.
    #[test]
    fn a_summoned_window_is_still_put_away_by_escape() {
        assert_eq!(posture(Mode::Daemon, true), Mode::Daemon);
        assert_eq!(close_action(posture(Mode::Daemon, true), true), Close::Hide);
    }

    /// Neither of the one-shot modes has two roles to tell apart.
    #[test]
    fn only_a_daemon_has_a_posture_to_choose() {
        for summoned in [true, false] {
            assert_eq!(posture(Mode::Popup, summoned), Mode::Popup);
            assert_eq!(posture(Mode::App, summoned), Mode::App);
        }
    }

    /// A window with room for more pixels than the picture has, on a file that
    /// proved it has them, is the one case worth a second decode.
    #[test]
    fn a_window_with_room_to_spare_asks_for_a_sharper_picture() {
        // Asked for 1024, got 1024: the file had at least that much.
        assert!(worth_rescaling(1800, 1024, 1024));
    }

    /// The cost of getting this wrong is a full decode and a GPU upload on
    /// every window drag, for a picture that cannot change.
    #[test]
    fn a_file_with_nothing_more_to_give_is_not_decoded_again() {
        // Asked for 1024, got 200: that is the whole file.
        assert!(!worth_rescaling(1800, 200, 1024));
        assert!(!worth_rescaling(4096, 200, 1024));
    }

    /// Shrinking a window is free — the texture scales down — so it must not
    /// cost a decode. Nor may sitting still at the size we already asked for.
    #[test]
    fn a_smaller_window_costs_nothing() {
        assert!(!worth_rescaling(800, 1800, 1800));
        assert!(!worth_rescaling(1800, 1800, 1800));
    }

    /// The whole point of the dialog: it is offered only where both answers
    /// exist. A popup and a plain window have nothing to stay resident for,
    /// and a daemon with no icon has nowhere to minimise to.
    #[test]
    fn only_a_daemon_with_an_icon_is_ever_asked() {
        for mode in [Mode::Popup, Mode::App] {
            for tray in [true, false] {
                assert_eq!(
                    close_intent(mode, tray, OnClose::Ask),
                    Intent::Quit,
                    "{mode:?} with tray={tray} has no second answer to offer"
                );
            }
        }
        assert_eq!(
            close_intent(Mode::Daemon, false, OnClose::Ask),
            Intent::Tray,
            "no icon means no way back from a hidden window, so do not offer one"
        );
        assert_eq!(close_intent(Mode::Daemon, true, OnClose::Ask), Intent::Ask);
    }

    /// "Don't ask again" has to actually stop the asking, in both directions.
    #[test]
    fn a_remembered_answer_is_not_asked_again() {
        assert_eq!(
            close_intent(Mode::Daemon, true, OnClose::Tray),
            Intent::Tray
        );
        assert_eq!(
            close_intent(Mode::Daemon, true, OnClose::Quit),
            Intent::Quit
        );
    }

    /// The icon gates the *question*, not an answer already given: a session
    /// where the tray host is missing must not quietly reinstate a behaviour
    /// the user turned off.
    #[test]
    fn losing_the_icon_does_not_reopen_the_question() {
        assert_eq!(
            close_intent(Mode::Daemon, false, OnClose::Tray),
            Intent::Tray
        );
        assert_eq!(
            close_intent(Mode::Daemon, false, OnClose::Quit),
            Intent::Quit,
            "'always quit' with no icon must not leave a process nothing can show"
        );
    }

    #[test]
    fn every_on_close_name_round_trips() {
        for name in OnClose::NAMES {
            let parsed = OnClose::parse(name).expect("a documented name must parse");
            assert_eq!(parsed.as_str(), name);
        }
        assert_eq!(OnClose::parse("Tray"), Some(OnClose::Tray));
        assert_eq!(OnClose::parse("minimize"), Some(OnClose::Tray));
        assert_eq!(OnClose::parse("sometimes"), None);
        assert_eq!(OnClose::default(), OnClose::Ask);
    }

    #[test]
    fn ids_increase_and_previous_request_is_cancelled() {
        let mut t = RequestTracker::new();
        let (id1, tok1) = t.begin();
        assert!(!tok1.is_cancelled());
        let (id2, tok2) = t.begin();
        assert!(id2 > id1);
        assert!(tok1.is_cancelled(), "moving on must cancel the old request");
        assert!(!tok2.is_cancelled());
    }

    #[test]
    fn stale_results_are_discarded_and_current_accepted() {
        let mut t = RequestTracker::new();
        let (stale, _) = t.begin();
        let (current, _) = t.begin();
        assert!(!t.accept(stale), "stale result must never be displayed");
        assert!(t.is_pending());
        assert!(t.accept(current));
        assert!(!t.is_pending(), "accepting clears the pending placeholder");
        assert!(!t.accept(current), "a duplicate result is rejected too");
    }

    #[test]
    fn cancel_all_stops_the_in_flight_request() {
        let mut t = RequestTracker::new();
        let (id, tok) = t.begin();
        t.cancel_all();
        assert!(tok.is_cancelled());
        assert!(!t.is_pending());
        assert!(!t.accept(id));
    }

    fn files(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let list = files(&["/d/a.txt", "/d/b.txt", "/d/c.txt"]);
        let mut s = Siblings::from_files(list, Path::new("/d/b.txt"), false);
        assert_eq!(s.position(), Some(2));
        assert_eq!(s.step(1), Some(PathBuf::from("/d/c.txt")));
        assert_eq!(s.step(1), None, "clamped at the last file");
        assert_eq!(s.step(-1), Some(PathBuf::from("/d/b.txt")));
        assert_eq!(s.step(-1), Some(PathBuf::from("/d/a.txt")));
        assert_eq!(s.step(-1), None, "clamped at the first file");
        assert_eq!(s.position(), Some(1));
    }

    #[test]
    fn navigation_wraps_when_enabled() {
        let list = files(&["/d/a.txt", "/d/b.txt"]);
        let mut s = Siblings::from_files(list, Path::new("/d/b.txt"), true);
        assert_eq!(s.step(1), Some(PathBuf::from("/d/a.txt")));
        assert_eq!(s.step(-1), Some(PathBuf::from("/d/b.txt")));
    }

    #[test]
    fn empty_or_single_directory_never_moves() {
        let mut none = Siblings::from_files(vec![], Path::new("/d/a.txt"), true);
        assert_eq!(none.step(1), None);
        assert!(none.is_empty());

        let mut one = Siblings::from_files(files(&["/d/a.txt"]), Path::new("/d/a.txt"), true);
        assert_eq!(one.step(1), None, "wrapping onto itself is not a move");
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn unknown_current_path_enters_the_list_from_the_matching_end() {
        let list = files(&["/d/a.txt", "/d/b.txt", "/d/c.txt"]);
        let mut fwd = Siblings::from_files(list.clone(), Path::new("/d"), false);
        assert_eq!(fwd.position(), None);
        assert_eq!(fwd.step(1), Some(PathBuf::from("/d/a.txt")));
        let mut back = Siblings::from_files(list, Path::new("/d"), false);
        assert_eq!(back.step(-1), Some(PathBuf::from("/d/c.txt")));
    }

    #[test]
    fn scan_skips_directories_and_finds_the_current_file() {
        let dir = std::env::temp_dir().join(format!("sekio-gui-nav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("create fixture dir");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(name), b"x").expect("write fixture");
        }

        let current = dir.join("b.txt");
        let mut s = Siblings::scan(&current, false);
        assert_eq!(s.len(), 3, "the `sub` directory must not be listed");
        assert_eq!(s.position(), Some(2));
        assert_eq!(s.step(1), Some(dir.join("c.txt")));
        assert_eq!(s.step(1), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_popup_closes_but_an_app_goes_home() {
        // The behaviour `sekio-gui <path>` has always had.
        assert_eq!(close_action(Mode::Popup, true), Close::Window);
        // A resident daemon only ever hides.
        assert_eq!(close_action(Mode::Daemon, true), Close::Hide);
        assert_eq!(close_action(Mode::Daemon, false), Close::Hide);
        // Launched with no path: Escape backs out of the file, and pressing it
        // again on the home screen must not close an app the user just opened.
        assert_eq!(close_action(Mode::App, true), Close::Home);
        assert_eq!(close_action(Mode::App, false), Close::Nothing);
    }

    #[test]
    fn opening_a_file_inside_the_app_makes_it_an_app() {
        assert_eq!(Mode::Popup.promoted(), Mode::App);
        assert_eq!(Mode::App.promoted(), Mode::App);
        assert_eq!(
            Mode::Daemon.promoted(),
            Mode::Daemon,
            "a daemon must stay resident whatever the user opens"
        );
        // …and the promoted popup no longer exits on Escape.
        assert_eq!(close_action(Mode::Popup.promoted(), true), Close::Home);
    }

    #[test]
    fn human_size_matches_the_cli() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
