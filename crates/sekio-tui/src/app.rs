//! All of the browser's state and logic, with no terminal dependency at all.
//!
//! Everything here is a plain struct with plain methods so the fiddly parts —
//! request staleness, cursor clamping, directory navigation, scroll clamping —
//! can be unit tested without driving a terminal. The event loop in `main.rs`
//! only translates key events into these calls and drains [`App::take_requests`].

use std::path::{Path, PathBuf};

use sekio_core::{CancelToken, ListEntry, Preview, PreviewContent};

use crate::worker::{Kind, Request, Response};

/// Tracks the newest request issued for one pane and the token that cancels it.
///
/// A single rule: issuing a new request cancels the previous one, and only a
/// result carrying the newest id is ever accepted. Ids start at 1 so the
/// default (`latest == 0`) accepts nothing.
#[derive(Debug, Default)]
pub struct RequestTracker {
    latest: u64,
    inflight: Option<CancelToken>,
}

impl RequestTracker {
    /// Cancel whatever is in flight and mint the id/token for its replacement.
    pub fn issue(&mut self) -> (u64, CancelToken) {
        self.cancel_inflight();
        self.latest += 1;
        let token = CancelToken::new();
        self.inflight = Some(token.clone());
        (self.latest, token)
    }

    pub fn cancel_inflight(&mut self) {
        if let Some(token) = self.inflight.take() {
            token.cancel();
        }
    }

    /// Is `id` the newest request we issued?
    pub fn is_current(&self, id: u64) -> bool {
        id != 0 && id == self.latest
    }

    /// Accept a result if it is current; stale results are dropped on the floor.
    pub fn accept(&mut self, id: u64) -> bool {
        if self.is_current(id) {
            self.inflight = None;
            true
        } else {
            false
        }
    }

    /// True while we are waiting on a result — drives the "loading…" placeholder.
    pub fn pending(&self) -> bool {
        self.inflight.is_some()
    }

    #[cfg(test)]
    pub fn latest(&self) -> u64 {
        self.latest
    }
}

#[derive(Debug)]
pub enum PreviewState {
    /// Nothing to preview (empty directory).
    Empty,
    Loading,
    Ready(Box<Preview>),
    Failed(String),
}

pub struct App {
    /// Directory the left pane is showing.
    pub dir: PathBuf,
    pub entries: Vec<ListEntry>,
    pub cursor: usize,
    /// Set when the listing itself could not be read.
    pub listing_error: Option<String>,
    pub listing_truncated: bool,

    pub preview: PreviewState,
    /// Bumped every time a *new* preview is accepted, so the image cache in the
    /// UI layer knows when to re-encode instead of comparing image buffers.
    pub preview_seq: u64,

    /// First visible row of the preview pane.
    pub scroll: usize,
    /// Height of the preview viewport in rows, refreshed by the renderer each
    /// frame so scroll clamping matches what is actually on screen.
    pub viewport: usize,

    pub listing_req: RequestTracker,
    pub preview_req: RequestTracker,

    /// Entry name to put the cursor on once the pending listing arrives — used
    /// for "start on this file" and for "go to parent, land on where we were".
    select_after_load: Option<String>,

    outbox: Vec<Request>,
    pub should_quit: bool,
}

impl App {
    /// `select` names an entry inside `dir` to start the cursor on.
    pub fn new(dir: PathBuf, select: Option<String>) -> Self {
        let mut app = Self {
            dir,
            entries: Vec::new(),
            cursor: 0,
            listing_error: None,
            listing_truncated: false,
            preview: PreviewState::Loading,
            preview_seq: 0,
            scroll: 0,
            viewport: 1,
            listing_req: RequestTracker::default(),
            preview_req: RequestTracker::default(),
            select_after_load: select,
            outbox: Vec::new(),
            should_quit: false,
        };
        app.request_listing();
        app
    }

    /// Drain the requests produced since the last call; the caller hands them
    /// to the worker. Keeping them in an outbox is what lets the whole state
    /// machine be tested with no channels involved.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.outbox)
    }

    // ---- navigation -----------------------------------------------------

    pub fn selected(&self) -> Option<&ListEntry> {
        self.entries.get(self.cursor)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected().map(|e| self.dir.join(&e.name))
    }

    /// Move the cursor by `delta`, clamped to the list bounds. Re-previews only
    /// when the cursor actually moved.
    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta as usize).min(last)
        };
        if next != self.cursor {
            self.cursor = next;
            self.request_preview();
        }
    }

    pub fn select_first(&mut self) {
        self.select_index(0);
    }

    pub fn select_last(&mut self) {
        self.select_index(self.entries.len().saturating_sub(1));
    }

    fn select_index(&mut self, index: usize) {
        if self.entries.is_empty() {
            return;
        }
        let index = index.min(self.entries.len() - 1);
        if index != self.cursor {
            self.cursor = index;
            self.request_preview();
        }
    }

    /// Descend into the selected entry when it is a directory. Files do nothing
    /// — sekio is strictly a viewer.
    pub fn enter(&mut self) {
        let Some(entry) = self.selected() else {
            return;
        };
        if !entry.is_dir {
            return;
        }
        let target = self.dir.join(&entry.name);
        self.goto(target, None);
    }

    /// Go to the parent directory, landing the cursor on the directory we just
    /// left. At a filesystem root this is a no-op.
    pub fn go_parent(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return;
        };
        let leaving = self
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.goto(parent, leaving);
    }

    fn goto(&mut self, dir: PathBuf, select: Option<String>) {
        self.dir = dir;
        self.entries.clear();
        self.cursor = 0;
        self.listing_error = None;
        self.listing_truncated = false;
        self.select_after_load = select;
        // Whatever was being previewed is now irrelevant.
        self.preview_req.cancel_inflight();
        self.preview = PreviewState::Loading;
        self.reset_scroll();
        self.request_listing();
    }

    /// Re-read the current directory and the current preview.
    pub fn reload(&mut self) {
        self.select_after_load = self.selected().map(|e| e.name.clone());
        self.request_listing();
    }

    // ---- requests -------------------------------------------------------

    fn request_listing(&mut self) {
        let (id, cancel) = self.listing_req.issue();
        let path = self.dir.clone();
        self.outbox.push(Request {
            id,
            kind: Kind::Listing,
            path,
            cancel,
        });
    }

    fn request_preview(&mut self) {
        self.reset_scroll();
        match self.selected_path() {
            Some(path) => {
                let (id, cancel) = self.preview_req.issue();
                self.preview = PreviewState::Loading;
                self.outbox.push(Request {
                    id,
                    kind: Kind::Preview,
                    path,
                    cancel,
                });
            }
            None => {
                self.preview_req.cancel_inflight();
                self.preview = PreviewState::Empty;
            }
        }
    }

    /// Feed a worker result back in. Stale results are discarded here.
    pub fn on_response(&mut self, response: Response) {
        match response.kind {
            Kind::Listing => {
                if !self.listing_req.accept(response.id) {
                    return;
                }
                match response.result {
                    Ok(preview) => {
                        self.listing_truncated = preview.truncated;
                        self.listing_error = None;
                        let entries = match preview.content {
                            PreviewContent::Listing { entries } => entries,
                            // The start path was validated as a directory, so
                            // this only happens if it changed underneath us.
                            _ => Vec::new(),
                        };
                        self.set_entries(entries);
                    }
                    Err(msg) => {
                        self.entries.clear();
                        self.cursor = 0;
                        self.listing_error = Some(msg);
                        self.preview = PreviewState::Empty;
                        self.preview_req.cancel_inflight();
                    }
                }
            }
            Kind::Preview => {
                if !self.preview_req.accept(response.id) {
                    return;
                }
                self.preview_seq += 1;
                self.reset_scroll();
                self.preview = match response.result {
                    Ok(preview) => PreviewState::Ready(Box::new(preview)),
                    Err(msg) => PreviewState::Failed(msg),
                };
            }
        }
    }

    /// Install a fresh listing, honouring a pending "select this name" request
    /// and clamping the cursor if the directory shrank underneath us.
    pub fn set_entries(&mut self, entries: Vec<ListEntry>) {
        self.entries = entries;
        self.cursor = match self.select_after_load.take() {
            Some(name) => self
                .entries
                .iter()
                .position(|e| e.name == name)
                .unwrap_or(0),
            None => self.cursor,
        };
        if self.cursor >= self.entries.len() {
            self.cursor = self.entries.len().saturating_sub(1);
        }
        self.request_preview();
    }

    // ---- scrolling ------------------------------------------------------

    fn reset_scroll(&mut self) {
        self.scroll = 0;
    }

    /// Number of rows the current preview occupies. Images are widget-rendered
    /// and never scroll.
    pub fn content_len(&self) -> usize {
        match &self.preview {
            PreviewState::Ready(preview) => content_len(preview),
            _ => 0,
        }
    }

    /// Largest valid scroll offset: always keep at least one row on screen.
    pub fn max_scroll(&self) -> usize {
        self.content_len().saturating_sub(self.viewport.max(1))
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let next = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll = next.min(self.max_scroll());
    }

    /// A "page" is the viewport minus one row of overlap for context.
    pub fn page(&self) -> isize {
        (self.viewport.max(2) - 1) as isize
    }

    pub fn half_page(&self) -> isize {
        (self.page() / 2).max(1)
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// Called by the renderer with the real pane height; re-clamps in case the
    /// window grew and left the view scrolled past the end.
    pub fn set_viewport(&mut self, height: usize) {
        self.viewport = height.max(1);
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn is_loading(&self) -> bool {
        self.preview_req.pending() || self.listing_req.pending()
    }
}

/// Rows the preview would occupy if fully painted.
pub fn content_len(preview: &Preview) -> usize {
    let extra = usize::from(preview.truncated);
    match &preview.content {
        PreviewContent::Text { lines, .. } => lines.len() + extra,
        PreviewContent::Listing { entries } => entries.len() + extra,
        PreviewContent::Metadata { fields, .. } => fields.len(),
        PreviewContent::HexDump { data, .. } => data.len().div_ceil(16) + extra,
        // Rendered as a widget into the whole pane; scrolling is meaningless.
        PreviewContent::Image { .. } => 0,
    }
}

/// Split a CLI path into "directory to list" plus "entry to select". A file
/// starts the browser in its parent with the file under the cursor.
///
/// The directory is canonicalised where possible so `.` has a real parent to
/// walk up into. `std::path` throughout — no separator assumptions.
pub fn start_location(path: &Path) -> std::io::Result<(PathBuf, Option<String>)> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Ok((absolutise(path), None));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let parent = match path.parent() {
        // `Path::new("foo.txt").parent()` is `Some("")`, which is not a usable
        // directory — that means "the current directory".
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((absolutise(&parent), name))
}

fn absolutise(path: &Path) -> PathBuf {
    // Through core rather than `fs::canonicalize` directly: on Windows the raw
    // call returns a verbatim `\\?\C:\...` path, and this one is rendered in
    // the pane header.
    sekio_core::paths::canonical(path).unwrap_or_else(|_| sekio_core::paths::plain(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekio_core::{Span, StyledLine};

    fn entry(name: &str, is_dir: bool) -> ListEntry {
        ListEntry {
            name: name.to_owned(),
            is_dir,
            size: if is_dir { None } else { Some(1) },
        }
    }

    fn listing(names: &[(&str, bool)]) -> Preview {
        Preview {
            content: PreviewContent::Listing {
                entries: names.iter().map(|(n, d)| entry(n, *d)).collect(),
            },
            truncated: false,
        }
    }

    fn text(lines: usize) -> Preview {
        Preview {
            content: PreviewContent::Text {
                lines: (0..lines)
                    .map(|i| StyledLine {
                        spans: vec![Span {
                            text: format!("line {i}"),
                            fg: None,
                            bold: false,
                            italic: false,
                        }],
                    })
                    .collect(),
                language: "Rust".to_owned(),
            },
            truncated: false,
        }
    }

    /// Build an app already showing `names`, with the outbox drained.
    fn app_with(names: &[(&str, bool)]) -> App {
        let mut app = App::new(PathBuf::from("/tmp/root"), None);
        let id = app.listing_req.latest();
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(names)),
        });
        app.take_requests();
        app
    }

    // ---- generation / staleness ----

    /// Regression guard: the very first listing must be requested at startup
    /// *and* accepted when it comes back. An off-by-one in the id numbering
    /// (ids starting at 0, say) would discard it as stale and leave the browser
    /// showing an empty pane forever — invisible without a pty.
    #[test]
    fn the_first_listing_is_requested_and_accepted() {
        let mut app = App::new(PathBuf::from("/tmp/root"), None);
        let requests = app.take_requests();
        assert_eq!(requests.len(), 1, "startup must issue exactly one request");
        assert_eq!(requests[0].kind, Kind::Listing);
        assert_eq!(requests[0].path, PathBuf::from("/tmp/root"));
        assert!(!requests[0].cancel.is_cancelled());
        assert!(
            app.listing_req.is_current(requests[0].id),
            "the first request must be the current generation"
        );

        app.on_response(Response {
            id: requests[0].id,
            kind: Kind::Listing,
            result: Ok(listing(&[("sub", true), ("a.txt", false)])),
        });
        assert_eq!(app.entries.len(), 2, "the first listing must be accepted");
        assert_eq!(app.cursor, 0);

        // …and it must immediately ask for the preview of the first entry.
        let follow_up = app.take_requests();
        assert_eq!(follow_up.len(), 1);
        assert_eq!(follow_up[0].kind, Kind::Preview);
        assert!(app.preview_req.is_current(follow_up[0].id));

        app.on_response(Response {
            id: follow_up[0].id,
            kind: Kind::Preview,
            result: Ok(text(2)),
        });
        assert!(
            matches!(app.preview, PreviewState::Ready(_)),
            "the first preview must be accepted too"
        );
    }

    #[test]
    fn tracker_accepts_current_and_rejects_stale() {
        let mut tracker = RequestTracker::default();
        let (first, _) = tracker.issue();
        let (second, _) = tracker.issue();
        assert!(second > first);
        assert!(!tracker.is_current(first));
        assert!(tracker.is_current(second));
        assert!(!tracker.accept(first));
        assert!(tracker.accept(second));
    }

    #[test]
    fn tracker_accepts_nothing_before_any_request() {
        let tracker = RequestTracker::default();
        assert!(!tracker.is_current(0));
        assert!(!tracker.is_current(1));
    }

    #[test]
    fn issuing_cancels_the_previous_token() {
        let mut tracker = RequestTracker::default();
        let (_, first_token) = tracker.issue();
        assert!(!first_token.is_cancelled());
        let (_, second_token) = tracker.issue();
        assert!(first_token.is_cancelled(), "old request must be cancelled");
        assert!(!second_token.is_cancelled());
        assert!(tracker.pending());
    }

    #[test]
    fn moving_the_cursor_cancels_the_inflight_preview() {
        let mut app = app_with(&[("a.txt", false), ("b.txt", false), ("c.txt", false)]);
        let first = app.take_requests();
        assert_eq!(first.len(), 0, "outbox already drained by app_with");

        app.move_cursor(1);
        let reqs = app.take_requests();
        assert_eq!(reqs.len(), 1);
        let first_token = reqs[0].cancel.clone();
        assert!(!first_token.is_cancelled());

        app.move_cursor(1);
        assert!(
            first_token.is_cancelled(),
            "navigating must cancel the in-flight preview immediately"
        );
    }

    #[test]
    fn stale_preview_result_is_discarded() {
        let mut app = app_with(&[("a.txt", false), ("b.txt", false)]);
        app.move_cursor(1);
        let stale_id = app.take_requests()[0].id;
        app.move_cursor(-1);
        let fresh_id = app.take_requests()[0].id;

        app.on_response(Response {
            id: stale_id,
            kind: Kind::Preview,
            result: Ok(text(3)),
        });
        assert!(
            matches!(app.preview, PreviewState::Loading),
            "a stale result must never be rendered"
        );

        app.on_response(Response {
            id: fresh_id,
            kind: Kind::Preview,
            result: Ok(text(3)),
        });
        assert!(matches!(app.preview, PreviewState::Ready(_)));
        assert_eq!(app.preview_seq, 1, "only the accepted result bumps the seq");
    }

    #[test]
    fn stale_listing_result_is_discarded() {
        let mut app = App::new(PathBuf::from("/tmp/root"), None);
        let stale_id = app.take_requests()[0].id;
        app.reload();
        let fresh_id = app.take_requests()[0].id;

        app.on_response(Response {
            id: stale_id,
            kind: Kind::Listing,
            result: Ok(listing(&[("ghost", false)])),
        });
        assert!(app.entries.is_empty());

        app.on_response(Response {
            id: fresh_id,
            kind: Kind::Listing,
            result: Ok(listing(&[("real", false)])),
        });
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].name, "real");
    }

    #[test]
    fn preview_failure_renders_as_a_message_not_a_crash() {
        let mut app = app_with(&[("a.bin", false)]);
        let id = app.preview_req.latest();
        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Err("io error: permission denied".to_owned()),
        });
        match &app.preview {
            PreviewState::Failed(msg) => assert!(msg.contains("permission denied")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // ---- navigation state machine ----

    #[test]
    fn cursor_clamps_at_both_ends() {
        let mut app = app_with(&[("a", false), ("b", false), ("c", false)]);
        app.move_cursor(-1);
        assert_eq!(app.cursor, 0);
        app.move_cursor(10);
        assert_eq!(app.cursor, 2);
        app.move_cursor(1);
        assert_eq!(app.cursor, 2);
        app.select_first();
        assert_eq!(app.cursor, 0);
        app.select_last();
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn cursor_moves_are_noops_on_an_empty_listing() {
        let mut app = app_with(&[]);
        app.move_cursor(1);
        app.select_last();
        assert_eq!(app.cursor, 0);
        assert!(app.selected().is_none());
        assert!(matches!(app.preview, PreviewState::Empty));
    }

    #[test]
    fn enter_descends_into_a_directory_only() {
        let mut app = app_with(&[("sub", true), ("file.txt", false)]);
        app.enter();
        assert_eq!(app.dir, PathBuf::from("/tmp/root").join("sub"));
        let reqs = app.take_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].kind, Kind::Listing);

        let id = reqs[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(&[("file.txt", false)])),
        });
        app.take_requests();

        // Cursor is on a file now — Enter must not move anywhere.
        let before = app.dir.clone();
        app.enter();
        assert_eq!(app.dir, before);
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn parent_navigation_selects_the_directory_we_came_from() {
        let mut app = App::new(PathBuf::from("/tmp/root/sub"), None);
        app.take_requests();
        app.go_parent();
        assert_eq!(app.dir, PathBuf::from("/tmp/root"));

        let id = app.take_requests()[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(&[("other", true), ("sub", true), ("z.txt", false)])),
        });
        assert_eq!(app.cursor, 1, "cursor should land on `sub`");
        assert_eq!(app.selected().map(|e| e.name.as_str()), Some("sub"));
    }

    #[test]
    fn parent_at_the_root_is_a_noop() {
        let root = {
            let mut p = PathBuf::from("/tmp/root");
            while let Some(parent) = p.parent().map(Path::to_path_buf) {
                p = parent;
            }
            p
        };
        let mut app = App::new(root.clone(), None);
        app.take_requests();
        app.go_parent();
        assert_eq!(app.dir, root);
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn initial_selection_is_applied_when_the_listing_arrives() {
        let mut app = App::new(PathBuf::from("/tmp/root"), Some("b.txt".to_owned()));
        let id = app.take_requests()[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(&[("a.txt", false), ("b.txt", false)])),
        });
        assert_eq!(app.cursor, 1);
        let reqs = app.take_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].kind, Kind::Preview);
        assert!(reqs[0].path.ends_with("b.txt"));
    }

    #[test]
    fn missing_selection_falls_back_to_the_first_entry() {
        let mut app = App::new(PathBuf::from("/tmp/root"), Some("gone.txt".to_owned()));
        let id = app.take_requests()[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(&[("a.txt", false)])),
        });
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn cursor_is_clamped_when_the_directory_shrinks() {
        let mut app = app_with(&[("a", false), ("b", false), ("c", false)]);
        app.select_last();
        assert_eq!(app.cursor, 2);
        app.take_requests();

        app.reload();
        let id = app.take_requests()[0].id;
        // `c` disappeared and the remembered name is gone, so we fall back to 0.
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(&[("a", false)])),
        });
        assert_eq!(app.cursor, 0);
        assert!(app.cursor < app.entries.len());
    }

    #[test]
    fn listing_error_empties_the_pane_without_panicking() {
        let mut app = App::new(PathBuf::from("/nope"), None);
        let id = app.take_requests()[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Err("io error: no such directory".to_owned()),
        });
        assert!(app.entries.is_empty());
        assert!(app.listing_error.is_some());
        assert!(matches!(app.preview, PreviewState::Empty));
    }

    // ---- scroll clamping ----

    fn app_showing(preview: Preview, viewport: usize) -> App {
        let mut app = app_with(&[("a.txt", false)]);
        let id = app.preview_req.latest();
        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Ok(preview),
        });
        app.set_viewport(viewport);
        app
    }

    #[test]
    fn scroll_clamps_to_the_last_page() {
        let mut app = app_showing(text(100), 10);
        assert_eq!(app.max_scroll(), 90);
        app.scroll_by(1000);
        assert_eq!(app.scroll, 90);
        app.scroll_by(-1000);
        assert_eq!(app.scroll, 0);
        app.scroll_to_bottom();
        assert_eq!(app.scroll, 90);
        app.scroll_to_top();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn short_content_never_scrolls() {
        let mut app = app_showing(text(3), 10);
        assert_eq!(app.max_scroll(), 0);
        app.scroll_by(5);
        assert_eq!(app.scroll, 0);
        app.scroll_to_bottom();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn growing_the_viewport_reclamps_the_offset() {
        let mut app = app_showing(text(50), 10);
        app.scroll_to_bottom();
        assert_eq!(app.scroll, 40);
        app.set_viewport(50);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn selecting_another_file_resets_the_scroll() {
        let mut app = app_with(&[("a.txt", false), ("b.txt", false)]);
        let id = app.preview_req.latest();
        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Ok(text(100)),
        });
        app.set_viewport(10);
        app.scroll_to_bottom();
        assert!(app.scroll > 0);
        app.move_cursor(1);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn hexdump_and_image_content_lengths() {
        let hex = Preview {
            content: PreviewContent::HexDump {
                data: vec![0u8; 33],
                file_size: 33,
                mime: None,
            },
            truncated: true,
        };
        // 33 bytes is three 16-byte rows, plus the "truncated" marker row.
        assert_eq!(content_len(&hex), 4);

        let img = Preview {
            content: PreviewContent::Image {
                image: image::RgbaImage::new(1, 1),
                original_width: 1,
                original_height: 1,
                format: "PNG".to_owned(),
                fields: Vec::new(),
            },
            truncated: false,
        };
        assert_eq!(content_len(&img), 0);
    }

    #[test]
    fn page_sizes_are_sane_for_tiny_viewports() {
        let mut app = app_showing(text(100), 1);
        assert!(app.page() >= 1);
        assert!(app.half_page() >= 1);
        app.set_viewport(0);
        assert!(app.page() >= 1);
        assert!(app.half_page() >= 1);
    }
}
