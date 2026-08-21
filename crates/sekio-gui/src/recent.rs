//! The list of recently previewed paths shown on the home screen.
//!
//! This is a convenience and nothing more, so every failure mode here is
//! "silently carry on with an empty list": a missing file, a file with no read
//! permission, invalid UTF-8, junk lines, a state directory that cannot be
//! created. None of it may ever block or fail startup, which is also why the
//! read happens on a thread (see [`Store`]) rather than before the first frame.
//!
//! The serialised form is deliberately the dumbest thing that round-trips: one
//! absolute path per line, newest first, UTF-8. No dependency, no schema, and a
//! human can fix it in an editor.
//!
//! Where it lives:
//!
//! * Linux/Unix — `$XDG_STATE_HOME/sekio/recent`, falling back to
//!   `~/.local/state/sekio/recent` (the XDG basedir spec's own default).
//! * Windows — `%LOCALAPPDATA%\sekio\recent`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

/// How many paths are remembered. Ten fits the home screen without turning it
/// into a file manager.
pub const CAP: usize = 10;

/// File name inside the per-platform state directory.
const FILE: &str = "recent";

/// Newest-first list of absolute paths, capped at [`CAP`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recent {
    paths: Vec<PathBuf>,
}

impl Recent {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Record `path` as the most recent entry.
    ///
    /// Relative paths are refused: an entry is only useful if it still means
    /// the same file in the next process, whose working directory is unknown.
    /// Everything the GUI feeds in here is already canonical.
    ///
    /// Returns whether the list actually changed, so a re-preview of the file
    /// already at the top does not trigger a pointless write.
    pub fn add(&mut self, path: &Path) -> bool {
        if !path.is_absolute() {
            return false;
        }
        if self.paths.first().is_some_and(|first| first == path) {
            return false;
        }
        self.paths.retain(|existing| existing != path);
        self.paths.insert(0, path.to_path_buf());
        self.paths.truncate(CAP);
        true
    }

    /// The entries that still exist, in order. Called when the home screen is
    /// rebuilt (not per frame): a deleted file must not sit in the list
    /// offering to open itself.
    pub fn existing(&self) -> Vec<PathBuf> {
        self.paths
            .iter()
            .filter(|path| path.exists())
            .cloned()
            .collect()
    }

    /// Parse the serialised form. Anything unparseable is skipped rather than
    /// rejected: half a readable list beats none.
    pub fn parse(text: &str) -> Self {
        let mut recent = Self::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let path = Path::new(line);
            if !path.is_absolute() {
                continue;
            }
            // Append rather than `add`, which would reverse the file.
            if !recent.paths.iter().any(|existing| existing == path) {
                recent.paths.push(path.to_path_buf());
            }
            if recent.paths.len() == CAP {
                break;
            }
        }
        recent
    }

    /// The serialised form. Paths that are not valid UTF-8 are dropped — the
    /// format has no way to express them, and inventing one for a convenience
    /// list is not worth it.
    pub fn serialise(&self) -> String {
        let mut out = String::new();
        for path in &self.paths {
            if let Some(text) = path.to_str() {
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    }

    /// Read the list from `file`. Every error — missing, unreadable, not
    /// UTF-8 — is an empty list.
    pub fn load(file: &Path) -> Self {
        match std::fs::read_to_string(file) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::new(),
        }
    }

    /// Best-effort write. Failure is ignored on purpose: nothing the user is
    /// doing should stop because a convenience list could not be saved.
    pub fn save(&self, file: &Path) {
        if let Some(dir) = file.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        // Written aside and renamed so a crash mid-write cannot leave a
        // truncated list behind. A failed rename just leaves the temp file.
        let temp = file.with_extension("tmp");
        if std::fs::write(&temp, self.serialise()).is_err() {
            let _ = std::fs::remove_file(&temp);
            return;
        }
        if std::fs::rename(&temp, file).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// The per-platform state directory, from the environment values that decide
/// it. Pure so the platform rules are tested on either platform.
///
/// `windows` selects the rule, not the host: on Windows only `%LOCALAPPDATA%`
/// is consulted, everywhere else `$XDG_STATE_HOME` then `$HOME`. A relative
/// `$XDG_STATE_HOME` is ignored, as the basedir spec requires.
pub fn state_dir_from(
    local_app_data: Option<PathBuf>,
    xdg_state: Option<PathBuf>,
    home: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    if windows {
        return local_app_data
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("sekio"));
    }
    if let Some(dir) = xdg_state.filter(|dir| dir.is_absolute()) {
        return Some(dir.join("sekio"));
    }
    home.filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(".local").join("state").join("sekio"))
}

/// Where this machine keeps the list, or `None` when the environment says
/// nothing useful (a service account with no `$HOME`, say) — in which case the
/// list simply does not persist.
pub fn state_file() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    state_dir_from(
        var("LOCALAPPDATA"),
        var("XDG_STATE_HOME"),
        var("HOME").or_else(|| var("USERPROFILE")),
        cfg!(windows),
    )
    .map(|dir| dir.join(FILE))
}

/// Owns the one thread that touches the state file.
///
/// Startup never waits on it: the app is constructed with an empty list and
/// picks up the real one from [`Store::poll`] on whichever frame it arrives,
/// which is what keeps the home screen painting immediately. The same thread
/// then serves writes, so saving a newly previewed path costs the UI thread a
/// channel send.
pub struct Store {
    loaded: Option<Receiver<Recent>>,
    writes: Option<Sender<Recent>>,
}

impl Store {
    /// Spawn the thread. A failed spawn (or no state directory at all) is a
    /// store that never loads and never saves — never an error.
    pub fn spawn(ctx: egui::Context) -> Self {
        let Some(file) = state_file() else {
            return Self {
                loaded: None,
                writes: None,
            };
        };
        let (loaded_tx, loaded_rx) = mpsc::channel::<Recent>();
        let (write_tx, write_rx) = mpsc::channel::<Recent>();
        let spawned = std::thread::Builder::new()
            .name("sekio-recent".to_owned())
            .spawn(move || run(file, loaded_tx, write_rx, ctx));
        match spawned {
            Ok(_) => Self {
                loaded: Some(loaded_rx),
                writes: Some(write_tx),
            },
            Err(_) => Self {
                loaded: None,
                writes: None,
            },
        }
    }

    /// The list read from disk, once, on the frame it lands.
    pub fn poll(&mut self) -> Option<Recent> {
        let received = self.loaded.as_ref()?.try_recv().ok();
        if received.is_some() {
            // One shot: the file is read once per process.
            self.loaded = None;
        }
        received
    }

    /// Queue a write. Dropped silently if the thread is gone.
    pub fn remember(&self, recent: &Recent) {
        if let Some(writes) = &self.writes {
            let _ = writes.send(recent.clone());
        }
    }
}

fn run(file: PathBuf, loaded: Sender<Recent>, writes: Receiver<Recent>, ctx: egui::Context) {
    if loaded.send(Recent::load(&file)).is_err() {
        return; // The window is already gone.
    }
    // Wake the UI so the home screen shows the list without waiting for the
    // next mouse move.
    ctx.request_repaint();

    while let Ok(mut recent) = writes.recv() {
        // Coalesce: a burst of previews writes the last list, not each one.
        while let Ok(newer) = writes.try_recv() {
            recent = newer;
        }
        recent.save(&file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path on every platform sekio targets.
    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sekio-gui-recent-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn newest_first_and_re_adding_moves_to_the_top() {
        let mut recent = Recent::new();
        assert!(recent.add(&abs("a.txt")));
        assert!(recent.add(&abs("b.txt")));
        assert_eq!(recent.paths(), [abs("b.txt"), abs("a.txt")]);

        assert!(recent.add(&abs("a.txt")), "a move counts as a change");
        assert_eq!(recent.paths(), [abs("a.txt"), abs("b.txt")]);
        assert_eq!(recent.paths().len(), 2, "no duplicate entry");

        assert!(
            !recent.add(&abs("a.txt")),
            "re-previewing the top file changes nothing, so nothing is written"
        );
    }

    #[test]
    fn the_list_is_capped() {
        let mut recent = Recent::new();
        for i in 0..CAP + 5 {
            recent.add(&abs(&format!("f{i}.txt")));
        }
        assert_eq!(recent.paths().len(), CAP);
        assert_eq!(recent.paths()[0], abs(&format!("f{}.txt", CAP + 4)));
        assert_eq!(recent.paths()[CAP - 1], abs(&format!("f{}.txt", 5)));
    }

    #[test]
    fn relative_paths_are_refused() {
        let mut recent = Recent::new();
        assert!(!recent.add(Path::new("notes.md")));
        assert!(!recent.add(Path::new("")));
        assert!(recent.paths().is_empty());
    }

    #[test]
    fn serialised_form_round_trips() {
        let mut recent = Recent::new();
        recent.add(&abs("one.txt"));
        recent.add(&abs("two with spaces.txt"));
        let text = recent.serialise();
        assert_eq!(text.lines().count(), 2);
        assert_eq!(Recent::parse(&text), recent);
    }

    #[test]
    fn parsing_skips_junk_and_keeps_order() {
        let good = abs("kept.txt");
        let text = format!(
            "{}\n\n   \nnot/absolute.txt\n{}\n",
            good.display(),
            good.display()
        );
        let recent = Recent::parse(&text);
        assert_eq!(
            recent.paths(),
            [good],
            "blank, relative and duplicate lines go"
        );
    }

    #[test]
    fn parsing_caps_a_file_that_grew_too_long() {
        let text: String = (0..CAP + 7)
            .map(|i| format!("{}\n", abs(&format!("f{i}.txt")).display()))
            .collect();
        assert_eq!(Recent::parse(&text).paths().len(), CAP);
    }

    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_list() {
        let dir = scratch("corrupt");
        assert_eq!(Recent::load(&dir.join("nothing-here")), Recent::new());

        let corrupt = dir.join("recent");
        std::fs::write(&corrupt, [0xff, 0xfe, 0x00, 0x01, 0x80]).expect("write junk");
        assert_eq!(
            Recent::load(&corrupt),
            Recent::new(),
            "invalid UTF-8 must never be a startup error"
        );

        // A directory where the file should be: still just an empty list.
        assert_eq!(Recent::load(&dir), Recent::new());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_survives_a_missing_directory() {
        let dir = scratch("save");
        let file = dir.join("nested").join("deeper").join("recent");
        let mut recent = Recent::new();
        recent.add(&abs("saved.txt"));
        recent.save(&file);
        assert_eq!(Recent::load(&file), recent);
        assert!(
            !file.with_extension("tmp").exists(),
            "the temp file is renamed away, not left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_drops_paths_that_are_gone() {
        let dir = scratch("existing");
        let here = dir.join("here.txt");
        std::fs::write(&here, b"x").expect("write fixture");
        let gone = dir.join("gone.txt");

        let mut recent = Recent::new();
        recent.add(&gone);
        recent.add(&here);
        assert_eq!(recent.paths().len(), 2);
        assert_eq!(recent.existing(), [here]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_dir_follows_the_platform_rules() {
        let local = PathBuf::from(r"C:\Users\x\AppData\Local");
        let xdg = PathBuf::from("/home/x/.state");
        let home = PathBuf::from("/home/x");

        assert_eq!(
            state_dir_from(Some(local.clone()), None, None, true),
            Some(local.join("sekio"))
        );
        assert_eq!(
            state_dir_from(None, Some(xdg.clone()), Some(home.clone()), true),
            None
        );

        assert_eq!(
            state_dir_from(None, Some(xdg.clone()), Some(home.clone()), false),
            Some(xdg.join("sekio"))
        );
        assert_eq!(
            state_dir_from(None, None, Some(home.clone()), false),
            Some(PathBuf::from("/home/x/.local/state/sekio"))
        );
        assert_eq!(
            state_dir_from(
                None,
                Some(PathBuf::from("relative")),
                Some(home.clone()),
                false
            ),
            Some(PathBuf::from("/home/x/.local/state/sekio")),
            "a relative XDG_STATE_HOME is ignored, per the basedir spec"
        );
        assert_eq!(state_dir_from(None, None, None, false), None);
    }
}
