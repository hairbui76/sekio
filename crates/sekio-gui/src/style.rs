//! Mapping from the core IR's styling (24-bit RGB spans from syntect) to
//! egui's text formatting. Pure functions so they can be tested headlessly.

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Color32, FontId, TextWrapMode};
use sekio_core::{CellKind, Span, StyledLine};

/// Font size used for every monospace surface (text, hexdump, listings).
pub const MONO_SIZE: f32 = 13.0;

/// Dimmed key/label color; matches the CLI's `\x1b[38;2;143;161;179m`.
///
/// This is also base0D of the base16-ocean.dark palette below, which is why the
/// table's column letters and row numbers use it rather than a colour of their
/// own: the sheet chrome and the rest of the app's labels are the same grey.
pub const DIM: Color32 = Color32::from_rgb(143, 161, 179);

// ---------------------------------------------------------------------------
// Table palette
// ---------------------------------------------------------------------------
//
// `PreviewContent::Table` carries a `CellKind` per cell instead of a colour, so
// the colour is the frontend's to pick. These are the base16-ocean.dark slots
// the CLI's spreadsheet output has always used, so a sheet in the GUI reads the
// same as the same sheet in a terminal, and sits beside a highlighted source
// file without clashing. They are written out here rather than imported: core's
// palette is private to its spreadsheet renderer, and the IR is deliberately
// colour-free.

/// base0B — the sheet actually being previewed, in the sheet list.
pub const ACTIVE: Color32 = Color32::from_rgb(0xa3, 0xbe, 0x8c);
/// base03 — the rules between columns, and anything only just worth painting.
/// Faint on purpose: a grid needs separation, not a border round every cell.
pub const FAINT: Color32 = Color32::from_rgb(0x65, 0x73, 0x7e);
/// base05 — an ordinary string cell.
pub const CELL_TEXT: Color32 = Color32::from_rgb(0xc0, 0xc5, 0xce);
/// base09 — a numeric cell, deliberately distinct from text.
pub const CELL_NUMBER: Color32 = Color32::from_rgb(0xd0, 0x87, 0x70);
/// base0E — a boolean.
pub const CELL_BOOL: Color32 = Color32::from_rgb(0xb4, 0x8e, 0xad);
/// base0C — a date, time or duration.
pub const CELL_DATE: Color32 = Color32::from_rgb(0x96, 0xb5, 0xb4);
/// base08 — `#REF!` and friends.
pub const CELL_ERROR: Color32 = Color32::from_rgb(0xbf, 0x61, 0x6a);

/// The colour a cell is painted in, from what it holds.
pub fn cell_color(kind: CellKind) -> Color32 {
    match kind {
        CellKind::Text => CELL_TEXT,
        CellKind::Number => CELL_NUMBER,
        CellKind::Bool => CELL_BOOL,
        CellKind::Date => CELL_DATE,
        CellKind::Error => CELL_ERROR,
    }
}

/// One table cell, laid out to fit `max_width` and elided with `…` if it
/// cannot.
///
/// Truncation is the frontend's decision now that the IR carries whole cells,
/// and this is where it is made: a column has a ceiling (see `table.rs`), and a
/// cell wider than the column it is in gets egui's own single-row elision
/// rather than a hard clip that cuts a glyph in half.
pub fn cell_job(text: &str, color: Color32, max_width: f32) -> LayoutJob {
    let mut job = LayoutJob::single_section(
        one_line(text),
        TextFormat::simple(FontId::monospace(MONO_SIZE), color),
    );
    job.wrap = TextWrapping {
        max_width: max_width.max(1.0),
        // One row, always: a cell that wrapped would push every row below it
        // out of step with the virtualiser's fixed row height.
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job
}

/// A cell's text with every control character flattened to a space.
///
/// A spreadsheet cell may legally contain newlines and tabs. Left alone they
/// would either open a second row — which the fixed-height virtualiser cannot
/// account for — or paint as nothing at all, silently swallowing the words
/// after them. Borrowed unchanged in the overwhelmingly common case.
pub fn one_line(text: &str) -> String {
    if text.chars().any(char::is_control) {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
    } else {
        text.to_owned()
    }
}

/// Convert a core span color to egui's, falling back to the theme's text color
/// when the renderer gave us no color (plain text, hexdump ASCII, …).
pub fn span_color(fg: Option<(u8, u8, u8)>, fallback: Color32) -> Color32 {
    match fg {
        Some((r, g, b)) => Color32::from_rgb(r, g, b),
        None => fallback,
    }
}

/// egui's bundled fonts ship no bold face and `TextFormat` has no weight axis,
/// so bold is approximated by lifting the color toward white.
///
/// The lift is deliberately small. At 45% a syntect keyword colour like
/// (180,142,173) came out (214,193,210) — visibly washed toward white, so
/// highlighted code looked desaturated in the GUI while the CLI, where bold is
/// a real ANSI attribute, kept the true colour. Emphasis is not worth losing
/// the hue that carries the actual meaning.
pub fn brighten(color: Color32) -> Color32 {
    let lift = |c: u8| (c as u16 + (255 - c as u16) * 18 / 100) as u8;
    Color32::from_rgba_premultiplied(lift(color.r()), lift(color.g()), lift(color.b()), color.a())
}

/// `Span` -> `TextFormat` in a monospace font.
pub fn span_format(span: &Span, fallback: Color32, size: f32) -> TextFormat {
    let mut color = span_color(span.fg, fallback);
    if span.bold {
        color = brighten(color);
    }
    TextFormat {
        font_id: FontId::monospace(size),
        color,
        italics: span.italic,
        ..Default::default()
    }
}

/// Lay out a whole styled document into one `LayoutJob`.
///
/// Built once when a preview arrives, not per frame: egui memoizes galleys by
/// the job's hash, so re-showing the same job every frame is nearly free.
pub fn text_job(lines: &[StyledLine], fallback: Color32, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    // Code keeps its own line breaks; horizontal scrolling beats reflowing.
    job.wrap.max_width = f32::INFINITY;
    job.wrap.break_anywhere = false;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            job.append(
                "\n",
                0.0,
                TextFormat::simple(FontId::monospace(size), fallback),
            );
        }
        for span in &line.spans {
            // syntect hands back trailing newlines inside spans; the job adds
            // its own separators, so strip them to avoid double spacing.
            let text = span.text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                continue;
            }
            job.append(text, 0.0, span_format(span, fallback, size));
        }
    }
    job
}

/// A single monospace line built from (text, color) runs — used for hexdump
/// rows and listing rows, which have no syntect styling of their own.
pub fn mono_job(runs: &[(&str, Color32)], size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    for (text, color) in runs {
        job.append(
            text,
            0.0,
            TextFormat::simple(FontId::monospace(size), *color),
        );
    }
    job
}

/// Wrap mode for surfaces that must never reflow (code, hex).
pub const NO_WRAP: TextWrapMode = TextWrapMode::Extend;

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, fg: Option<(u8, u8, u8)>, bold: bool, italic: bool) -> Span {
        Span {
            text: text.to_owned(),
            fg,
            bold,
            italic,
        }
    }

    #[test]
    fn rgb_spans_map_to_the_same_egui_color() {
        assert_eq!(
            span_color(Some((10, 20, 30)), Color32::RED),
            Color32::from_rgb(10, 20, 30)
        );
    }

    #[test]
    fn colorless_spans_use_the_theme_fallback() {
        assert_eq!(
            span_color(None, Color32::from_rgb(1, 2, 3)),
            Color32::from_rgb(1, 2, 3)
        );
    }

    #[test]
    fn bold_brightens_and_italic_sets_the_italics_flag() {
        let base = span("x", Some((100, 100, 100)), false, false);
        let bold = span("x", Some((100, 100, 100)), true, false);
        let italic = span("x", Some((100, 100, 100)), false, true);

        let plain = span_format(&base, Color32::WHITE, MONO_SIZE);
        let strong = span_format(&bold, Color32::WHITE, MONO_SIZE);
        let slanted = span_format(&italic, Color32::WHITE, MONO_SIZE);

        assert_eq!(plain.color, Color32::from_rgb(100, 100, 100));
        assert!(strong.color.r() > plain.color.r(), "bold must be brighter");
        assert!(!plain.italics);
        assert!(slanted.italics);
        assert_eq!(plain.font_id, FontId::monospace(MONO_SIZE));
    }

    #[test]
    fn brighten_is_saturating_and_keeps_white_white() {
        assert_eq!(brighten(Color32::WHITE), Color32::WHITE);
        let dark = brighten(Color32::from_rgb(0, 0, 0));
        assert!(dark.r() > 0 && dark.r() < 255);
    }

    #[test]
    fn text_job_joins_lines_and_drops_embedded_newlines() {
        let lines = vec![
            StyledLine {
                spans: vec![span("fn main() {\n", Some((200, 200, 200)), false, false)],
            },
            StyledLine {
                spans: vec![
                    span("    let x", None, false, false),
                    span(" = 1;", Some((1, 2, 3)), true, false),
                ],
            },
        ];
        let job = text_job(&lines, Color32::GRAY, MONO_SIZE);
        assert_eq!(job.text, "fn main() {\n    let x = 1;");
        assert!(job.sections.len() >= 3, "each span keeps its own format");
        assert!(job.wrap.max_width.is_infinite(), "code must not reflow");
    }

    #[test]
    fn empty_document_produces_an_empty_job() {
        let job = text_job(&[], Color32::GRAY, MONO_SIZE);
        assert!(job.text.is_empty());
    }

    #[test]
    fn every_cell_kind_gets_its_own_colour() {
        let colors = [
            cell_color(CellKind::Text),
            cell_color(CellKind::Number),
            cell_color(CellKind::Bool),
            cell_color(CellKind::Date),
            cell_color(CellKind::Error),
        ];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b, "two cell kinds share a colour: {colors:?}");
            }
        }
        // The one that has to be unmistakable.
        assert_eq!(cell_color(CellKind::Error), CELL_ERROR);
    }

    #[test]
    fn a_cell_is_laid_out_as_one_elided_row() {
        let job = cell_job("a very long note indeed", CELL_TEXT, 40.0);
        assert_eq!(job.wrap.max_rows, 1, "a wrapped cell breaks the row pitch");
        assert_eq!(job.wrap.overflow_character, Some('…'));
        assert_eq!(job.wrap.max_width, 40.0);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, CELL_TEXT);
        assert_eq!(
            job.sections[0].format.font_id,
            FontId::monospace(MONO_SIZE),
            "cells are monospace, or no column ever lines up"
        );
        // A zero-width column must not produce a zero or negative wrap width,
        // which epaint would treat as "fit nothing at all".
        assert!(cell_job("x", CELL_TEXT, 0.0).wrap.max_width > 0.0);
    }

    #[test]
    fn a_cell_containing_a_newline_is_flattened_rather_than_swallowed() {
        assert_eq!(one_line("two\nlines"), "two lines");
        assert_eq!(one_line("a\tb\r\nc"), "a b  c");
        // The common case is unchanged, accents and all.
        assert_eq!(one_line("Đứng lớp"), "Đứng lớp");
        assert_eq!(cell_job("two\nlines", CELL_TEXT, 400.0).text, "two lines");
    }

    #[test]
    fn mono_job_concatenates_runs_in_order() {
        let job = mono_job(&[("00000000  ", DIM), ("ff ", Color32::WHITE)], MONO_SIZE);
        assert_eq!(job.text, "00000000  ff ");
        assert_eq!(job.sections.len(), 2);
    }
}
