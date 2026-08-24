//! The fallback fonts, for text egui's bundled ones cannot draw.
//!
//! egui ships Ubuntu-Light (proportional) and Hack (monospace). Between them
//! they cover Latin-1 and 8–12 characters of Latin Extended Additional
//! (U+1E00–U+1EFF) — which is 244 short of the block, and the missing ones are
//! most of Vietnamese. So `Thông cáo báo chí về FLC.pdf` rendered as
//!
//! ```text
//! 00. Thông cáo báo chí v□ FLC.pdf
//! ```
//!
//! `ô` and `á` are Latin-1 and came out fine; `ề` (U+1EC1) is not, and became
//! egui's replacement box. That is every file name in the UI for a user whose
//! files are Vietnamese-named, and every line of any Vietnamese text file they
//! preview — which is drawn in the monospace family, so both families need
//! covering.
//!
//! Two Noto faces fix it, vendored in `assets/` (see `assets/README.md` for
//! provenance and the SIL Open Font License they carry):
//!
//! * Noto Sans — 256/256 of Latin Extended Additional — for `Proportional`.
//! * Noto Mono — 100/256, including all 90 precomposed Vietnamese letters
//!   (U+1EA0–U+1EF9) — for `Monospace`, where a proportional face would ruin
//!   the column alignment that hexdumps and code listings depend on.
//!
//! They are installed as **fallbacks appended to the end** of each family, not
//! as replacements. epaint walks a family's list in order and takes the first
//! face that has the character, so Ubuntu-Light and Hack still draw everything
//! they already drew — the app looks pixel-for-pixel identical on an English
//! UI — and Noto only ever engages for a character that would otherwise have
//! been a box.
//!
//! Cost: 620,520 bytes of TTF baked in, measured as +618,496 bytes (604 KiB)
//! on the stripped release binary — 26.8 MB to 27.4 MB. Nothing measurable at
//! runtime: epaint parses a face the first time a glyph needs it, so a session
//! that never draws a non-Latin-1 character never touches either file.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// Family-name key for the proportional fallback.
const SANS: &str = "noto-sans";

/// Family-name key for the monospace fallback.
const MONO: &str = "noto-mono";

/// Noto Sans Regular. Covers the whole of Latin Extended Additional.
const SANS_TTF: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");

/// Noto Mono Regular. Covers every Vietnamese precomposed letter, at a fixed
/// advance width so monospace surfaces stay in their columns.
const MONO_TTF: &[u8] = include_bytes!("../assets/NotoMono-Regular.ttf");

/// Append the fallbacks to `ctx`'s font families.
///
/// Called once, from `SekioApp::new`, before the first frame is laid out.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(with_fallbacks(FontDefinitions::default()));
}

/// The pure half of [`install`]: egui's defaults plus our two faces, last in
/// each family.
///
/// Separate so a test can assert the ordering without a `Context`, and so the
/// "appended, never prepended" rule is stated in one place.
pub fn with_fallbacks(mut fonts: FontDefinitions) -> FontDefinitions {
    fonts
        .font_data
        .insert(SANS.to_owned(), Arc::new(FontData::from_static(SANS_TTF)));
    fonts
        .font_data
        .insert(MONO.to_owned(), Arc::new(FontData::from_static(MONO_TTF)));

    for (family, fallback) in [
        (FontFamily::Proportional, SANS),
        (FontFamily::Monospace, MONO),
    ] {
        let chain = fonts.families.entry(family).or_default();
        if !chain.iter().any(|name| name == fallback) {
            // Push, never insert at 0: the first face that has a character
            // wins, so putting Noto first would silently restyle the entire
            // UI. Last means "only for what the defaults cannot draw".
            chain.push(fallback.to_owned());
        }
    }
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One character from each of the two blocks in play: `ô` is Latin-1 and
    /// egui already had it, `ề` is Latin Extended Additional and is the one
    /// from the bug report.
    const LATIN1: char = 'ô';
    const EXTENDED: char = 'ề';

    fn chain(fonts: &FontDefinitions, family: FontFamily) -> Vec<String> {
        fonts.families.get(&family).cloned().unwrap_or_default()
    }

    #[test]
    fn the_fallbacks_go_last_in_both_families() {
        let defaults = FontDefinitions::default();
        let fonts = with_fallbacks(FontDefinitions::default());

        for (family, fallback) in [
            (FontFamily::Proportional, SANS),
            (FontFamily::Monospace, MONO),
        ] {
            let before = chain(&defaults, family.clone());
            let after = chain(&fonts, family.clone());
            assert_eq!(
                after.last().map(String::as_str),
                Some(fallback),
                "{family:?} must end with the fallback"
            );
            assert_eq!(
                &after[..before.len()],
                &before[..],
                "{family:?}: egui's own faces keep their order and their priority"
            );
        }
    }

    #[test]
    fn installing_twice_does_not_stack_the_fallback_up() {
        let once = with_fallbacks(FontDefinitions::default());
        let twice = with_fallbacks(once.clone());
        assert_eq!(
            chain(&twice, FontFamily::Monospace),
            chain(&once, FontFamily::Monospace)
        );
    }

    /// A code point no font on earth has a glyph for, so whatever epaint
    /// rasterises for it *is* the replacement box. Comparing against this is
    /// how "was a glyph drawn?" is asked without hard-coding which character
    /// epaint picked for its box (`◻`, or `?` when it cannot draw that either).
    ///
    /// Note `Fonts::has_glyph` is not usable for this: it answers by comparing
    /// the resolved face against the face that owns the replacement character,
    /// so in the Monospace family — where Hack owns both `◻` and `ô` — it
    /// reports false for characters that render perfectly well.
    const NEVER_DRAWN: char = '\u{10FFFD}';

    /// Build a real `Fonts` and ask it what it would rasterise.
    fn fonts(definitions: FontDefinitions) -> egui::epaint::text::Fonts {
        egui::epaint::text::Fonts::new(egui::epaint::text::TextOptions::default(), definitions)
    }

    /// The corners, in the font atlas, of the glyph epaint would rasterise for
    /// `c` — read off a laid-out galley, not claimed from a charmap. Two
    /// characters sharing a rectangle are literally the same picture.
    ///
    /// `epaint::text::font::UvRect` is not publicly re-exported, hence the
    /// tuple of its two corners rather than the type itself.
    fn glyph(
        fonts: &mut egui::epaint::text::Fonts,
        family: &FontFamily,
        c: char,
    ) -> ([u16; 2], [u16; 2]) {
        let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            c.to_string(),
            egui::FontId::new(13.0, family.clone()),
            egui::Color32::WHITE,
        );
        let uv = galley.rows[0].row.glyphs[0].uv_rect;
        (uv.min, uv.max)
    }

    /// Every glyph the chrome paints, in the family it paints it in.
    ///
    /// 0.12.1 shipped the theme control as a replacement box: `◐` and `☾` are
    /// the obvious pictures for "half lit" and "night", and Noto Sans has
    /// neither. Arrows are worse, because they are in the *monospace* face
    /// only — so the key legend renders them correctly and a proportional `↑`
    /// on a button does not, which is exactly the pair of symptoms that got
    /// reported.
    ///
    /// A glyph added to the UI belongs in this list.
    #[test]
    fn every_ui_glyph_has_a_picture() {
        use crate::style::Theme;

        let mut ours = fonts(with_fallbacks(FontDefinitions::default()));

        let mut proportional: Vec<String> = ["⚙", "×"].iter().map(|s| (*s).to_owned()).collect();
        for theme in [Theme::System, Theme::Light, Theme::Dark] {
            proportional.push(theme.icon().to_owned());
        }
        // Both the parent button and the key legend draw these, and both ask
        // for the monospace face precisely because it is the one that has them.
        let monospace: Vec<String> = ["↑", "←", "→", "↓"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        for (family, glyphs) in [
            (FontFamily::Proportional, &proportional),
            (FontFamily::Monospace, &monospace),
        ] {
            let boxed = glyph(&mut ours, &family, NEVER_DRAWN);
            for text in glyphs {
                for c in text.chars() {
                    assert_ne!(
                        glyph(&mut ours, &family, c),
                        boxed,
                        "{family:?}: U+{:04X} {c:?} rasterises as the replacement box",
                        c as u32
                    );
                }
            }
        }
    }

    /// The 90 precomposed Vietnamese letters of Latin Extended Additional,
    /// U+1EA0–U+1EF9. `ề` (U+1EC1), the one from the screenshot, is in here.
    fn vietnamese() -> impl Iterator<Item = char> {
        (0x1EA0..=0x1EF9u32).filter_map(char::from_u32)
    }

    /// The heart of it: with egui's fonts alone, Vietnamese letters rasterise
    /// as the same box as an unassigned code point; with the fallbacks every
    /// one of them rasterises as itself.
    ///
    /// The `bare` half is what makes the `ours` half mean anything — remove
    /// the `chain.push` from [`with_fallbacks`] and the `ours` assertions fail
    /// on the very characters `bare` is proved to be drawing as boxes.
    ///
    /// Not every one of the 90 boxes in `bare`, which is why this counts them
    /// rather than demanding all: Hack has no U+1EC1 either, but it does have
    /// `ê` and a combining grave, and harfrust composes the two. That trick
    /// runs out — `ậ` needs a combining dot below, which Hack does not have —
    /// so the monospace family is genuinely broken for Vietnamese too, just
    /// not for every letter.
    #[test]
    fn the_fallback_turns_the_replacement_box_into_a_real_glyph() {
        let mut bare = fonts(FontDefinitions::default());
        let mut ours = fonts(with_fallbacks(FontDefinitions::default()));

        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let bare_box = glyph(&mut bare, &family, NEVER_DRAWN);
            let boxes: Vec<char> = vietnamese()
                .filter(|c| glyph(&mut bare, &family, *c) == bare_box)
                .collect();
            assert!(
                !boxes.is_empty(),
                "{family:?}: egui's own fonts draw all 90 Vietnamese letters, so \
                 there is no tofu left for the fallback to fix and this test \
                 proves nothing"
            );
            assert_ne!(
                glyph(&mut bare, &family, LATIN1),
                bare_box,
                "{family:?}: Latin-1 was never the problem"
            );

            let our_box = glyph(&mut ours, &family, NEVER_DRAWN);
            // Whitespace legitimately rasterises to nothing.
            for c in vietnamese()
                .chain(FILENAME.chars())
                .filter(|c| !c.is_whitespace())
            {
                let drawn = glyph(&mut ours, &family, c);
                assert_ne!(
                    drawn, our_box,
                    "{family:?}: U+{:04X} {c:?} is still the replacement box",
                    c as u32
                );
                assert_ne!(
                    drawn.0, drawn.1,
                    "{family:?}: U+{:04X} {c:?} drew nothing at all",
                    c as u32
                );
            }
            assert_ne!(
                glyph(&mut ours, &family, EXTENDED),
                glyph(&mut ours, &family, 'ê'),
                "{family:?}: `ề` must be its own glyph, not a bare `ê` with the \
                 grave dropped"
            );
        }
    }

    /// The file name from the bug report, so the exact string in the
    /// screenshot is on record as something the app can draw.
    const FILENAME: &str = "Thông cáo báo chí về FLC.pdf";

    /// A reviewer must see no difference on an English UI. If the fallbacks
    /// were prepended instead of appended, Noto would take over every ASCII
    /// character and the metrics would move.
    #[test]
    fn ascii_is_laid_out_exactly_as_it_was_before() {
        let mut bare = fonts(FontDefinitions::default());
        let mut ours = fonts(with_fallbacks(FontDefinitions::default()));

        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let font = egui::FontId::new(13.0, family.clone());
            let lay = |fonts: &mut egui::epaint::text::Fonts| {
                let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
                    "The quick brown fox jumps over the lazy dog 0123456789".to_owned(),
                    font.clone(),
                    egui::Color32::WHITE,
                );
                (
                    galley.rect,
                    galley.rows[0]
                        .row
                        .glyphs
                        .iter()
                        .map(|g| (g.chr, g.pos, g.advance_width))
                        .collect::<Vec<_>>(),
                )
            };
            assert_eq!(lay(&mut bare), lay(&mut ours), "{family:?} moved");
        }
    }
}
