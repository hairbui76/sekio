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
    fn human_size_matches_the_cli() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
