//! Mapping from the core IR's styling (24-bit RGB spans from syntect) to
//! egui's text formatting. Pure functions so they can be tested headlessly.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, TextWrapMode};
use sekio_core::{Span, StyledLine};

/// Font size used for every monospace surface (text, hexdump, listings).
pub const MONO_SIZE: f32 = 13.0;

/// Dimmed key/label color; matches the CLI's `\x1b[38;2;143;161;179m`.
pub const DIM: Color32 = Color32::from_rgb(143, 161, 179);

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
    fn mono_job_concatenates_runs_in_order() {
        let job = mono_job(&[("00000000  ", DIM), ("ff ", Color32::WHITE)], MONO_SIZE);
        assert_eq!(job.text, "00000000  ff ");
        assert_eq!(job.sections.len(), 2);
    }
}
