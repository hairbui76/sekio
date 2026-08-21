//! The eframe application: keyboard handling, per-variant painting, and the
//! UI half of the worker protocol. Nothing here blocks — every frame polls the
//! worker with `try_recv` and paints whatever state it has.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::{RichText, TextureHandle, TextureOptions, Vec2, ViewportCommand};
use sekio_core::{ListEntry, MetaField, PreviewContent};

use crate::state::{human_size, RequestTracker, Siblings};
use crate::style::{self, MONO_SIZE};
use crate::timing::Timing;
use crate::worker::{Loaded, Outcome, Request, Worker};

const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 20.0;

/// What the central panel is currently showing.
enum View {
    /// Daemon mode before the first handoff, and after a popup is dismissed:
    /// no path, no preview, and (deliberately) no window on screen.
    Idle,
    Loading,
    Ready(Box<Shown>),
    Failed(String),
}

/// A preview that is on screen, together with everything derived from it that
/// we refuse to recompute per frame.
struct Shown {
    loaded: Box<Loaded>,
    /// Built once per preview, on its first frame (see `style::text_job`).
    text_job: Option<egui::text::LayoutJob>,
    /// Uploaded to the GPU exactly once per preview.
    texture: Option<TextureHandle>,
    elapsed: Duration,
}

/// Everything the app needs at startup. A struct rather than eight positional
/// arguments, and the one place where "one-shot window" and "resident daemon"
/// differ: the daemon starts with no `path` and a `Receiver` of paths.
pub struct Startup {
    pub worker: Worker,
    pub tracker: RequestTracker,
    /// The path to show immediately, or `None` for a daemon waiting for its
    /// first handoff (its window stays hidden until one arrives).
    pub path: Option<PathBuf>,
    pub wrap: bool,
    pub borderless: bool,
    pub timing: Timing,
    /// Paths arriving from the daemon socket thread; `None` in one-shot mode.
    pub incoming: Option<Receiver<PathBuf>>,
}

pub struct SekioApp {
    worker: Worker,
    tracker: RequestTracker,
    siblings: Siblings,
    path: PathBuf,
    view: View,
    /// Image zoom; 1.0 means "scaled to fit".
    zoom: f32,
    timing: Timing,
    first_paint_logged: bool,
    /// The window is fitted to the content once per previewed path.
    sized: bool,
    borderless: bool,
    wrap: bool,
    /// Daemon mode when `Some`: paths handed over by the socket thread, and
    /// the reason dismissing the popup hides the window instead of exiting.
    incoming: Option<Receiver<PathBuf>>,
    visible: bool,
}

impl SekioApp {
    /// `tracker` already carries the first in-flight request (when there is a
    /// path): `main` fires it before the window exists so the preview renders
    /// while the GL context is still being created. By the time this runs the
    /// result may already be waiting in the channel.
    pub fn new(ctx: &egui::Context, startup: Startup) -> Self {
        ctx.set_visuals(egui::Visuals::dark());
        // We drive zoom ourselves (image scale for pictures, UI scale for
        // everything else), so egui's built-in Ctrl+± must not also fire.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);

        let Startup {
            worker,
            tracker,
            path,
            wrap,
            borderless,
            timing,
            incoming,
        } = startup;
        let visible = path.is_some();
        let path = path.unwrap_or_default();
        let siblings = if visible {
            Siblings::scan(&path, wrap)
        } else {
            Siblings::default()
        };
        Self {
            worker,
            tracker,
            siblings,
            path,
            view: if visible { View::Loading } else { View::Idle },
            zoom: 1.0,
            timing,
            first_paint_logged: false,
            sized: false,
            borderless,
            wrap,
            incoming,
            visible,
        }
    }

    /// Show a path that arrived over the daemon socket.
    ///
    /// It goes through `request_current`, i.e. `RequestTracker::begin`, like
    /// every other navigation: whatever preview is in flight is cancelled and
    /// its result discarded when it lands, so a handoff during a slow render
    /// cannot paint the previous file.
    fn show(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.path = path;
        self.siblings = Siblings::scan(&self.path, self.wrap);
        self.view = View::Loading;
        self.zoom = 1.0;
        // Each popup is a new file, so the window refits to it.
        self.sized = false;
        self.request_current();

        ctx.send_viewport_cmd(ViewportCommand::Title(format!(
            "sekio — {}",
            file_name(&self.path)
        )));
        if !self.visible {
            self.visible = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        }
        // Always re-raise: the window may be visible but behind the file
        // manager that triggered the popup.
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.timing.log("handoff shown");
    }

    /// Dismiss the popup. A one-shot process exits; the daemon only hides,
    /// dropping the preview (and its GPU texture) so a resident process does
    /// not sit on the last hexdump it was shown.
    fn dismiss(&mut self, ctx: &egui::Context) {
        self.tracker.cancel_all();
        if self.incoming.is_none() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }
        self.view = View::Idle;
        self.path = PathBuf::new();
        self.siblings = Siblings::default();
        self.zoom = 1.0;
        self.visible = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    /// The window manager's close button must not kill a resident daemon: for
    /// it, "close" means the same as Esc — hide, stay warm. eframe closes the
    /// root viewport unless `CancelClose` is sent during this very frame,
    /// which is why this runs in `logic`.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if self.incoming.is_none() || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        self.dismiss(ctx);
    }

    /// Drain the socket thread's channel. Only the newest path is shown: a
    /// burst of handoffs (a user leaning on the spacebar) costs one preview,
    /// not one per message.
    fn poll_incoming(&mut self, ctx: &egui::Context) {
        let Some(incoming) = &self.incoming else {
            return;
        };
        let mut latest = None;
        while let Ok(path) = incoming.try_recv() {
            latest = Some(path);
        }
        if let Some(path) = latest {
            self.show(ctx, path);
        }
    }

    /// Cancel whatever is in flight and ask the worker for `self.path`.
    fn request_current(&mut self) {
        let (id, cancel) = self.tracker.begin();
        self.worker.request(Request {
            id,
            path: self.path.clone(),
            cancel,
        });
    }

    /// Move `delta` files within the directory and re-preview immediately.
    fn navigate(&mut self, delta: isize) {
        if self.siblings.is_empty() {
            return;
        }
        if let Some(next) = self.siblings.step(delta) {
            self.path = next;
            self.view = View::Loading;
            self.zoom = 1.0;
            // `request_current` cancels the in-flight token first, so a huge
            // file being decoded is abandoned rather than finished.
            self.request_current();
        }
    }

    fn poll_worker(&mut self, ctx: &egui::Context) {
        while let Some(response) = self.worker.poll() {
            // Generation check: anything but the newest request is stale.
            if !self.tracker.accept(response.id) {
                continue;
            }
            match response.outcome {
                Outcome::Ready(loaded) => {
                    // One upload per preview; the handle lives until the next
                    // preview replaces it (dropping it frees the GPU texture).
                    let texture = loaded.image.as_ref().map(|img| {
                        ctx.load_texture("sekio-preview", img.clone(), TextureOptions::LINEAR)
                    });
                    if !self.sized {
                        self.sized = true;
                        ctx.send_viewport_cmd(ViewportCommand::InnerSize(desired_size(
                            &loaded.preview.content,
                        )));
                    }
                    self.view = View::Ready(Box::new(Shown {
                        loaded,
                        text_job: None,
                        texture,
                        elapsed: response.elapsed,
                    }));
                }
                Outcome::Failed(message) => self.view = View::Failed(message),
                // Cancelled results are normal control flow: a newer request
                // is already on its way, so keep showing "loading…".
                Outcome::Cancelled => self.view = View::Loading,
            }
            ctx.send_viewport_cmd(ViewportCommand::Title(format!(
                "sekio — {}",
                file_name(&response.path)
            )));
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let keys = ctx.input(|i| Keys {
            close: i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Space),
            prev: i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowUp),
            next: i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowDown),
            zoom_in: i.modifiers.command
                && (i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals)),
            zoom_out: i.modifiers.command && i.key_pressed(egui::Key::Minus),
            zoom_reset: i.modifiers.command && i.key_pressed(egui::Key::Num0),
        });

        if keys.close {
            self.dismiss(ctx);
            return;
        }
        if keys.prev {
            self.navigate(-1);
        } else if keys.next {
            self.navigate(1);
        }

        if keys.zoom_in {
            self.apply_zoom(ctx, ZOOM_STEP);
        }
        if keys.zoom_out {
            self.apply_zoom(ctx, 1.0 / ZOOM_STEP);
        }
        if keys.zoom_reset {
            self.zoom = 1.0;
            ctx.set_zoom_factor(1.0);
        }
    }

    /// Images zoom themselves; text/hex/listing zoom the whole UI, which is
    /// the closest thing to "make the font bigger" egui offers for free.
    fn apply_zoom(&mut self, ctx: &egui::Context, factor: f32) {
        if self.has_image() {
            self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        } else {
            ctx.set_zoom_factor((ctx.zoom_factor() * factor).clamp(0.5, 4.0));
        }
    }

    fn has_image(&self) -> bool {
        matches!(&self.view, View::Ready(shown) if shown.texture.is_some())
    }

    /// Scroll wheel over an image zooms it (everything else scrolls normally).
    /// The scroll delta is *consumed* so the surrounding `ScrollArea` does not
    /// pan at the same time.
    fn handle_wheel_zoom(&mut self, ctx: &egui::Context) {
        if !self.has_image() {
            return;
        }
        let scroll = ctx.input_mut(|i| {
            let delta = i.smooth_scroll_delta.y;
            i.smooth_scroll_delta = Vec2::ZERO;
            delta
        });
        if scroll != 0.0 {
            self.zoom = (self.zoom * (1.0 + scroll * 0.002)).clamp(ZOOM_MIN, ZOOM_MAX);
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        let response = egui::Panel::top("sekio-header")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // The daemon has no path between popups.
                    let title = if self.path.as_os_str().is_empty() {
                        "sekio".to_owned()
                    } else {
                        file_name(&self.path)
                    };
                    ui.label(RichText::new(title).strong());
                    if let (Some(pos), true) = (self.siblings.position(), self.siblings.len() > 1) {
                        ui.label(
                            RichText::new(format!("{pos} / {}", self.siblings.len()))
                                .color(style::DIM),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.tracker.is_pending() {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                        if let View::Ready(shown) = &self.view {
                            if shown.loaded.preview.truncated {
                                ui.label(RichText::new("truncated").color(style::DIM));
                            }
                        }
                    });
                });
            })
            .response;

        // A borderless popup still has to be movable: dragging the header moves
        // the window, the way a title bar would.
        if self.borderless && response.interact(egui::Sense::drag()).drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
    }

    fn footer(&self, ui: &mut egui::Ui) {
        let View::Ready(shown) = &self.view else {
            return;
        };
        egui::Panel::bottom("sekio-footer").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match &shown.loaded.preview.content {
                    PreviewContent::Text { lines, language } => {
                        dim_label(ui, format!("{language} · {} lines", lines.len()));
                    }
                    PreviewContent::Image {
                        original_width,
                        original_height,
                        format,
                        fields,
                        ..
                    } => {
                        dim_label(ui, format!("{format} · {original_width}×{original_height}"));
                        if self.zoom != 1.0 {
                            dim_label(ui, format!("{:.0}%", self.zoom * 100.0));
                        }
                        for field in fields {
                            dim_label(ui, format!("{}: {}", field.key, field.value));
                        }
                    }
                    PreviewContent::Listing { entries } => {
                        dim_label(ui, format!("{} entries", entries.len()));
                    }
                    PreviewContent::Metadata { fields, .. } => {
                        dim_label(ui, format!("{} fields", fields.len()));
                    }
                    PreviewContent::HexDump {
                        file_size, mime, ..
                    } => {
                        dim_label(
                            ui,
                            format!(
                                "{} · {}",
                                mime.as_deref().unwrap_or("binary"),
                                human_size(*file_size)
                            ),
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    dim_label(ui, format!("{} ms", shown.elapsed.as_millis()));
                });
            });
        });
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        let zoom = self.zoom;
        let view = &mut self.view;
        egui::CentralPanel::default().show(ui, |ui| match view {
            // Only reachable in daemon mode, and only on a frame where the
            // hide has not been applied yet.
            View::Idle => {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("sekio").color(style::DIM));
                });
            }
            View::Loading => {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("loading…").color(style::DIM));
                });
            }
            // A failed preview is a message in the window, never a crash; the
            // header still names the file it belongs to.
            View::Failed(message) => {
                let color = ui.visuals().error_fg_color;
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new(format!("cannot preview: {message}")).color(color));
                });
            }
            View::Ready(shown) => paint_content(ui, shown, zoom),
        });
    }
}

struct Keys {
    close: bool,
    prev: bool,
    next: bool,
    zoom_in: bool,
    zoom_out: bool,
    zoom_reset: bool,
}

impl eframe::App for SekioApp {
    /// Runs before every repaint (and when the window is hidden): all the
    /// non-painting work — polling the worker and reacting to keys — lives
    /// here so it happens even on frames we skip drawing.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // First, so a path that arrived while the window was hidden is picked
        // up on the very repaint the socket thread asked for.
        self.poll_incoming(ctx);
        self.poll_worker(ctx);
        self.handle_close_request(ctx);
        self.handle_keys(ctx);
        self.handle_wheel_zoom(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.header(ui);
        self.footer(ui);
        self.body(ui);

        if !self.first_paint_logged {
            self.first_paint_logged = true;
            self.timing.log("first paint");
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Don't let a long-running render keep the process alive after the
        // window closes.
        self.tracker.cancel_all();
    }
}

fn dim_label(ui: &mut egui::Ui, text: String) {
    ui.label(RichText::new(text).color(style::DIM).size(11.0));
}

fn paint_content(ui: &mut egui::Ui, shown: &mut Shown, zoom: f32) {
    match &shown.loaded.preview.content {
        PreviewContent::Text { lines, .. } => {
            let job = shown.text_job.get_or_insert_with(|| {
                style::text_job(lines, ui.visuals().text_color(), MONO_SIZE)
            });
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(style::NO_WRAP);
                    // The job is pre-built; egui memoizes the galley by its
                    // hash, so this does not re-layout every frame.
                    ui.label(job.clone());
                });
        }
        PreviewContent::Image { .. } => {
            if let Some(texture) = &shown.texture {
                paint_image(ui, texture, zoom);
            }
        }
        PreviewContent::Listing { entries } => paint_listing(ui, entries),
        PreviewContent::Metadata { fields, .. } => {
            let texture = shown.texture.clone();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(texture) = &texture {
                        ui.vertical_centered(|ui| {
                            let size = fit(texture.size_vec2(), Vec2::new(240.0, 240.0), 1.0);
                            ui.image((texture.id(), size));
                        });
                        ui.add_space(8.0);
                    }
                    paint_fields(ui, fields);
                });
        }
        PreviewContent::HexDump { data, .. } => paint_hex(ui, data),
    }
}

fn paint_image(ui: &mut egui::Ui, texture: &TextureHandle, zoom: f32) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let size = fit(texture.size_vec2(), ui.available_size(), zoom);
            ui.vertical_centered(|ui| {
                ui.add_space(((ui.available_height() - size.y) / 2.0).max(0.0));
                ui.image((texture.id(), size));
            });
        });
}

/// Scale `image` down to fit `avail` (never up, unless the user zoomed in).
fn fit(image: Vec2, avail: Vec2, zoom: f32) -> Vec2 {
    if image.x <= 0.0 || image.y <= 0.0 {
        return image;
    }
    let scale = (avail.x / image.x).min(avail.y / image.y).clamp(0.01, 1.0);
    image * scale * zoom
}

fn paint_listing(ui: &mut egui::Ui, entries: &[ListEntry]) {
    // `show_rows` assumes every row is exactly this tall, so measure the font
    // we actually paint with rather than the theme's monospace text style.
    let row_height = mono_row_height(ui);
    let dir_color = ui.visuals().hyperlink_color;
    let text_color = ui.visuals().text_color();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, entries.len(), |ui, range| {
            ui.style_mut().wrap_mode = Some(style::NO_WRAP);
            for entry in &entries[range] {
                let size = entry.size.map(human_size).unwrap_or_default();
                let name = if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                };
                ui.label(style::mono_job(
                    &[
                        (&format!("{size:>10}  "), style::DIM),
                        (&name, if entry.is_dir { dir_color } else { text_color }),
                    ],
                    MONO_SIZE,
                ));
            }
        });
}

/// Height of one line in the monospace font used by the row-based views.
fn mono_row_height(ui: &egui::Ui) -> f32 {
    ui.ctx()
        .fonts_mut(|f| f.row_height(&egui::FontId::monospace(MONO_SIZE)))
}

fn paint_fields(ui: &mut egui::Ui, fields: &[MetaField]) {
    egui::Grid::new("sekio-fields")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for field in fields {
                ui.label(RichText::new(&field.key).color(style::DIM).monospace());
                ui.label(RichText::new(&field.value).monospace());
                ui.end_row();
            }
        });
}

/// Offset / hex / ASCII columns, mirroring the CLI's hexdump layout.
fn paint_hex(ui: &mut egui::Ui, data: &[u8]) {
    // `show_rows` assumes every row is exactly this tall, so measure the font
    // we actually paint with rather than the theme's monospace text style.
    let row_height = mono_row_height(ui);
    let rows = data.len().div_ceil(16);
    let text_color = ui.visuals().text_color();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            ui.style_mut().wrap_mode = Some(style::NO_WRAP);
            for row in range {
                let start = row * 16;
                let chunk = &data[start..(start + 16).min(data.len())];
                ui.label(style::mono_job(
                    &[
                        (&format!("{start:08x}  "), style::DIM),
                        (&hex_columns(chunk), text_color),
                        (&format!(" |{}|", ascii_columns(chunk)), style::DIM),
                    ],
                    MONO_SIZE,
                ));
            }
        });
}

fn hex_columns(chunk: &[u8]) -> String {
    let mut out = String::with_capacity(16 * 3 + 1);
    for j in 0..16 {
        match chunk.get(j) {
            Some(b) => out.push_str(&format!("{b:02x} ")),
            None => out.push_str("   "),
        }
        if j == 7 {
            out.push(' ');
        }
    }
    out
}

fn ascii_columns(chunk: &[u8]) -> String {
    chunk
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// A window size that suits the content, applied once when the first preview
/// lands (we cannot know it before the preview exists).
fn desired_size(content: &PreviewContent) -> Vec2 {
    const CHAR_W: f32 = 7.5;
    const LINE_H: f32 = 17.0;
    const CHROME: f32 = 72.0;
    let clamp = |w: f32, h: f32| Vec2::new(w.clamp(420.0, 1400.0), h.clamp(300.0, 900.0));

    match content {
        PreviewContent::Text { lines, .. } => {
            let cols = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.text.chars().count())
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(80)
                .clamp(40, 120) as f32;
            clamp(cols * CHAR_W + 40.0, lines.len() as f32 * LINE_H + CHROME)
        }
        PreviewContent::Image {
            image,
            original_width,
            original_height,
            ..
        } => {
            let (w, h) = if *original_width > 0 && *original_height > 0 {
                (*original_width as f32, *original_height as f32)
            } else {
                (image.width() as f32, image.height() as f32)
            };
            let scale = (1200.0 / w).min(800.0 / h).min(1.0);
            clamp(w * scale + 32.0, h * scale + CHROME)
        }
        PreviewContent::Listing { entries } => {
            let cols = entries
                .iter()
                .map(|e| e.name.chars().count() + 13)
                .max()
                .unwrap_or(50)
                .clamp(40, 100) as f32;
            clamp(cols * CHAR_W + 40.0, entries.len() as f32 * LINE_H + CHROME)
        }
        PreviewContent::Metadata { fields, .. } => {
            clamp(560.0, fields.len() as f32 * 22.0 + CHROME + 260.0)
        }
        PreviewContent::HexDump { data, .. } => clamp(
            78.0 * CHAR_W + 40.0,
            data.len().div_ceil(16) as f32 * LINE_H + CHROME,
        ),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_row_matches_the_cli_layout() {
        let chunk: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            hex_columns(&chunk),
            "00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f "
        );
        assert_eq!(ascii_columns(b"ab\x00~"), "ab.~");
    }

    #[test]
    fn short_hex_row_is_padded_to_full_width() {
        let full = hex_columns(&[0u8; 16]).len();
        assert_eq!(hex_columns(&[1, 2, 3]).len(), full);
    }

    #[test]
    fn fit_scales_down_but_not_up() {
        let big = fit(Vec2::new(2000.0, 1000.0), Vec2::new(500.0, 500.0), 1.0);
        assert!((big.x - 500.0).abs() < 0.1 && (big.y - 250.0).abs() < 0.1);
        let small = fit(Vec2::new(100.0, 50.0), Vec2::new(500.0, 500.0), 1.0);
        assert_eq!(small, Vec2::new(100.0, 50.0));
        let zoomed = fit(Vec2::new(100.0, 50.0), Vec2::new(500.0, 500.0), 2.0);
        assert_eq!(zoomed, Vec2::new(200.0, 100.0));
    }

    #[test]
    fn desired_size_stays_within_sane_bounds() {
        let content = PreviewContent::HexDump {
            data: vec![0; 4096],
            file_size: 4096,
            mime: None,
        };
        let size = desired_size(&content);
        assert!(size.x >= 420.0 && size.x <= 1400.0);
        assert!(size.y >= 300.0 && size.y <= 900.0);
    }
}
