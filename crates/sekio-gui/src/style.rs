//! The app's look: the theme the user asked for, the two palettes it resolves
//! to, the `egui::Style` built from one, and the mapping from the core IR's
//! styling (24-bit RGB spans from syntect) to egui's text formatting.
//!
//! Everything here is a pure function of a [`Palette`], so a frame can be
//! rendered — and asserted on — headlessly in either mode. Nothing reads a
//! global: the app holds one palette and passes `&Palette` down, which is what
//! makes "the same widget, both modes" testable at all.

use egui::style::{Selection, WidgetVisuals, Widgets};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Color32, CornerRadius, FontId, Margin, Stroke, TextWrapMode, Visuals};
use sekio_core::{CellKind, Span, StyledLine};

/// Font size used for every monospace surface (text, hexdump, listings).
pub const MONO_SIZE: f32 = 13.0;

/// Corner rounding, one number for the whole app.
///
/// Small on purpose: a preview window is a *document* surface, and a large
/// radius on a button next to a hexdump reads as a toy. Six points is enough to
/// look deliberate at 100% and still square-ish next to monospace text.
const RADIUS: CornerRadius = CornerRadius::same(6);

/// Wrap mode for surfaces that must never reflow (code, hex).
pub const NO_WRAP: TextWrapMode = TextWrapMode::Extend;

// ---------------------------------------------------------------------------
// The theme the user asked for
// ---------------------------------------------------------------------------

/// What the user asked for, as opposed to what is on screen.
///
/// `System` is the default because a preview window is summoned *over* whatever
/// the user is already doing: matching the desktop is the difference between a
/// popup and a flashbang. It is resolved fresh every frame (see
/// [`Theme::resolve`]), not once at startup, so switching the desktop to light
/// while sekio is open switches sekio too.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Theme {
    Dark,
    Light,
    #[default]
    System,
}

impl Theme {
    /// Every value the config file and `--theme` accept, in the order they are
    /// worth showing to a user.
    pub const NAMES: [&'static str; 3] = ["dark", "light", "system"];

    /// Parse a config value. `None` is an unusable value, which the caller
    /// turns into a warning and a fallback — never a failed startup.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "system" | "auto" => Some(Self::System),
            _ => None,
        }
    }

    /// The spelling this parses back from, for warnings and `--doctor`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }

    /// egui's own preference type. Handing egui the preference rather than a
    /// resolved mode is what makes "System" live: egui re-resolves it against
    /// the system theme at the start of every pass, and winit keeps that up to
    /// date from `WindowEvent::ThemeChanged`.
    pub fn preference(self) -> egui::ThemePreference {
        match self {
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
            Self::System => egui::ThemePreference::System,
        }
    }

    /// Which of the two palettes this means, given what the desktop says.
    ///
    /// `system` is `None` whenever the desktop will not answer — a session with
    /// no XDG desktop portal, an X11 WM that implements no colour-scheme
    /// setting, a headless test. Dark is the fallback there because it is what
    /// sekio has always looked like, and because a previewer that flashes white
    /// over a dark desktop is the worse of the two wrong answers.
    pub fn resolve(self, system: Option<egui::Theme>) -> egui::Theme {
        match self {
            Self::Dark => egui::Theme::Dark,
            Self::Light => egui::Theme::Light,
            Self::System => system.unwrap_or(egui::Theme::Dark),
        }
    }
}

impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// The palette
// ---------------------------------------------------------------------------

/// Every colour the app paints with, for one of the two modes.
///
/// A struct rather than the constants this used to be, because the same roles
/// have to exist twice and the call sites must not care which set they got. The
/// dark set is base16-ocean.dark, unchanged, so a sheet or a highlighted file
/// looks exactly as it always has; the light set is base16-ocean.light's greys
/// with the accents darkened until they actually read on a light surface (the
/// base16 accent slots are shared between the two variants, and pastels on
/// near-white are unreadable — every colour below was checked against the
/// surface it is painted on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Which mode this is. Carried so direction-dependent decisions — bolding,
    /// the syntect theme — can be made from the palette alone.
    pub theme: egui::Theme,

    // -- surfaces, darkest-to-lightest in dark mode and the reverse in light --
    /// Behind free-floating things: popups, menus, the drop overlay's frame.
    pub window: Color32,
    /// The chrome: header, footer and the browser pane.
    pub panel: Color32,
    /// The preview surface itself, raised one step off `panel`. This is also
    /// the background the syntect theme was designed against, which is why the
    /// dark one is base16-ocean.dark's base00 rather than egui's near-black.
    pub card: Color32,
    /// Recessed: text fields, and anything that should read as an inset well.
    pub sunken: Color32,
    /// Alternating rows in the metadata grid.
    pub stripe: Color32,

    // -- text --
    /// Ordinary body text.
    pub text: Color32,
    /// A rank *above* body text, which is what `RichText::strong()` paints in:
    /// the window title, the app name, a section heading. egui's bundled fonts
    /// have no bold face, so this contrast step is the only emphasis there is —
    /// if it equalled `text`, "strong" would be a no-op.
    pub strong: Color32,
    /// Secondary labels — counts, paths, key hints. The typographic second
    /// rank, and the reason the footer does not compete with the preview.
    pub dim: Color32,
    /// The rules between columns, and anything only just worth painting.
    /// Faint on purpose: a grid needs separation, not a border round every cell.
    pub faint: Color32,

    // -- interaction --
    /// Selection, focus and links. One colour, so "this is the thing you are
    /// pointing at" always looks the same.
    pub accent: Color32,
    /// Hairlines: panel separators, button outlines, the edge of a card.
    pub outline: Color32,
    /// A button at rest, and a disabled one.
    pub fill: Color32,
    /// The same button under the pointer. Deliberately a step, not a tint:
    /// hover that cannot be seen is hover that does not exist.
    pub hover: Color32,
    /// …and while it is held.
    pub press: Color32,

    // -- status --
    /// The sheet actually being previewed, in the sheet list.
    pub active: Color32,
    pub warn: Color32,
    /// "cannot preview" and friends, as chrome rather than as a cell.
    pub error: Color32,

    // -- table cells --
    //
    // `PreviewContent::Table` carries a `CellKind` per cell instead of a
    // colour, so the colour is the frontend's to pick. The dark set is the one
    // the CLI's spreadsheet output has always used, so a sheet in the GUI reads
    // the same as the same sheet in a terminal. They are written out here
    // rather than imported: core's palette is private to its spreadsheet
    // renderer, and the IR is deliberately colour-free.
    /// An ordinary string cell.
    pub cell_text: Color32,
    /// A numeric cell, deliberately distinct from text.
    pub cell_number: Color32,
    /// A boolean.
    pub cell_bool: Color32,
    /// A date, time or duration.
    pub cell_date: Color32,
    /// `#REF!` and friends — the one that has to be unmistakable.
    pub cell_error: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    /// base16-ocean.dark, which is also the syntect theme core highlights with
    /// by default — so highlighted code and the chrome around it come from one
    /// palette rather than two that happen to sit next to each other.
    pub const fn dark() -> Self {
        Self {
            theme: egui::Theme::Dark,
            window: rgb(0x1c2027),
            panel: rgb(0x232830),
            card: rgb(0x2b303b), // base00
            sunken: rgb(0x1f242c),
            stripe: rgb(0x323845),
            text: rgb(0xc0c5ce),   // base05
            strong: rgb(0xeff1f5), // base07
            dim: rgb(0x8fa1b3),    // base0D
            faint: rgb(0x65737e),  // base03
            accent: rgb(0x7aa2c8),
            outline: rgb(0x3b4351),
            fill: rgb(0x333b47),
            hover: rgb(0x3f4959),
            press: rgb(0x4b5668),
            active: rgb(0xa3be8c), // base0B
            warn: rgb(0xebcb8b),   // base0A
            error: rgb(0xe08a92),
            cell_text: rgb(0xc0c5ce),   // base05
            cell_number: rgb(0xd08770), // base09
            cell_bool: rgb(0xb48ead),   // base0E
            cell_date: rgb(0x96b5b4),   // base0C
            cell_error: rgb(0xbf616a),  // base08
        }
    }

    /// base16-ocean.light's surfaces and greys, with accents darkened to earn
    /// their contrast.
    ///
    /// The surface is base00 (`#eff1f5`) for the same reason the dark card is:
    /// it is the background base16-ocean.light's syntax colours were chosen
    /// against, so a light-mode source file sits on the ground it expects.
    /// Every text colour below clears 4.5:1 against that surface; the accents
    /// had to move to do it (base16 shares one accent row between its light and
    /// dark variants, and `#a3be8c` on near-white is a smudge).
    pub const fn light() -> Self {
        Self {
            theme: egui::Theme::Light,
            window: rgb(0xf7f9fb),
            panel: rgb(0xe1e6ee),
            card: rgb(0xeff1f5), // base00
            sunken: rgb(0xffffff),
            stripe: rgb(0xe4e8ef),
            text: rgb(0x343d46),   // base06 — 9.8:1
            strong: rgb(0x22282f), // a rank darker again
            dim: rgb(0x5b6672),    // 5.2:1, so an 11 px footer label still reads
            faint: rgb(0x949cab),  // as faint against white as base03 is on base00
            accent: rgb(0x2f6096), // 5.7:1
            outline: rgb(0xccd3de),
            fill: rgb(0xdfe4ec),
            hover: rgb(0xd2dae6),
            press: rgb(0xc3ccdb),
            active: rgb(0x46742c),      // 4.9:1
            warn: rgb(0x8a6100),        // 4.9:1
            error: rgb(0xa3232f),       // 6.5:1
            cell_text: rgb(0x4f5b66),   // base05 — 6.2:1
            cell_number: rgb(0x9c5426), // 5.0:1
            cell_bool: rgb(0x7d4e93),   // 5.5:1
            cell_date: rgb(0x2f6f6d),   // 5.1:1
            cell_error: rgb(0xa3232f),  // 6.5:1
        }
    }

    pub const fn for_theme(theme: egui::Theme) -> Self {
        match theme {
            egui::Theme::Dark => Self::dark(),
            egui::Theme::Light => Self::light(),
        }
    }

    /// The colour a cell is painted in, from what it holds.
    pub fn cell_color(&self, kind: CellKind) -> Color32 {
        match kind {
            CellKind::Text => self.cell_text,
            CellKind::Number => self.cell_number,
            CellKind::Bool => self.cell_bool,
            CellKind::Date => self.cell_date,
            CellKind::Error => self.cell_error,
        }
    }

    /// egui's bundled fonts ship no bold face and `TextFormat` has no weight
    /// axis, so bold is approximated by pushing the colour away from the
    /// background — toward white on a dark surface, toward black on a light one.
    ///
    /// The direction is the whole point: lifting toward white on a light
    /// background makes bold text *vanish*, which is exactly what a
    /// mode-blind version of this did.
    ///
    /// The shift is deliberately small. At 45% a syntect keyword colour like
    /// (180,142,173) came out (214,193,210) — visibly washed toward white, so
    /// highlighted code looked desaturated in the GUI while the CLI, where bold
    /// is a real ANSI attribute, kept the true colour. Emphasis is not worth
    /// losing the hue that carries the actual meaning.
    pub fn emphasize(&self, color: Color32) -> Color32 {
        brighten(color, self.theme)
    }

    /// The syntect theme core must highlight with so the code on screen belongs
    /// to the window around it. `None` means core's own default.
    ///
    /// A light window showing dark-theme code is the single most visible way
    /// this feature can look broken, and the colours arrive in the IR already
    /// baked — so this is a decision the *worker* has to act on, by rebuilding
    /// its `Previewer` (see `worker::Worker::set_theme`).
    pub fn syntect_theme(&self) -> Option<&'static str> {
        match self.theme {
            // Core's default already is base16-ocean.dark.
            egui::Theme::Dark => None,
            egui::Theme::Light => Some(LIGHT_SYNTECT_THEME),
        }
    }
}

/// The light counterpart of core's default `base16-ocean.dark`.
///
/// **Not `base16-ocean.light`**, which is the obvious choice and the wrong one.
/// The two ocean variants share their base08–base0F slots verbatim — a keyword
/// is `(180, 142, 173)` in both — so the "same hue in either mode" property
/// they look like they offer is really just dark-background hues reused on a
/// light one. Measured against the `card` each is painted on:
///
/// | theme | comment | keyword |
/// |---|---|---|
/// | `base16-ocean.dark` (what dark mode ships) | 2.71 | 4.67 |
/// | `base16-ocean.light` | **1.99** | **2.51** |
/// | `InspiredGitHub` | 2.57 | 6.24 |
/// | `Catppuccin Latte` | 3.49 | 4.79 |
///
/// Latte is the only candidate that holds dark mode's contrast profile instead
/// of falling below it, and its own base *is* [`Palette::light`]'s `card`, so
/// code sits on exactly the background it was drawn for. Syntax themes dim
/// comments on purpose, so 2.71 — what dark mode already ships and nobody has
/// complained about — is the bar here, not WCAG's 4.5.
pub const LIGHT_SYNTECT_THEME: &str = "Catppuccin Latte";

/// See [`Palette::emphasize`]; free-standing so a span mapper can bold without
/// a whole palette in hand.
pub fn brighten(color: Color32, theme: egui::Theme) -> Color32 {
    let shift = |c: u8| -> u8 {
        let c = c as u16;
        match theme {
            egui::Theme::Dark => (c + (255 - c) * 18 / 100) as u8,
            egui::Theme::Light => (c - c * 18 / 100) as u8,
        }
    };
    Color32::from_rgba_premultiplied(
        shift(color.r()),
        shift(color.g()),
        shift(color.b()),
        color.a(),
    )
}

// ---------------------------------------------------------------------------
// The egui style built from a palette
// ---------------------------------------------------------------------------

/// Register both palettes, without saying which to use.
///
/// Both, not just the active one, so the switch costs nothing: egui picks
/// between them itself at the start of every pass, which is what makes
/// `Theme::System` follow the desktop live rather than only at startup.
///
/// Split from [`install`] because the app builds its window before anything has
/// necessarily read a config: this half is safe to call unconditionally and
/// leaves whatever preference is already set alone. egui's own default
/// preference is "follow the system", which is also sekio's.
pub fn install_styles(ctx: &egui::Context) {
    ctx.set_style_of(egui::Theme::Dark, style_of(&Palette::dark()));
    ctx.set_style_of(egui::Theme::Light, style_of(&Palette::light()));
    // What "System" means when the desktop says nothing — see `Theme::resolve`,
    // which has to agree with this.
    ctx.options_mut(|opts| opts.fallback_theme = egui::Theme::Dark);
}

/// Register both palettes and say which the user asked for. This is the call
/// that turns a resolved `theme` setting into pixels; everything after it is
/// egui's own resolution, once per pass.
pub fn install(ctx: &egui::Context, theme: Theme) {
    install_styles(ctx);
    ctx.set_theme(theme.preference());
}

/// The whole `Style` for one palette.
pub fn style_of(palette: &Palette) -> egui::Style {
    let mut style = egui::Style {
        visuals: visuals_of(palette),
        ..Default::default()
    };
    // One rhythm everywhere. egui's defaults are (8, 3) and a 4×1 button pad,
    // which is why stock egui reads as cramped; these are still tight enough
    // that a hexdump row does not gain visible leading.
    style.spacing.item_spacing = egui::vec2(8.0, 4.0);
    style.spacing.button_padding = egui::vec2(9.0, 4.0);
    style.spacing.menu_margin = Margin::same(6);
    style.spacing.window_margin = Margin::same(8);
    style
}

/// `Visuals` for one palette, built from that mode's own base rather than by
/// poking at the dark one.
///
/// The base is egui's `Visuals::dark()`/`light()` for the matching mode and
/// then every colour the app actually shows is assigned. A struct literal
/// would be the purer thing, but `Visuals` has three dozen fields this app has
/// no opinion about (IME underlines, slider handle shape, numeric colour
/// space) and spelling them out would mean re-deciding them on every egui bump.
/// What matters is that neither mode is derived from the other.
fn visuals_of(palette: &Palette) -> Visuals {
    let Palette {
        theme,
        window,
        panel,
        card,
        sunken,
        stripe,
        text,
        strong,
        dim,
        accent,
        outline,
        fill,
        hover,
        press,
        warn,
        error,
        ..
    } = *palette;

    let mut visuals = theme.default_visuals();

    // The four states differ in fill *and* in outline, so a button under the
    // pointer is obvious without anything animating.
    visuals.widgets = Widgets {
        noninteractive: WidgetVisuals {
            bg_fill: panel,
            weak_bg_fill: panel,
            // Panel separators and `ui.separator()` both come from here: one
            // hairline, no shadowed groove.
            bg_stroke: Stroke::new(1.0, outline),
            corner_radius: RADIUS,
            // This is `Visuals::text_color()` — the app's body text.
            fg_stroke: Stroke::new(1.0, text),
            expansion: 0.0,
        },
        inactive: WidgetVisuals {
            bg_fill: fill,
            weak_bg_fill: fill,
            bg_stroke: Stroke::new(1.0, outline),
            corner_radius: RADIUS,
            fg_stroke: Stroke::new(1.0, text),
            expansion: 0.0,
        },
        hovered: WidgetVisuals {
            bg_fill: hover,
            weak_bg_fill: hover,
            bg_stroke: Stroke::new(1.0, accent),
            corner_radius: RADIUS,
            fg_stroke: Stroke::new(1.0, strong),
            expansion: 0.0,
        },
        // `active.fg_stroke` is also what `Visuals::strong_text_color` returns,
        // so this slot carries the whole app's emphasis, not just a pressed
        // button's label.
        active: WidgetVisuals {
            bg_fill: press,
            weak_bg_fill: press,
            bg_stroke: Stroke::new(1.0, accent),
            corner_radius: RADIUS,
            fg_stroke: Stroke::new(1.0, strong),
            expansion: 0.0,
        },
        open: WidgetVisuals {
            bg_fill: press,
            weak_bg_fill: press,
            bg_stroke: Stroke::new(1.0, outline),
            corner_radius: RADIUS,
            fg_stroke: Stroke::new(1.0, text),
            expansion: 0.0,
        },
    };

    // Selection is the accent at a weight that leaves the text on top of it
    // legible, rather than a block of solid colour with white on it.
    visuals.selection = Selection {
        bg_fill: accent.gamma_multiply(0.40),
        stroke: Stroke::new(1.0, text),
    };

    visuals.panel_fill = panel;
    visuals.window_fill = window;
    visuals.window_stroke = Stroke::new(1.0, outline);
    visuals.window_corner_radius = RADIUS;
    visuals.menu_corner_radius = RADIUS;
    visuals.extreme_bg_color = sunken;
    visuals.faint_bg_color = stripe;
    visuals.code_bg_color = card;
    visuals.hyperlink_color = accent;
    visuals.warn_fg_color = warn;
    visuals.error_fg_color = error;
    // The secondary rank, used by `ui.weak(..)` and by disabled text.
    visuals.weak_text_color = Some(dim);
    visuals.text_cursor.stroke = Stroke::new(2.0, accent);
    // Wide, soft and cheap: a hard drop shadow under a popup is the single most
    // dated thing egui does out of the box.
    visuals.window_shadow = shadow(theme);
    visuals.popup_shadow = shadow(theme);
    visuals
}

fn shadow(theme: egui::Theme) -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: match theme {
            egui::Theme::Dark => Color32::from_black_alpha(96),
            egui::Theme::Light => Color32::from_black_alpha(28),
        },
    }
}

// ---------------------------------------------------------------------------
// IR -> egui text
// ---------------------------------------------------------------------------

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

/// `Span` -> `TextFormat` in a monospace font.
pub fn span_format(span: &Span, palette: &Palette, size: f32) -> TextFormat {
    let mut color = span_color(span.fg, palette.text);
    if span.bold {
        color = palette.emphasize(color);
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
/// the job's hash, so re-showing the same job every frame is nearly free. The
/// palette is baked in here, which is why a mode switch drops the cached job
/// (see `app::SekioApp::poll_theme`) instead of being paid for on every frame.
pub fn text_job(lines: &[StyledLine], palette: &Palette, size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    // Code keeps its own line breaks; horizontal scrolling beats reflowing.
    job.wrap.max_width = f32::INFINITY;
    job.wrap.break_anywhere = false;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            job.append(
                "\n",
                0.0,
                TextFormat::simple(FontId::monospace(size), palette.text),
            );
        }
        for span in &line.spans {
            // syntect hands back trailing newlines inside spans; the job adds
            // its own separators, so strip them to avoid double spacing.
            let text = span.text.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                continue;
            }
            job.append(text, 0.0, span_format(span, palette, size));
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

    /// WCAG relative luminance, so a claim about contrast in this file is
    /// checked rather than asserted in a comment.
    fn luminance(color: Color32) -> f32 {
        let channel = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    // ---- the theme setting ----

    #[test]
    fn following_the_system_is_the_default() {
        assert_eq!(Theme::default(), Theme::System);
    }

    #[test]
    fn every_documented_theme_name_parses_back_to_itself() {
        for name in Theme::NAMES {
            let theme = Theme::parse(name).unwrap_or_else(|| panic!("{name} must parse"));
            assert_eq!(theme.as_str(), name);
        }
        // Case and stray whitespace are the user's business, not an error.
        assert_eq!(Theme::parse(" Dark "), Some(Theme::Dark));
        assert_eq!(Theme::parse("LIGHT"), Some(Theme::Light));
        // A value the program cannot use answers `None` rather than panicking,
        // which is what lets the config warn and fall through.
        assert_eq!(Theme::parse("solarized"), None);
        assert_eq!(Theme::parse(""), None);
    }

    #[test]
    fn an_explicit_theme_ignores_the_desktop_and_system_follows_it() {
        for desktop in [None, Some(egui::Theme::Dark), Some(egui::Theme::Light)] {
            assert_eq!(Theme::Dark.resolve(desktop), egui::Theme::Dark);
            assert_eq!(Theme::Light.resolve(desktop), egui::Theme::Light);
        }
        assert_eq!(
            Theme::System.resolve(Some(egui::Theme::Light)),
            egui::Theme::Light
        );
        assert_eq!(
            Theme::System.resolve(Some(egui::Theme::Dark)),
            egui::Theme::Dark
        );
        // A desktop that will not say — no portal, no display — is dark.
        assert_eq!(Theme::System.resolve(None), egui::Theme::Dark);
    }

    // ---- the palettes ----

    #[test]
    fn the_dark_palette_is_still_base16_ocean_dark() {
        // The values a dark-mode sheet has always been painted in. A change
        // here is a regression, not a redesign.
        let dark = Palette::dark();
        assert_eq!(dark.dim, Color32::from_rgb(143, 161, 179));
        assert_eq!(dark.active, Color32::from_rgb(0xa3, 0xbe, 0x8c));
        assert_eq!(dark.faint, Color32::from_rgb(0x65, 0x73, 0x7e));
        assert_eq!(dark.cell_text, Color32::from_rgb(0xc0, 0xc5, 0xce));
        assert_eq!(dark.cell_number, Color32::from_rgb(0xd0, 0x87, 0x70));
        assert_eq!(dark.cell_bool, Color32::from_rgb(0xb4, 0x8e, 0xad));
        assert_eq!(dark.cell_date, Color32::from_rgb(0x96, 0xb5, 0xb4));
        assert_eq!(dark.cell_error, Color32::from_rgb(0xbf, 0x61, 0x6a));
    }

    #[test]
    fn every_cell_kind_gets_its_own_colour_in_both_modes() {
        for palette in [Palette::dark(), Palette::light()] {
            let colors = [
                palette.cell_color(CellKind::Text),
                palette.cell_color(CellKind::Number),
                palette.cell_color(CellKind::Bool),
                palette.cell_color(CellKind::Date),
                palette.cell_color(CellKind::Error),
            ];
            for (i, a) in colors.iter().enumerate() {
                for b in &colors[i + 1..] {
                    assert_ne!(a, b, "two cell kinds share a colour: {colors:?}");
                }
            }
            // The one that has to be unmistakable.
            assert_eq!(palette.cell_color(CellKind::Error), palette.cell_error);
        }
    }

    /// The failure mode this palette exists to prevent: a light window painted
    /// in colours picked for a dark one.
    #[test]
    fn every_light_cell_colour_reads_on_the_light_surface() {
        let light = Palette::light();
        let on_card = |color| contrast(color, light.card);
        for (what, color) in [
            ("text", light.cell_text),
            ("number", light.cell_number),
            ("bool", light.cell_bool),
            ("date", light.cell_date),
            ("error", light.cell_error),
            ("body", light.text),
            ("dim", light.dim),
            ("accent", light.accent),
            ("active", light.active),
        ] {
            assert!(
                on_card(color) >= 4.5,
                "the light {what} colour is {:.2}:1 against the surface it is painted on",
                on_card(color)
            );
        }
    }

    #[test]
    fn the_dark_palette_reads_on_its_own_surface_too() {
        let dark = Palette::dark();
        for (what, color) in [
            ("text", dark.cell_text),
            ("number", dark.cell_number),
            ("bool", dark.cell_bool),
            ("date", dark.cell_date),
            ("body", dark.text),
            ("dim", dark.dim),
            ("accent", dark.accent),
            ("active", dark.active),
        ] {
            let ratio = contrast(color, dark.card);
            assert!(ratio >= 4.5, "the dark {what} colour is only {ratio:.2}:1");
        }
    }

    /// The surfaces have to be a *hierarchy*, or "raised card" is a claim with
    /// no pixels behind it.
    #[test]
    fn the_surfaces_are_ordered_and_distinct_in_both_modes() {
        for palette in [Palette::dark(), Palette::light()] {
            assert_ne!(palette.panel, palette.card);
            assert_ne!(palette.window, palette.card);
            assert_ne!(palette.fill, palette.hover, "hover must be visible");
            assert_ne!(palette.hover, palette.press);
            let step = contrast(palette.panel, palette.card);
            assert!(
                step > 1.0 && step < 1.6,
                "panel and card are {step:.2}:1 apart — a card, not a second window"
            );
        }
        // Dark really is darker and light really is lighter, whatever the hexes
        // say.
        assert!(luminance(Palette::dark().card) < luminance(Palette::light().card));
    }

    #[test]
    fn light_mode_asks_core_for_a_light_syntax_theme() {
        assert_eq!(Palette::dark().syntect_theme(), None, "core's own default");
        assert_eq!(
            Palette::light().syntect_theme(),
            Some(LIGHT_SYNTECT_THEME),
            "a light window must not show dark-theme code"
        );
        assert_eq!(Palette::for_theme(egui::Theme::Light), Palette::light());
        assert_eq!(Palette::for_theme(egui::Theme::Dark), Palette::dark());
    }

    // ---- the style built from a palette ----

    #[test]
    fn the_style_paints_body_text_and_separators_from_the_palette() {
        for palette in [Palette::dark(), Palette::light()] {
            let style = style_of(&palette);
            let visuals = &style.visuals;
            assert_eq!(visuals.text_color(), palette.text);
            // Emphasis has to be a step, or `RichText::strong()` paints
            // nothing at all: egui's fonts have no bold face to fall back on.
            assert_eq!(visuals.strong_text_color(), palette.strong);
            assert_ne!(visuals.strong_text_color(), visuals.text_color());
            assert!(
                contrast(palette.strong, palette.card) > contrast(palette.text, palette.card),
                "strong text must stand out from the surface more than body text"
            );
            assert_eq!(visuals.weak_text_color(), palette.dim);
            assert_eq!(visuals.panel_fill, palette.panel);
            assert_eq!(visuals.hyperlink_color, palette.accent);
            assert_eq!(visuals.error_fg_color, palette.error);
            assert_eq!(
                visuals.widgets.noninteractive.bg_stroke.color, palette.outline,
                "separators are a hairline in the palette's own outline colour"
            );
            assert_eq!(visuals.dark_mode, palette.theme == egui::Theme::Dark);
            // Hover has to differ from rest in fill *and* outline, or there is
            // no feedback at all.
            assert_ne!(
                visuals.widgets.hovered.bg_fill,
                visuals.widgets.inactive.bg_fill
            );
            assert_ne!(
                visuals.widgets.hovered.bg_stroke.color,
                visuals.widgets.inactive.bg_stroke.color
            );
            // One radius, everywhere.
            for widget in [
                &visuals.widgets.noninteractive,
                &visuals.widgets.inactive,
                &visuals.widgets.hovered,
                &visuals.widgets.active,
                &visuals.widgets.open,
            ] {
                assert_eq!(widget.corner_radius, RADIUS);
            }
            assert!(visuals.selection.bg_fill.a() > 0);
        }
    }

    // ---- IR -> egui ----

    #[test]
    fn rgb_spans_map_to_the_same_egui_color() {
        assert_eq!(
            span_color(Some((10, 20, 30)), Color32::RED),
            Color32::from_rgb(10, 20, 30)
        );
    }

    #[test]
    fn colorless_spans_use_the_palettes_body_colour() {
        assert_eq!(
            span_color(None, Color32::from_rgb(1, 2, 3)),
            Color32::from_rgb(1, 2, 3)
        );
        let light = Palette::light();
        let plain = span_format(&span("x", None, false, false), &light, MONO_SIZE);
        assert_eq!(plain.color, light.text);
    }

    #[test]
    fn bold_brightens_and_italic_sets_the_italics_flag() {
        let dark = Palette::dark();
        let base = span("x", Some((100, 100, 100)), false, false);
        let bold = span("x", Some((100, 100, 100)), true, false);
        let italic = span("x", Some((100, 100, 100)), false, true);

        let plain = span_format(&base, &dark, MONO_SIZE);
        let strong = span_format(&bold, &dark, MONO_SIZE);
        let slanted = span_format(&italic, &dark, MONO_SIZE);

        assert_eq!(plain.color, Color32::from_rgb(100, 100, 100));
        assert!(strong.color.r() > plain.color.r(), "bold must be brighter");
        assert!(!plain.italics);
        assert!(slanted.italics);
        assert_eq!(plain.font_id, FontId::monospace(MONO_SIZE));
    }

    /// The bug a mode-blind `brighten` has: on a light background, lifting
    /// toward white is how you make emphasised text disappear.
    #[test]
    fn bold_moves_away_from_the_background_in_both_modes() {
        let mid = Color32::from_rgb(100, 100, 100);
        assert!(brighten(mid, egui::Theme::Dark).r() > mid.r());
        assert!(brighten(mid, egui::Theme::Light).r() < mid.r());

        let light = Palette::light();
        let bold = span_format(&span("x", Some((100, 100, 100)), true, false), &light, 13.0);
        assert!(
            contrast(bold.color, light.card) > contrast(mid, light.card),
            "bold must gain contrast against the surface, not lose it"
        );
    }

    #[test]
    fn brighten_is_saturating_and_keeps_the_extremes_put() {
        assert_eq!(brighten(Color32::WHITE, egui::Theme::Dark), Color32::WHITE);
        assert_eq!(brighten(Color32::BLACK, egui::Theme::Light), Color32::BLACK);
        let dark = brighten(Color32::BLACK, egui::Theme::Dark);
        assert!(dark.r() > 0 && dark.r() < 255);
        let light = brighten(Color32::WHITE, egui::Theme::Light);
        assert!(light.r() > 0 && light.r() < 255);
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
        let job = text_job(&lines, &Palette::dark(), MONO_SIZE);
        assert_eq!(job.text, "fn main() {\n    let x = 1;");
        assert!(job.sections.len() >= 3, "each span keeps its own format");
        assert!(job.wrap.max_width.is_infinite(), "code must not reflow");
    }

    #[test]
    fn empty_document_produces_an_empty_job() {
        let job = text_job(&[], &Palette::dark(), MONO_SIZE);
        assert!(job.text.is_empty());
    }

    #[test]
    fn a_cell_is_laid_out_as_one_elided_row() {
        let cell_text = Palette::dark().cell_text;
        let job = cell_job("a very long note indeed", cell_text, 40.0);
        assert_eq!(job.wrap.max_rows, 1, "a wrapped cell breaks the row pitch");
        assert_eq!(job.wrap.overflow_character, Some('…'));
        assert_eq!(job.wrap.max_width, 40.0);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, cell_text);
        assert_eq!(
            job.sections[0].format.font_id,
            FontId::monospace(MONO_SIZE),
            "cells are monospace, or no column ever lines up"
        );
        // A zero-width column must not produce a zero or negative wrap width,
        // which epaint would treat as "fit nothing at all".
        assert!(cell_job("x", cell_text, 0.0).wrap.max_width > 0.0);
    }

    #[test]
    fn a_cell_containing_a_newline_is_flattened_rather_than_swallowed() {
        assert_eq!(one_line("two\nlines"), "two lines");
        assert_eq!(one_line("a\tb\r\nc"), "a b  c");
        // The common case is unchanged, accents and all.
        assert_eq!(one_line("Đứng lớp"), "Đứng lớp");
        assert_eq!(
            cell_job("two\nlines", Palette::dark().cell_text, 400.0).text,
            "two lines"
        );
    }

    #[test]
    fn mono_job_concatenates_runs_in_order() {
        let job = mono_job(
            &[("00000000  ", Palette::dark().dim), ("ff ", Color32::WHITE)],
            MONO_SIZE,
        );
        assert_eq!(job.text, "00000000  ff ");
        assert_eq!(job.sections.len(), 2);
    }
}
