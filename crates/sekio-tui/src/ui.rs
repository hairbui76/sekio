//! Painting the IR. Nothing here knows how a preview was produced — it only
//! maps `PreviewContent` onto ratatui widgets, exactly like `sekio-cli` maps it
//! onto ANSI.

use image::{DynamicImage, RgbaImage};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use sekio_core::{CellKind, MetaField, Preview, PreviewContent, TableRow};

use crate::app::{App, PreviewState};
use crate::config::Theme;
use crate::table;

/// Terminal-side state that outlives a frame: the list's scroll offset, the
/// encoded image protocol, and the palette from the config file.
pub struct Ui {
    list: ListState,
    images: ImageCache,
    tables: TableCache,
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
            tables: TableCache::default(),
            theme,
            tick: 0,
        }
    }
}

/// What a table's columns need, measured once per preview.
///
/// Sizing a column means looking at every cell in it, and a 10 000-row sheet
/// has rather a lot of them — far too many to redo sixty times a second. The
/// measurement depends only on the *content*, so it is cached against
/// `preview_seq` and reused until another preview lands; the cheap part that
/// depends on the pane's width ([`table::plan`]) still runs every frame, which
/// is what makes a resize free.
///
/// Measuring only the rows on screen would be cheaper still and wrong: columns
/// would jump about as the reader scrolled.
#[derive(Default)]
struct TableCache {
    seq: u64,
    /// Natural column widths plus the row-number gutter's width.
    measured: Option<(Vec<usize>, usize)>,
}

impl TableCache {
    fn measure(&mut self, seq: u64, columns: &[String], rows: &[TableRow]) -> (&[usize], usize) {
        if self.seq != seq {
            self.seq = seq;
            self.measured = None;
        }
        let measured = self.measured.get_or_insert_with(|| {
            (
                table::natural_widths(columns, rows),
                table::gutter_width(rows),
            )
        });
        (&measured.0, measured.1)
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
    // Two uses, and they are not the same one. A table is laid out here, per
    // frame, against exactly this width — resizing costs nothing and needs no
    // new preview. Every other content type is still laid out by core for the
    // width it was asked for, so the event loop decides when a change is worth
    // re-requesting for; see `App::poll_reflow`.
    app.set_preview_width(inner.width as usize);

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
            PreviewContent::Table { .. } => {
                draw_table(
                    frame,
                    inner,
                    preview,
                    app.preview_seq,
                    app.scroll,
                    &mut ui.tables,
                    &theme,
                );
            }
            _ => {
                let lines = visible_lines(preview, app.scroll, inner.height as usize, &theme);
                frame.render_widget(Paragraph::new(lines), inner);
            }
        },
    }
}

/// Paint a spreadsheet as a real grid.
///
/// The pane is split into three: the sheet strip, the grid itself, and the note
/// saying what is not on screen. [`table::chrome_rows`] counts those same three
/// things for [`crate::app::content_len`], so the scroll bounds and the pane
/// agree on where the last row is.
///
/// Only the rows in view are turned into [`Row`]s — a 10 000-row sheet costs a
/// pane's worth of work per frame, not ten thousand.
fn draw_table(
    frame: &mut Frame,
    area: Rect,
    preview: &Preview,
    seq: u64,
    scroll: usize,
    cache: &mut TableCache,
    theme: &Theme,
) {
    let PreviewContent::Table {
        columns,
        rows,
        sheets,
        active_sheet,
        total_rows,
        total_cols,
    } = &preview.content
    else {
        return;
    };
    let (natural, gutter) = cache.measure(seq, columns, rows);
    // Content decides what a column needs; the pane decides what it gets.
    let widths = table::plan(natural, area.width as usize, gutter);
    let note = table::note(
        rows.len(),
        widths.len(),
        *total_rows,
        *total_cols,
        preview.truncated,
    );

    let [strip, grid, footer] = Layout::vertical([
        Constraint::Length(u16::from(!sheets.is_empty())),
        Constraint::Min(0),
        Constraint::Length(u16::from(note.is_some())),
    ])
    .areas(area);

    if !sheets.is_empty() {
        frame.render_widget(
            Paragraph::new(sheet_strip(sheets, *active_sheet, theme)),
            strip,
        );
    }
    if let Some(note) = &note {
        frame.render_widget(
            Paragraph::new(Line::from(note.as_str().fg(theme.warning))),
            footer,
        );
    }
    if grid.height == 0 || widths.is_empty() {
        return;
    }

    // Column letters and row numbers are labels, not data, so they take the
    // same colour metadata keys and the hexdump's gutter do — a shade back from
    // whatever the terminal paints ordinary text in.
    let labels = Style::new().fg(theme.key);
    let header = Row::new(
        std::iter::once(Cell::new("")).chain(widths.iter().enumerate().map(|(i, &width)| {
            let name = columns.get(i).map(String::as_str).unwrap_or("");
            Cell::new(Line::from(table::fit(name, width)))
        })),
    )
    .style(labels);

    // The `Table` widget spends the first row of its area on the header.
    let body = grid.height.saturating_sub(1) as usize;
    let visible: Vec<Row> =
        table::window(rows, scroll, body)
            .iter()
            .map(|row| {
                let number = Cell::new(Line::from(table::fit(&row.label, gutter)).right_aligned())
                    .style(labels);
                Row::new(std::iter::once(number).chain(
                    table::paint_row(row, &widths).into_iter().map(|painted| {
                        let line = Line::from(painted.text);
                        let line = if painted.right {
                            line.right_aligned()
                        } else {
                            line.left_aligned()
                        };
                        Cell::new(line).style(cell_style(painted.kind, theme))
                    }),
                ))
            })
            .collect();

    let constraints: Vec<Constraint> = std::iter::once(Constraint::Length(clamp_u16(gutter)))
        .chain(widths.iter().map(|&w| Constraint::Length(clamp_u16(w))))
        .collect();
    let widget = Table::new(visible, constraints)
        .header(header)
        .column_spacing(table::COL_GAP as u16);
    frame.render_widget(widget, grid);
}

/// `Sheets: [Data]  Notes  Q3` — the one being previewed bracketed and bold.
fn sheet_strip(sheets: &[String], active: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::styled("Sheets: ", Style::new().fg(theme.key))];
    for (i, name) in sheets.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if i == active {
            // Bold and bracketed, so the active sheet is still marked on a
            // terminal with no colour and under a theme that sets no accent.
            let mut style = Style::new().add_modifier(Modifier::BOLD);
            if theme.accent != Color::Reset {
                style = style.fg(theme.accent);
            }
            spans.push(Span::styled(format!("[{name}]"), style));
        } else {
            spans.push(Span::styled(name.clone(), Style::new().fg(theme.dim)));
        }
    }
    Line::from(spans)
}

/// A cell's colour comes from what it holds, out of the user's theme. Text
/// cells are left in the terminal's own foreground, exactly like the body of a
/// text preview.
fn cell_style(kind: CellKind, theme: &Theme) -> Style {
    match kind {
        CellKind::Text => Style::new(),
        CellKind::Number => Style::new().fg(theme.number),
        CellKind::Bool => Style::new().fg(theme.boolean),
        CellKind::Date => Style::new().fg(theme.date),
        CellKind::Error => Style::new().fg(theme.error),
    }
}

/// Widths are computed in `usize` and spent in `u16`. A pane wider than 65 535
/// columns does not exist, but saturating beats wrapping a column to nothing.
fn clamp_u16(width: usize) -> u16 {
    u16::try_from(width).unwrap_or(u16::MAX)
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
        PreviewContent::Table {
            columns,
            rows,
            sheets,
            active_sheet,
            total_rows,
            total_cols,
        } => {
            // Never claim a sheet is smaller than what is already on screen.
            let extent = format!(
                "{}×{}",
                (*total_rows).max(rows.len() as u64),
                (*total_cols).max(columns.len() as u64)
            );
            match sheets.get(*active_sheet) {
                Some(name) => format!("{name} · {extent}"),
                None => format!("table · {extent}"),
            }
        }
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
        // Painted by `draw_table` as a real `Table` widget rather than as
        // lines: a terminal cannot scroll sideways, so the columns have to be
        // laid out against the pane's width, which a `Line` knows nothing
        // about. Same reason `Image` produces none.
        PreviewContent::Table { .. } => Vec::new(),
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

    fn table_preview(sheets: &[&str], active: usize, truncated: bool) -> Preview {
        Preview {
            content: PreviewContent::Table {
                columns: vec!["A".to_owned(), "B".to_owned()],
                rows: vec![sekio_core::TableRow {
                    label: "1".to_owned(),
                    cells: vec![
                        sekio_core::TableCell {
                            text: "Nguyễn".to_owned(),
                            kind: CellKind::Text,
                        },
                        sekio_core::TableCell {
                            text: "42".to_owned(),
                            kind: CellKind::Number,
                        },
                    ],
                }],
                sheets: sheets.iter().map(|s| (*s).to_owned()).collect(),
                active_sheet: active,
                total_rows: 900,
                total_cols: 12,
            },
            truncated,
        }
    }

    /// A table is a widget, not lines — `draw_table` paints it, so the line
    /// path must produce nothing rather than a half-rendered grid.
    #[test]
    fn a_table_produces_no_lines_because_it_is_a_widget() {
        let preview = table_preview(&["Data"], 0, true);
        assert!(visible_lines(&preview, 0, 20, &theme()).is_empty());
    }

    #[test]
    fn a_tables_title_names_the_sheet_and_its_extent() {
        assert_eq!(
            kind_label(&table_preview(&["Data"], 0, false)),
            "Data · 900×12"
        );
        // An out-of-range active sheet must not index-panic.
        assert_eq!(
            kind_label(&table_preview(&["Data"], 7, false)),
            "table · 900×12"
        );
        assert_eq!(kind_label(&table_preview(&[], 0, false)), "table · 900×12");
    }

    /// The active sheet is marked by brackets *and* weight, so it is still
    /// marked when the theme sets no accent — which is the default.
    #[test]
    fn the_sheet_strip_marks_the_active_sheet_under_any_theme() {
        let line = sheet_strip(
            &["Data".to_owned(), "Notes".to_owned()],
            1,
            &Theme::default(),
        );
        let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.starts_with("Sheets: "), "{text}");
        assert!(text.contains("[Notes]"), "{text}");
        assert!(!text.contains("[Data]"), "{text}");
        let active = line
            .spans
            .iter()
            .find(|s| s.content.contains("[Notes]"))
            .expect("the active sheet must be painted");
        assert!(active.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(active.style.fg, None, "the default theme sets no accent");

        let themed = Theme {
            accent: Color::Rgb(9, 9, 9),
            ..Theme::default()
        };
        let line = sheet_strip(&["Data".to_owned()], 0, &themed);
        let active = &line.spans[1];
        assert_eq!(active.style.fg, Some(Color::Rgb(9, 9, 9)));
    }

    /// Cell colours come from the user's theme, not from constants baked in
    /// here — a `[theme]` table has to reach a spreadsheet too.
    #[test]
    fn cell_colours_follow_the_configured_theme() {
        let theme = Theme {
            number: Color::Rgb(1, 1, 1),
            boolean: Color::Rgb(2, 2, 2),
            date: Color::Rgb(3, 3, 3),
            error: Color::Rgb(4, 4, 4),
            ..Theme::default()
        };
        assert_eq!(
            cell_style(CellKind::Number, &theme).fg,
            Some(Color::Rgb(1, 1, 1))
        );
        assert_eq!(
            cell_style(CellKind::Bool, &theme).fg,
            Some(Color::Rgb(2, 2, 2))
        );
        assert_eq!(
            cell_style(CellKind::Date, &theme).fg,
            Some(Color::Rgb(3, 3, 3))
        );
        assert_eq!(
            cell_style(CellKind::Error, &theme).fg,
            Some(Color::Rgb(4, 4, 4))
        );
        assert_eq!(
            cell_style(CellKind::Text, &theme).fg,
            None,
            "text cells keep the terminal's own foreground"
        );
    }

    /// Render a table into an off-screen terminal and read the grid back as
    /// text. Not a substitute for the layout tests — it is here to catch the
    /// one thing they cannot see, that the widget is wired to the pane at all.
    fn painted(preview: &Preview, width: u16, height: u16, scroll: usize) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("an off-screen terminal");
        let mut cache = TableCache::default();
        terminal
            .draw(|frame| {
                draw_table(
                    frame,
                    frame.area(),
                    preview,
                    1,
                    scroll,
                    &mut cache,
                    &theme(),
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn a_table_paints_a_sheet_strip_headings_a_gutter_and_a_note() {
        let lines = painted(&table_preview(&["Data", "Notes"], 0, true), 30, 6, 0);
        assert!(lines[0].starts_with("Sheets: [Data]"), "{lines:?}");
        assert!(lines[0].contains("Notes"), "{lines:?}");
        // Column letters, indented past the row-number gutter.
        assert!(
            lines[1].contains('A') && lines[1].contains('B'),
            "{lines:?}"
        );
        // The row number in the gutter, then the cells.
        assert!(lines[2].starts_with('1'), "{lines:?}");
        assert!(lines[2].contains("Nguyễn"), "{lines:?}");
        assert!(lines[2].contains("42"), "{lines:?}");
        // The note is the last row of the pane.
        assert!(lines[5].contains("900 rows"), "{lines:?}");
    }

    /// Every painted row must stay inside the pane — the point of measuring in
    /// display cells rather than `char`s.
    #[test]
    fn a_narrow_pane_never_paints_past_its_edge() {
        for width in 4..40u16 {
            let lines = painted(&table_preview(&["Data"], 0, true), width, 6, 0);
            for line in &lines {
                assert!(
                    line.chars().count() <= width as usize,
                    "width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn scrolling_past_the_end_paints_an_empty_grid_not_a_panic() {
        let lines = painted(&table_preview(&["Data"], 0, false), 30, 6, 9_999);
        assert!(lines[0].starts_with("Sheets:"), "{lines:?}");
        // Headings stay put; there is simply nothing under them.
        assert!(lines[1].contains('A'), "{lines:?}");
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 MB");
    }
}
