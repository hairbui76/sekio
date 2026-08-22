//! Laying a [`PreviewContent::Table`] out for the pane it actually has.
//!
//! Core stopped flattening spreadsheets into space-aligned text: it now hands
//! over columns, rows and cell kinds, and eliding is the frontend's call. This
//! module is that call, kept free of every ratatui type so the fiddly parts —
//! how the pane's columns are shared out, where the `…` goes, which rows are on
//! screen — can be unit tested without a terminal.
//!
//! Two things it is careful about:
//!
//! * **Display cells, not `char`s.** A terminal grid counts columns, and a
//!   Vietnamese sheet is where the two part company: `ề` written as `e` plus two
//!   combining marks is three `char`s and one column, and a column sized by
//!   `chars().count()` is then padded three-wide for something one wide, so
//!   every column to its right sits crooked. Everything here measures with
//!   `unicode-width`.
//! * **Nothing indexes.** A ragged row — fewer cells than there are columns —
//!   is normal input, not a bug to panic on.
//!
//! [`PreviewContent::Table`]: sekio_core::PreviewContent::Table

use sekio_core::{CellKind, TableCell, TableRow};
use unicode_width::UnicodeWidthChar;

/// Blank columns between two table columns, and between the row-number gutter
/// and the first of them. Matches the two spaces core used to print.
pub const COL_GAP: usize = 2;
/// Hard ceiling on one column's printed width, however much room the pane has.
/// Without it a single 4 000-character note would be the only thing on the row.
pub const MAX_COL_WIDTH: usize = 40;
/// Narrowest a squeezed column is allowed to get: two cells and the `…` that
/// says there was more. A column that cannot have this much is dropped instead,
/// and reported in the note under the table.
pub const MIN_COL_WIDTH: usize = 3;
/// Widest the row-number gutter is allowed to grow. Seven digits is a million
/// rows; past that the numbers are worth less than the space they cost.
pub const MAX_GUTTER: usize = 7;

/// Columns one character occupies on the terminal grid.
///
/// Combining marks measure zero, which is exactly right: the `̀ ` in a decomposed
/// `ề` is painted into the cell of the letter before it. Control characters have
/// no width of their own and are painted as a space (see [`fit`]), so they
/// measure one here to keep the two in step.
fn char_cells(ch: char) -> usize {
    if ch.is_control() {
        1
    } else {
        ch.width().unwrap_or(0)
    }
}

/// Columns a string occupies on the terminal grid.
pub fn cells(text: &str) -> usize {
    text.chars().map(char_cells).sum()
}

/// Cut `text` down to `width` display columns, marking the cut with `…`.
///
/// Returns the text unchanged when it already fits, so a cell that fits is
/// never touched. Control characters become spaces rather than being passed
/// through — a stray newline inside a cell would otherwise make one table row
/// two rows tall and shear the whole grid.
pub fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clean = |ch: char| if ch.is_control() { ' ' } else { ch };
    if cells(text) <= width {
        return if text.chars().any(char::is_control) {
            text.chars().map(clean).collect()
        } else {
            // The overwhelmingly common case: it fits and it is clean, so hand
            // the text straight back rather than rebuilding it.
            text.to_owned()
        };
    }
    // One column goes to the ellipsis.
    let room = width - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars().map(clean) {
        let w = char_cells(ch);
        if used + w > room {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Look a cell up without indexing: a row shorter than the column list is
/// ordinary input, and reading past its end must be an empty cell, not a panic.
pub fn cell_at(row: &TableRow, column: usize) -> Option<&TableCell> {
    row.cells.get(column)
}

/// Width of the row-number gutter: the widest label there is, capped. Never
/// zero, so the gutter is always a visible column of its own.
pub fn gutter_width(rows: &[TableRow]) -> usize {
    rows.iter()
        .map(|row| cells(&row.label))
        .max()
        .unwrap_or(0)
        .clamp(1, MAX_GUTTER)
}

/// What each column would need to show everything in it, unelided and capped at
/// [`MAX_COL_WIDTH`]. The heading counts too — a column narrower than its own
/// letter would be unreadable.
///
/// Scans every row, so the caller caches the result rather than recomputing it
/// per frame.
pub fn natural_widths(columns: &[String], rows: &[TableRow]) -> Vec<usize> {
    let mut natural: Vec<usize> = columns
        .iter()
        .map(|name| cells(name).min(MAX_COL_WIDTH))
        .collect();
    for row in rows {
        for (width, cell) in natural.iter_mut().zip(row.cells.iter()) {
            *width = (*width).max(cells(&cell.text).min(MAX_COL_WIDTH));
        }
    }
    natural
}

/// How many columns a pane `budget` wide can seat at all.
///
/// A column costs its own width plus the gap in front of it, so
/// [`MIN_COL_WIDTH`] plus [`COL_GAP`] is what one has to be worth before it is
/// shown. At least one is always seated: a table with no columns is not a
/// preview. Deliberately independent of the *content* — [`crate::app`] needs
/// this answer to size the scrollback and must not rescan every cell to get it.
pub fn seated_columns(columns: usize, budget: usize, gutter: usize) -> usize {
    let room = budget.saturating_sub(gutter);
    let seats = (room / (COL_GAP + MIN_COL_WIDTH)).max(1);
    columns.min(seats)
}

/// Decide a printed width for every column, spending the `budget` display
/// columns the pane has.
///
/// Two rules, in order:
///
/// 1. **A column is never padded past what it needs.** `natural[i]` is the
///    widest thing in column `i`, and no column is ever given more than that —
///    an `STT` column stays three wide however much room is going spare. So
///    when the natural widths fit, they are used unchanged and nothing is
///    elided at all.
/// 2. **When they do not fit, the shortfall comes out of the widest columns.**
///    A single cap is chosen by water-filling: every column narrower than the
///    cap keeps its full width, and every column above it is cut to the cap. A
///    three-character number column therefore never loses a digit so a prose
///    column can keep one — the width comes out of whoever has the most of it,
///    which is also where a `…` costs the least meaning. The few columns the
///    integer division leaves over go to the cut columns, left to right, so the
///    table fills the pane exactly rather than stopping short of it.
///
/// Returns *fewer* widths than `natural` when the budget cannot seat every
/// column at [`MIN_COL_WIDTH`]; past that point another column would show
/// nothing but an ellipsis. The caller reports the dropped ones in the note
/// under the table.
pub fn plan(natural: &[usize], budget: usize, gutter: usize) -> Vec<usize> {
    let shown = seated_columns(natural.len(), budget, gutter);
    let Some(natural) = natural.get(..shown) else {
        return Vec::new();
    };
    let room = budget.saturating_sub(gutter);
    // `.max(shown)` keeps the pathological case (a pane too narrow to seat what
    // it just seated) off the zero-width path below; every column still gets at
    // least one cell.
    let content = room.saturating_sub(COL_GAP * shown).max(shown);

    // Water level: walk the columns narrowest first, letting each keep its
    // natural width for as long as everything still to come could be capped at
    // that width and fit.
    let mut sorted: Vec<usize> = natural.to_vec();
    sorted.sort_unstable();
    let mut spent = 0usize;
    let mut rest = shown;
    for &width in &sorted {
        if spent + rest * width > content {
            break;
        }
        spent += width;
        rest -= 1;
    }
    if rest == 0 {
        // Everything fits at its natural width: nothing is elided.
        return natural.to_vec();
    }

    let left = content - spent;
    let cap = (left / rest).max(1);
    let mut spare = left.saturating_sub(cap * rest);
    natural
        .iter()
        .map(|&width| {
            if width <= cap {
                width
            } else if spare > 0 {
                spare -= 1;
                cap + 1
            } else {
                cap
            }
        })
        .collect()
}

/// The rows of the table that are on screen, and nothing else.
///
/// The whole point of virtualisation: a 10 000-row sheet costs the pane's worth
/// of `Row`s per frame, not ten thousand. Past the end this is empty rather
/// than a panic.
pub fn window(rows: &[TableRow], scroll: usize, height: usize) -> &[TableRow] {
    let start = scroll.min(rows.len());
    let end = start.saturating_add(height).min(rows.len());
    rows.get(start..end).unwrap_or(&[])
}

/// Rows the pane spends on chrome rather than data: the sheet strip, the column
/// letters, and the note under the table. Both [`crate::app::content_len`] and
/// the painter derive the data area from this, so they cannot disagree about
/// where the last row is.
pub fn chrome_rows(sheets: &[String], note: Option<&String>) -> usize {
    usize::from(!sheets.is_empty()) + 1 + usize::from(note.is_some())
}

/// What is not on screen, said once under the table — or `None` when the pane
/// is showing the whole sheet.
///
/// `truncated` is core's own flag: a read cap bit and the sheet goes on past
/// what we were given. `shown_cols` is what [`plan`] could seat, which is the
/// one part of this the pane's width decides.
pub fn note(
    shown_rows: usize,
    shown_cols: usize,
    total_rows: u64,
    total_cols: u64,
    truncated: bool,
) -> Option<String> {
    let shown_rows = shown_rows as u64;
    let shown_cols = shown_cols as u64;
    // Core reports the sheet's full extent; a sheet that never declared one
    // reports what it read, so never claim less than we are showing.
    let total_rows = total_rows.max(shown_rows);
    let total_cols = total_cols.max(shown_cols);
    let more = total_rows > shown_rows || total_cols > shown_cols;
    if !more && !truncated {
        return None;
    }
    if more {
        Some(format!(
            "{total_rows} rows × {total_cols} columns — showing {shown_rows} × {shown_cols}"
        ))
    } else {
        // A cap bit, but the sheet never declared its size (an xlsx written
        // without a `<dimension>`), so "there is more" is all we honestly know.
        // Better a vague note than a confident wrong number.
        Some(format!(
            "showing first {shown_rows} rows × {shown_cols} columns — more follow"
        ))
    }
}

/// One painted cell: the text as it will appear, and how to place it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Painted {
    pub text: String,
    pub kind: CellKind,
    /// Numbers and dates sit flush right so they line up on the decimal point.
    pub right: bool,
}

/// Paint one row into its columns — elided to the widths [`plan`] chose, with
/// the alignment each cell's kind asks for.
///
/// Separate from the ratatui layer on purpose: this is the part with rules in
/// it, so this is the part with tests on it.
pub fn paint_row(row: &TableRow, widths: &[usize]) -> Vec<Painted> {
    widths
        .iter()
        .enumerate()
        .map(|(i, &width)| match cell_at(row, i) {
            Some(cell) => Painted {
                text: fit(&cell.text, width),
                kind: cell.kind,
                right: cell.kind.align_right(),
            },
            // A ragged row's missing cells still occupy their columns.
            None => Painted {
                text: String::new(),
                kind: CellKind::Text,
                right: false,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, kind: CellKind) -> TableCell {
        TableCell {
            text: text.to_owned(),
            kind,
        }
    }

    fn row(label: &str, cells: &[(&str, CellKind)]) -> TableRow {
        TableRow {
            label: label.to_owned(),
            cells: cells.iter().map(|(t, k)| cell(t, *k)).collect(),
        }
    }

    fn columns(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("C{i}")).collect()
    }

    // ---- display width ----

    /// The reason this module exists. `ề` decomposed is one column of terminal
    /// and three `char`s; a layout that counts `char`s reserves three columns
    /// for it and every column to its right sits one or two cells out.
    #[test]
    fn vietnamese_measures_in_display_cells_not_chars() {
        // NFC: one char, one cell.
        let composed = "\u{1EC1}"; // ề
        assert_eq!(composed.chars().count(), 1);
        assert_eq!(cells(composed), 1);

        // NFD: e + combining circumflex + combining grave. Three chars, still
        // one cell — this is the case `chars().count()` gets wrong.
        let decomposed = "e\u{0302}\u{0300}";
        assert_eq!(decomposed.chars().count(), 3);
        assert_eq!(cells(decomposed), 1, "combining marks take no column");

        // A whole word: "Tiếng Việt" decomposed is 14 chars, 10 columns.
        let word = "Tie\u{0302}\u{0301}ng Vie\u{0323}\u{0302}t";
        assert_eq!(word.chars().count(), 14);
        assert_eq!(cells(word), 10);
        assert_eq!(cells("Tiếng Việt"), 10, "composed measures the same");

        // …and a column sized from it holds it whole.
        let widths = natural_widths(&columns(1), &[row("1", &[(word, CellKind::Text)])]);
        assert_eq!(widths, vec![10]);
        assert_eq!(fit(word, widths[0]), word, "it fits, so nothing is elided");
    }

    #[test]
    fn wide_characters_count_two_cells() {
        assert_eq!(cells("日本語"), 6);
        assert_eq!(cells("ab"), 2);
        // Eliding a wide string keeps the total inside the budget.
        let cut = fit("日本語です", 6);
        assert!(cells(&cut) <= 6, "{cut:?} is {} cells", cells(&cut));
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn fit_elides_only_when_it_has_to() {
        assert_eq!(fit("abc", 5), "abc");
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("abcd", 3), "ab…");
        assert_eq!(fit("abcd", 1), "…");
        assert_eq!(fit("abcd", 0), "");
        // The ellipsis is not spent when the text already fits exactly.
        assert_eq!(cells(&fit("abcde", 4)), 4);
    }

    #[test]
    fn control_characters_never_break_the_grid() {
        let cut = fit("a\nb\tc", 10);
        assert!(!cut.contains('\n'), "{cut:?}");
        assert!(!cut.contains('\t'), "{cut:?}");
        assert_eq!(cells(&cut), 5, "a control character still costs its cell");
    }

    // ---- column width distribution ----

    #[test]
    fn everything_fits_so_nothing_is_padded_or_elided() {
        // gutter 3, three columns needing 3 + 5 + 4, gaps 2 each: 3 + 6 + 12 = 21.
        let natural = vec![3, 5, 4];
        let widths = plan(&natural, 60, 3);
        assert_eq!(
            widths, natural,
            "a spare pane must not stretch columns past what they need"
        );
    }

    /// The rule the whole feature turns on: when it is tight, the width comes
    /// out of the widest column, not out of everyone equally.
    #[test]
    fn a_squeeze_takes_width_from_the_widest_first() {
        // Three-cell number column, and two prose columns.
        let natural = vec![3, 40, 40];
        // gutter 4, gaps 6, so 30 cells of content to share out.
        let widths = plan(&natural, 40, 4);
        assert_eq!(widths.len(), 3);
        assert_eq!(widths[0], 3, "the narrow column keeps every digit");
        assert!(widths[1] >= 13 && widths[2] >= 13, "{widths:?}");
        assert_eq!(
            widths.iter().sum::<usize>(),
            30,
            "the leftovers are spent, so the table fills the pane"
        );
    }

    #[test]
    fn one_giant_cell_does_not_take_the_whole_row() {
        let natural = natural_widths(
            &columns(3),
            &[row(
                "1",
                &[
                    ("7", CellKind::Number),
                    (&"x".repeat(4000), CellKind::Text),
                    ("ok", CellKind::Text),
                ],
            )],
        );
        assert_eq!(
            natural[1], MAX_COL_WIDTH,
            "a 4000-character note is capped before layout even starts"
        );
        let widths = plan(&natural, 100, 3);
        assert_eq!(widths[0], 2, "column heading `C0` is two cells wide");
        assert_eq!(widths[2], 2);
        assert!(widths[1] <= MAX_COL_WIDTH);
    }

    #[test]
    fn a_very_narrow_pane_drops_columns_instead_of_shredding_them() {
        let natural = vec![10; 8];
        // Room for the gutter and two columns at MIN_COL_WIDTH + COL_GAP.
        let widths = plan(&natural, 14, 3);
        assert_eq!(widths.len(), 2, "{widths:?}");
        assert!(widths.iter().all(|&w| w >= 1));
        assert_eq!(seated_columns(8, 14, 3), 2);

        // …and one column is always shown, however hopeless the pane.
        for budget in 0..6 {
            let widths = plan(&natural, budget, 3);
            assert_eq!(widths.len(), 1, "budget {budget}");
            assert!(widths[0] >= 1, "budget {budget}");
        }
        assert!(plan(&[], 80, 3).is_empty(), "no columns, no widths");
    }

    /// Whatever the pane, the table must not be wider than it.
    #[test]
    fn a_planned_table_never_overruns_the_pane() {
        let naturals: [&[usize]; 4] = [&[3, 40, 40], &[1; 20], &[7, 7, 7], &[40]];
        for natural in naturals {
            for budget in 6..120usize {
                for gutter in 1..5usize {
                    let widths = plan(natural, budget, gutter);
                    let used: usize = gutter + widths.iter().map(|w| w + COL_GAP).sum::<usize>();
                    assert!(
                        used <= budget.max(gutter + COL_GAP + 1),
                        "natural {natural:?} budget {budget} gutter {gutter} -> {widths:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_gutter_fits_the_widest_row_number_and_no_more() {
        let rows: Vec<TableRow> = (1..=12).map(|i| row(&i.to_string(), &[])).collect();
        assert_eq!(gutter_width(&rows), 2);
        assert_eq!(gutter_width(&[]), 1, "an empty sheet still has a gutter");
        let huge = vec![row("123456789012", &[])];
        assert_eq!(gutter_width(&huge), MAX_GUTTER);
    }

    // ---- alignment and colour keys ----

    #[test]
    fn numbers_and_dates_align_right_everything_else_left() {
        let r = row(
            "7",
            &[
                ("hello", CellKind::Text),
                ("42", CellKind::Number),
                ("TRUE", CellKind::Bool),
                ("2026-08-22", CellKind::Date),
                ("#REF!", CellKind::Error),
            ],
        );
        let painted = paint_row(&r, &[10, 10, 10, 10, 10]);
        let right: Vec<bool> = painted.iter().map(|p| p.right).collect();
        assert_eq!(right, vec![false, true, false, true, false]);
        let kinds: Vec<CellKind> = painted.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            vec![
                CellKind::Text,
                CellKind::Number,
                CellKind::Bool,
                CellKind::Date,
                CellKind::Error
            ]
        );
    }

    #[test]
    fn a_ragged_row_paints_blanks_rather_than_panicking() {
        let r = row("3", &[("only", CellKind::Text)]);
        let painted = paint_row(&r, &[6, 6, 6]);
        assert_eq!(painted.len(), 3);
        assert_eq!(painted[0].text, "only");
        assert_eq!(painted[1].text, "");
        assert_eq!(painted[2].text, "");
        assert!(cell_at(&r, 9).is_none());

        // A row with *more* cells than there are columns is equally survivable.
        let long = row("4", &[("a", CellKind::Text); 9]);
        assert_eq!(paint_row(&long, &[3]).len(), 1);
    }

    #[test]
    fn painting_elides_to_the_planned_width() {
        let r = row("1", &[("a long piece of prose", CellKind::Text)]);
        let painted = paint_row(&r, &[8]);
        assert_eq!(cells(&painted[0].text), 8);
        assert!(painted[0].text.ends_with('…'));
    }

    // ---- the visible window ----

    #[test]
    fn the_window_is_bounded_by_the_pane_not_the_sheet() {
        let rows: Vec<TableRow> = (0..10_000).map(|i| row(&i.to_string(), &[])).collect();
        let shown = window(&rows, 0, 30);
        assert_eq!(shown.len(), 30, "a 10 000-row sheet costs 30 rows a frame");
        assert_eq!(shown[0].label, "0");

        let scrolled = window(&rows, 9_990, 30);
        assert_eq!(scrolled.len(), 10, "the last page stops at the last row");
        assert_eq!(scrolled[0].label, "9990");

        assert!(
            window(&rows, 99_999, 30).is_empty(),
            "past the end is empty"
        );
        assert!(window(&rows, 0, 0).is_empty());
        assert!(window(&[], 0, 10).is_empty());
    }

    // ---- the note ----

    #[test]
    fn the_note_only_appears_when_something_is_missing() {
        assert_eq!(note(10, 3, 10, 3, false), None);
        // Core's own flag, with no declared extent to quantify it.
        let vague = note(10, 3, 10, 3, true).expect("truncated must say so");
        assert!(vague.contains("more follow"), "{vague}");
        // More rows than we were given.
        let rows = note(200, 8, 5_000, 8, true).expect("a bigger sheet must say so");
        assert!(rows.contains("5000 rows"), "{rows}");
        assert!(rows.contains("showing 200 × 8"), "{rows}");
        // Columns dropped by a narrow pane, with nothing else missing.
        let cols = note(10, 2, 10, 6, false).expect("dropped columns must say so");
        assert!(cols.contains("6 columns"), "{cols}");
        // Never claim a sheet is smaller than what is on screen: a sheet that
        // under-reports its own extent still gets counted honestly.
        let honest = note(10, 2, 3, 5, false).expect("dropped columns");
        assert!(honest.contains("10 rows"), "{honest}");
        assert!(!honest.contains("3 rows"), "{honest}");
    }

    #[test]
    fn chrome_is_the_sheet_strip_the_headings_and_the_note() {
        let sheets = vec!["Data".to_owned()];
        let note = "…".to_owned();
        assert_eq!(chrome_rows(&sheets, Some(&note)), 3);
        assert_eq!(chrome_rows(&sheets, None), 2);
        assert_eq!(chrome_rows(&[], None), 1, "headings always cost a row");
    }
}
