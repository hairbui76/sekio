//! The spreadsheet grid: `PreviewContent::Table` painted as a table.
//!
//! Core used to flatten a sheet into space-aligned text and hand the frontend a
//! pre-truncated string, so the GUI had to tell it how many characters wide the
//! pane was and the answer was cells ending in `…` while most of a maximised
//! window sat empty. The IR now carries the grid itself — whole cells, a kind
//! per cell, the column letters, the sheet names and the sheet's real extent —
//! and every layout decision is made here instead.
//!
//! ## Why this is hand-painted rather than `egui::Grid`
//!
//! `egui::Grid` lays out real widgets in real rows, which rules out
//! [`egui::ScrollArea::show_rows`]: a 10 000-row sheet would allocate ten
//! thousand rows of widgets every frame just to clip all but thirty of them. It
//! also cannot freeze anything — a `Grid` scrolls as one lump, so the column
//! letters and the row numbers disappear the moment the user moves. `egui_extras`
//! has a `Table` that does both, but it is another dependency in a shipped
//! binary for a widget whose entire job here is "put text at an x and a y", and
//! it would still have to be told the column widths we measure below.
//!
//! So: one [`egui::ScrollArea`] for the scrolling and the virtualisation, and a
//! [`egui::Painter`] for the contents. That buys exact control over the three
//! things a grid needs and a stack of labels does not — a frozen header and
//! gutter, faint rules on the column boundaries, and per-cell alignment.
//!
//! ## What scrolls and what does not
//!
//! The whole grid lives in one `ScrollArea::both`, so a sheet too wide for the
//! window scrolls sideways instead of being squeezed into it. Inside that area
//! the column letters are painted at the top of the *viewport* rather than the
//! top of the content, and the row numbers at its left edge, each over an opaque
//! strip — so both stay put while the cells move under them. The content
//! reserves one row's height at the top for the header strip, so the first data
//! row is not born underneath it.
//!
//! ## Widths
//!
//! Measured once per preview ([`Grid::measure`]) from egui's own text metrics,
//! not from a characters-times-pixels guess, and capped per column
//! ([`COL_MAX`]) so one 4 000-character note cannot make the sheet 10 000 px
//! wide. A cell wider than its column is elided by egui with a `…`, which is
//! now this crate's decision rather than something baked into the IR.

use egui::{Color32, FontId, Pos2, Rect, RichText, Stroke, Ui, Vec2};
use sekio_core::{TableCell, TableRow};

use crate::style::{self, Palette, MONO_SIZE};

/// Blank space either side of a cell's text, inside its column.
const CELL_PAD: f32 = 6.0;

/// Widest a column's *text* may be, however long its longest cell is. Past this
/// the cell is elided; the whole value is still one horizontal scroll away in
/// the neighbouring columns, and the alternative is a sheet nobody can navigate.
const COL_MAX: f32 = 320.0;

/// Narrowest a column may be, so a sheet of empty columns still reads as a grid
/// and the column letter above it still fits.
const COL_MIN: f32 = 16.0;

/// Vertical space between two rows. Also the `ScrollArea`'s item spacing, so
/// the row pitch this module paints at and the one `show_rows` virtualises with
/// are the same number.
const ROW_GAP: f32 = 3.0;

/// Cells measured per column. The candidates are the longest few by character
/// count — a cheap proxy for "which cell is widest" — and only those are
/// actually laid out, so sizing a 500-row sheet costs a few hundred galleys
/// rather than sixteen thousand.
const SAMPLES: usize = 4;

/// Sheet names listed before the strip gives up and counts the rest. Mirrors
/// the cap the CLI's sheet line has always used.
const MAX_SHEETS: usize = 24;

/// A borrowed [`sekio_core::PreviewContent::Table`], so the painter takes one
/// argument instead of six.
pub struct Table<'a> {
    pub columns: &'a [String],
    pub rows: &'a [TableRow],
    pub sheets: &'a [String],
    pub active_sheet: usize,
    pub total_rows: u64,
    pub total_cols: u64,
}

impl Table<'_> {
    /// The line the footer shows: the sheet's size, and what of it is on
    /// screen when that is less than all of it.
    ///
    /// Deliberately the same wording the CLI's trailing summary uses, so the
    /// two frontends describe one sheet the same way.
    pub fn footer(&self, truncated: bool) -> String {
        let shown_rows = self.rows.len() as u64;
        let shown_cols = self.columns.len() as u64;
        // A malformed IR claiming fewer rows than it carries must not produce
        // "3 rows … showing 5".
        let total_rows = self.total_rows.max(shown_rows);
        let total_cols = self.total_cols.max(shown_cols);

        if total_rows > shown_rows || total_cols > shown_cols {
            format!(
                "{total_rows} rows × {total_cols} columns — showing {shown_rows} × {shown_cols}"
            )
        } else if truncated {
            // A cap bit, but the sheet never declared its size (an xlsx written
            // with no `<dimension>`), so "there is more" is all we honestly
            // know. Better a vague note than a confident wrong number.
            format!("showing first {shown_rows} rows × {shown_cols} columns — more follow")
        } else {
            format!("{shown_rows} rows × {shown_cols} columns")
        }
    }
}

/// Column geometry, measured once per preview and reused every frame.
pub struct Grid {
    /// Left edge of each column in content coordinates, where 0 is the left
    /// edge of the row-number gutter. `lefts[i]` therefore starts at `gutter`.
    lefts: Vec<f32>,
    widths: Vec<f32>,
    gutter: f32,
    total_width: f32,
    row_height: f32,
    /// Distance from one row's top to the next.
    pitch: f32,
}

impl Grid {
    /// Measure the column widths for a table, using the font the cells are
    /// actually painted in.
    pub fn measure(ctx: &egui::Context, table: &Table<'_>) -> Self {
        let row_height = ctx.fonts_mut(|f| f.row_height(&FontId::monospace(MONO_SIZE)));

        let mut widths = Vec::with_capacity(table.columns.len());
        for (index, label) in table.columns.iter().enumerate() {
            // The column letter has to fit too, or the header elides into "…".
            let mut text = width_of(ctx, label);
            for candidate in widest_cells(table.rows, index) {
                text = text.max(width_of(ctx, candidate));
            }
            widths.push(text.clamp(COL_MIN, COL_MAX) + 2.0 * CELL_PAD);
        }

        let gutter = table
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .max_by_key(|label| label.chars().count())
            .map_or(0.0, |label| width_of(ctx, label))
            .max(COL_MIN)
            + 2.0 * CELL_PAD;

        let mut lefts = Vec::with_capacity(widths.len());
        let mut x = gutter;
        for width in &widths {
            lefts.push(x);
            x += width;
        }

        Self {
            lefts,
            widths,
            gutter,
            total_width: x,
            row_height,
            pitch: row_height + ROW_GAP,
        }
    }

    /// Screen rectangle of one column's text area, given where content x = 0
    /// landed and the row's top edge.
    fn cell(&self, index: usize, origin_x: f32, top: f32) -> Option<Rect> {
        let (left, width) = (*self.lefts.get(index)?, *self.widths.get(index)?);
        Some(Rect::from_min_size(
            Pos2::new(origin_x + left + CELL_PAD, top),
            Vec2::new(width - 2.0 * CELL_PAD, self.row_height),
        ))
    }
}

/// Text width in the monospace face the cells are painted in.
fn width_of(ctx: &egui::Context, text: &str) -> f32 {
    ctx.fonts_mut(|f| {
        f.layout_no_wrap(
            style::one_line(text),
            FontId::monospace(MONO_SIZE),
            Color32::WHITE,
        )
        .size()
        .x
    })
}

/// The [`SAMPLES`] longest cells of one column, longest first.
///
/// Character count picks the candidates and egui measures them, which is the
/// cheap half of "measure with real text metrics": the count is only ever used
/// to choose *which* cells are worth laying out, never as a width.
fn widest_cells(rows: &[TableRow], column: usize) -> Vec<&str> {
    let mut best: Vec<(usize, &str)> = Vec::with_capacity(SAMPLES + 1);
    for row in rows {
        // A malformed row with fewer cells than there are columns is normal
        // input here, not a reason to index and panic.
        let Some(TableCell { text, .. }) = row.cells.get(column) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let len = text.chars().count();
        let at = best
            .iter()
            .position(|(other, _)| *other < len)
            .unwrap_or(best.len());
        if at < SAMPLES {
            best.insert(at, (len, text.as_str()));
            best.truncate(SAMPLES);
        }
    }
    best.into_iter().map(|(_, text)| text).collect()
}

/// Paint the sheet strip, then the grid, filling `ui`.
/// Returns the sheet the user clicked, if any.
pub fn paint(ui: &mut Ui, table: &Table<'_>, grid: &Grid, palette: &Palette) -> Option<usize> {
    let chosen = paint_sheets(ui, table, palette);

    let rule = Stroke::new(1.0, palette.faint);
    // The strips that the cells slide *under* have to be the surface the grid
    // is painted on, which is the raised preview card rather than the chrome
    // around it — otherwise a scrolled cell shows through them.
    let chrome = palette.card;
    let pitch = grid.pitch;
    // `show_rows` derives its row pitch from the ui's item spacing, so pin it
    // to the same gap this module paints at.
    ui.spacing_mut().item_spacing.y = ROW_GAP;

    // One extra row at the top of the content: the header strip's own space, so
    // the first data row is not born underneath the frozen header.
    let content_rows = table.rows.len() + 1;

    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, grid.row_height, content_rows, |ui, range| {
            // What makes the area scroll sideways rather than compress: the
            // content is as wide as the columns need, not as wide as the window.
            ui.set_min_width(grid.total_width);

            let origin = ui.max_rect().min;
            let clip = ui.clip_rect();
            let painter = ui.painter().clone();
            // Content row 0 is the header strip; data row `i` is content row
            // `i + 1`.
            let top_of = |content_row: usize| {
                origin.y + (content_row.saturating_sub(range.start)) as f32 * pitch
            };
            let header = Rect::from_min_max(clip.min, Pos2::new(clip.max.x, clip.min.y + pitch));
            let gutter =
                Rect::from_min_max(clip.min, Pos2::new(clip.min.x + grid.gutter, clip.max.y));

            // 1. The cells themselves.
            for content_row in range.clone() {
                let Some(row) = content_row.checked_sub(1).and_then(|i| table.rows.get(i)) else {
                    continue;
                };
                let top = top_of(content_row);
                for index in 0..grid.widths.len() {
                    let Some(rect) = grid.cell(index, origin.x, top) else {
                        continue;
                    };
                    // Columns scrolled off either side cost nothing.
                    if rect.max.x < clip.min.x || rect.min.x > clip.max.x {
                        continue;
                    }
                    let Some(cell) = row.cells.get(index) else {
                        continue;
                    };
                    if cell.text.is_empty() {
                        continue;
                    }
                    paint_text(
                        &painter,
                        rect,
                        &cell.text,
                        palette.cell_color(cell.kind),
                        // Numbers and dates flush right so a column of them
                        // lines up on the decimal point, exactly as the CLI and
                        // any spreadsheet show them.
                        cell.kind.align_right(),
                    );
                }
            }

            // 2. Opaque strips for the two frozen edges, so the cells slide
            // *under* the row numbers and the column letters.
            painter.rect_filled(gutter, 0.0, chrome);
            painter.rect_filled(header, 0.0, chrome);

            // 3. Faint rules on the column boundaries, run through the header
            // strip so the letters are separated too — but stopped at the last
            // row and the last column, so a three-row sheet is not drawn as an
            // empty cage the height and width of the window.
            let bottom = (top_of(range.end) - ROW_GAP).min(clip.max.y);
            let right = (origin.x + grid.total_width).min(clip.max.x);
            let rules = clip.min.y..=bottom;
            for left in grid.lefts.iter().skip(1) {
                let x = origin.x + left;
                if x > gutter.max.x && x < clip.max.x {
                    painter.vline(x, rules.clone(), rule);
                }
            }
            painter.vline(gutter.max.x, rules, rule);
            // Centred in the gap between the letters and the first row rather
            // than hard against either of them.
            painter.hline(clip.min.x..=right, header.max.y - ROW_GAP / 2.0, rule);

            // 4. The row numbers, dimmed and flush right against the rule, and
            // clipped so the top one cannot spill over the header strip.
            let below = painter.with_clip_rect(Rect::from_min_max(
                Pos2::new(clip.min.x, header.max.y),
                clip.max,
            ));
            for content_row in range.clone() {
                let Some(row) = content_row.checked_sub(1).and_then(|i| table.rows.get(i)) else {
                    continue;
                };
                let rect = Rect::from_min_size(
                    Pos2::new(clip.min.x + CELL_PAD, top_of(content_row)),
                    Vec2::new(grid.gutter - 2.0 * CELL_PAD, grid.row_height),
                );
                paint_text(&below, rect, &row.label, palette.dim, true);
            }

            // 5. The column letters, in the same dim as the gutter so neither
            // reads as data.
            let strip = painter.with_clip_rect(header);
            for (index, label) in table.columns.iter().enumerate() {
                let Some(rect) = grid.cell(index, origin.x, header.min.y) else {
                    continue;
                };
                if rect.max.x < clip.min.x || rect.min.x > clip.max.x {
                    continue;
                }
                paint_text(&strip, rect, label, palette.dim, false);
            }
        });

    chosen
}

/// One string inside `rect`, elided if it does not fit, flush left or right.
fn paint_text(painter: &egui::Painter, rect: Rect, text: &str, color: Color32, right: bool) {
    let job = style::cell_job(text, color, rect.width());
    let galley = painter.fonts_mut(|f| f.layout_job(job));
    let x = if right {
        // `cell_job` already capped the galley at the column width, so this
        // never pushes text out of the left of its own column.
        rect.max.x - galley.size().x
    } else {
        rect.min.x
    };
    painter.galley(Pos2::new(x, rect.min.y), galley, color);
}

/// The sheet names across the top, the previewed one bracketed — the same
/// shape the CLI's first line has always had, except that here they are
/// buttons: the CLI cannot switch sheets and a window can.
///
/// Returns the index the user picked, or `None` if they picked nothing.
fn paint_sheets(ui: &mut Ui, table: &Table<'_>, palette: &Palette) -> Option<usize> {
    if table.sheets.is_empty() {
        return None;
    }
    let mut chosen = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            RichText::new("Sheets:")
                .color(palette.dim)
                .monospace()
                .size(11.0),
        );
        for (index, name) in table.sheets.iter().take(MAX_SHEETS).enumerate() {
            let name = style::one_line(name);
            let active = index == table.active_sheet;
            let text = if active {
                RichText::new(format!("[{name}]"))
                    .color(palette.active)
                    .strong()
            } else {
                RichText::new(name).color(palette.faint)
            };
            if style::selectable(ui, active, text.monospace().size(11.0))
                .on_hover_text("Show this sheet")
                .clicked()
                && !active
            {
                chosen = Some(index);
            }
        }
        if table.sheets.len() > MAX_SHEETS {
            ui.label(
                RichText::new(format!("+{} more", table.sheets.len() - MAX_SHEETS))
                    .color(palette.faint)
                    .monospace()
                    .size(11.0),
            );
        }
    });
    ui.add_space(2.0);
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekio_core::CellKind;

    fn cell(text: &str) -> TableCell {
        TableCell {
            text: text.to_owned(),
            kind: CellKind::Text,
        }
    }

    fn row(label: &str, cells: &[&str]) -> TableRow {
        TableRow {
            label: label.to_owned(),
            cells: cells.iter().map(|text| cell(text)).collect(),
        }
    }

    fn table<'a>(columns: &'a [String], rows: &'a [TableRow]) -> Table<'a> {
        Table {
            columns,
            rows,
            sheets: &[],
            active_sheet: 0,
            total_rows: rows.len() as u64,
            total_cols: columns.len() as u64,
        }
    }

    #[test]
    fn the_widest_cells_of_a_column_come_back_longest_first() {
        let rows = vec![
            row("1", &["a", "zzzz"]),
            row("2", &["aaa", ""]),
            row("3", &["aa", "zz"]),
        ];
        assert_eq!(widest_cells(&rows, 0), vec!["aaa", "aa", "a"]);
        // Empty cells are not candidates, and a short row is skipped rather
        // than indexed.
        assert_eq!(widest_cells(&rows, 1), vec!["zzzz", "zz"]);
        assert!(widest_cells(&rows, 9).is_empty());
    }

    #[test]
    fn only_a_few_cells_per_column_are_ever_measured() {
        let rows: Vec<TableRow> = (0..500)
            .map(|i| row(&i.to_string(), &["x".repeat(i % 40 + 1).as_str()]))
            .collect();
        assert_eq!(widest_cells(&rows, 0).len(), SAMPLES);
        // …and the longest really is first, so the column is sized for it.
        assert_eq!(widest_cells(&rows, 0)[0].chars().count(), 40);
    }

    #[test]
    fn a_row_shorter_than_the_column_list_is_survivable() {
        let columns = vec!["A".to_owned(), "B".to_owned(), "C".to_owned()];
        let rows = vec![row("1", &["only one"])];
        let view = table(&columns, &rows);
        // No panic, and the footer still describes the sheet it was given.
        assert_eq!(view.footer(false), "1 rows × 3 columns");
    }

    #[test]
    fn the_footer_says_how_much_of_the_sheet_is_on_screen() {
        let columns = vec!["A".to_owned(), "B".to_owned()];
        let rows = vec![row("1", &["x", "y"])];
        let mut view = table(&columns, &rows);

        assert_eq!(view.footer(false), "1 rows × 2 columns");
        // A cap bit but the sheet never declared its size.
        assert_eq!(
            view.footer(true),
            "showing first 1 rows × 2 columns — more follow"
        );

        view.total_rows = 4000;
        view.total_cols = 90;
        assert_eq!(view.footer(false), "4000 rows × 90 columns — showing 1 × 2");
        // An IR that under-reports its own size must not produce "1 rows …
        // showing 1 × 2" backwards.
        view.total_rows = 0;
        view.total_cols = 0;
        assert_eq!(view.footer(false), "1 rows × 2 columns");
    }
}
