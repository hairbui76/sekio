//! All of the browser's state and logic, with no terminal dependency at all.
//!
//! Everything here is a plain struct with plain methods so the fiddly parts —
//! request staleness, cursor clamping, directory navigation, scroll clamping —
//! can be unit tested without driving a terminal. The event loop in `main.rs`
//! only translates key events into these calls and drains [`App::take_requests`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sekio_core::{CancelToken, ListEntry, Preview, PreviewContent, Reflow};

use crate::table;
use crate::worker::{Kind, Request, Response};

/// How many columns the pane has to gain or lose before the preview is worth
/// laying out again. Under four characters the difference is at most a
/// character or two spread over a handful of columns — invisible — while a
/// one-column threshold would re-render on every step of a window drag.
const REFLOW_THRESHOLD: usize = 4;

/// How long a new pane width has to hold still before we act on it. Long
/// enough that dragging a terminal's edge across fifty columns costs one
/// render rather than fifty, short enough that letting go feels immediate.
const REFLOW_SETTLE: Duration = Duration::from_millis(120);

/// Pane width assumed before a frame has ever measured one. Only reached on the
/// tick between the first preview landing and the first paint.
const DEFAULT_PANE_WIDTH: usize = 80;

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
    /// Width of the preview viewport in columns, refreshed the same way. Core
    /// still lays *text* out for this, so a resized terminal has to re-request
    /// the preview — see [`App::poll_reflow_at`]. A table is laid out by the
    /// renderer instead, per frame, so for that variant this width is only what
    /// [`content_len`] measures the scrollback against.
    preview_width: Option<usize>,
    /// Decides when a width change is worth a new render.
    reflow: Reflow,
    /// The in-flight preview is a re-layout of the file already on screen, not
    /// a move to another one: keep the scroll position when it lands.
    reflowing: bool,

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
            preview_width: None,
            reflow: Reflow::new(REFLOW_THRESHOLD, REFLOW_SETTLE),
            reflowing: false,
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
            // A listing has no columns to lay out.
            text_width: None,
        });
    }

    fn request_preview(&mut self) {
        self.reset_scroll();
        match self.selected_path() {
            Some(path) => {
                self.reflowing = false;
                self.preview = PreviewState::Loading;
                self.send_preview(path);
            }
            None => {
                self.preview_req.cancel_inflight();
                self.preview = PreviewState::Empty;
            }
        }
    }

    /// Re-request the file already on screen because the pane changed width.
    ///
    /// Deliberately *not* `request_preview`: the user has not moved, so the
    /// scroll position stays and the pane keeps painting what it has instead of
    /// flashing "loading…" behind a resize. It still goes through
    /// `RequestTracker::issue`, so a render already in flight is cancelled and
    /// its result discarded like any other superseded one.
    fn request_reflow(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        self.reflowing = true;
        self.send_preview(path);
    }

    fn send_preview(&mut self, path: PathBuf) {
        let (id, cancel) = self.preview_req.issue();
        // Whatever width we asked for is now the one on screen, so the next
        // resize is measured against it.
        if let Some(width) = self.preview_width {
            self.reflow.issued(width);
        }
        self.outbox.push(Request {
            id,
            kind: Kind::Preview,
            path,
            cancel,
            text_width: self.preview_width,
        });
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
                // A re-layout of the same file keeps the reader where they
                // were; moving to another file starts at the top.
                if !std::mem::take(&mut self.reflowing) {
                    self.reset_scroll();
                }
                self.preview = match response.result {
                    Ok(preview) => PreviewState::Ready(Box::new(preview)),
                    Err(msg) => PreviewState::Failed(msg),
                };
                // The re-laid-out preview may be shorter than the old one.
                self.scroll = self.scroll.min(self.max_scroll());
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
            PreviewState::Ready(preview) => content_len(preview, self.pane_width()),
            _ => 0,
        }
    }

    /// The pane width the last frame measured. Only a table's height depends on
    /// it (a narrow pane drops columns, which adds the note row saying so), and
    /// before the first frame there is nothing on screen to scroll anyway.
    fn pane_width(&self) -> usize {
        self.preview_width.unwrap_or(DEFAULT_PANE_WIDTH)
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

    /// Called by the renderer with the real pane width. Only records it — the
    /// decision to act belongs to [`App::poll_reflow`], which the event loop
    /// calls on every tick whether or not a frame was drawn.
    pub fn set_preview_width(&mut self, width: usize) {
        self.preview_width = Some(width.max(1));
    }

    /// Re-request the preview when the pane has settled at a materially
    /// different width. Called once per event-loop tick; `true` means a
    /// request went out.
    pub fn poll_reflow(&mut self) -> bool {
        self.poll_reflow_at(Instant::now())
    }

    /// The clock is a parameter so the rule can be tested without a terminal.
    pub fn poll_reflow_at(&mut self, now: Instant) -> bool {
        let Some(width) = self.preview_width else {
            return false;
        };
        // Nothing on screen to re-lay-out.
        let PreviewState::Ready(preview) = &self.preview else {
            return false;
        };
        // A table is laid out by the renderer against the pane it actually has,
        // per frame, so a resize needs no new preview at all. Re-reading the
        // workbook would produce byte-identical IR — the widths are decided in
        // `crate::table`, not in core. Still fed through `observe`, which
        // records the new width, so nothing fires the moment the reader moves
        // on to a file core *does* lay out.
        let is_table = matches!(preview.content, PreviewContent::Table { .. });
        if self.reflow.observe(width, now).is_none() || is_table {
            return false;
        }
        self.request_reflow();
        true
    }

    pub fn is_loading(&self) -> bool {
        self.preview_req.pending() || self.listing_req.pending()
    }
}

/// Rows the preview would occupy if fully painted.
///
/// `width` is the preview pane's width in columns. Only a table needs it: how
/// many of its columns fit decides whether there is a note under it saying what
/// was left out, and that note is a row like any other. Deriving the table's
/// chrome here from the same helpers the painter uses is what keeps the two
/// from disagreeing about where the last scrollable row is.
pub fn content_len(preview: &Preview, width: usize) -> usize {
    let extra = usize::from(preview.truncated);
    match &preview.content {
        PreviewContent::Table {
            columns,
            rows,
            sheets,
            total_rows,
            total_cols,
            ..
        } => {
            let gutter = table::gutter_width(rows);
            let seated = table::seated_columns(columns.len(), width, gutter);
            let note = table::note(
                rows.len(),
                seated,
                *total_rows,
                *total_cols,
                preview.truncated,
            );
            table::chrome_rows(sheets, note.as_ref()) + rows.len()
        }
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
    use sekio_core::{CellKind, Span, StyledLine, TableCell, TableRow};

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
        assert_eq!(content_len(&hex, 80), 4);

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
        assert_eq!(content_len(&img, 80), 0);
    }

    fn table(rows: usize, cols: usize, total_rows: u64, truncated: bool) -> Preview {
        Preview {
            content: PreviewContent::Table {
                columns: (0..cols).map(|i| format!("C{i}")).collect(),
                rows: (0..rows)
                    .map(|r| TableRow {
                        label: (r + 1).to_string(),
                        cells: (0..cols)
                            .map(|c| TableCell {
                                text: format!("r{r}c{c}"),
                                kind: CellKind::Text,
                            })
                            .collect(),
                    })
                    .collect(),
                sheets: vec!["Data".to_owned(), "Notes".to_owned()],
                active_sheet: 0,
                total_rows,
                total_cols: cols as u64,
            },
            truncated,
        }
    }

    /// The whole sheet is on screen: a sheet strip and a heading row of chrome,
    /// no note.
    #[test]
    fn a_whole_table_costs_its_rows_plus_two_chrome_rows() {
        let preview = table(20, 3, 20, false);
        assert_eq!(content_len(&preview, 80), 22);
    }

    /// A sheet that goes on past what core gave us gets a note row, and that
    /// row is part of the scrollback like any other.
    #[test]
    fn a_truncated_table_reserves_a_row_for_the_note() {
        let preview = table(20, 3, 5_000, true);
        assert_eq!(content_len(&preview, 80), 23);
    }

    /// A pane too narrow to seat every column also needs the note — the height
    /// of a table is not a property of the table alone.
    #[test]
    fn a_narrow_pane_adds_the_note_row_a_wide_one_does_not_need() {
        let wide = table(20, 6, 20, false);
        assert_eq!(content_len(&wide, 120), 22, "everything fits: no note");
        assert_eq!(
            content_len(&wide, 16),
            23,
            "columns dropped for width must be reported"
        );
    }

    /// Scrolling a table is scrolling like every other content type: the keys
    /// in `main.rs` do not know a table from a hexdump.
    #[test]
    fn a_table_scrolls_and_clamps_like_any_other_preview() {
        let mut app = app_showing(table(500, 4, 500, false), 25);
        app.set_preview_width(100);
        // 500 rows + 2 chrome rows, in a 25-row pane.
        assert_eq!(app.content_len(), 502);
        assert_eq!(app.max_scroll(), 477);
        app.scroll_by(10_000);
        assert_eq!(app.scroll, 477);
        app.scroll_to_top();
        assert_eq!(app.scroll, 0);
        app.scroll_by(app.page());
        assert_eq!(app.scroll, 24);
        app.scroll_to_bottom();
        assert_eq!(app.scroll, 477);
    }

    /// A ragged sheet — rows shorter than the column list, an empty sheet, no
    /// sheet names at all — must be measurable without panicking.
    #[test]
    fn a_ragged_or_empty_table_is_measured_without_panicking() {
        let ragged = Preview {
            content: PreviewContent::Table {
                columns: vec!["A".to_owned(), "B".to_owned(), "C".to_owned()],
                rows: vec![
                    TableRow {
                        label: "1".to_owned(),
                        cells: vec![TableCell {
                            text: "only one".to_owned(),
                            kind: CellKind::Text,
                        }],
                    },
                    TableRow::default(),
                ],
                sheets: Vec::new(),
                active_sheet: 9,
                total_rows: 0,
                total_cols: 0,
            },
            truncated: false,
        };
        assert_eq!(content_len(&ragged, 40), 3, "no sheet strip, no note");
        assert_eq!(
            content_len(&ragged, 0),
            3,
            "a zero-width pane is survivable"
        );
    }

    // ---- reflow on resize ----

    /// An app showing a preview, with the pane already measured at `width`
    /// and the reflow tracker settled on it.
    fn app_at_width(width: usize) -> App {
        let mut app = app_with(&[("sheet.xlsx", false)]);
        app.set_preview_width(width);
        // Re-request so the tracker records the width we just measured, the
        // way the first real frame does.
        app.reload();
        let id = app.take_requests()[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Listing,
            result: Ok(listing(&[("sheet.xlsx", false)])),
        });
        let id = app.take_requests()[0].id;
        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Ok(text(50)),
        });
        app.set_viewport(20);
        app
    }

    /// The requested width really is the pane width the renderer measured —
    /// that is the whole point of the hint.
    #[test]
    fn a_preview_request_carries_the_pane_width() {
        let mut app = app_with(&[("a.xlsx", false), ("b.xlsx", false)]);
        app.set_preview_width(137);
        app.move_cursor(1);
        let reqs = app.take_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].text_width, Some(137));
        // A directory listing has no columns and asks for no width.
        app.reload();
        let listing = app.take_requests();
        assert_eq!(listing[0].kind, Kind::Listing);
        assert_eq!(listing[0].text_width, None);
    }

    #[test]
    fn a_small_resize_does_not_re_render() {
        let mut app = app_at_width(100);
        let start = Instant::now();
        app.set_preview_width(103);
        assert!(!app.poll_reflow_at(start));
        assert!(!app.poll_reflow_at(start + Duration::from_secs(5)));
        assert!(
            app.take_requests().is_empty(),
            "three columns is not worth a render"
        );
    }

    #[test]
    fn a_real_resize_re_renders_once_it_settles() {
        let mut app = app_at_width(100);
        let start = Instant::now();
        app.set_preview_width(200);

        assert!(!app.poll_reflow_at(start), "not settled yet");
        assert!(app.take_requests().is_empty());

        assert!(app.poll_reflow_at(start + Duration::from_millis(150)));
        let reqs = app.take_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].kind, Kind::Preview);
        assert_eq!(
            reqs[0].text_width,
            Some(200),
            "the new request must carry the new width"
        );
        assert!(reqs[0].path.ends_with("sheet.xlsx"));

        // Exactly once: the pane has not moved again.
        assert!(!app.poll_reflow_at(start + Duration::from_secs(5)));
        assert!(app.take_requests().is_empty());
    }

    /// The re-render goes through the same generation counter as everything
    /// else, so a render still in flight is abandoned rather than painted.
    #[test]
    fn a_reflow_cancels_the_render_it_supersedes() {
        let mut app = app_at_width(100);
        app.move_cursor(0); // no-op; keeps the outbox clean
        app.take_requests();

        let start = Instant::now();
        app.set_preview_width(200);
        assert!(!app.poll_reflow_at(start), "the clock starts here");
        assert!(app.poll_reflow_at(start + Duration::from_millis(150)));
        let first = app.take_requests().remove(0);
        assert!(!first.cancel.is_cancelled());

        app.set_preview_width(60);
        assert!(!app.poll_reflow_at(start + Duration::from_millis(300)));
        assert!(app.poll_reflow_at(start + Duration::from_millis(500)));
        assert!(
            first.cancel.is_cancelled(),
            "the superseded render must be cancelled, not left running"
        );
        // …and its result, if it lands anyway, is dropped.
        app.on_response(Response {
            id: first.id,
            kind: Kind::Preview,
            result: Ok(text(1)),
        });
        assert_eq!(
            app.content_len(),
            50,
            "the stale result must not be painted"
        );
    }

    /// Resizing is not navigating: the reader stays where they were reading.
    #[test]
    fn a_reflow_keeps_the_scroll_position() {
        let mut app = app_at_width(100);
        app.scroll_by(10);
        assert_eq!(app.scroll, 10);

        let start = Instant::now();
        app.set_preview_width(200);
        assert!(!app.poll_reflow_at(start), "the clock starts here");
        assert!(app.poll_reflow_at(start + Duration::from_millis(150)));
        let id = app.take_requests()[0].id;
        // Still painting the old preview while the new one renders.
        assert!(matches!(app.preview, PreviewState::Ready(_)));

        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Ok(text(50)),
        });
        assert_eq!(app.scroll, 10, "a resize must not scroll the reader away");
        // Moving to another file still starts at the top — see
        // `selecting_another_file_resets_the_scroll`.
    }

    /// Core no longer lays a table out, so re-reading the workbook on every
    /// resize would cost a full parse to produce byte-identical IR. The pane
    /// re-lays it out itself, for free, every frame.
    #[test]
    fn resizing_a_table_re_lays_it_out_without_re_reading_the_workbook() {
        let mut app = app_at_width(100);
        let id = app.preview_req.latest();
        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Ok(table(20, 3, 20, false)),
        });
        app.take_requests();

        let start = Instant::now();
        app.set_preview_width(200);
        assert!(!app.poll_reflow_at(start));
        assert!(!app.poll_reflow_at(start + Duration::from_millis(150)));
        assert!(
            app.take_requests().is_empty(),
            "a table must not be re-requested for a resize"
        );

        // The new width was still recorded, so a text file opened at that width
        // does not immediately fire a reflow of its own.
        let id = app.preview_req.latest();
        app.on_response(Response {
            id,
            kind: Kind::Preview,
            result: Ok(text(50)),
        });
        assert!(!app.poll_reflow_at(start + Duration::from_secs(5)));
        assert!(app.take_requests().is_empty());
    }

    #[test]
    fn nothing_reflows_before_there_is_a_preview_to_reflow() {
        let mut app = App::new(PathBuf::from("/tmp/root"), None);
        app.take_requests();
        app.set_preview_width(300);
        let start = Instant::now();
        assert!(!app.poll_reflow_at(start));
        assert!(!app.poll_reflow_at(start + Duration::from_secs(1)));
        assert!(app.take_requests().is_empty());
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
