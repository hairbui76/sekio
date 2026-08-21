//! Painting the IR. Nothing here knows how a preview was produced — it only
//! maps `PreviewContent` onto ratatui widgets, exactly like `sekio-cli` maps it
//! onto ANSI.

use image::{DynamicImage, RgbaImage};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use sekio_core::{MetaField, Preview, PreviewContent};

use crate::app::{App, PreviewState};
use crate::config::Theme;

/// Terminal-side state that outlives a frame: the list's scroll offset, the
/// encoded image protocol, and the palette from the config file.
pub struct Ui {
    list: ListState,
    images: ImageCache,
    /// Chrome colors. `Theme::default()` is the palette the TUI shipped with,
    /// so an absent `[theme]` table paints exactly as before.
    pub theme: Theme,
    /// Advances while a preview is pending, to animate the spinner.
    pub tick: usize,
}

impl Ui {
    pub fn new(picker: Picker, theme: Theme) -> Self {
        Self {
            list: ListState::default(),
            images: ImageCache {
                picker,
                seq: 0,
                protocol: None,
            },
            theme,
            tick: 0,
        }
    }
}

/// Encoding an image into sixel/kitty/iTerm2 data is expensive, so keep the
/// protocol around and only rebuild it when a *different* preview arrives —
/// `preview_seq` changes exactly then.
struct ImageCache {
    picker: Picker,
    seq: u64,
    protocol: Option<StatefulProtocol>,
}

impl ImageCache {
    fn protocol(&mut self, seq: u64, image: &RgbaImage) -> &mut StatefulProtocol {
        let Self {
            picker,
            seq: cached,
            protocol,
        } = self;
        if *cached != seq {
            *cached = seq;
            *protocol = None;
        }
        protocol.get_or_insert_with(|| {
            picker.new_resize_protocol(DynamicImage::ImageRgba8(image.clone()))
        })
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, ui: &mut Ui) {
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Min(20)]).areas(body);

    draw_list(frame, app, ui, left);
    draw_preview(frame, app, ui, right);
    draw_status(frame, app, ui, status);
}

fn draw_list(frame: &mut Frame, app: &mut App, ui: &mut Ui, area: Rect) {
    let theme = ui.theme;
    let title = format!(" {} ", display_dir(app));
    let block = Block::bordered()
        .border_style(Style::new().fg(theme.border))
        .title(title.fg(theme.dim));

    if let Some(err) = &app.listing_error {
        let text = Paragraph::new(vec![
            Line::from("cannot read directory".fg(theme.error)),
            Line::from(err.as_str().fg(theme.dim)),
        ])
        .wrap(Wrap { trim: false })
        .block(block);
        frame.render_widget(text, area);
        return;
    }

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| {
            let (name, style) = if entry.is_dir {
                (format!("{}/", entry.name), Style::new().fg(theme.directory))
            } else {
                (entry.name.clone(), Style::new())
            };
            ListItem::new(Line::from(Span::styled(name, style)))
        })
        .collect();

    // REVERSED swaps fg and bg, so `accent` ends up as the highlight's
    // background. Setting *any* fg here overrides the item's own color, which
    // would flatten the directory tint on the selected row — so the unset case
    // (`Color::Reset`) leaves the style alone, exactly as before themes existed.
    let mut highlight = Style::new().add_modifier(Modifier::REVERSED);
    if theme.accent != Color::Reset {
        highlight = highlight.fg(theme.accent);
    }
    let list = List::new(items)
        .block(block)
        .highlight_symbol("› ")
        .highlight_style(highlight);

    ui.list.select(if app.entries.is_empty() {
        None
    } else {
        Some(app.cursor)
    });
    frame.render_stateful_widget(list, area, &mut ui.list);
}

fn draw_preview(frame: &mut Frame, app: &mut App, ui: &mut Ui, area: Rect) {
    let theme = ui.theme;
    let block = Block::bordered()
        .border_style(Style::new().fg(theme.border))
        .title(format!(" {} ", preview_title(app)).fg(theme.dim));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.set_viewport(inner.height as usize);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match &app.preview {
        PreviewState::Empty => {
            frame.render_widget(placeholder("nothing to preview", theme.dim), inner);
        }
        PreviewState::Loading => {
            frame.render_widget(placeholder(&loading_text(ui.tick), theme.dim), inner);
        }
        PreviewState::Failed(msg) => {
            let text = Paragraph::new(vec![
                Line::from("preview failed".fg(theme.error)),
                Line::from(""),
                Line::from(msg.as_str()),
            ])
            .wrap(Wrap { trim: false });
            frame.render_widget(text, inner);
        }
        PreviewState::Ready(preview) => match &preview.content {
            PreviewContent::Image {
                image,
                original_width,
                original_height,
                format,
                fields,
            } => {
                let footer = 1 + fields.len() as u16;
                let [pic, facts] =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(footer)]).areas(inner);
                draw_image(frame, ui, app.preview_seq, image, pic);
                let mut lines = vec![Line::from(format!(
                    "{format} · {original_width}×{original_height}"
                ))];
                lines.extend(field_lines(fields, &theme));
                frame.render_widget(Paragraph::new(lines), facts);
            }
            PreviewContent::Metadata { fields, thumbnail } => {
                let rows = match thumbnail {
                    // Cap the thumbnail so the facts always stay visible.
                    Some(_) => inner.height.saturating_sub(fields.len() as u16).min(12),
                    None => 0,
                };
                let [thumb, facts] =
                    Layout::vertical([Constraint::Length(rows), Constraint::Min(0)]).areas(inner);
                if let (Some(image), true) = (thumbnail.as_ref(), rows > 0) {
                    draw_image(frame, ui, app.preview_seq, image, thumb);
                }
                let lines = visible_lines(preview, app.scroll, facts.height as usize, &theme);
                frame.render_widget(Paragraph::new(lines), facts);
            }
            _ => {
                let lines = visible_lines(preview, app.scroll, inner.height as usize, &theme);
                frame.render_widget(Paragraph::new(lines), inner);
            }
        },
    }
}

fn draw_image(frame: &mut Frame, ui: &mut Ui, seq: u64, image: &RgbaImage, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let protocol = ui.images.protocol(seq, image);
    // `StatefulImage` re-encodes at render time when the area changes. The
    // image is already downscaled by core to `image_max_dim`, so this is cheap
    // enough to sit on the UI thread; the *decode* is what ran on the worker.
    frame.render_stateful_widget(
        StatefulImage::default().resize(Resize::Fit(None)),
        area,
        protocol,
    );
}

fn draw_status(frame: &mut Frame, app: &App, ui: &Ui, area: Rect) {
    let mut spans = vec![Span::styled(
        " q quit  j/k move  ⏎ open  h/⌫ up  ^d/^u ⇞/⇟ scroll  g/G top/bottom  r reload",
        Style::new().fg(ui.theme.dim),
    )];
    if app.is_loading() {
        spans.push(Span::styled(
            format!("  {}", loading_text(ui.tick)),
            Style::new().fg(ui.theme.warning),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn placeholder(text: &str, dim: Color) -> Paragraph<'_> {
    Paragraph::new(Line::from(text.fg(dim))).alignment(Alignment::Center)
}

fn loading_text(tick: usize) -> String {
    const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    format!("{} loading…", FRAMES[tick % FRAMES.len()])
}

/// Pane title: the entry name plus whatever core knows about it.
fn preview_title(app: &App) -> String {
    let name = app
        .selected()
        .map(|e| e.name.as_str())
        .unwrap_or("—")
        .to_owned();
    match &app.preview {
        PreviewState::Ready(preview) => {
            let mut title = format!("{name} · {}", kind_label(preview));
            if preview.truncated {
                title.push_str(" · truncated");
            }
            title
        }
        PreviewState::Loading => format!("{name} · loading…"),
        PreviewState::Failed(_) => format!("{name} · error"),
        PreviewState::Empty => name,
    }
}

/// The `language` / `format` / mime label for the current variant.
fn kind_label(preview: &Preview) -> String {
    match &preview.content {
        PreviewContent::Text { language, .. } => language.clone(),
        PreviewContent::Image {
            format,
            original_width,
            original_height,
            ..
        } => format!("{format} {original_width}×{original_height}"),
        PreviewContent::Listing { entries } => format!("{} entries", entries.len()),
        PreviewContent::Metadata { fields, .. } => format!("{} fields", fields.len()),
        PreviewContent::HexDump {
            file_size, mime, ..
        } => format!(
            "{} · {}",
            mime.as_deref().unwrap_or("binary"),
            human_size(*file_size)
        ),
    }
}

fn display_dir(app: &App) -> String {
    let full = app.dir.display().to_string();
    if full.is_empty() {
        ".".to_owned()
    } else {
        full
    }
}

/// Build just the rows in `[scroll, scroll + height)`. Slicing here rather than
/// handing a whole hexdump to `Paragraph::scroll` keeps per-frame work bounded
/// by the pane size, not by the file size.
pub fn visible_lines(
    preview: &Preview,
    scroll: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let truncated_marker = Line::from("── truncated ──".fg(theme.warning));

    match &preview.content {
        PreviewContent::Text { lines, .. } => {
            let mut out: Vec<Line<'static>> = lines
                .iter()
                .skip(scroll)
                .take(height)
                .map(|line| {
                    Line::from(
                        line.spans
                            .iter()
                            .map(|span| Span::styled(span.text.clone(), span_style(span)))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            if preview.truncated && scroll + out.len() >= lines.len() && out.len() < height {
                out.push(truncated_marker);
            }
            out
        }
        PreviewContent::Listing { entries } => {
            let mut out: Vec<Line<'static>> = entries
                .iter()
                .skip(scroll)
                .take(height)
                .map(|entry| {
                    let size = entry
                        .size
                        .map(human_size)
                        .unwrap_or_else(|| String::from(""));
                    let name = if entry.is_dir {
                        Span::styled(format!("{}/", entry.name), Style::new().fg(theme.directory))
                    } else {
                        Span::raw(entry.name.clone())
                    };
                    Line::from(vec![
                        Span::styled(format!("{size:>10}  "), Style::new().fg(theme.dim)),
                        name,
                    ])
                })
                .collect();
            if preview.truncated && scroll + out.len() >= entries.len() && out.len() < height {
                out.push(Line::from("── more entries not shown ──".fg(theme.warning)));
            }
            out
        }
        PreviewContent::Metadata { fields, .. } => field_lines(fields, theme)
            .into_iter()
            .skip(scroll)
            .take(height)
            .collect(),
        PreviewContent::HexDump { data, .. } => {
            let rows = data.len().div_ceil(16);
            let mut out: Vec<Line<'static>> = (scroll..rows.min(scroll + height))
                .map(|row| hex_line(data, row, theme))
                .collect();
            if preview.truncated && scroll + out.len() >= rows && out.len() < height {
                out.push(truncated_marker);
            }
            out
        }
        PreviewContent::Image { .. } => Vec::new(),
    }
}

fn span_style(span: &sekio_core::Span) -> Style {
    let mut style = Style::new();
    if let Some((r, g, b)) = span.fg {
        style = style.fg(Color::Rgb(r, g, b));
    }
    if span.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if span.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

fn field_lines(fields: &[MetaField], theme: &Theme) -> Vec<Line<'static>> {
    let width = fields
        .iter()
        .map(|f| f.key.chars().count())
        .max()
        .unwrap_or(0);
    fields
        .iter()
        .map(|field| {
            Line::from(vec![
                Span::styled(
                    format!("{:>width$}  ", field.key, width = width),
                    Style::new().fg(theme.key),
                ),
                Span::raw(field.value.clone()),
            ])
        })
        .collect()
}

/// One 16-byte hexdump row: offset, hex columns, ASCII gutter.
fn hex_line(data: &[u8], row: usize, theme: &Theme) -> Line<'static> {
    let start = row * 16;
    let chunk = &data[start..(start + 16).min(data.len())];

    let mut hex = String::with_capacity(50);
    for i in 0..16 {
        match chunk.get(i) {
            Some(b) => hex.push_str(&format!("{b:02x} ")),
            None => hex.push_str("   "),
        }
        if i == 7 {
            hex.push(' ');
        }
    }

    let ascii: String = chunk
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            }
        })
        .collect();

    Line::from(vec![
        Span::styled(format!("{start:08x}  "), Style::new().fg(theme.dim)),
        Span::raw(hex),
        Span::styled(format!(" |{ascii}|"), Style::new().fg(theme.key)),
    ])
}

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
    use sekio_core::{ListEntry, StyledLine};

    fn theme() -> Theme {
        Theme::default()
    }

    fn text_preview(n: usize, truncated: bool) -> Preview {
        Preview {
            content: PreviewContent::Text {
                lines: (0..n)
                    .map(|i| StyledLine {
                        spans: vec![sekio_core::Span {
                            text: format!("{i}"),
                            fg: Some((1, 2, 3)),
                            bold: true,
                            italic: false,
                        }],
                    })
                    .collect(),
                language: "Rust".to_owned(),
            },
            truncated,
        }
    }

    #[test]
    fn visible_lines_window_respects_scroll_and_height() {
        let preview = text_preview(100, false);
        let lines = visible_lines(&preview, 10, 5, &theme());
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].spans[0].content.as_ref(), "10");
        assert_eq!(lines[4].spans[0].content.as_ref(), "14");
    }

    #[test]
    fn visible_lines_past_the_end_is_empty_not_a_panic() {
        let preview = text_preview(3, false);
        assert!(visible_lines(&preview, 99, 5, &theme()).is_empty());
        assert!(visible_lines(&preview, 0, 0, &theme()).is_empty());
    }

    #[test]
    fn truncation_marker_only_shows_at_the_end() {
        let preview = text_preview(4, true);
        let top = visible_lines(&preview, 0, 2, &theme());
        assert_eq!(top.len(), 2);
        let bottom = visible_lines(&preview, 0, 10, &theme());
        assert_eq!(bottom.len(), 5);
        assert!(bottom[4].spans[0].content.contains("truncated"));
    }

    #[test]
    fn span_styling_maps_rgb_and_modifiers() {
        let style = span_style(&sekio_core::Span {
            text: "x".to_owned(),
            fg: Some((10, 20, 30)),
            bold: true,
            italic: true,
        });
        assert_eq!(style.fg, Some(Color::Rgb(10, 20, 30)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn hex_rows_pad_the_last_partial_row() {
        let data: Vec<u8> = (0u8..20).collect();
        let last = hex_line(&data, 1, &theme());
        let text: String = last.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.starts_with("00000010"));
        // 4 bytes of data, 12 blank columns.
        assert!(text.contains("10 11 12 13"));
        assert!(text.ends_with("|....|"));
    }

    #[test]
    fn hexdump_window_is_bounded_by_pane_height() {
        let preview = Preview {
            content: PreviewContent::HexDump {
                data: vec![0u8; 1024 * 1024],
                file_size: 1024 * 1024,
                mime: Some("application/octet-stream".to_owned()),
            },
            truncated: false,
        };
        assert_eq!(visible_lines(&preview, 0, 30, &theme()).len(), 30);
    }

    #[test]
    fn listing_lines_mark_directories() {
        let preview = Preview {
            content: PreviewContent::Listing {
                entries: vec![
                    ListEntry {
                        name: "sub".to_owned(),
                        is_dir: true,
                        size: None,
                    },
                    ListEntry {
                        name: "f".to_owned(),
                        is_dir: false,
                        size: Some(2048),
                    },
                ],
            },
            truncated: false,
        };
        let lines = visible_lines(&preview, 0, 10, &theme());
        assert!(lines[0].spans[1].content.ends_with('/'));
        assert!(lines[1].spans[0].content.contains("2.0 KB"));
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 MB");
    }
}
