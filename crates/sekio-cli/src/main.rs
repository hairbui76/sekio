use std::io::{BufWriter, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use sekio_core::{CancelToken, Preview, PreviewContent, PreviewOptions, Previewer};

/// sekio — quick preview of any file, straight to your terminal.
/// Also works as a preview backend for fzf / lf / yazi.
#[derive(Parser)]
// The crate is `sekio-cli` but the binary is `sekio`; without this, `--version`
// and usage errors would name a crate the user never typed.
#[command(name = "sekio", version, about)]
struct Args {
    /// File or directory to preview
    #[arg(required_unless_present = "list_themes")]
    path: Option<PathBuf>,

    /// Max lines of text output
    #[arg(long, default_value_t = 200)]
    lines: usize,

    /// Output width in terminal columns; defaults to the terminal width
    ///
    /// Governs both the image rendering and the column layout of table
    /// previews (spreadsheets). Preview panes are narrower than the terminal,
    /// so pass the pane width — which is exactly what the fzf/lf/yazi recipes
    /// in docs/integration.md already do.
    #[arg(long)]
    width: Option<u32>,

    /// Disable ANSI colors
    #[arg(long, conflicts_with = "color")]
    no_color: bool,

    /// Force ANSI colors even when stdout is a pipe (fzf/lf preview panes)
    #[arg(long)]
    color: bool,

    /// Syntax theme for highlighted text (see --list-themes)
    #[arg(long)]
    theme: Option<String>,

    /// List the available syntax themes and exit
    #[arg(long, exclusive = true)]
    list_themes: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_themes {
        for name in Previewer::theme_names() {
            if name == Previewer::DEFAULT_THEME {
                println!("{name}  (default)");
            } else {
                println!("{name}");
            }
        }
        return Ok(());
    }

    let color = args.color || (!args.no_color && std::io::stdout().is_terminal());
    // One width for the whole output: the same number that scales an image is
    // the number a table has to lay its columns out inside.
    let width = resolve_width(args.width, term_width());

    let opts = PreviewOptions {
        max_lines: args.lines,
        text_width: Some(width as usize),
        ..Default::default()
    };

    let previewer = match &args.theme {
        Some(name) => Previewer::with_theme(name).ok_or_else(|| {
            anyhow::anyhow!("unknown theme {name:?}; run --list-themes to see the options")
        })?,
        None => Previewer::new(),
    };
    // `required_unless_present` guarantees this is Some once past --list-themes.
    let path = args
        .path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no path given"))?;
    let preview = previewer.preview(path, &opts, &CancelToken::new())?;

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    // A closed pipe (fzf/lf/head stopped reading) is a normal exit, not an error.
    if let Err(e) = paint(&mut out, &preview, color, width).and_then(|_| Ok(out.flush()?)) {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
        }
        return Err(e);
    }
    Ok(())
}

fn paint(out: &mut impl Write, preview: &Preview, color: bool, width: u32) -> Result<()> {
    match &preview.content {
        PreviewContent::Text { lines, language } => {
            for line in lines {
                for span in &line.spans {
                    if color {
                        if let Some((r, g, b)) = span.fg {
                            write!(out, "\x1b[38;2;{r};{g};{b}m")?;
                        }
                        if span.bold {
                            write!(out, "\x1b[1m")?;
                        }
                        if span.italic {
                            write!(out, "\x1b[3m")?;
                        }
                    }
                    write!(out, "{}", span.text)?;
                    if color {
                        write!(out, "\x1b[0m")?;
                    }
                }
                writeln!(out)?;
            }
            if preview.truncated {
                writeln!(out, "── truncated ({language}) ──")?;
            }
        }
        PreviewContent::Image {
            image,
            original_width,
            original_height,
            format,
            fields,
        } => {
            paint_halfblocks(out, image, width, color)?;
            writeln!(out, "{format} · {original_width}×{original_height}")?;
            paint_fields(out, fields, color)?;
        }
        PreviewContent::Listing { entries } => {
            for e in entries {
                let marker = if e.is_dir { "/" } else { "" };
                match e.size {
                    Some(size) => writeln!(out, "{:>10}  {}{marker}", human_size(size), e.name)?,
                    None => writeln!(out, "{:>10}  {}{marker}", "", e.name)?,
                }
            }
            if preview.truncated {
                writeln!(out, "── more entries not shown ──")?;
            }
        }
        PreviewContent::Metadata { fields, thumbnail } => {
            if let Some(thumb) = thumbnail {
                paint_halfblocks(out, thumb, width.min(40), color)?;
            }
            paint_fields(out, fields, color)?;
        }
        PreviewContent::HexDump {
            data,
            file_size,
            mime,
        } => {
            let kind = mime.as_deref().unwrap_or("binary");
            writeln!(out, "{kind} · {}", human_size(*file_size))?;
            for (i, chunk) in data.chunks(16).enumerate() {
                write!(out, "{:08x}  ", i * 16)?;
                for j in 0..16 {
                    match chunk.get(j) {
                        Some(b) => write!(out, "{b:02x} ")?,
                        None => write!(out, "   ")?,
                    }
                    if j == 7 {
                        write!(out, " ")?;
                    }
                }
                write!(out, " |")?;
                for b in chunk {
                    let c = if b.is_ascii_graphic() || *b == b' ' {
                        *b as char
                    } else {
                        '.'
                    };
                    write!(out, "{c}")?;
                }
                writeln!(out, "|")?;
            }
            if preview.truncated {
                writeln!(out, "── truncated ──")?;
            }
        }
    }
    Ok(())
}

/// Render an RGBA image as ▀ halfblocks: one column per pixel, two image rows
/// per terminal row (upper pixel = fg, lower pixel = bg). Works in any
/// truecolor terminal on Linux and Windows Terminal alike.
fn paint_halfblocks(
    out: &mut impl Write,
    img: &image::RgbaImage,
    cols: u32,
    color: bool,
) -> Result<()> {
    if !color {
        writeln!(out, "(image preview requires a color terminal)")?;
        return Ok(());
    }
    let img = if img.width() > cols {
        image::imageops::resize(
            img,
            cols,
            (img.height() * cols / img.width()).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img.clone()
    };

    let blend = |p: &image::Rgba<u8>| -> (u8, u8, u8) {
        // Blend alpha onto a dark checker-free background.
        let a = p[3] as u16;
        let bg = 30u16;
        let mix = |c: u8| ((c as u16 * a + bg * (255 - a)) / 255) as u8;
        (mix(p[0]), mix(p[1]), mix(p[2]))
    };

    for y in (0..img.height()).step_by(2) {
        for x in 0..img.width() {
            let (tr, tg, tb) = blend(img.get_pixel(x, y));
            if y + 1 < img.height() {
                let (br, bg_, bb) = blend(img.get_pixel(x, y + 1));
                write!(
                    out,
                    "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg_};{bb}m\u{2580}"
                )?;
            } else {
                write!(out, "\x1b[38;2;{tr};{tg};{tb}m\u{2580}")?;
            }
        }
        writeln!(out, "\x1b[0m")?;
    }
    Ok(())
}

/// Right-aligned key column, dimmed, with values in the default color.
fn paint_fields(out: &mut impl Write, fields: &[sekio_core::MetaField], color: bool) -> Result<()> {
    let key_width = fields.iter().map(|f| f.key.len()).max().unwrap_or(0);
    for field in fields {
        if color {
            writeln!(
                out,
                "\x1b[38;2;143;161;179m{:>key_width$}\x1b[0m  {}",
                field.key, field.value
            )?;
        } else {
            writeln!(out, "{:>key_width$}  {}", field.key, field.value)?;
        }
    }
    Ok(())
}

/// Columns the terminal actually has, or `None` when there is no terminal to
/// ask (a cron job, a `sekio x.xlsx > out.txt` with no tty anywhere).
///
/// `$COLUMNS` alone is not enough now that this number decides the table
/// layout too: it is a shell *variable*, not an exported one, so a child
/// process almost never sees it. crossterm asks the tty directly and falls back
/// to `$COLUMNS` only when it cannot.
fn term_width() -> Option<u32> {
    if let Ok((columns, _)) = crossterm::terminal::size() {
        if columns > 0 {
            return Some(u32::from(columns));
        }
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok())
}

/// The one width the whole preview is laid out for.
///
/// `--width` wins when it is given — that is the pane width the fzf/lf/yazi
/// recipes pass, and a preview pane is narrower than its terminal. Otherwise
/// the terminal's own width, and 80 when there is no terminal to ask.
///
/// Pure so the precedence is testable without a tty; the floor keeps the
/// halfblock resizer off a zero-width image.
fn resolve_width(explicit: Option<u32>, terminal: Option<u32>) -> u32 {
    const FALLBACK: u32 = 80;
    explicit.or(terminal).unwrap_or(FALLBACK).max(2)
}

fn human_size(bytes: u64) -> String {
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

    #[test]
    fn the_clap_definition_is_well_formed() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    /// `--width` is the pane width the integration recipes pass, and it now
    /// governs the table layout as well as the image scaling — so it has to
    /// beat the terminal's own width, which is bigger than the pane.
    #[test]
    fn an_explicit_width_beats_the_terminal() {
        assert_eq!(resolve_width(Some(62), Some(200)), 62);
        assert_eq!(resolve_width(Some(62), None), 62);
    }

    #[test]
    fn without_the_flag_the_terminal_decides() {
        assert_eq!(resolve_width(None, Some(200)), 200);
        // Nothing to ask (no tty, no $COLUMNS): the classic default.
        assert_eq!(resolve_width(None, None), 80);
    }

    /// A zero or one column width would make the halfblock resizer produce a
    /// zero-width image; core clamps its own layout separately.
    #[test]
    fn absurd_widths_are_floored() {
        assert_eq!(resolve_width(Some(0), None), 2);
        assert_eq!(resolve_width(None, Some(1)), 2);
    }

    /// The flag reaches core as a hint, so the same number lays the table out.
    #[test]
    fn the_width_becomes_the_layout_hint() {
        let width = resolve_width(Some(140), None);
        let opts = PreviewOptions {
            text_width: Some(width as usize),
            ..Default::default()
        };
        assert_eq!(opts.line_width(), 140);
    }
}
