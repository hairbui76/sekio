//! Headless rendering tests for the egui frontend.
//!
//! Everything else about `sekio-gui` is checked by pure unit tests — the
//! generation counter, the sibling walk, the Esc rule, the span → `TextFormat`
//! mapping. None of that proves a single pixel ever lands on screen. A texture
//! that never uploads, a panel laid out off-window, a `LayoutJob` that
//! collapses to nothing, a `ScrollArea` that clips its own contents: all of it
//! compiles, and all of it passes those tests, while looking broken.
//!
//! So these tests run the real `eframe::App` — `SekioApp::logic` then
//! `SekioApp::ui`, the same two calls eframe makes — through `egui_kittest`,
//! which drives a real `egui::Context` with no window, no display server and no
//! GPU. Then they assert on what the frame actually produced:
//!
//! * **the painted shapes** (`Harness::output().shapes`) — the tessellator's
//!   input, i.e. every `Galley` egui was about to rasterise, at the position
//!   and size it was going to rasterise it. Text that is missing, empty,
//!   zero-sized or off-screen is visible here.
//! * **the AccessKit tree** — what a screen reader (and `kittest`) sees, which
//!   is how the interactive controls are found.
//!
//! What is deliberately *not* here: committed pixel snapshots.
//! `egui_kittest`'s `wgpu` + `snapshot` features do rasterise these screens
//! (verified by hand, on a software Mesa stack), but committing the PNGs would
//! make `cargo test` depend on a working GPU adapter in CI, which the test job
//! installs nothing for, and would compare llvmpipe output on `ubuntu-latest`
//! against WARP output on `windows-latest` — two rasterisers that do not agree
//! pixel for pixel. A snapshot that silently skips itself, or that only ever
//! passes on one runner, is worse than no snapshot. Shapes are one step short
//! of pixels and need nothing but a CPU.
//!
//! No worker thread and no `Previewer` are involved: [`Worker::from_channels`]
//! lets each test hand the UI exactly the `PreviewContent` it wants to paint,
//! so every assertion below is deterministic.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Once;
use std::time::Duration;

use eframe::App as _;
use egui::{Color32, Rect, Shape, TextureId};
use egui_kittest::kittest::Queryable as _;
use egui_kittest::Harness;
use sekio_core::{
    CellKind, ListEntry, MetaField, Preview, PreviewContent, Span, StyledLine, TableCell, TableRow,
};
use sekio_gui::app::{SekioApp, Startup};
use sekio_gui::state::{Mode, RequestTracker};
use sekio_gui::style::{self, Palette};

/// The palette every test below paints in unless it says otherwise: the
/// harness feeds egui no system theme, so `ThemePreference::System` resolves to
/// the documented dark fallback.
const DARK: Palette = Palette::dark();
use sekio_gui::timing::Timing;
use sekio_gui::worker::{self, Kind, Loaded, Outcome, Request, Response, Worker};

/// Window size every test renders at. Big enough that nothing is squeezed out
/// of the layout, small enough that the row-virtualising views (hexdump,
/// listing) still have to decide what fits — which is exactly what we want to
/// check them doing.
const SIZE: [f32; 2] = [900.0, 620.0];

/// The generation id `SekioApp` is waiting on. `main.rs` fires the first
/// request before the window exists, so a freshly built app with a path has
/// exactly one request in flight; [`ui_with_path`] reproduces that.
const FIRST: u64 = 1;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Give the recent-files store nowhere to live, so it never touches a file.
///
/// `SekioApp::new` unconditionally spawns `recent::Store`, which would read and
/// write the *user's* real list. Pointing it at a scratch directory is not
/// enough: every test in this binary shares one process and therefore one file,
/// so a test that previews something writes a recent entry that the home-screen
/// test then reads, and whether it does so first is a matter of scheduling. That
/// is exactly how it passed on Linux and failed on the Windows runner.
///
/// `recent::state_file()` returns `None` when the environment names no usable
/// directory, and `Store::spawn` answers that with an inert store — no reads, no
/// writes, no thread. The *in-memory* list still fills as previews land, which is
/// what the behaviour tests actually assert on, so nothing is lost by removing
/// the file.
fn isolate_state_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Empty, not unset: `state_dir_from` rejects an empty LOCALAPPDATA and a
        // non-absolute XDG_STATE_HOME/HOME, which is what makes it answer `None`
        // on both platforms.
        for var in ["XDG_STATE_HOME", "LOCALAPPDATA", "HOME", "USERPROFILE"] {
            std::env::set_var(var, "");
        }
    });
}

/// A running, headless `SekioApp`.
struct AppUi {
    harness: Harness<'static>,
    /// Results handed to the UI, standing in for the worker thread.
    responses: Sender<Response>,
    /// The requests the UI made. Held so `Worker::request` keeps succeeding.
    requests: Receiver<Request>,
}

impl AppUi {
    fn new(path: Option<PathBuf>, mode: Mode) -> Self {
        Self::sized(path, mode, SIZE)
    }

    fn sized(path: Option<PathBuf>, mode: Mode, size: [f32; 2]) -> Self {
        isolate_state_dir();

        let (req_tx, requests) = mpsc::channel::<Request>();
        let (responses, res_rx) = mpsc::channel::<Response>();

        let mut tracker = RequestTracker::new();
        if path.is_some() {
            // Same as `main::start_preview`: the request is already in flight
            // by the time the app is constructed, so it opens on "loading…".
            let (id, _cancel) = tracker.begin();
            assert_eq!(id, FIRST, "the first generation id is 1");
        }

        let startup = Startup {
            worker: Worker::from_channels(req_tx, res_rx),
            tracker,
            path,
            mode,
            wrap: false,
            borderless: false,
            timing: Timing::start(false),
            incoming: None,
            presses: None,
            // No tray in a headless render: `tray::spawn` would find no host
            // anyway, and these tests are about what gets painted.
            tray: None,
            hotkey_spec: None,
            config_path: None,
            theme: sekio_gui::style::Theme::Dark,
        };

        // The app is built inside the closure because it needs the harness's
        // own `egui::Context` — the one `set_visuals`, the texture uploads and
        // the recent-store wake-ups all have to go through.
        let mut startup = Some(startup);
        let mut app: Option<SekioApp> = None;
        let mut frame = eframe::Frame::_new_kittest();

        let harness = Harness::builder()
            .with_size(size)
            // Nothing here loads images through egui's async loaders, and
            // waiting for them would sleep a quarter-second per frame.
            .with_wait_for_pending_images(false)
            .build_ui(move |ui| {
                let app = app.get_or_insert_with(|| {
                    let startup = startup.take().expect("startup is consumed exactly once");
                    SekioApp::new(ui.ctx(), startup)
                });
                // Exactly what eframe does every frame, in this order.
                let ctx = ui.ctx().clone();
                app.logic(&ctx, &mut frame);
                app.ui(ui, &mut frame);
            });

        Self {
            harness,
            responses,
            requests,
        }
    }

    /// Paint frames until the UI settles (or until the harness gives up, which
    /// it does whenever a `Spinner` is on screen asking for the next frame —
    /// that is a steady state here, not a failure).
    fn run(&mut self) {
        self.harness.run_ok();
    }

    /// Hand the UI a finished preview, as the worker thread would.
    fn deliver(&mut self, id: u64, path: &Path, content: PreviewContent) {
        let image = worker::egui_image(&content);
        let loaded = Loaded {
            preview: Preview {
                content,
                truncated: false,
            },
            image,
        };
        self.respond(id, path, Outcome::Ready(Box::new(loaded)));
    }

    fn respond(&mut self, id: u64, path: &Path, outcome: Outcome) {
        self.responses
            .send(Response {
                id,
                path: path.to_path_buf(),
                outcome,
                elapsed: Duration::from_millis(7),
                kind: Kind::Preview,
            })
            .expect("the app still owns the response channel");
        self.run();
    }

    /// Every string the last frame was about to rasterise, with the rectangle
    /// it would have occupied.
    fn painted(&self) -> Vec<PaintedText> {
        let mut out = Vec::new();
        for clipped in &self.harness.output().shapes {
            collect_text(&clipped.shape, clipped.clip_rect, &mut out);
        }
        out
    }

    /// The painted text, joined — what the window "says".
    fn text(&self) -> String {
        let mut joined = String::new();
        for painted in self.painted() {
            joined.push_str(&painted.text);
            joined.push('\n');
        }
        joined
    }

    /// Every picture in the last frame — i.e. every shape textured with
    /// something other than the font atlas.
    fn images(&self) -> Vec<PaintedImage> {
        let mut out = Vec::new();
        for clipped in &self.harness.output().shapes {
            collect_images(&clipped.shape, &mut out);
        }
        out
    }

    /// The dimensions egui actually holds for a texture, straight out of the
    /// texture manager. This is the upload itself, not a claim about it.
    fn uploaded_size(&self, texture: TextureId) -> [usize; 2] {
        self.harness
            .ctx
            .tex_manager()
            .read()
            .meta(texture)
            .unwrap_or_else(|| panic!("{texture:?} is painted but was never uploaded"))
            .size
    }

    /// Spin the wheel over the middle of the window. `ScrollArea` clamps the
    /// offset, and `kittest` disables egui's scroll animation, so one big shove
    /// lands exactly at the far end.
    fn scroll_by(&mut self, delta: egui::Vec2) {
        let middle = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into()).center();
        self.harness.event(egui::Event::PointerMoved(middle));
        self.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        self.run();
    }

    fn scroll_to_the_bottom(&mut self) {
        self.scroll_by(egui::Vec2::new(0.0, -100_000.0));
    }

    /// All the way to the right-hand edge of the content.
    fn scroll_to_the_right(&mut self) {
        self.scroll_by(egui::Vec2::new(-100_000.0, 0.0));
    }

    /// Every straight line the frame was about to draw — the table's column
    /// rules and the rule under its header.
    fn lines(&self) -> Vec<PaintedLine> {
        let mut out = Vec::new();
        for clipped in &self.harness.output().shapes {
            collect_lines(&clipped.shape, &mut out);
        }
        out
    }

    fn fills(&self) -> Vec<PaintedFill> {
        let mut out = Vec::new();
        for clipped in &self.harness.output().shapes {
            collect_fills(&clipped.shape, &mut out);
        }
        out
    }

    /// The one galley whose text is exactly `needle`.
    #[track_caller]
    fn galley_of(&self, needle: &str) -> PaintedText {
        self.painted()
            .into_iter()
            .find(|painted| painted.text == needle)
            .unwrap_or_else(|| panic!("nothing painted for {needle:?}:\n{}", self.text()))
    }

    /// The colour `needle` was laid out in.
    #[track_caller]
    fn color_of(&self, needle: &str) -> Color32 {
        self.galley_of(needle)
            .galley
            .job
            .sections
            .first()
            .map(|section| section.format.color)
            .unwrap_or_else(|| panic!("{needle:?} was painted with no format at all"))
    }

    /// Where a galley whose text is exactly `needle` was painted.
    #[track_caller]
    fn rect_of(&self, needle: &str) -> Rect {
        self.galley_of(needle).rect
    }

    /// Is anything at all painted with exactly this text?
    fn has(&self, needle: &str) -> bool {
        self.painted().iter().any(|painted| painted.text == needle)
    }

    #[track_caller]
    fn assert_shows(&self, needle: &str, what: &str) {
        let text = self.text();
        assert!(
            text.contains(needle),
            "{what}: expected {needle:?} in the painted frame, but it said:\n{text}"
        );
    }

    #[track_caller]
    fn assert_hides(&self, needle: &str, what: &str) {
        let text = self.text();
        assert!(
            !text.contains(needle),
            "{what}: {needle:?} should be gone, but the frame still said:\n{text}"
        );
    }
}

/// One `Galley` the frame was about to draw.
struct PaintedText {
    text: String,
    rect: Rect,
    /// The clip rectangle it would have been drawn through. A galley entirely
    /// outside this is laid out but invisible.
    clip: Rect,
    galley: std::sync::Arc<egui::Galley>,
}

impl PaintedText {
    /// Is any of this text actually inside its own clip rectangle, and inside
    /// the window? This is what catches "laid out off-screen".
    fn is_visible(&self, screen: Rect) -> bool {
        self.rect.intersects(self.clip) && self.rect.intersects(screen)
    }
}

fn collect_text(shape: &Shape, clip: Rect, out: &mut Vec<PaintedText>) {
    match shape {
        Shape::Text(text) => out.push(PaintedText {
            text: text.galley.text().to_owned(),
            rect: Rect::from_min_size(text.pos, text.galley.size()),
            clip,
            galley: text.galley.clone(),
        }),
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, clip, out);
            }
        }
        _ => {}
    }
}

/// A filled rectangle the frame was about to draw. The table's frozen header
/// and gutter are opaque strips: without them the cells would scroll straight
/// through the column letters and the row numbers.
struct PaintedFill {
    rect: Rect,
    color: Color32,
}

fn collect_fills(shape: &Shape, out: &mut Vec<PaintedFill>) {
    match shape {
        Shape::Rect(rect) if rect.fill.a() > 0 => out.push(PaintedFill {
            rect: rect.rect,
            color: rect.fill,
        }),
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_fills(shape, out);
            }
        }
        _ => {}
    }
}

/// One straight line the frame was about to draw.
struct PaintedLine {
    from: egui::Pos2,
    to: egui::Pos2,
    color: Color32,
}

impl PaintedLine {
    fn is_vertical(&self) -> bool {
        (self.from.x - self.to.x).abs() < 0.5 && (self.from.y - self.to.y).abs() > 1.0
    }

    fn is_horizontal(&self) -> bool {
        (self.from.y - self.to.y).abs() < 0.5 && (self.from.x - self.to.x).abs() > 1.0
    }
}

fn collect_lines(shape: &Shape, out: &mut Vec<PaintedLine>) {
    match shape {
        Shape::LineSegment { points, stroke } => out.push(PaintedLine {
            from: points[0],
            to: points[1],
            color: stroke.color,
        }),
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_lines(shape, out);
            }
        }
        _ => {}
    }
}

/// A picture on screen: the texture it samples, and where it lands.
struct PaintedImage {
    texture: TextureId,
    rect: Rect,
}

/// Pull out every shape that samples a texture the app uploaded.
///
/// `Managed(0)` is egui's own font atlas, which every glyph draws from; the
/// only other managed textures in this app are the ones `accept_preview`
/// uploads. Both shapes that can carry one are checked: `ui.image` paints a
/// `RectShape` with a textured brush, and anything hand-tessellated arrives as
/// a `Mesh`.
fn collect_images(shape: &Shape, out: &mut Vec<PaintedImage>) {
    let font_atlas = TextureId::Managed(0);
    match shape {
        Shape::Rect(rect) => {
            if let Some(brush) = &rect.brush {
                if brush.fill_texture_id != font_atlas {
                    out.push(PaintedImage {
                        texture: brush.fill_texture_id,
                        rect: rect.rect,
                    });
                }
            }
        }
        Shape::Mesh(mesh) if mesh.texture_id != font_atlas => out.push(PaintedImage {
            texture: mesh.texture_id,
            rect: mesh.calc_bounds(),
        }),
        Shape::Vec(shapes) => {
            for shape in shapes {
                collect_images(shape, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn span(text: &str, fg: Option<(u8, u8, u8)>, bold: bool, italic: bool) -> Span {
    Span {
        text: text.to_owned(),
        fg,
        bold,
        italic,
    }
}

fn line(spans: Vec<Span>) -> StyledLine {
    StyledLine { spans }
}

fn text_content() -> PreviewContent {
    PreviewContent::Text {
        lines: vec![
            line(vec![span(
                "fn main() {",
                Some((198, 120, 221)),
                false,
                false,
            )]),
            line(vec![
                span("    println!", None, false, false),
                span("(\"sekio\");", Some((152, 195, 121)), false, false),
            ]),
            line(vec![span("}", Some((198, 120, 221)), false, false)]),
        ],
        language: "Rust".to_owned(),
    }
}

fn listing_content() -> PreviewContent {
    PreviewContent::Listing {
        entries: vec![
            ListEntry {
                name: "src".to_owned(),
                is_dir: true,
                size: None,
            },
            ListEntry {
                name: "Cargo.toml".to_owned(),
                is_dir: false,
                size: Some(2048),
            },
            ListEntry {
                name: "README.md".to_owned(),
                is_dir: false,
                size: Some(512),
            },
        ],
    }
}

fn metadata_content() -> PreviewContent {
    PreviewContent::Metadata {
        fields: vec![
            MetaField::new("title", "Sekio Theme"),
            MetaField::new("artist", "Nobody At All"),
            MetaField::new("duration", "3:21"),
        ],
        thumbnail: None,
    }
}

// ---------------------------------------------------------------------------
// Table fixtures
//
// The IR is built by hand rather than read out of a workbook: `PreviewContent`
// is the contract between core and this frontend, so a painting test that goes
// through calamine is testing the wrong half. The one end-to-end case that does
// build a real .xlsx lives at the bottom of this file.
// ---------------------------------------------------------------------------

fn cell(text: &str, kind: CellKind) -> TableCell {
    TableCell {
        text: text.to_owned(),
        kind,
    }
}

fn text_cell(text: &str) -> TableCell {
    cell(text, CellKind::Text)
}

fn table_row(label: &str, cells: Vec<TableCell>) -> TableRow {
    TableRow {
        label: label.to_owned(),
        cells,
    }
}

/// `columns` letters, A onwards.
fn column_letters(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| ((b'A' + i as u8) as char).to_string())
        .collect()
}

/// The sheet from the bug report: Vietnamese prose in one column, numbers in
/// another, and a note long enough to be worth eliding.
fn table_content() -> PreviewContent {
    PreviewContent::Table {
        columns: column_letters(4),
        rows: vec![
            table_row(
                "1",
                vec![
                    text_cell("STT"),
                    text_cell("Hoạt động"),
                    text_cell("Kết quả (giờ quy đổi)"),
                    text_cell("Ghi chú"),
                ],
            ),
            table_row(
                "2",
                vec![
                    cell("1.3", CellKind::Number),
                    text_cell("Đứng lớp hướng dẫn thực hành"),
                    cell("47.3", CellKind::Number),
                    text_cell(""),
                ],
            ),
            table_row(
                "3",
                vec![
                    cell("8", CellKind::Number),
                    text_cell("Các hoạt động hỗ trợ khác"),
                    cell("12", CellKind::Number),
                    text_cell("Hỗ trợ lễ bảo vệ khóa luận"),
                ],
            ),
        ],
        sheets: vec!["Tong".to_owned(), "Chi tiết".to_owned()],
        active_sheet: 0,
        total_rows: 3,
        total_cols: 4,
    }
}

fn hexdump_content() -> PreviewContent {
    PreviewContent::HexDump {
        data: (0u8..=255).collect(),
        file_size: 256,
        mime: Some("application/octet-stream".to_owned()),
    }
}

/// A 64×32 gradient, standing in for a decoded picture.
fn image_content() -> PreviewContent {
    let image = image::RgbaImage::from_fn(64, 32, |x, y| {
        image::Rgba([(x * 4) as u8, (y * 8) as u8, 200, 255])
    });
    PreviewContent::Image {
        image,
        original_width: 1280,
        original_height: 640,
        format: "PNG".to_owned(),
        fields: vec![MetaField::new("camera", "Test Rig")],
    }
}

fn ui_with_path(path: &str) -> AppUi {
    let mut ui = AppUi::new(Some(PathBuf::from(path)), Mode::Popup);
    ui.run();
    ui
}

fn home_ui() -> AppUi {
    let mut ui = AppUi::new(None, Mode::App);
    ui.run();
    ui
}

// ---------------------------------------------------------------------------
// 1. Every PreviewContent variant paints, and says what it is
// ---------------------------------------------------------------------------

#[test]
fn a_text_preview_paints_its_code_and_names_the_language() {
    let path = PathBuf::from("/tmp/main.rs");
    let mut ui = ui_with_path("/tmp/main.rs");
    ui.deliver(FIRST, &path, text_content());

    ui.assert_shows("fn main() {", "the text body");
    ui.assert_shows("println!(\"sekio\");", "the text body");
    // The header names the file, the footer the language and the line count.
    ui.assert_shows("main.rs", "the header");
    ui.assert_shows("Rust · 3 lines", "the footer");
    ui.assert_shows("7 ms", "the footer's timing");

    // …and it is on screen, not laid out past the edge of the window.
    let screen = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into());
    let body = ui
        .painted()
        .into_iter()
        .find(|painted| painted.text.contains("fn main() {"))
        .expect("the code galley must be among the painted shapes");
    assert!(
        body.is_visible(screen),
        "the code was laid out at {:?}, clipped to {:?}, outside the {screen:?} window",
        body.rect,
        body.clip
    );
    assert!(
        body.galley.rows.len() >= 3,
        "the LayoutJob collapsed: three source lines laid out as {} row(s)",
        body.galley.rows.len()
    );
    assert!(
        body.rect.width() > 1.0 && body.rect.height() > 1.0,
        "the code galley is {:?}, which is not a size anyone can read",
        body.rect.size()
    );
}

#[test]
fn a_listing_paints_every_entry_with_its_size() {
    let path = PathBuf::from("/tmp/project");
    let mut ui = ui_with_path("/tmp/project");
    ui.deliver(FIRST, &path, listing_content());

    ui.assert_shows("src/", "a directory row, with its trailing slash");
    ui.assert_shows("Cargo.toml", "a file row");
    ui.assert_shows("README.md", "a file row");
    ui.assert_shows("2.0 KB", "the size column");
    ui.assert_shows("512 B", "the size column");
    ui.assert_shows("3 entries", "the footer");
}

#[test]
fn metadata_paints_every_key_next_to_its_value() {
    let path = PathBuf::from("/tmp/song.flac");
    let mut ui = ui_with_path("/tmp/song.flac");
    ui.deliver(FIRST, &path, metadata_content());

    for (key, value) in [
        ("title", "Sekio Theme"),
        ("artist", "Nobody At All"),
        ("duration", "3:21"),
    ] {
        ui.assert_shows(key, "a metadata key");
        ui.assert_shows(value, "a metadata value");

        // Same row: the grid is only useful if the value lines up with its key.
        let painted = ui.painted();
        let key_rect = painted
            .iter()
            .find(|p| p.text == key)
            .map(|p| p.rect)
            .unwrap_or_else(|| panic!("no galley painted for the key {key:?}"));
        let value_rect = painted
            .iter()
            .find(|p| p.text == value)
            .map(|p| p.rect)
            .unwrap_or_else(|| panic!("no galley painted for the value {value:?}"));
        assert!(
            (key_rect.center().y - value_rect.center().y).abs() < 2.0,
            "{key:?} at y={} but {value:?} at y={}: the grid rows do not line up",
            key_rect.center().y,
            value_rect.center().y
        );
        assert!(
            value_rect.min.x > key_rect.max.x,
            "{value:?} is not to the right of {key:?}"
        );
    }
    ui.assert_shows("3 fields", "the footer");
}

#[test]
fn a_hexdump_paints_offsets_hex_and_ascii() {
    let path = PathBuf::from("/tmp/blob.bin");
    let mut ui = ui_with_path("/tmp/blob.bin");
    ui.deliver(FIRST, &path, hexdump_content());

    ui.assert_shows("00000000", "the first offset");
    ui.assert_shows("00000010", "the second offset");
    // Row 0 of 0x00..=0xff, in the CLI's exact column layout.
    ui.assert_shows(
        "00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f",
        "the hex columns",
    );
    // Row 4 is printable ASCII: "@ABCDEFGHIJKLMNO".
    ui.assert_shows("|@ABCDEFGHIJKLMNO|", "the ASCII column");
    ui.assert_shows("application/octet-stream · 256 B", "the footer");

    // Every one of the sixteen rows is painted, each somewhere different.
    let rows = hexdump_rows(&ui);
    assert_eq!(rows.len(), 16, "a 256-byte dump is sixteen rows");
    for pair in rows.windows(2) {
        assert!(
            pair[1].rect.min.y - pair[0].rect.min.y > 1.0,
            "two hexdump rows were painted on top of each other at y={}",
            pair[0].rect.min.y
        );
    }
}

#[test]
fn a_long_hexdump_paints_the_rows_that_fit_and_no_more() {
    // `paint_hex` hands `ScrollArea::show_rows` a row height it measures from
    // the monospace font. If that number ever stops matching the height the
    // rows are actually laid out at, the virtualiser picks the wrong range:
    // either it paints far more rows than the pane can show (all of them
    // landing below the clip rectangle, invisible), or it leaves a gap at the
    // bottom. Neither shows up in any logic test.
    let path = PathBuf::from("/tmp/big.bin");
    let mut ui = ui_with_path("/tmp/big.bin");
    ui.deliver(
        FIRST,
        &path,
        PreviewContent::HexDump {
            data: (0..8192u32).map(|i| (i % 251) as u8).collect(),
            file_size: 8192,
            mime: None,
        },
    );

    let rows = hexdump_rows(&ui);
    assert!(
        rows.len() >= 20,
        "only {} of 512 rows were painted into a {}px pane — the dump is \
         barely visible",
        rows.len(),
        SIZE[1]
    );
    assert!(
        rows.len() <= 60,
        "{} rows were painted for a pane that fits about 30: `show_rows` is \
         working off the wrong row height",
        rows.len()
    );

    let clip = rows[0].clip;
    let pitch = rows[1].rect.min.y - rows[0].rect.min.y;
    for row in &rows {
        assert!(
            row.rect.min.y < clip.max.y + pitch && row.rect.max.y > clip.min.y - pitch,
            "a hexdump row was laid out at {:?}, outside the pane it is \
             clipped to ({clip:?})",
            row.rect
        );
    }
    ui.assert_shows("binary · 8.0 KB", "the footer of a dump with no mime type");

    // The row height `paint_hex` measures is also what `show_rows` uses to map
    // scroll offsets onto rows. Get it wrong and the pane reports a content
    // height that does not match what it draws, so the end of the file becomes
    // unreachable however far you scroll — which is invisible to every other
    // kind of test.
    ui.assert_hides("00001ff0", "the last row, before anyone scrolls");
    ui.scroll_to_the_bottom();
    ui.assert_shows("00001ff0", "the last row of the dump, after scrolling down");
}

/// The hexdump rows in the last frame, top to bottom. A row is a galley whose
/// first column is an eight-digit hex offset.
fn hexdump_rows(ui: &AppUi) -> Vec<PaintedText> {
    let mut rows: Vec<PaintedText> = ui
        .painted()
        .into_iter()
        .filter(|painted| {
            let offset = painted.text.split_whitespace().next().unwrap_or_default();
            offset.len() == 8 && offset.chars().all(|c| c.is_ascii_hexdigit())
        })
        .collect();
    rows.sort_by(|a, b| a.rect.min.y.total_cmp(&b.rect.min.y));
    rows
}

#[test]
fn an_image_preview_uploads_a_texture_and_paints_it_at_a_sane_size() {
    let path = PathBuf::from("/tmp/photo.png");
    let mut ui = ui_with_path("/tmp/photo.png");
    ui.deliver(FIRST, &path, image_content());

    let images = ui.images();
    assert_eq!(
        images.len(),
        1,
        "expected exactly one picture on screen, found {}",
        images.len()
    );
    let painted = &images[0];

    // The bitmap really reached the GPU-side texture manager, at its own size.
    assert_eq!(
        ui.uploaded_size(painted.texture),
        [64, 32],
        "the uploaded texture is not the 64×32 bitmap the preview carried"
    );

    let rect = painted.rect;
    assert!(
        rect.width() > 1.0 && rect.height() > 1.0,
        "the image painted at {:?} — a zero-sized picture is the classic \
         'the upload never happened' symptom",
        rect.size()
    );
    // `fit` never scales up, so a 64×32 bitmap in a 900×620 window paints at
    // its own size, and keeps its 2:1 aspect ratio either way.
    assert!(
        (rect.width() - 64.0).abs() < 1.0 && (rect.height() - 32.0).abs() < 1.0,
        "expected the 64×32 bitmap at its own size, got {:?}",
        rect.size()
    );
    let screen = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into());
    assert!(
        screen.contains_rect(rect),
        "the image was painted at {rect:?}, outside the {screen:?} window"
    );

    // The footer describes the *original*, not the downscaled bitmap.
    ui.assert_shows("PNG · 1280×640", "the footer");
    ui.assert_shows("camera: Test Rig", "the footer's extra fields");
}

// ---------------------------------------------------------------------------
// 1b. The table painter
//
// A spreadsheet is the one preview that is a *layout* rather than a stream of
// text, and every way it can be wrong is invisible to a unit test: column
// letters that scroll off, a gutter that never freezes, a number painted flush
// left, a virtualiser that lays out ten thousand rows a frame, a cell elided
// while half the window sits empty. So all of it is asserted on the frame.
// ---------------------------------------------------------------------------

#[test]
fn a_table_paints_a_grid_with_column_letters_a_row_gutter_and_rules() {
    let path = PathBuf::from("/tmp/bang-cong.xlsx");
    let mut ui = ui_with_path("/tmp/bang-cong.xlsx");
    ui.deliver(FIRST, &path, table_content());

    // The column letters, left to right, each its own galley.
    let letters: Vec<Rect> = ["A", "B", "C", "D"]
        .into_iter()
        .map(|letter| ui.rect_of(letter))
        .collect();
    for pair in letters.windows(2) {
        assert!(
            pair[0].max.x <= pair[1].min.x,
            "the column letters are out of order: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }

    // The row-number gutter, down the left of the data and left of column A.
    let gutter: Vec<Rect> = ["1", "2", "3"]
        .into_iter()
        .map(|number| ui.rect_of(number))
        .collect();
    let first_cell = ui.rect_of("STT");
    for (number, row) in ["1", "2", "3"].into_iter().zip(gutter.iter()) {
        assert!(
            row.max.x <= first_cell.min.x,
            "row number {number:?} at {row:?} is not left of the first column ({first_cell:?})"
        );
    }
    for pair in gutter.windows(2) {
        assert!(
            pair[1].min.y - pair[0].min.y > 1.0,
            "two rows were painted on top of each other at y={}",
            pair[0].min.y
        );
    }

    // The letters sit above the data, and the data lines up in rows.
    assert!(
        letters[0].max.y <= gutter[0].min.y,
        "the header row ({:?}) overlaps the first data row ({:?})",
        letters[0],
        gutter[0]
    );
    let quy_doi = ui.rect_of("Kết quả (giờ quy đổi)");
    assert!(
        (quy_doi.min.y - first_cell.min.y).abs() < 1.0,
        "two cells of row 1 were painted at different heights: {first_cell:?} and {quy_doi:?}"
    );

    // Whole cells, because the IR now carries whole cells: `rect_of` matches
    // the galley text exactly, so an elided cell would not be found at all.
    ui.rect_of("Đứng lớp hướng dẫn thực hành");
    ui.rect_of("Hỗ trợ lễ bảo vệ khóa luận");

    // Faint rules on the column boundaries and under the header — separation
    // without a box round every cell.
    let lines = ui.lines();
    let verticals = lines
        .iter()
        .filter(|line| line.is_vertical() && line.color == DARK.faint)
        .count();
    assert!(
        verticals >= 4,
        "expected a rule between each of the four columns and after the \
         gutter, found {verticals} vertical rules in {} lines",
        lines.len()
    );
    let under_header = lines
        .iter()
        .filter(|line| line.is_horizontal() && line.color == DARK.faint)
        .any(|line| line.from.y > letters[0].max.y && line.from.y < gutter[0].min.y + 4.0);
    assert!(
        under_header,
        "no rule was painted between the column letters and the first data row"
    );

    // Colour by `CellKind`, and chrome that does not read as data.
    assert_eq!(ui.color_of("47.3"), DARK.cell_number, "a numeric cell");
    assert_eq!(ui.color_of("Hoạt động"), DARK.cell_text, "a text cell");
    assert_eq!(ui.color_of("A"), DARK.dim, "a column letter");
    assert_eq!(ui.color_of("2"), DARK.dim, "a row number");

    // The sheet names, with the previewed one bracketed and picked out.
    ui.assert_shows("Sheets:", "the sheet strip");
    assert_eq!(ui.color_of("[Tong]"), DARK.active, "the active sheet");
    assert_eq!(ui.color_of("Chi tiết"), DARK.faint, "the other sheet");
    assert!(
        ui.rect_of("[Tong]").max.y <= letters[0].min.y,
        "the sheet strip must sit above the grid"
    );

    ui.assert_shows("3 rows × 4 columns", "the footer");
}

#[test]
fn numbers_right_align_in_their_column_while_text_does_not() {
    let content = PreviewContent::Table {
        columns: column_letters(2),
        rows: vec![
            table_row("9", vec![cell("7", CellKind::Number), text_cell("y")]),
            table_row(
                "1000",
                vec![cell("7000000", CellKind::Number), text_cell("yyyyyyy")],
            ),
        ],
        sheets: Vec::new(),
        active_sheet: 0,
        total_rows: 2,
        total_cols: 2,
    };

    let path = PathBuf::from("/tmp/numbers.xlsx");
    let mut ui = ui_with_path("/tmp/numbers.xlsx");
    ui.deliver(FIRST, &path, content);

    let (short, long) = (ui.rect_of("7"), ui.rect_of("7000000"));
    assert!(
        (short.max.x - long.max.x).abs() < 1.0,
        "a numeric column must line up on its right edge: {short:?} against {long:?}"
    );
    assert!(
        short.min.x > long.min.x + 1.0,
        "the short number was not pushed right: {short:?} against {long:?}"
    );

    let (short, long) = (ui.rect_of("y"), ui.rect_of("yyyyyyy"));
    assert!(
        (short.min.x - long.min.x).abs() < 1.0,
        "a text column must line up on its left edge: {short:?} against {long:?}"
    );
    assert!(
        short.max.x < long.max.x - 1.0,
        "the two text cells came out the same width: {short:?} against {long:?}"
    );

    // The gutter is right-aligned too, the way every spreadsheet shows it.
    let (short, long) = (ui.rect_of("9"), ui.rect_of("1000"));
    assert!(
        (short.max.x - long.max.x).abs() < 1.0,
        "the row numbers must line up on their right edge: {short:?} against {long:?}"
    );
}

/// Twenty columns of eight-character cells want roughly 1 600 px. The window is
/// 900. Before this change core was told "you have N characters" and elided
/// until the sheet fitted; now it scrolls.
fn wide_table(columns: usize, rows: usize) -> PreviewContent {
    PreviewContent::Table {
        columns: column_letters(columns),
        rows: (0..rows)
            .map(|row| {
                table_row(
                    &(row + 1).to_string(),
                    (0..columns)
                        .map(|column| {
                            text_cell(&format!(
                                "cell-{}-{}",
                                (b'A' + column as u8) as char,
                                row + 1
                            ))
                        })
                        .collect(),
                )
            })
            .collect(),
        sheets: Vec::new(),
        active_sheet: 0,
        total_rows: rows as u64,
        total_cols: columns as u64,
    }
}

#[test]
fn a_wide_table_scrolls_sideways_instead_of_eliding_everything() {
    let path = PathBuf::from("/tmp/wide.xlsx");
    let mut ui = ui_with_path("/tmp/wide.xlsx");
    ui.deliver(FIRST, &path, wide_table(20, 12));

    // The left-hand columns are on screen whole; the right-hand ones are past
    // the window and cost nothing to not paint.
    ui.rect_of("cell-A-1");
    assert!(
        !ui.has("cell-T-1"),
        "a 20-column sheet cannot fit in a 900px window, so the last column \
         must be off to the right, not squeezed in:\n{}",
        ui.text()
    );

    ui.scroll_to_the_right();

    // …and one shove of the wheel reaches it, whole.
    ui.rect_of("cell-T-1");
    assert!(
        !ui.has("cell-A-1"),
        "scrolling right did not move the columns at all"
    );
    // The column letter travelled with its column.
    ui.rect_of("T");

    // The gutter did not travel: every row number is still on screen, still at
    // the left edge, after the cells have slid a full window sideways. Painted
    // is not enough — a gutter that scrolled with the content would still be
    // laid out, just a thousand pixels off the left of the window.
    let screen = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into());
    for row in 1..=12 {
        let number = ui.galley_of(&row.to_string());
        assert!(
            number.is_visible(screen) && number.rect.min.x >= 0.0,
            "row number {row} was laid out at {:?}, off the {screen:?} window: \
             the gutter scrolled away with the cells",
            number.rect
        );
    }
    assert!(
        ui.rect_of("1").max.x <= ui.rect_of("cell-T-1").min.x,
        "the row-number gutter is not left of the cells it labels"
    );
}

#[test]
fn the_column_letters_stay_put_while_the_rows_scroll() {
    let path = PathBuf::from("/tmp/tall.xlsx");
    let mut ui = ui_with_path("/tmp/tall.xlsx");
    ui.deliver(FIRST, &path, wide_table(3, 200));

    let before = ui.rect_of("A");
    ui.rect_of("cell-A-1");

    ui.scroll_to_the_bottom();

    let letter = ui.galley_of("A");
    let after = letter.rect;
    assert!(
        (before.min.y - after.min.y).abs() < 1.0,
        "the column letters slid from {before:?} to {after:?} while the rows \
         scrolled — the header is not frozen"
    );
    let screen = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into());
    assert!(
        letter.is_visible(screen),
        "the column letter is still laid out, at {after:?}, but no longer \
         inside the {screen:?} window"
    );
    ui.rect_of("cell-A-200");
    assert!(
        !ui.has("cell-A-1"),
        "the first row is still painted after scrolling 200 rows down"
    );

    // Rows that scroll under the header and the gutter must not show through
    // them, so both are opaque strips rather than bare text.
    let fills = ui.fills();
    let strip = |what: &str, over: Rect, wide: bool| {
        assert!(
            fills.iter().any(|fill| {
                fill.color.a() == 255
                    && fill.rect.contains_rect(over)
                    && if wide {
                        fill.rect.width() > 400.0
                    } else {
                        fill.rect.height() > 200.0
                    }
            }),
            "{what} at {over:?} is painted over nothing opaque, so the rows \
             scrolling under it would show through"
        );
    };
    strip("the header", after, true);
    strip("the gutter", ui.rect_of("200"), false);
}

#[test]
fn a_five_thousand_row_table_paints_only_a_window_of_rows() {
    // The whole point of virtualising: `Grid` measures once, and each frame
    // lays out the thirty-odd rows the pane can actually show. Without it a
    // 5 000-row sheet is 5 000 galleys a frame, all but a handful of them
    // outside the clip rectangle.
    let path = PathBuf::from("/tmp/huge.xlsx");
    let mut ui = ui_with_path("/tmp/huge.xlsx");
    ui.deliver(FIRST, &path, wide_table(3, 5000));

    let painted = ui
        .painted()
        .iter()
        .filter(|painted| painted.text.starts_with("cell-A-"))
        .count();
    assert!(
        painted >= 10,
        "only {painted} of 5000 rows were painted into a {}px pane",
        SIZE[1]
    );
    assert!(
        painted <= 60,
        "{painted} rows were painted for a pane that fits about thirty: the \
         rows are not being virtualised"
    );
    assert!(
        !ui.has("cell-A-5000"),
        "the last row, before anyone scrolls"
    );

    // And the row height the virtualiser is told about is the one the rows are
    // really painted at, or the end of the sheet would be unreachable however
    // far you scrolled.
    ui.scroll_to_the_bottom();
    ui.rect_of("cell-A-5000");
    ui.rect_of("5000");
}

#[test]
fn one_enormous_cell_does_not_make_the_table_ten_thousand_pixels_wide() {
    let note = "x".repeat(4000);
    let content = PreviewContent::Table {
        columns: column_letters(3),
        rows: vec![table_row(
            "1",
            vec![text_cell("a"), text_cell(&note), text_cell("b")],
        )],
        sheets: Vec::new(),
        active_sheet: 0,
        total_rows: 1,
        total_cols: 3,
    };

    let path = PathBuf::from("/tmp/note.xlsx");
    let mut ui = ui_with_path("/tmp/note.xlsx");
    ui.deliver(FIRST, &path, content);

    let painted = ui.galley_of(&note);
    assert!(
        painted.galley.elided,
        "a 4000-character cell was painted in full at {:?}",
        painted.rect.size()
    );
    assert!(
        painted.rect.width() < 400.0,
        "the note column came out {:.0}px wide; the per-column ceiling is not \
         holding",
        painted.rect.width()
    );
    // The column after it is still reachable without scrolling a mile.
    let after = ui.rect_of("b");
    assert!(
        after.min.x < SIZE[0],
        "column C landed at x={:.0}, off the right of a {}px window",
        after.min.x,
        SIZE[0]
    );
}

#[test]
fn a_table_bigger_than_its_preview_says_so_in_the_footer() {
    let PreviewContent::Table { columns, rows, .. } = table_content() else {
        unreachable!("table_content is a table");
    };
    let content = PreviewContent::Table {
        columns,
        rows,
        sheets: Vec::new(),
        active_sheet: 0,
        total_rows: 4000,
        total_cols: 90,
    };

    let path = PathBuf::from("/tmp/big.xlsx");
    let mut ui = ui_with_path("/tmp/big.xlsx");
    ui.respond(
        FIRST,
        &path,
        Outcome::Ready(Box::new(Loaded {
            preview: Preview {
                content,
                truncated: true,
            },
            image: None,
        })),
    );

    ui.assert_shows(
        "4000 rows × 90 columns — showing 3 × 4",
        "the footer's note about what is not shown",
    );
    ui.assert_shows("truncated", "the header's truncation marker");
    ui.assert_shows("STT", "the part of the sheet that did fit");
}

#[test]
fn a_capped_table_that_never_declared_its_size_still_admits_there_is_more() {
    let path = PathBuf::from("/tmp/nodim.xlsx");
    let mut ui = ui_with_path("/tmp/nodim.xlsx");
    // An xlsx written without a `<dimension>`: the row cap bit, but the sheet
    // never said how big it was, so "there is more" is all anyone knows.
    ui.respond(
        FIRST,
        &path,
        Outcome::Ready(Box::new(Loaded {
            preview: Preview {
                content: table_content(),
                truncated: true,
            },
            image: None,
        })),
    );

    ui.assert_shows(
        "showing first 3 rows × 4 columns — more follow",
        "the vaguer footer note",
    );
}

#[test]
fn a_ragged_table_paints_what_it_has_instead_of_panicking() {
    // Everything a malformed sheet can hand over at once: a row with no cells,
    // a row shorter than the column list, a row longer than it, an empty row
    // label, a cell holding a newline, and an `active_sheet` that indexes past
    // the end of the sheet list.
    let content = PreviewContent::Table {
        columns: column_letters(3),
        rows: vec![
            table_row("1", Vec::new()),
            table_row("", vec![text_cell("short")]),
            table_row(
                "3",
                vec![
                    text_cell("one"),
                    text_cell("two\nlines"),
                    cell("#REF!", CellKind::Error),
                    text_cell("extra"),
                    text_cell("more"),
                ],
            ),
        ],
        sheets: vec!["only".to_owned()],
        active_sheet: 9,
        total_rows: 0,
        total_cols: 0,
    };

    let path = PathBuf::from("/tmp/ragged.xlsx");
    let mut ui = ui_with_path("/tmp/ragged.xlsx");
    ui.deliver(FIRST, &path, content);

    ui.assert_shows("short", "a row shorter than the column list");
    ui.assert_shows("#REF!", "an error cell");
    assert_eq!(ui.color_of("#REF!"), DARK.cell_error);
    // A newline inside a cell must not open a second row and push every row
    // below it out of step with the virtualiser.
    let wrapped = ui.galley_of("two lines");
    assert_eq!(
        wrapped.galley.rows.len(),
        1,
        "a cell with a newline in it laid out as {} rows",
        wrapped.galley.rows.len()
    );
    // The IR under-reported its own size; the footer must not say "0 rows".
    ui.assert_shows("3 rows × 3 columns", "the footer");
}

#[test]
fn a_table_is_not_re_requested_when_the_window_is_resized() {
    // The reflow machinery exists because core used to lay a spreadsheet out
    // for a width the frontend had to supply. A `Table` carries the grid, and
    // every width in it is decided on this side, so asking for it again at a
    // new width would re-read the workbook to get byte-identical IR back.
    let path = PathBuf::from("/tmp/steady.xlsx");
    let mut ui = ui_with_path("/tmp/steady.xlsx");
    ui.deliver(FIRST, &path, table_content());

    // Long enough for the reflow settle timer to expire several times over.
    // The same wait against `text_content` produces exactly one re-request —
    // see `a_window_that_is_not_resized_never_re_requests_its_preview`.
    std::thread::sleep(Duration::from_millis(300));
    ui.run();
    ui.run();

    let requests: Vec<Request> = ui
        .requests
        .try_iter()
        .filter(|request| request.kind == Kind::Preview)
        .collect();
    assert!(
        requests.is_empty(),
        "a table on screen must never be re-requested for a new pane width, \
         but {} request(s) went out",
        requests.len()
    );
    ui.assert_shows("STT", "the table, still the one that was delivered");
}

// ---------------------------------------------------------------------------
// 2. The home screen
// ---------------------------------------------------------------------------

#[test]
fn the_home_screen_offers_a_way_to_open_something() {
    let ui = home_ui();

    ui.assert_shows("sekio", "the app name");
    ui.assert_shows("Quick preview for any file", "the tagline");
    ui.assert_shows("or drop a file anywhere in this window", "the drop hint");

    // The controls, as widgets a user (or a screen reader) can actually reach.
    for label in ["Open file…", "Browse files", "Open…", "Browse"] {
        assert!(
            ui.harness.query_by_label(label).is_some(),
            "the home screen has no {label:?} control; the AccessKit tree is:\n{:#?}",
            ui.harness
        );
    }

    // The settings control, which is the only way to reach the theme and the
    // version from inside the window.
    assert!(
        ui.harness.query_by_label("⚙").is_some(),
        "the header has no settings button; the AccessKit tree is:\n{:#?}",
        ui.harness
    );

    // The recent-files area, in its empty state.
    ui.assert_shows("Recent", "the recent-files heading");
    ui.assert_shows(
        "Nothing yet — what you preview shows up here.",
        "the empty recent list",
    );

    // And the key list, which is the other half of "how do I use this".
    ui.assert_shows("Ctrl+O", "the key list");
    ui.assert_shows("open a file", "the key list");
    ui.assert_shows("Ctrl+B", "the key list");

    // Everything above is inside the window, not laid out past its edge.
    let screen = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into());
    for painted in ui.painted() {
        assert!(
            painted.is_visible(screen),
            "{:?} was laid out at {:?}, outside the window or its own clip rect {:?}",
            painted.text,
            painted.rect,
            painted.clip
        );
    }
}

#[test]
fn a_previewed_file_shows_up_in_the_recent_list_on_the_way_home() {
    let dir = std::env::temp_dir().join(format!("sekio-gui-recent-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join("remembered.rs");
    std::fs::write(&path, b"fn main() {}\n").expect("write the fixture file");

    // Mode::App is the window a user launched: Escape goes home rather than
    // closing, which is the only way to see the home screen after a preview.
    let mut ui = AppUi::new(Some(path.clone()), Mode::App);
    ui.run();
    ui.deliver(FIRST, &path, text_content());
    ui.assert_shows("fn main() {", "the preview before going home");

    ui.harness.key_press(egui::Key::Escape);
    ui.run();

    ui.assert_hides("fn main() {", "the preview after Escape");
    ui.assert_shows("Recent", "the recent-files heading");
    ui.assert_shows("remembered.rs", "the file just previewed");
    assert!(
        ui.harness.query_by_label("remembered.rs").is_some(),
        "the recent entry must be a link the user can click"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. The error state
// ---------------------------------------------------------------------------

#[test]
fn a_failed_preview_paints_the_message_rather_than_a_blank_window() {
    let path = PathBuf::from("/tmp/broken.zip");
    let mut ui = ui_with_path("/tmp/broken.zip");
    ui.respond(
        FIRST,
        &path,
        Outcome::Failed("zip central directory is corrupt".to_owned()),
    );

    ui.assert_shows(
        "cannot preview: zip central directory is corrupt",
        "the error message",
    );
    // The header still names the file the error belongs to.
    ui.assert_shows("broken.zip", "the header");
    ui.assert_hides("loading…", "the loading placeholder");

    // Painted in the error colour, not silently in the body colour.
    let message = ui
        .painted()
        .into_iter()
        .find(|p| p.text.starts_with("cannot preview:"))
        .expect("the error galley must be among the painted shapes");
    let color = message
        .galley
        .job
        .sections
        .first()
        .map(|section| section.format.color)
        .expect("the error galley must carry a format");
    assert_ne!(
        color,
        Color32::PLACEHOLDER,
        "the error message was left unstyled"
    );
    assert!(
        color.r() > color.g() && color.r() > color.b(),
        "the error message is painted {color:?}, which does not read as an error"
    );
}

// ---------------------------------------------------------------------------
// 4. Loading, and what replaces it
// ---------------------------------------------------------------------------

#[test]
fn the_loading_placeholder_shows_until_the_result_lands() {
    let path = PathBuf::from("/tmp/huge.log");
    let mut ui = ui_with_path("/tmp/huge.log");

    ui.assert_shows("loading…", "the pending preview");
    ui.assert_shows("huge.log", "the header, which names the file being read");
    assert!(
        ui.harness.query_by_label("loading…").is_some(),
        "the placeholder must be a real widget, not just ink on the canvas"
    );

    ui.deliver(FIRST, &path, text_content());
    ui.assert_hides("loading…", "the settled preview");
    ui.assert_shows("fn main() {", "the preview that replaced it");
}

#[test]
fn a_stale_result_never_replaces_the_loading_placeholder() {
    let path = PathBuf::from("/tmp/huge.log");
    let mut ui = ui_with_path("/tmp/huge.log");

    // Generation 0 belongs to a request that was never made; the tracker is
    // waiting on 1. Painting this would mean showing a file the user has
    // already moved past.
    ui.deliver(0, Path::new("/tmp/somethingelse.rs"), text_content());
    ui.assert_shows("loading…", "the still-pending preview");
    ui.assert_hides("fn main() {", "the stale result");

    ui.deliver(FIRST, &path, text_content());
    ui.assert_shows("fn main() {", "the result that was actually asked for");
}

#[test]
fn a_cancelled_result_goes_back_to_loading() {
    let path = PathBuf::from("/tmp/huge.log");
    let mut ui = ui_with_path("/tmp/huge.log");
    ui.respond(FIRST, &path, Outcome::Cancelled);

    ui.assert_shows("loading…", "a cancellation is normal control flow");
    ui.assert_hides("cannot preview", "a cancellation is not an error");
}

// ---------------------------------------------------------------------------
// 5b. Light mode
// ---------------------------------------------------------------------------
//
// The frames above all render in the dark fallback, which is what a headless
// harness resolves `System` to. These drive the other half: the same app, the
// same content, with egui's theme preference flipped — which is exactly what a
// desktop switching to light mode does to a running window.

impl AppUi {
    /// Switch the whole window to the light palette, the way the desktop would.
    ///
    /// `set_theme` is what `style::install` calls; going through it rather than
    /// through `SekioApp` proves the app *follows* the theme rather than
    /// deciding it once at startup.
    fn switch_to_light(&mut self) {
        self.harness.ctx.set_theme(egui::ThemePreference::Light);
        self.run();
    }
}

/// The home screen's one-line tagline.
///
/// The version used to live at the end of this string and now rides beside the
/// wordmark as its own mono chip, so the tagline is a fixed sentence and the
/// build number is asserted separately by `the_home_screen_shows_its_version`.
fn subtitle() -> String {
    "Quick preview for any file".to_owned()
}

#[test]
fn a_window_opens_in_the_dark_palette_when_the_desktop_says_nothing() {
    // No system theme reaches a headless harness, so this is the documented
    // fallback path — and the one every other test in this file relies on.
    let ui = home_ui();
    assert_eq!(ui.harness.ctx.theme(), egui::Theme::Dark);
    assert_eq!(
        ui.color_of(&subtitle()),
        DARK.dim,
        "the home screen's subtitle is painted in the dark palette's dim"
    );
}

#[test]
fn switching_to_light_repaints_the_chrome_in_the_light_palette() {
    let light = Palette::light();
    let mut ui = home_ui();
    let subtitle = subtitle();
    assert_eq!(ui.color_of(&subtitle), DARK.dim);

    ui.switch_to_light();

    assert_eq!(ui.harness.ctx.theme(), egui::Theme::Light);
    assert_eq!(
        ui.color_of(&subtitle),
        light.dim,
        "a secondary label kept its dark-mode colour on a light background"
    );
    // The surface really did change too, not just the text on it.
    assert!(
        ui.fills().iter().any(|fill| fill.color == light.card),
        "nothing on the frame is painted on the light preview surface"
    );
    assert!(
        !ui.fills().iter().any(|fill| fill.color == DARK.card),
        "a dark surface survived the switch to light mode"
    );
}

#[test]
fn a_light_window_paints_its_cells_in_the_light_palette() {
    let light = Palette::light();
    let path = PathBuf::from("/tmp/sheet.xlsx");
    let mut ui = ui_with_path("/tmp/sheet.xlsx");
    ui.deliver(FIRST, &path, table_content());
    assert_eq!(ui.color_of("47.3"), DARK.cell_number);

    ui.switch_to_light();

    assert_eq!(
        ui.color_of("47.3"),
        light.cell_number,
        "a numeric cell must not keep a dark-mode colour on a light sheet"
    );
    assert_eq!(ui.color_of("Hoạt động"), light.cell_text);
    assert_eq!(ui.color_of("A"), light.dim, "a column letter");
    assert_eq!(ui.color_of("[Tong]"), light.active, "the active sheet");
    // The frozen strips the cells slide under are the light surface, or the
    // grid would scroll straight through the row numbers.
    assert!(
        ui.fills().iter().any(|fill| fill.color == light.card),
        "the frozen header and gutter are not painted on the light surface"
    );
}

#[test]
fn a_light_window_relays_out_its_text_in_the_light_palette() {
    // The colourless spans are the ones that take the palette's own body
    // colour; a cached `LayoutJob` that survived the switch would still be
    // painting them in the dark one.
    let path = PathBuf::from("/tmp/main.rs");
    let mut ui = ui_with_path("/tmp/main.rs");
    ui.deliver(FIRST, &path, text_content());

    let plain_color = |ui: &AppUi| {
        let body = ui.galley_of("fn main() {\n    println!(\"sekio\");\n}");
        let job = &body.galley.job;
        job.sections
            .iter()
            .find(|section| {
                // The document's own newline carries the same format as the
                // colourless span after it, so the two arrive as one run.
                let range = section.byte_range.start.0..section.byte_range.end.0;
                job.text[range].contains("println!")
            })
            .map(|section| section.format.color)
            .expect("the uncoloured run")
    };
    assert_eq!(plain_color(&ui), DARK.text);

    ui.switch_to_light();
    assert_eq!(
        plain_color(&ui),
        Palette::light().text,
        "the cached layout job kept the dark palette's body colour"
    );
}

/// The syntax colours arrive in the IR already baked, so the *only* way a light
/// window stops showing dark-theme code is for the preview to be rendered
/// again. This is the request that makes that happen.
#[test]
fn switching_mode_re_requests_the_preview_so_the_code_colours_follow() {
    let path = PathBuf::from("/tmp/main.rs");
    let mut ui = ui_with_path("/tmp/main.rs");
    ui.deliver(FIRST, &path, text_content());
    // Whatever the first frames asked for is water under the bridge.
    let _ = ui.requests.try_iter().count();

    ui.switch_to_light();

    let requests: Vec<Request> = ui
        .requests
        .try_iter()
        .filter(|request| request.kind == Kind::Preview)
        .collect();
    assert_eq!(
        requests.len(),
        1,
        "a mode switch must re-render the file on screen exactly once"
    );
    assert_eq!(requests[0].path, path);
    // …and the file stays on screen while it does, rather than blanking.
    ui.assert_shows("fn main() {", "the preview during a theme switch");
    ui.assert_hides("loading…", "a theme switch is not a navigation");
}

#[test]
fn the_home_screen_does_not_re_request_anything_when_the_mode_changes() {
    let mut ui = home_ui();
    let _ = ui.requests.try_iter().count();
    ui.switch_to_light();
    assert!(
        ui.requests.try_iter().next().is_none(),
        "there is nothing on screen to re-render"
    );
}

// ---------------------------------------------------------------------------
// 5. Styling survives into the layout
// ---------------------------------------------------------------------------

#[test]
fn bold_coloured_and_italic_spans_keep_their_attributes_in_the_painted_galley() {
    let plain = (200u8, 200u8, 200u8);
    let coloured = (12u8, 240u8, 90u8);
    let content = PreviewContent::Text {
        lines: vec![line(vec![
            span("plain ", Some(plain), false, false),
            span("BOLD ", Some(plain), true, false),
            span("coloured ", Some(coloured), false, false),
            span("italic", Some(plain), false, true),
        ])],
        language: "Rust".to_owned(),
    };

    let path = PathBuf::from("/tmp/styled.rs");
    let mut ui = ui_with_path("/tmp/styled.rs");
    ui.deliver(FIRST, &path, content);

    let painted = ui.painted();
    let body = painted
        .iter()
        .find(|p| p.text.contains("plain BOLD coloured italic"))
        .expect("the styled line must be among the painted shapes");

    let sections = &body.galley.job.sections;
    assert_eq!(
        sections.len(),
        4,
        "each span must keep its own run: {sections:#?}"
    );

    let run = |needle: &str| {
        sections
            .iter()
            .find(|section| {
                let range = section.byte_range.start.0..section.byte_range.end.0;
                body.galley.job.text[range].starts_with(needle)
            })
            .map(|section| section.format.clone())
            .unwrap_or_else(|| panic!("no laid-out run for {needle:?}"))
    };

    let plain_run = run("plain");
    let bold_run = run("BOLD");
    let coloured_run = run("coloured");
    let italic_run = run("italic");

    assert_eq!(plain_run.color, Color32::from_rgb(200, 200, 200));
    assert_eq!(
        bold_run.color,
        style::brighten(Color32::from_rgb(200, 200, 200), egui::Theme::Dark),
        "bold is painted as a lifted colour (egui's bundled fonts have no bold face)"
    );
    assert!(
        bold_run.color.r() > plain_run.color.r(),
        "the bold run is not visibly brighter than the plain one"
    );
    assert_eq!(
        coloured_run.color,
        Color32::from_rgb(12, 240, 90),
        "the syntect colour did not survive into the layout"
    );
    assert!(italic_run.italics, "the italic run lost its slant");
    assert!(!plain_run.italics);

    // All four runs are monospace at the size the rest of the app uses…
    for (label, format) in [
        ("plain", &plain_run),
        ("bold", &bold_run),
        ("coloured", &coloured_run),
        ("italic", &italic_run),
    ] {
        assert_eq!(
            format.font_id,
            egui::FontId::monospace(style::MONO_SIZE),
            "the {label} run is not in the monospace face the preview is laid out in"
        );
    }

    // …and the galley really was laid out, rather than collapsing to nothing.
    assert!(
        body.rect.width() > 20.0 && body.rect.height() > 5.0,
        "the styled line laid out to {:?}",
        body.rect.size()
    );
}

// ---------------------------------------------------------------------------
// The panels the previews live between
// ---------------------------------------------------------------------------

#[test]
fn the_header_and_footer_sit_above_and_below_the_body() {
    let path = PathBuf::from("/tmp/main.rs");
    let mut ui = ui_with_path("/tmp/main.rs");
    ui.deliver(FIRST, &path, text_content());

    let painted = ui.painted();
    let find = |needle: &str| {
        painted
            .iter()
            .find(|p| p.text.contains(needle))
            .map(|p| p.rect)
            .unwrap_or_else(|| panic!("nothing painted containing {needle:?}"))
    };

    let header = find("main.rs");
    let body = find("fn main() {");
    let footer = find("Rust · 3 lines");

    assert!(
        header.max.y <= body.min.y,
        "the header ({header:?}) overlaps the body ({body:?})"
    );
    assert!(
        body.max.y <= footer.min.y,
        "the body ({body:?}) overlaps the footer ({footer:?})"
    );
    assert!(
        footer.max.y <= SIZE[1],
        "the footer at {footer:?} is below the bottom of the window"
    );
}

/// A workbook is more than its first sheet, and the strip across the top is
/// the only way to reach the rest. Clicking one re-renders the same file with
/// that sheet chosen, which is a fresh request rather than anything the
/// frontend can do to the table it already has.
#[test]
fn clicking_a_sheet_asks_the_worker_for_that_sheet() {
    let path = PathBuf::from("/tmp/book.xlsx");
    let mut ui = ui_with_path("/tmp/book.xlsx");
    ui.deliver(FIRST, &path, table_content());

    ui.assert_shows("[Tong]", "the active sheet");
    ui.assert_shows("Chi tiết", "the sheet that is not being shown");

    // Drain the request that opened the file, so what is left is the click's.
    let _ = ui.requests.try_iter().count();

    ui.harness.get_by_label("Chi tiết").click();
    ui.run();

    let request = ui
        .requests
        .try_iter()
        .find(|request| request.kind == Kind::Preview)
        .expect("clicking a sheet must ask for a new preview");
    assert_eq!(request.sheet, 1, "the sheet the user clicked");
    assert_eq!(request.path, path, "the same workbook, a different sheet");
}

/// A pane cannot be dragged narrower than the widest thing inside it, so a
/// listing that draws its names in full pins the pane open at the width of its
/// longest filename — which is exactly what happened when the rows were laid
/// out with `TextWrapMode::Extend`. Names are truncated instead, so one long
/// entry cannot decide how much of the window the browser owns.
#[test]
fn a_very_long_filename_does_not_pin_the_browser_pane_open() {
    let path = PathBuf::from("/tmp/main.rs");
    let mut ui = ui_with_path("/tmp/main.rs");
    ui.deliver(FIRST, &path, text_content());

    ui.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::B);
    ui.run();

    let listing = ui
        .requests
        .try_iter()
        .find(|request| request.kind == Kind::Browse)
        .expect("opening the browser must request a directory listing");

    let long = "a".repeat(400);
    ui.responses
        .send(Response {
            id: listing.id,
            path: listing.path.clone(),
            outcome: Outcome::Ready(Box::new(Loaded {
                preview: Preview {
                    content: PreviewContent::Listing {
                        entries: vec![ListEntry {
                            name: long.clone(),
                            is_dir: false,
                            size: Some(1),
                        }],
                    },
                    truncated: false,
                },
                image: None,
            })),
            elapsed: Duration::from_millis(1),
            kind: Kind::Browse,
        })
        .expect("the app still owns the response channel");
    ui.run();

    // The pane's configured ceiling is 640; allow a little for its own margins
    // and the separator. Laid out with `Extend`, 400 characters at 12 px draw
    // something like 2400 px wide and drag the pane along with them.
    const CEILING: f32 = 700.0;
    let entry = ui
        .painted()
        .into_iter()
        .find(|painted| painted.text.starts_with("aaa"))
        .expect("the entry should still be listed, just cut short");
    assert!(
        entry.rect.max.x < CEILING,
        "the row is painted out to x={}, past the pane's ceiling of {CEILING}",
        entry.rect.max.x
    );
    assert!(
        entry.text.chars().count() < long.chars().count(),
        "the name should be cut, not merely clipped: {} characters were laid out",
        entry.text.chars().count()
    );
    assert!(
        entry.text.ends_with('…'),
        "a cut name should say so: {:?}",
        entry.text.chars().rev().take(4).collect::<String>()
    );
}

#[test]
fn the_browser_pane_opens_beside_the_preview_and_lists_a_directory() {
    let path = PathBuf::from("/tmp/main.rs");
    let mut ui = ui_with_path("/tmp/main.rs");
    ui.deliver(FIRST, &path, text_content());

    // Ctrl+B, exactly as the home screen advertises.
    ui.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::B);
    ui.run();

    // The pane asked the worker for its listing on its own generation counter.
    let listing = ui
        .requests
        .try_iter()
        .find(|request| request.kind == Kind::Browse)
        .expect("opening the browser must request a directory listing");
    ui.assert_shows("listing…", "the pane before its listing lands");

    let dir = listing.path.clone();
    ui.responses
        .send(Response {
            id: listing.id,
            path: dir,
            outcome: Outcome::Ready(Box::new(Loaded {
                preview: Preview {
                    content: listing_content(),
                    truncated: false,
                },
                image: None,
            })),
            elapsed: Duration::from_millis(1),
            kind: Kind::Browse,
        })
        .expect("the app still owns the response channel");
    ui.run();

    ui.assert_shows("Cargo.toml", "the browser pane's entries");
    ui.assert_shows("fn main() {", "the preview, still there beside the pane");

    let painted = ui.painted();
    let entry = painted
        .iter()
        .find(|p| p.text == "Cargo.toml")
        .map(|p| p.rect)
        .expect("the pane entry must be painted");
    let body = painted
        .iter()
        .find(|p| p.text.contains("fn main() {"))
        .map(|p| p.rect)
        .expect("the preview must still be painted");
    assert!(
        entry.max.x <= body.min.x,
        "the pane ({entry:?}) is not to the left of the preview ({body:?})"
    );
}

#[test]
fn a_cover_image_paints_above_the_metadata_fields() {
    let content = PreviewContent::Metadata {
        fields: vec![MetaField::new("title", "Sekio Theme")],
        thumbnail: Some(image::RgbaImage::from_pixel(
            300,
            300,
            image::Rgba([90, 30, 30, 255]),
        )),
    };

    let path = PathBuf::from("/tmp/song.flac");
    let mut ui = ui_with_path("/tmp/song.flac");
    ui.deliver(FIRST, &path, content);

    let images = ui.images();
    assert_eq!(
        images.len(),
        1,
        "the cover art was not painted; a `Metadata` thumbnail is easy to \
         upload and then never draw"
    );
    let cover = &images[0];
    assert_eq!(ui.uploaded_size(cover.texture), [300, 300]);
    // `fit` boxes the thumbnail into 240×240, and never stretches it.
    assert!(
        (cover.rect.width() - 240.0).abs() < 1.0 && (cover.rect.height() - 240.0).abs() < 1.0,
        "the 300×300 cover should be boxed to 240×240, got {:?}",
        cover.rect.size()
    );

    let title = ui
        .painted()
        .into_iter()
        .find(|p| p.text == "title")
        .map(|p| p.rect)
        .expect("the fields must still be painted under the cover");
    assert!(
        cover.rect.max.y <= title.min.y,
        "the cover ({:?}) overlaps the fields ({title:?})",
        cover.rect
    );
}

#[test]
fn zooming_an_image_changes_the_size_it_is_painted_at() {
    let path = PathBuf::from("/tmp/photo.png");
    let mut ui = ui_with_path("/tmp/photo.png");
    ui.deliver(FIRST, &path, image_content());

    let before = ui.images().first().map(|img| img.rect).expect("an image");

    // Ctrl++ zooms the picture itself, not the whole UI (see `apply_zoom`).
    ui.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Plus);
    ui.run();

    let after = ui
        .images()
        .first()
        .map(|img| img.rect)
        .expect("the image must survive being zoomed");
    assert!(
        after.width() > before.width() * 1.2,
        "Ctrl++ took the image from {:?} to {:?} — the zoom never reached the paint",
        before.size(),
        after.size()
    );
    // The aspect ratio must survive the zoom.
    assert!(
        (after.width() / after.height() - before.width() / before.height()).abs() < 0.01,
        "zooming distorted the image: {:?} -> {:?}",
        before.size(),
        after.size()
    );
    ui.assert_shows("125%", "the footer, which reports the zoom");
}

#[test]
fn a_truncated_preview_says_so_in_the_header() {
    let path = PathBuf::from("/tmp/huge.log");
    let mut ui = ui_with_path("/tmp/huge.log");
    ui.respond(
        FIRST,
        &path,
        Outcome::Ready(Box::new(Loaded {
            preview: Preview {
                content: text_content(),
                truncated: true,
            },
            image: None,
        })),
    );

    ui.assert_shows("truncated", "the header's truncation marker");
    ui.assert_shows("fn main() {", "the part of the file that did fit");
}

#[test]
fn code_scrolls_sideways_instead_of_reflowing() {
    let long = "let x = ".to_owned() + &"reallylongidentifier + ".repeat(30) + "0;";
    let content = PreviewContent::Text {
        lines: vec![
            line(vec![span("fn main() {", None, false, false)]),
            line(vec![span(&long, None, false, false)]),
            line(vec![span("}", None, false, false)]),
        ],
        language: "Rust".to_owned(),
    };

    let path = PathBuf::from("/tmp/wide.rs");
    let mut ui = ui_with_path("/tmp/wide.rs");
    ui.deliver(FIRST, &path, content);

    let body = ui
        .painted()
        .into_iter()
        .find(|p| p.text.contains("reallylongidentifier"))
        .expect("the wide line must be painted");
    assert_eq!(
        body.galley.rows.len(),
        3,
        "three source lines must lay out as three rows; reflowing code is \
         exactly what `TextWrapMode::Extend` is there to prevent"
    );
    assert!(
        body.rect.width() > SIZE[0],
        "the wide line laid out to {:.0}px, narrower than the {}px window, so \
         it must have been wrapped after all",
        body.rect.width(),
        SIZE[0]
    );
    // …and the scroll area clips it at the window edge rather than letting it
    // paint over the chrome.
    assert!(
        body.clip.max.x <= SIZE[0] + 0.5,
        "the code is clipped to {:?}, which reaches past the window",
        body.clip
    );
}

#[test]
fn dragging_a_file_over_the_window_paints_the_drop_hint() {
    let mut ui = home_ui();
    ui.assert_hides("Drop to preview", "before anything is dragged over");

    ui.harness
        .input_mut()
        .hovered_files
        .push(egui::HoveredFile {
            path: Some(PathBuf::from("/tmp/dragged.png")),
            ..Default::default()
        });
    ui.harness.step();

    ui.assert_shows("Drop to preview", "the drop overlay");
    let hint = ui
        .painted()
        .into_iter()
        .find(|p| p.text == "Drop to preview")
        .expect("the hint must be painted");
    let screen = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into());
    assert!(
        screen.contains_rect(hint.rect),
        "the drop hint landed at {:?}, outside the window",
        hint.rect
    );
}

// ---------------------------------------------------------------------------
// Text egui's bundled fonts cannot draw, and paths Windows spells oddly
// ---------------------------------------------------------------------------

/// A code point no font has a glyph for. Whatever epaint rasterises for it in
/// a given font *is* that font's replacement box, which is how "is this a
/// box?" is asked below without hard-coding which character egui chose for it.
const NEVER_DRAWN: char = '\u{10FFFD}';

/// The file name from the bug report — it came out as
/// `00. Thông cáo báo chí v□ FLC.pdf`, because `ề` (U+1EC1) is in Latin
/// Extended Additional and neither Ubuntu-Light nor Hack has it.
const VIETNAMESE_FILE: &str = "Thông cáo báo chí về FLC.pdf";

/// Vietnamese prose for the *contents* of a preview, which is drawn in the
/// monospace family rather than the proportional one.
const VIETNAMESE_TEXT: &str = "Mật độ dân số tăng — nguyện vọng của người dân về việc này.";

/// The atlas rectangle of the replacement box, for every font `galley` was
/// laid out in.
///
/// Per font, because the box is a rasterised glyph like any other: the same
/// `◻` at 11pt and at 13pt occupies two different rectangles, so a single
/// "the box" value would compare against the wrong one.
fn replacement_boxes(ctx: &egui::Context, galley: &egui::Galley) -> Vec<([u16; 2], [u16; 2])> {
    let pixels_per_point = ctx.pixels_per_point();
    let mut fonts: Vec<egui::FontId> = Vec::new();
    for section in &galley.job.sections {
        if !fonts.contains(&section.format.font_id) {
            fonts.push(section.format.font_id.clone());
        }
    }

    let mut boxes = Vec::new();
    for font_id in fonts {
        // epaint bins each glyph's fractional x position into one of four
        // sub-pixel offsets and rasterises a separate bitmap per bin, so "the"
        // replacement box is really four rectangles in the atlas. Which one a
        // painted glyph got depends on where in its row it landed, so all four
        // are collected: a probe at one fixed offset would miss.
        for bin in 0..4 {
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = f32::INFINITY;
            job.append(
                &NEVER_DRAWN.to_string(),
                bin as f32 * 0.25 / pixels_per_point,
                egui::TextFormat::simple(font_id.clone(), Color32::WHITE),
            );
            let probe = ctx.fonts_mut(|f| f.layout_job(job));
            let uv = probe.rows[0].row.glyphs[0].uv_rect;
            boxes.push((uv.min, uv.max));
        }
    }
    boxes
}

/// Assert that every character of `needle` really was rasterised as itself in
/// the frame that painted it.
///
/// This reads the *painted galley*, not the string: a galley whose text is
/// `…về…` while its glyphs all point at the replacement box is exactly the bug
/// in the screenshot, and `assert_shows` cannot tell the difference. Drop the
/// fallback from `fonts::with_fallbacks` and this fails.
#[track_caller]
fn assert_no_tofu(ui: &AppUi, needle: &str) {
    let painted = ui.painted();
    let hit = painted
        .iter()
        .find(|p| p.text.contains(needle))
        .unwrap_or_else(|| panic!("nothing painted containing {needle:?}:\n{}", ui.text()));
    let boxes = replacement_boxes(&ui.harness.ctx, &hit.galley);

    let wanted: std::collections::BTreeSet<char> =
        needle.chars().filter(|c| !c.is_whitespace()).collect();
    let mut drawn = std::collections::BTreeSet::new();
    for row in &hit.galley.rows {
        for glyph in &row.row.glyphs {
            if !wanted.contains(&glyph.chr) {
                continue;
            }
            let uv = (glyph.uv_rect.min, glyph.uv_rect.max);
            assert!(
                !boxes.contains(&uv),
                "U+{:04X} {:?} in {needle:?} painted as the replacement box, not as itself",
                glyph.chr as u32,
                glyph.chr
            );
            assert_ne!(
                uv.0, uv.1,
                "U+{:04X} {:?} in {needle:?} painted nothing at all",
                glyph.chr as u32, glyph.chr
            );
            drawn.insert(glyph.chr);
        }
    }
    assert_eq!(
        drawn, wanted,
        "some characters of {needle:?} produced no glyph at all"
    );
}

#[test]
fn a_vietnamese_file_name_paints_real_glyphs_in_the_header_and_the_recent_list() {
    let dir = std::env::temp_dir().join(format!("sekio-gui-tofu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join(format!("00. {VIETNAMESE_FILE}"));
    std::fs::write(&path, b"%PDF-1.4\n").expect("write the fixture file");

    let mut ui = AppUi::new(Some(path.clone()), Mode::App);
    ui.run();
    ui.deliver(FIRST, &path, metadata_content());

    // The header, in the proportional family.
    ui.assert_shows(VIETNAMESE_FILE, "the file name in the header");
    assert_no_tofu(&ui, VIETNAMESE_FILE);

    // And the recent list on the way home, which is where the screenshot was
    // taken.
    ui.harness.key_press(egui::Key::Escape);
    ui.run();
    ui.assert_shows("Recent", "the recent-files heading");
    ui.assert_shows(VIETNAMESE_FILE, "the file name in the recent list");
    assert_no_tofu(&ui, VIETNAMESE_FILE);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vietnamese_file_contents_paint_real_glyphs_in_the_monospace_body() {
    let content = PreviewContent::Text {
        lines: vec![
            line(vec![span(
                VIETNAMESE_TEXT,
                Some((200, 200, 200)),
                false,
                false,
            )]),
            // Every precomposed Vietnamese letter there is, so this cannot
            // pass by covering only the handful the default fonts compose out
            // of a base letter and a combining mark.
            line(vec![span(
                &(0x1EA0..=0x1EF9u32)
                    .filter_map(char::from_u32)
                    .collect::<String>(),
                None,
                false,
                false,
            )]),
        ],
        language: "Plain Text".to_owned(),
    };

    let path = PathBuf::from("/tmp/ghi-chú.txt");
    let mut ui = ui_with_path("/tmp/ghi-chú.txt");
    ui.deliver(FIRST, &path, content);

    ui.assert_shows(VIETNAMESE_TEXT, "the previewed Vietnamese text");
    assert_no_tofu(&ui, VIETNAMESE_TEXT);
    assert_no_tofu(
        &ui,
        &(0x1EA0..=0x1EF9u32)
            .filter_map(char::from_u32)
            .collect::<String>(),
    );

    // The body really is the monospace family — a proportional fallback here
    // would break every hexdump and listing column in the app.
    let painted = ui.painted();
    let body = painted
        .iter()
        .find(|p| p.text.contains(VIETNAMESE_TEXT))
        .expect("the Vietnamese line must be among the painted shapes");
    for section in &body.galley.job.sections {
        assert_eq!(
            section.format.font_id,
            egui::FontId::monospace(style::MONO_SIZE)
        );
    }
}

#[test]
fn the_window_never_paints_a_windows_verbatim_path_prefix() {
    // What `Path::canonicalize` hands back on Windows. `paths::plain` is the
    // one place that rewrites it, and it works on the string form, so this
    // runs the same on either host.
    let previewed = sekio_core::paths::plain(Path::new(r"\\?\C:\Users\Admin\Downloads\note.txt"));
    assert_eq!(
        previewed,
        PathBuf::from(r"C:\Users\Admin\Downloads\note.txt")
    );

    let mut ui = AppUi::new(Some(previewed.clone()), Mode::App);
    ui.run();
    ui.deliver(FIRST, &previewed, text_content());

    // Only the negative is asserted about the frame. The header paints the
    // file *name*, not the whole path, so asserting the full path appears is
    // wrong on Windows and passes on Linux only because Linux cannot split a
    // `C:\...` path into components and hands back the entire string as the
    // file name. That host dependence is exactly the trap CLAUDE.md records;
    // the rewrite itself is covered by the unit tests beside the helper.
    ui.assert_shows("note.txt", "the file name in the header");
    ui.assert_hides(r"\\?\", "the extended-length prefix must never be painted");
}

/// The four shapes the UI depends on `paths::strip_verbatim` getting right.
/// Host-independent by construction: it is a string rewrite, and nothing in it
/// asks the OS what a path means. The exhaustive cases (device paths, long
/// paths, trailing dots) live beside the helper in `src/paths.rs`.
#[test]
fn the_verbatim_prefix_helper_handles_both_platforms_shapes_on_either_host() {
    use sekio_core::paths::strip_verbatim;

    assert_eq!(strip_verbatim(r"\\?\C:\x"), r"C:\x");
    assert_eq!(strip_verbatim(r"\\?\UNC\srv\share"), r"\\srv\share");
    assert_eq!(strip_verbatim(r"C:\x"), r"C:\x");
    assert_eq!(strip_verbatim("/home/x"), "/home/x");
    assert_eq!(strip_verbatim(r"\\server\share"), r"\\server\share");
}

// ---------------------------------------------------------------------------
// Laying the preview out for the window it is actually in
// ---------------------------------------------------------------------------

/// A spreadsheet whose natural layout wants about ninety characters: wide
/// enough that a small window has to squeeze it, narrow enough that a big one
/// does not. Built here rather than committed as a binary fixture.
fn xlsx() -> Vec<u8> {
    use std::io::Write as _;

    let rows: [[&str; 4]; 3] = [
        ["STT", "Hoạt động", "Kết quả (giờ quy đổi)", "Ghi chú"],
        ["1.3", "Đứng lớp hướng dẫn thực hành", "47.3", ""],
        [
            "8",
            "Các hoạt động hỗ trợ khác",
            "12",
            "Hỗ trợ lễ bảo vệ khóa luận",
        ],
    ];
    let mut sheet = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    for (r, row) in rows.iter().enumerate() {
        sheet.push_str(&format!(r#"<row r="{}">"#, r + 1));
        for (c, value) in row.iter().enumerate() {
            if value.is_empty() {
                continue;
            }
            let column = (b'A' + c as u8) as char;
            let reference = format!("{column}{}", r + 1);
            if value.parse::<f64>().is_ok() {
                sheet.push_str(&format!(r#"<c r="{reference}"><v>{value}</v></c>"#));
            } else {
                sheet.push_str(&format!(
                    r#"<c r="{reference}" t="inlineStr"><is><t>{value}</t></is></c>"#
                ));
            }
        }
        sheet.push_str("</row>");
    }
    sheet.push_str("</sheetData></worksheet>");

    let parts: [(&str, String); 5] = [
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.to_owned(),
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_owned(),
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Tong" sheetId="1" r:id="rId1"/></sheets></workbook>"#.to_owned(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_owned(),
        ),
        ("xl/worksheets/sheet1.xml", sheet),
    ];

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, body) in parts {
        writer.start_file(name, options).expect("start part");
        writer.write_all(body.as_bytes()).expect("write part");
    }
    writer.finish().expect("finish zip").into_inner()
}

/// The bug, end to end: a real workbook, read by core's real spreadsheet
/// renderer, painted by this app, with every cell whole.
///
/// This is the one test here that goes through core rather than a hand-built
/// IR, and it deliberately asserts nothing about *which* IR came back. It used
/// to assert that a wider window produced a wider table, because core flattened
/// a sheet into space-aligned text and had to be told how many characters the
/// pane had. It no longer does: the grid comes over structured and this
/// frontend decides the widths, so "wider window, wider table" is not the
/// contract any more — "the cells arrive whole and are painted whole" is.
#[test]
fn a_real_workbook_paints_every_cell_whole() {
    let dir = std::env::temp_dir().join(format!("sekio-gui-width-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join("bang-cong.xlsx");
    std::fs::write(&path, xlsx()).expect("write the fixture workbook");

    let preview = sekio_core::Previewer::new()
        .preview(
            &path,
            &sekio_core::PreviewOptions::default(),
            &sekio_core::CancelToken::new(),
        )
        .expect("the workbook must render");

    let mut ui = AppUi::sized(Some(path.clone()), Mode::App, [1500.0, 620.0]);
    ui.run();
    ui.deliver(FIRST, &path, preview.content);

    // Every one of these is a whole cell of the fixture. `assert_shows` is a
    // substring match over the painted galleys, so a cell that came out as
    // "Đứng lớp hướng dẫn…" fails here — which is exactly the screenshot in
    // the bug report.
    for whole in [
        "STT",
        "Hoạt động",
        "Kết quả (giờ quy đổi)",
        "Ghi chú",
        "Đứng lớp hướng dẫn thực hành",
        "Các hoạt động hỗ trợ khác",
        "Hỗ trợ lễ bảo vệ khóa luận",
    ] {
        ui.assert_shows(whole, "a cell of the workbook");
    }
    // The numbers survive too: the cheapest thing to elide and the most
    // expensive thing to lose.
    ui.assert_shows("47.3", "a value");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A resize that changes nothing worth re-laying-out must not cost a render.
/// The threshold is what keeps a window drag from queueing one preview per
/// frame; without it this test would see a request every time.
#[test]
fn a_window_that_is_not_resized_never_re_requests_its_preview() {
    let path = PathBuf::from("/tmp/steady.rs");
    // 900px is nowhere near core's 120-character default, so this window does
    // re-request once — and then must settle down and stop.
    let mut ui = ui_with_path("/tmp/steady.rs");
    ui.deliver(FIRST, &path, text_content());
    std::thread::sleep(Duration::from_millis(200));
    ui.run();

    let first: Vec<Request> = ui
        .requests
        .try_iter()
        .filter(|request| request.kind == Kind::Preview)
        .collect();
    assert_eq!(
        first.len(),
        1,
        "one re-layout for the window's real width, no more"
    );
    ui.deliver(first[0].id, &path, text_content());

    // Nothing moves from here on, however long we wait.
    std::thread::sleep(Duration::from_millis(300));
    ui.run();
    ui.run();
    assert!(
        ui.requests
            .try_iter()
            .all(|request| request.kind != Kind::Preview),
        "a window sitting still must not keep re-rendering its preview"
    );
}
