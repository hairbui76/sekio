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
use sekio_core::{ListEntry, MetaField, Preview, PreviewContent, Span, StyledLine};
use sekio_gui::app::{SekioApp, Startup};
use sekio_gui::state::{Mode, RequestTracker};
use sekio_gui::style;
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

/// Point the recent-files store at a scratch directory.
///
/// `SekioApp::new` unconditionally spawns `recent::Store`, which reads and
/// writes the *user's* real list. A test suite must not touch it, so the whole
/// process is redirected once, before any harness (and therefore any store
/// thread) exists.
fn isolate_state_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("sekio-gui-render-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch state directory");
        std::env::set_var("XDG_STATE_HOME", &dir);
        std::env::set_var("LOCALAPPDATA", &dir);
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

    /// Spin the wheel far enough to hit the end of whatever is on screen.
    /// `ScrollArea` clamps the offset, and `kittest` disables egui's scroll
    /// animation, so one shove lands exactly at the bottom.
    fn scroll_to_the_bottom(&mut self) {
        let middle = Rect::from_min_size(egui::Pos2::ZERO, SIZE.into()).center();
        self.harness.event(egui::Event::PointerMoved(middle));
        self.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::Vec2::new(0.0, -100_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        self.run();
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
// 2. The home screen
// ---------------------------------------------------------------------------

#[test]
fn the_home_screen_offers_a_way_to_open_something() {
    let ui = home_ui();

    ui.assert_shows("sekio", "the app name");
    ui.assert_shows("quick preview for any file", "the tagline");
    ui.assert_shows("…or drop a file anywhere in this window.", "the drop hint");

    // The controls, as widgets a user (or a screen reader) can actually reach.
    for label in ["Open file…", "Browse files", "Open…", "Browse"] {
        assert!(
            ui.harness.query_by_label(label).is_some(),
            "the home screen has no {label:?} control; the AccessKit tree is:\n{:#?}",
            ui.harness
        );
    }

    // The recent-files area, in its empty state.
    ui.assert_shows("Recent", "the recent-files heading");
    ui.assert_shows(
        "nothing yet — what you preview shows up here.",
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
        style::brighten(Color32::from_rgb(200, 200, 200)),
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

/// The painted table itself — the body galley, not the chrome around it. The
/// header and the "Open…" button both carry an ellipsis of their own, so the
/// whole frame's text cannot answer "was a cell elided?".
fn painted_table(ui: &AppUi) -> String {
    let table: Vec<String> = ui
        .painted()
        .iter()
        .filter(|painted| painted.text.contains("STT"))
        .map(|painted| painted.text.clone())
        .collect();
    assert!(
        !table.is_empty(),
        "no table was painted; the frame said:\n{}",
        ui.text()
    );
    table.join("\n")
}

/// Widest row of the painted table, in characters.
fn painted_table_width(table: &str) -> usize {
    table
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
}

/// Run one window size end to end: render the workbook through core at the
/// width this window asked for, paint it, and report what came out.
fn table_in_a_window(
    previewer: &sekio_core::Previewer,
    path: &Path,
    size: [f32; 2],
) -> (usize, usize, String) {
    let mut ui = AppUi::sized(Some(path.to_path_buf()), Mode::App, size);
    ui.run();
    // Something text-shaped has to be on screen before a resize means
    // anything — the app does not re-lay-out an image or a home screen.
    ui.deliver(FIRST, path, text_content());

    // The window is not the width core assumed when `main` fired the first
    // request with no hint, so once it holds still the app re-requests.
    std::thread::sleep(Duration::from_millis(200));
    ui.run();

    let request = ui
        .requests
        .try_iter()
        .filter(|request| request.kind == Kind::Preview)
        .last()
        .unwrap_or_else(|| panic!("a {size:?} window must re-request its preview"));
    let asked = request
        .text_width
        .expect("a preview request must carry the width the pane measured");

    // Core's real spreadsheet renderer, at exactly that width.
    let opts = sekio_core::PreviewOptions {
        text_width: Some(asked),
        ..Default::default()
    };
    let preview = previewer
        .preview(path, &opts, &sekio_core::CancelToken::new())
        .expect("the workbook must render");
    ui.deliver(request.id, path, preview.content);

    let table = painted_table(&ui);
    (asked, painted_table_width(&table), table)
}

/// The bug, end to end: a spreadsheet in a wide window has to use the window.
///
/// Both halves are real — the width comes from the running UI's own
/// measurement of its text area, and the table comes from core's spreadsheet
/// renderer laying out for it.
#[test]
fn a_wide_window_lays_a_spreadsheet_out_wider_than_a_narrow_one() {
    let dir = std::env::temp_dir().join(format!("sekio-gui-width-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join("bang-cong.xlsx");
    std::fs::write(&path, xlsx()).expect("write the fixture workbook");

    let previewer = sekio_core::Previewer::new();
    let (narrow_ask, narrow_table, narrow_text) =
        table_in_a_window(&previewer, &path, [500.0, 620.0]);
    let (wide_ask, wide_table, wide_text) = table_in_a_window(&previewer, &path, [1500.0, 620.0]);

    assert!(
        wide_ask > narrow_ask * 2,
        "a 1500px window measured {wide_ask} characters and a 500px one \
         {narrow_ask}: the pane width is not reaching the request"
    );
    assert!(
        wide_table > narrow_table,
        "the table came out {wide_table} characters wide in the big window and \
         {narrow_table} in the small one — the width never reached the layout"
    );
    // The whole complaint: in the wide window nothing is elided at all…
    assert!(
        !wide_text.contains('…'),
        "a table that needs ~90 characters was still elided in a window with \
         room for {wide_ask}:\n{wide_text}"
    );
    assert!(
        wide_text.contains("Đứng lớp hướng dẫn thực hành"),
        "the longest cell should be whole in a wide window:\n{wide_text}"
    );
    // …while the small window, which genuinely cannot fit it, still says so.
    assert!(
        narrow_text.contains('…'),
        "a 500px window cannot fit a 90-character table, so something must be \
         elided:\n{narrow_text}"
    );
    // And the numbers survive at both sizes: they are the cheapest thing to
    // elide and the most expensive thing to lose.
    for text in [&narrow_text, &wide_text] {
        assert!(text.contains("47.3"), "a value lost a digit:\n{text}");
    }

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
