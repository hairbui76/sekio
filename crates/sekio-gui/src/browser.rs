//! The built-in file browser: a directory pane inside the window.
//!
//! This is the *guaranteed* way to open a file. The native dialog needs a
//! desktop portal (or zenity) that may not be running; this needs nothing but a
//! window, so it is what a failed dialog falls back to.
//!
//! It does no IO of its own. The listing is a `PreviewContent::Listing` from
//! core, produced on the worker thread like every other preview, so descending
//! into a directory of 100k files cannot stall a frame — and the "core does the
//! IO, frontends paint" rule stays intact. This module is only the state
//! machine around that: where we are, what is selected, and what a click means.

use std::path::{Path, PathBuf};

use sekio_core::ListEntry;

/// What activating the selected row should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activate {
    /// A directory: list it instead.
    Descend(PathBuf),
    /// A file: preview it.
    Preview(PathBuf),
}

#[derive(Debug, Default)]
pub struct Browser {
    open: bool,
    dir: PathBuf,
    entries: Vec<ListEntry>,
    /// Index into `matches` — a position in what is *on screen*, not in the
    /// full listing, so the cursor still means "the highlighted row" when a
    /// filter is hiding most of the directory.
    cursor: usize,
    /// What the user has typed into the search box.
    filter: String,
    /// Indices into `entries` that survive `filter`, best first. Equal to
    /// `0..entries.len()` when nothing is typed, which is the common case and
    /// costs one `collect`.
    matches: Vec<usize>,
    /// A listing is in flight for `dir`.
    loading: bool,
    /// The listing for `dir` came back unusable (unreadable directory).
    failed: bool,
    /// The search box should take the keyboard on the next frame it is drawn.
    /// Set when the pane opens, so it can be typed into straight away the way
    /// a launcher can, and cleared once the focus has been handed over.
    focus_search: bool,
}

impl Browser {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &[ListEntry] {
        &self.entries
    }

    /// What the pane should paint, in order: the entries that survive the
    /// filter. Rows the user can see and click are exactly these.
    pub fn visible(&self) -> impl Iterator<Item = &ListEntry> {
        self.matches.iter().filter_map(|i| self.entries.get(*i))
    }

    pub fn visible_len(&self) -> usize {
        self.matches.len()
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Type into the search box. The cursor goes back to the best match,
    /// because after a keystroke the row it used to point at is usually gone.
    pub fn set_filter(&mut self, filter: String) {
        if filter == self.filter {
            return;
        }
        self.filter = filter;
        self.rematch();
        self.cursor = 0;
    }

    /// Empty the search box. Returns whether there was anything to clear, so a
    /// caller can decide whether Escape meant "clear the filter" or "close the
    /// pane".
    pub fn clear_filter(&mut self) -> bool {
        if self.filter.is_empty() {
            return false;
        }
        self.set_filter(String::new());
        true
    }

    /// Recompute `matches` from `entries` and `filter`.
    fn rematch(&mut self) {
        if self.filter.trim().is_empty() {
            self.matches = (0..self.entries.len()).collect();
            return;
        }
        let needle = self.filter.trim();
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| score(&entry.name, needle).map(|s| (s, i)))
            .collect();
        // Best first, and ties keep the listing's own order — which is already
        // directories-then-names, so an exact-ish match never jumps below a
        // worse one just because it sorted later.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Open the pane on `dir` (or move an open pane to it). The caller must
    /// then ask the worker for the listing; until it lands the pane says
    /// "listing…" rather than showing the previous directory's contents, which
    /// would be a lie you could click on.
    /// Take (and clear) the pending request for keyboard focus.
    pub fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_search)
    }

    pub fn show(&mut self, dir: PathBuf) {
        self.open = true;
        self.focus_search = true;
        self.dir = dir;
        self.entries.clear();
        self.matches.clear();
        self.cursor = 0;
        // A search is about the directory it was typed in. Carrying it into
        // the next one hides most of that one for no reason the user asked
        // for.
        self.filter.clear();
        self.loading = true;
        self.failed = false;
    }

    /// Re-open the pane where it was last closed, keeping the listing on
    /// screen while the caller refreshes it — the directory may have changed
    /// while the pane was shut.
    pub fn reopen(&mut self) {
        self.open = true;
        self.focus_search = true;
        self.loading = true;
    }

    /// Accept a listing from the worker.
    ///
    /// The `dir` check is the same idea as the request generation counter: a
    /// listing that arrives after the user has moved on belongs to a directory
    /// we are no longer in, and painting it would be wrong.
    pub fn fill(&mut self, dir: &Path, entries: Vec<ListEntry>) -> bool {
        if dir != self.dir {
            return false;
        }
        self.entries = entries;
        self.rematch();
        // Clamp rather than reset: a refresh of the directory we are already
        // in must not throw the cursor back to the top. Entering a *new*
        // directory goes through `show`, which zeroes it.
        self.cursor = self.cursor.min(self.matches.len().saturating_sub(1));
        self.loading = false;
        self.failed = false;
        true
    }

    /// The listing for `dir` could not be produced. The pane stays open and
    /// says so; the parent button still works, so the user is never stuck.
    pub fn fail(&mut self, dir: &Path) -> bool {
        if dir != self.dir {
            return false;
        }
        self.entries.clear();
        self.matches.clear();
        self.cursor = 0;
        self.loading = false;
        self.failed = true;
        true
    }

    /// Move the cursor, clamped at both ends — never wrapping, so holding a
    /// key does not loop around a long directory, and never out of bounds on
    /// an empty or freshly-cleared list.
    pub fn move_cursor(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.cursor = 0;
            return;
        }
        let last = (self.matches.len() - 1) as isize;
        let next = (self.cursor as isize).saturating_add(delta).clamp(0, last);
        self.cursor = next as usize;
    }

    /// Point the cursor at a row (clicked). Out-of-range indices are clamped
    /// rather than ignored.
    pub fn select(&mut self, index: usize) {
        if self.matches.is_empty() {
            self.cursor = 0;
            return;
        }
        self.cursor = index.min(self.matches.len() - 1);
    }

    /// What the row at `index` means. Entry names come from core as lossy
    /// strings, so a name that was not valid UTF-8 yields a path that will fail
    /// to preview — visibly, in the window, rather than silently.
    pub fn activate(&self, index: usize) -> Option<Activate> {
        let entry = self.entries.get(*self.matches.get(index)?)?;
        let path = self.dir.join(&entry.name);
        Some(if entry.is_dir {
            Activate::Descend(path)
        } else {
            Activate::Preview(path)
        })
    }

    /// The directory above this one, or `None` at the root.
    pub fn parent(&self) -> Option<PathBuf> {
        self.dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
    }
}

/// fzf-style subsequence score for `name` against `needle`, or `None` when
/// `name` does not contain every character of `needle` in order.
///
/// Matching is a subsequence rather than a substring because that is what
/// makes typing `cts` find `components/tests` — the whole point of the box.
/// The score exists so the *best* of those is first, and it rewards the three
/// things that make a match feel intended rather than incidental:
///
/// * characters that ran together, so `read` beats `r..e..a..d`;
/// * a match at the start of a word — the beginning of the name, or just after
///   a separator — so `md` finds `my-doc` ahead of `formatted`;
/// * an early match, so a short name that starts with the query wins.
///
/// Case-insensitive, and ASCII-case-insensitive only: a fuzzy filter over
/// filenames does not need to reason about Turkish dotted i, and pulling in a
/// full case-folding table for it would be a lot of weight for a search box.
pub fn score(name: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut total = 0;
    let mut run = 0;
    let mut previous: Option<char> = None;
    let mut chars = name.chars().enumerate().peekable();

    for wanted in needle.chars() {
        let wanted = wanted.to_ascii_lowercase();
        loop {
            let (index, c) = chars.next()?;
            if c.to_ascii_lowercase() == wanted {
                total += 1;
                // Consecutive characters compound, so a solid run outscores
                // the same number of scattered hits.
                run += 1;
                total += run;
                if index == 0 {
                    total += 8;
                } else if previous.is_some_and(is_boundary) {
                    total += 4;
                }
                // Earlier is better, but only mildly: this must not outweigh a
                // genuinely tighter match later in a long name.
                total += (16 - index.min(16)) as i32 / 4;
                previous = Some(c);
                break;
            }
            run = 0;
            previous = Some(c);
        }
    }
    Some(total)
}

/// Is this the sort of character a new "word" starts after?
fn is_boundary(c: char) -> bool {
    matches!(c, '_' | '-' | '.' | ' ' | '/' | '\\')
}

/// Where to open the browser, given whatever is on screen.
///
/// Next to the file being previewed is nearly always what the user means; with
/// nothing on screen, their home directory; and if even that is unknown, the
/// working directory. Never `None`: the pane must always have somewhere to go.
pub fn start_dir(current: Option<&Path>) -> PathBuf {
    if let Some(path) = current.filter(|path| !path.as_os_str().is_empty()) {
        if path.is_dir() {
            return path.to_path_buf();
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            return parent.to_path_buf();
        }
    }
    home().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The user's home directory, from the environment only — no dependency, and
/// `None` rather than a guess when it says nothing.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// A path short enough for the pane header: `~` for the home directory, and
/// only the tail when it is still too long to fit a narrow pane.
pub fn compact(dir: &Path) -> String {
    compact_with(dir, home().as_deref())
}

/// The pure half of [`compact`], so the `~` rule is tested without an
/// environment.
pub fn compact_with(dir: &Path, home: Option<&Path>) -> String {
    const MAX: usize = 44;

    let text = match home.and_then(|home| dir.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    if text.chars().count() <= MAX {
        return text;
    }
    // Keep the end: the directory you are in matters more than the root.
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(MAX - 1))
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subsequence, not a substring — this is the whole reason the box is
    /// worth having over a plain "contains".
    #[test]
    fn a_scattered_subsequence_matches() {
        assert!(score("components/tests", "cts").is_some());
        assert!(score("Cargo.toml", "cgt").is_some());
        // Order still has to hold.
        assert!(score("abc", "cba").is_none());
        // And every character has to appear.
        assert!(score("abc", "abcd").is_none());
    }

    #[test]
    fn matching_ignores_case_either_way() {
        assert!(score("README.md", "rm").is_some());
        assert!(score("readme.md", "RM").is_some());
    }

    #[test]
    fn a_run_beats_the_same_characters_scattered() {
        let together = score("read", "rea").expect("matches");
        let apart = score("rxexax", "rea").expect("matches");
        assert!(together > apart, "{together} should beat {apart}");
    }

    #[test]
    fn a_word_boundary_beats_the_middle_of_a_word() {
        let boundary = score("my-doc", "md").expect("matches");
        let middle = score("formatted", "md").expect("matches");
        assert!(boundary > middle, "{boundary} should beat {middle}");
    }

    #[test]
    fn an_empty_needle_matches_everything() {
        assert_eq!(score("anything", ""), Some(0));
    }

    #[test]
    fn filtering_hides_rows_and_the_cursor_indexes_what_is_left() {
        let mut browser = Browser::default();
        browser.show(PathBuf::from("/tmp"));
        browser.fill(
            Path::new("/tmp"),
            vec![
                entry("alpha.txt", false),
                entry("beta.rs", false),
                entry("gamma.rs", false),
            ],
        );
        assert_eq!(browser.visible_len(), 3);

        browser.set_filter("rs".to_owned());
        assert_eq!(browser.visible_len(), 2, "only the two .rs files survive");

        // The cursor is a position in what is on screen, so activating row 1
        // must open the second *visible* entry, not the second of the listing.
        browser.select(1);
        assert_eq!(
            browser.activate(1),
            Some(Activate::Preview(PathBuf::from("/tmp/gamma.rs")))
        );
    }

    #[test]
    fn clearing_the_filter_brings_every_row_back() {
        let mut browser = Browser::default();
        browser.show(PathBuf::from("/tmp"));
        browser.fill(
            Path::new("/tmp"),
            vec![entry("one", false), entry("two", false)],
        );
        browser.set_filter("one".to_owned());
        assert_eq!(browser.visible_len(), 1);

        assert!(browser.clear_filter(), "there was a filter to clear");
        assert_eq!(browser.visible_len(), 2);
        assert!(
            !browser.clear_filter(),
            "an empty box reports nothing to clear, so Escape can close the pane"
        );
    }

    /// A search belongs to the directory it was typed in.
    #[test]
    fn moving_to_another_directory_drops_the_search() {
        let mut browser = Browser::default();
        browser.show(PathBuf::from("/tmp"));
        browser.fill(Path::new("/tmp"), vec![entry("one", false)]);
        browser.set_filter("one".to_owned());

        browser.show(PathBuf::from("/tmp/sub"));
        assert_eq!(browser.filter(), "");
    }

    #[test]
    fn a_cursor_past_the_filtered_end_is_clamped() {
        let mut browser = Browser::default();
        browser.show(PathBuf::from("/tmp"));
        browser.fill(
            Path::new("/tmp"),
            vec![
                entry("a.rs", false),
                entry("b.rs", false),
                entry("c.md", false),
            ],
        );
        browser.select(2);
        browser.set_filter("md".to_owned());
        assert_eq!(browser.visible_len(), 1);
        browser.move_cursor(5);
        assert_eq!(browser.cursor(), 0, "one row means the cursor is on it");
    }

    fn entry(name: &str, is_dir: bool) -> ListEntry {
        ListEntry {
            name: name.to_owned(),
            is_dir,
            size: if is_dir { None } else { Some(1) },
        }
    }

    fn listed() -> Browser {
        let mut browser = Browser::default();
        browser.show(PathBuf::from("/d"));
        assert!(browser.is_loading());
        assert!(browser.fill(
            Path::new("/d"),
            vec![
                entry("sub", true),
                entry("a.txt", false),
                entry("b.txt", false)
            ],
        ));
        browser
    }

    #[test]
    fn a_listing_for_another_directory_is_ignored() {
        let mut browser = listed();
        browser.show(PathBuf::from("/d/sub"));
        assert!(browser.entries().is_empty(), "the old listing is dropped");
        assert!(!browser.fill(Path::new("/d"), vec![entry("stale", false)]));
        assert!(browser.entries().is_empty());
        assert!(browser.is_loading(), "still waiting for the right listing");
        assert!(
            !browser.fail(Path::new("/d")),
            "a stale failure is ignored too"
        );
        assert!(browser.is_loading());
    }

    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let mut browser = listed();
        assert_eq!(browser.cursor(), 0);
        browser.move_cursor(-1);
        assert_eq!(browser.cursor(), 0, "clamped at the top");
        browser.move_cursor(1);
        browser.move_cursor(1);
        assert_eq!(browser.cursor(), 2);
        browser.move_cursor(1);
        assert_eq!(browser.cursor(), 2, "clamped at the bottom");
        browser.move_cursor(isize::MIN);
        assert_eq!(browser.cursor(), 0, "a huge step cannot overflow");
        browser.move_cursor(isize::MAX);
        assert_eq!(browser.cursor(), 2);
    }

    #[test]
    fn an_empty_or_failed_listing_has_no_selection() {
        let mut browser = Browser::default();
        browser.show(PathBuf::from("/d"));
        assert!(browser.fill(Path::new("/d"), vec![]));
        browser.move_cursor(5);
        assert_eq!(browser.cursor(), 0);
        assert!(browser.activate(browser.cursor()).is_none());

        assert!(browser.fail(Path::new("/d")));
        assert!(browser.has_failed());
        assert!(!browser.is_loading());
        assert!(browser.activate(browser.cursor()).is_none());
    }

    #[test]
    fn clicking_a_row_clamps_rather_than_panicking() {
        let mut browser = listed();
        browser.select(1);
        assert_eq!(
            browser.activate(browser.cursor()),
            Some(Activate::Preview(PathBuf::from("/d/a.txt")))
        );
        browser.select(99);
        assert_eq!(browser.cursor(), 2);
    }

    #[test]
    fn directories_descend_and_files_preview() {
        let browser = listed();
        assert_eq!(
            browser.activate(0),
            Some(Activate::Descend(PathBuf::from("/d/sub")))
        );
        assert_eq!(
            browser.activate(1),
            Some(Activate::Preview(PathBuf::from("/d/a.txt")))
        );
        assert_eq!(browser.activate(7), None);
    }

    #[test]
    fn the_root_has_no_parent() {
        let mut browser = listed();
        assert_eq!(browser.parent(), Some(PathBuf::from("/")));
        browser.show(PathBuf::from("/"));
        assert_eq!(browser.parent(), None, "there is nowhere above the root");
    }

    #[test]
    fn opening_and_closing_the_pane() {
        let mut browser = Browser::default();
        assert!(!browser.is_open());
        browser.show(PathBuf::from("/d"));
        assert!(browser.is_open());
        browser.close();
        assert!(!browser.is_open());
        assert_eq!(browser.dir(), Path::new("/d"), "closing keeps our place");
    }

    #[test]
    fn reopening_keeps_the_place_and_refreshes() {
        let mut browser = listed();
        browser.move_cursor(2);
        browser.close();
        browser.reopen();
        assert!(browser.is_open());
        assert!(
            browser.is_loading(),
            "a reopened pane refreshes its listing"
        );
        assert_eq!(
            browser.entries().len(),
            3,
            "and shows the old list meanwhile"
        );

        // The refresh comes back one entry shorter: the cursor clamps rather
        // than pointing past the end.
        assert!(browser.fill(
            Path::new("/d"),
            vec![entry("sub", true), entry("a.txt", false)]
        ));
        assert_eq!(browser.cursor(), 1);

        // A refresh that keeps the entry under the cursor keeps the cursor.
        assert!(browser.fill(
            Path::new("/d"),
            vec![
                entry("sub", true),
                entry("a.txt", false),
                entry("c.txt", false)
            ],
        ));
        assert_eq!(browser.cursor(), 1);
    }

    #[test]
    fn home_is_written_as_a_tilde_and_long_paths_are_trimmed() {
        let home = Path::new("/home/x");
        assert_eq!(compact_with(home, Some(home)), "~");
        assert_eq!(
            compact_with(Path::new("/home/x/docs"), Some(home)),
            "~/docs"
        );
        assert_eq!(compact_with(Path::new("/etc"), Some(home)), "/etc");
        assert_eq!(compact_with(Path::new("/etc"), None), "/etc");

        let deep = PathBuf::from("/a/very/long/".to_owned() + &"segment/".repeat(12));
        let short = compact_with(&deep, Some(home));
        assert!(short.chars().count() <= 44, "{short:?} still fits the pane");
        assert!(
            short.starts_with('…'),
            "the tail is what matters: {short:?}"
        );
        assert!(short.ends_with("segment/"));
    }

    #[test]
    fn start_dir_prefers_the_file_being_previewed() {
        let dir = std::env::temp_dir().join(format!("sekio-gui-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").expect("write fixture");

        assert_eq!(start_dir(Some(&file)), dir, "next to the file on screen");
        assert_eq!(start_dir(Some(&dir)), dir, "a directory opens itself");
        // Nothing on screen: somewhere absolute, never a panic.
        assert!(start_dir(None).is_absolute() || start_dir(None) == Path::new("."));
        assert!(!start_dir(Some(Path::new(""))).as_os_str().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
