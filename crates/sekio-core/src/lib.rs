//! sekio-core: filetype detection + rendering into a frontend-neutral
//! `PreviewContent` IR. Frontends (CLI/TUI/GUI) only know how to paint the IR.

mod cancel;
mod detect;
pub mod paths;
mod render;

pub use cancel::CancelToken;
pub use detect::Detected;
pub use paths::{canonical, plain};

use std::path::Path;
use std::time::{Duration, Instant};

/// Line width assumed when a frontend gives no `text_width` hint. Wide enough
/// that an ordinary spreadsheet lays out unsqueezed, narrow enough to still be
/// readable in a default-sized terminal.
pub const DEFAULT_TEXT_WIDTH: usize = 120;

/// Narrowest width any layout will lay out for. Below this a table stops being
/// a table, and pretending otherwise only produces columns one character wide.
pub const MIN_TEXT_WIDTH: usize = 20;

/// Limits baked into the core so every frontend inherits them.
/// A preview must never stall on a huge file: cap the work, not just the output.
#[derive(Debug, Clone)]
pub struct PreviewOptions {
    /// Max bytes read from a text/binary file.
    pub max_bytes: usize,
    /// Max lines produced for a text preview.
    pub max_lines: usize,
    /// Longest edge an image is downscaled to before handing to a frontend.
    pub image_max_dim: u32,
    /// Max entries in a directory/archive listing.
    pub max_entries: usize,
    /// How many characters wide the frontend's text surface is.
    ///
    /// Only renderers that lay out *columns* — spreadsheets today — read it;
    /// ordinary text keeps its own line breaks and is scrolled sideways by the
    /// frontend. `None` means "no hint", and layout falls back to
    /// [`DEFAULT_TEXT_WIDTH`]: a preview must render sensibly for a caller that
    /// has no idea how wide it is.
    pub text_width: Option<usize>,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024,
            max_lines: 500,
            image_max_dim: 1024,
            max_entries: 1000,
            text_width: None,
        }
    }
}

impl PreviewOptions {
    /// The line width a column layout should spend: the frontend's hint when
    /// there is one, [`DEFAULT_TEXT_WIDTH`] when there is not, never narrower
    /// than [`MIN_TEXT_WIDTH`].
    pub fn line_width(&self) -> usize {
        self.text_width
            .unwrap_or(DEFAULT_TEXT_WIDTH)
            .max(MIN_TEXT_WIDTH)
    }
}

/// Decides when a resized preview surface is different enough to be worth
/// re-rendering at.
///
/// A preview is rendered once, so without this a window dragged wider keeps the
/// table it was laid out for until the user opens another file. Re-requesting
/// on every frame of a drag is the other extreme: each one costs a full render
/// and cancels the last. So a new request needs two things — the width must
/// have moved by at least `threshold` characters from the one the visible
/// preview was rendered at, and it must then hold still for `settle`.
///
/// It lives here, beside [`PreviewOptions::text_width`], because it is the rule
/// for producing that value and both the GUI and the TUI need it to be the
/// same rule. It is pure: the caller passes the clock in, so the decision is
/// testable without an event loop.
#[derive(Debug, Clone)]
pub struct Reflow {
    threshold: usize,
    settle: Duration,
    /// Width the preview on screen was requested at.
    current: usize,
    /// A different width we are waiting to see hold still, and when we first
    /// saw it.
    pending: Option<(usize, Instant)>,
}

impl Reflow {
    /// `threshold` is in characters, `settle` is how long a new width must hold
    /// before it is acted on.
    pub fn new(threshold: usize, settle: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            settle,
            current: DEFAULT_TEXT_WIDTH,
            pending: None,
        }
    }

    /// Record the width a request was just issued at, so the next resize is
    /// measured against what is actually being rendered.
    pub fn issued(&mut self, width: usize) {
        self.current = width;
        self.pending = None;
    }

    /// The width the visible preview was requested at.
    pub fn current(&self) -> usize {
        self.current
    }

    /// Feed in the surface width for this frame. `Some(width)` means "re-request
    /// the preview at this width"; the width is recorded as current, so the
    /// caller does not have to call [`Reflow::issued`] again.
    pub fn observe(&mut self, width: usize, now: Instant) -> Option<usize> {
        if width.abs_diff(self.current) < self.threshold {
            // Back where we started (or never really left): whatever we were
            // waiting on is moot.
            self.pending = None;
            return None;
        }
        match self.pending {
            // Still hovering around the width we are waiting on — keep the
            // original timestamp, so a slow drag does not reset the clock
            // forever.
            Some((pending, since)) if pending.abs_diff(width) < self.threshold => {
                if now.duration_since(since) >= self.settle {
                    self.issued(width);
                    Some(width)
                } else {
                    None
                }
            }
            // A new target: start the clock again.
            _ => {
                self.pending = Some((width, now));
                None
            }
        }
    }
}

/// A single styled run of text. Colors are 24-bit RGB from the syntect theme;
/// each frontend maps them to its own output (ANSI, ratatui Style, egui Color32).
#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub fg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Default)]
pub struct StyledLine {
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

/// One labelled fact about a file, for the `Metadata` variant.
#[derive(Debug, Clone)]
pub struct MetaField {
    pub key: String,
    pub value: String,
}

impl MetaField {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// What a table cell holds. Frontends colour and align by this rather than
/// re-deriving it from the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Text,
    Number,
    Bool,
    Date,
    Error,
}

impl CellKind {
    /// Numbers and dates read better flush right, so columns of them line up
    /// on the decimal point rather than the first digit.
    pub fn align_right(self) -> bool {
        matches!(self, CellKind::Number | CellKind::Date)
    }
}

#[derive(Debug, Clone)]
pub struct TableCell {
    pub text: String,
    pub kind: CellKind,
}

#[derive(Debug, Clone, Default)]
pub struct TableRow {
    /// Shown in the gutter — the spreadsheet's own row number.
    pub label: String,
    /// One entry per column in `columns`; absent cells are empty strings.
    pub cells: Vec<TableCell>,
}

/// The frontend-neutral intermediate representation.
#[derive(Debug)]
pub enum PreviewContent {
    Text {
        lines: Vec<StyledLine>,
        /// Language name syntect matched, e.g. "Rust".
        language: String,
    },
    Image {
        /// Already downscaled to `image_max_dim`.
        image: image::RgbaImage,
        /// Dimensions of the original file, pre-downscale.
        original_width: u32,
        original_height: u32,
        format: String,
        /// Extra facts to show alongside the image (EXIF camera/date, page
        /// count, video duration). Empty when the format has none.
        fields: Vec<MetaField>,
    },
    Listing {
        entries: Vec<ListEntry>,
    },
    /// Key/value facts about a file we can describe but not render — audio
    /// tags, EXIF, a video's codec/duration. `thumbnail` carries a cover image
    /// or extracted frame when one is available.
    Metadata {
        fields: Vec<MetaField>,
        thumbnail: Option<image::RgbaImage>,
    },
    /// A real grid, kept structured so each frontend can lay it out for the
    /// space it actually has — egui columns, a ratatui table, box drawing —
    /// instead of core guessing a width and baking ellipses into text.
    Table {
        /// Column headings: a spreadsheet's letters, A/B/C.
        columns: Vec<String>,
        rows: Vec<TableRow>,
        /// Sheet or tab names, empty when the format has none.
        sheets: Vec<String>,
        /// Index into `sheets` of the one being shown.
        active_sheet: usize,
        /// The full extent, so a frontend can say how much is not shown.
        total_rows: u64,
        total_cols: u64,
    },
    HexDump {
        data: Vec<u8>,
        file_size: u64,
        mime: Option<String>,
    },
}

#[derive(Debug)]
pub struct Preview {
    pub content: PreviewContent,
    /// True if any limit in `PreviewOptions` cut the content short.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("image decode failed: {0}")]
    Image(#[from] image::ImageError),
    #[error("{0}")]
    Format(String),
    #[error("preview cancelled")]
    Cancelled,
}

/// Reusable previewer. Holds the loaded syntax/theme sets (loading them is the
/// expensive part — construct once, preview many).
pub struct Previewer {
    highlighter: render::text::Highlighter,
}

impl Previewer {
    pub fn new() -> Self {
        Self {
            highlighter: render::text::Highlighter::new(),
        }
    }

    /// Build a previewer whose text renderer uses the named syntax theme.
    /// Returns `None` for an unknown name; `theme_names` lists the valid ones.
    pub fn with_theme(name: &str) -> Option<Self> {
        Some(Self {
            highlighter: render::text::Highlighter::with_theme(name)?,
        })
    }

    /// Every theme name `with_theme` accepts, sorted. Frontends use this to
    /// validate a config value and to offer a choice.
    pub fn theme_names() -> Vec<String> {
        render::text::Highlighter::theme_names()
    }

    /// The theme used when none is named.
    pub const DEFAULT_THEME: &'static str = render::text::DEFAULT_THEME;

    /// Render a preview for `path`. Checks `cancel` at work boundaries so a
    /// frontend can abort a stale request (user already moved to the next file).
    pub fn preview(
        &self,
        path: &Path,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        let detected = detect::detect(path, opts)?;
        cancel.check()?;

        // A format renderer that fails on a malformed file degrades to the
        // hexdump rather than failing the whole preview — a broken zip is
        // still worth showing bytes for. Cancellation is never swallowed.
        macro_rules! or_hex {
            ($e:expr) => {
                match $e {
                    Ok(p) => Ok(p),
                    Err(PreviewError::Cancelled) => Err(PreviewError::Cancelled),
                    Err(_) => render::hex::fallback(path, opts),
                }
            };
        }

        match detected {
            Detected::Directory => render::dir::render(path, opts, cancel),
            Detected::Image { mime, head } => {
                or_hex!(render::image::render(path, &mime, head, opts, cancel))
            }
            Detected::Svg { head } => or_hex!(render::svg::render(path, head, opts, cancel)),
            Detected::Archive { mime, head } => {
                or_hex!(render::archive::render(path, &mime, head, opts, cancel))
            }
            Detected::Spreadsheet { format, head } => {
                or_hex!(render::spreadsheet::render(
                    path, &format, head, opts, cancel
                ))
            }
            // Legacy binary Word/PowerPoint have no pure-Rust reader, so they
            // go to the LibreOffice shell-out instead of the OOXML reader.
            Detected::Document { format, head } if matches!(format.as_str(), "doc" | "ppt") => {
                or_hex!(render::legacy_office::render(
                    path, &format, head, opts, cancel
                ))
            }
            Detected::Document { format, head } => {
                or_hex!(render::document::render(path, &format, head, opts, cancel))
            }
            Detected::Markdown { head } => {
                or_hex!(render::markdown::render(path, head, opts, cancel))
            }
            Detected::Audio { mime, head } => {
                or_hex!(render::audio::render(path, &mime, head, opts, cancel))
            }
            Detected::Pdf { head } => or_hex!(render::pdf::render(path, head, opts, cancel)),
            Detected::Video { mime, head } => {
                or_hex!(render::video::render(path, &mime, head, opts, cancel))
            }
            Detected::Text { head, encoding } => or_hex!(render::text::render(
                &self.highlighter,
                path,
                head,
                encoding,
                opts,
                cancel
            )),
            Detected::Binary { mime, head } => render::hex::render(path, mime, head, opts),
        }
    }
}

impl Default for Previewer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_width_hint_means_the_default_width() {
        assert_eq!(PreviewOptions::default().text_width, None);
        assert_eq!(PreviewOptions::default().line_width(), DEFAULT_TEXT_WIDTH);
    }

    #[test]
    fn a_supplied_width_is_honoured_but_never_below_the_floor() {
        let at = |w| {
            PreviewOptions {
                text_width: Some(w),
                ..PreviewOptions::default()
            }
            .line_width()
        };
        assert_eq!(at(200), 200);
        assert_eq!(at(40), 40);
        // A pane two characters wide is not a width anything can lay out for.
        assert_eq!(at(2), MIN_TEXT_WIDTH);
        assert_eq!(at(0), MIN_TEXT_WIDTH);
    }

    fn reflow() -> Reflow {
        Reflow::new(4, Duration::from_millis(120))
    }

    #[test]
    fn a_small_resize_never_asks_for_a_new_render() {
        let mut r = reflow();
        r.issued(100);
        let start = Instant::now();
        // Three characters is under the threshold, so it never even starts the
        // clock — however long it holds.
        assert_eq!(r.observe(103, start), None);
        assert_eq!(r.observe(103, start + Duration::from_secs(5)), None);
        assert_eq!(r.observe(97, start + Duration::from_secs(10)), None);
        assert_eq!(r.current(), 100);
    }

    #[test]
    fn a_real_resize_asks_once_the_width_holds_still() {
        let mut r = reflow();
        r.issued(100);
        let start = Instant::now();
        // Past the threshold, but not settled yet.
        assert_eq!(r.observe(200, start), None);
        assert_eq!(r.observe(200, start + Duration::from_millis(119)), None);
        assert_eq!(
            r.observe(200, start + Duration::from_millis(120)),
            Some(200),
            "a width that held for the settle period must be re-requested"
        );
        assert_eq!(r.current(), 200);
        // …and exactly once: the new width is now the current one.
        assert_eq!(r.observe(200, start + Duration::from_secs(1)), None);
    }

    #[test]
    fn a_drag_that_keeps_moving_keeps_resetting_the_clock() {
        let mut r = reflow();
        r.issued(100);
        let start = Instant::now();
        // One frame every 16 ms, ten characters wider each time: the width
        // never holds still, so not one request goes out.
        for step in 0..30u32 {
            let now = start + Duration::from_millis(u64::from(step) * 16);
            assert_eq!(
                r.observe(110 + step as usize * 10, now),
                None,
                "a drag in progress must not re-render at step {step}"
            );
        }
        // Let go, and it fires once for where the drag ended.
        let end = start + Duration::from_millis(30 * 16);
        let width = 110 + 29 * 10;
        assert_eq!(r.observe(width, end), None);
        assert_eq!(
            r.observe(width, end + Duration::from_millis(200)),
            Some(width)
        );
    }

    #[test]
    fn snapping_back_to_the_old_width_cancels_the_pending_request() {
        let mut r = reflow();
        r.issued(100);
        let start = Instant::now();
        assert_eq!(r.observe(200, start), None);
        // The user dragged back; nothing needs re-rendering after all.
        assert_eq!(r.observe(100, start + Duration::from_millis(50)), None);
        assert_eq!(r.observe(100, start + Duration::from_secs(5)), None);
        assert_eq!(r.current(), 100);
    }

    /// Before anything has been requested the tracker assumes core's default,
    /// because that is exactly what a request with no hint is rendered at.
    #[test]
    fn a_fresh_tracker_starts_at_the_default_width() {
        let mut r = reflow();
        assert_eq!(r.current(), DEFAULT_TEXT_WIDTH);
        let start = Instant::now();
        assert_eq!(r.observe(DEFAULT_TEXT_WIDTH + 1, start), None);
        assert_eq!(
            r.observe(DEFAULT_TEXT_WIDTH + 1, start + Duration::from_secs(1)),
            None,
            "a pane that happens to match the default needs no second render"
        );
    }
}
