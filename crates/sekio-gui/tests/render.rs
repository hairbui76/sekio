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
            .with_size(SIZE)
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
