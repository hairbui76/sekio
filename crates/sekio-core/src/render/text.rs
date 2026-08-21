use std::io::{BufReader, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::detect::Encoding;
use crate::{CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, Span, StyledLine};

/// Used when no theme is named, and as the fallback for an unknown name.
pub const DEFAULT_THEME: &str = "base16-ocean.dark";

/// Wall-clock ceiling on syntax highlighting for one preview. Generous enough
/// that normal source files finish styled, tight enough that a pathological
/// grammar can't make the preview feel broken.
const HIGHLIGHT_BUDGET: Duration = Duration::from_millis(40);

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        Self::with_theme(DEFAULT_THEME).unwrap_or_else(|| Self {
            syntaxes: load_syntaxes(),
            theme: Theme::default(),
        })
    }

    /// Build a highlighter using the named theme, or `None` if no such theme
    /// exists. Names come from `theme_names`.
    pub fn with_theme(name: &str) -> Option<Self> {
        Some(Self {
            syntaxes: load_syntaxes(),
            theme: find_theme(name)?,
        })
    }

    /// Every theme name accepted by `with_theme`, sorted.
    pub fn theme_names() -> Vec<String> {
        let mut names: Vec<String> = ThemeSet::load_defaults().themes.into_keys().collect();
        names.extend(
            two_face::theme::EmbeddedLazyThemeSet::theme_names()
                .iter()
                .map(|t| t.as_name().to_string()),
        );
        names.sort();
        names.dedup();
        names
    }
}

/// syntect's bundled set covers the Sublime defaults but is missing formats a
/// developer previews constantly — TOML, TypeScript, Dockerfile. `two-face`
/// carries bat's extended set, built against the same pure-Rust fancy-regex
/// backend we use everywhere else.
fn load_syntaxes() -> SyntaxSet {
    two_face::syntax::extra_newlines()
}

fn find_theme(name: &str) -> Option<Theme> {
    if let Some(theme) = ThemeSet::load_defaults().themes.remove(name) {
        return Some(theme);
    }
    let extra = two_face::theme::extra();
    two_face::theme::EmbeddedLazyThemeSet::theme_names()
        .iter()
        .find(|t| t.as_name() == name)
        .map(|t| extra.get(*t).clone())
}

/// Decode a byte sample using the detected encoding. Legacy encodings go
/// through `encoding_rs`; unknown labels fall back to lossy UTF-8.
pub(crate) fn decode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Encoding::Legacy(label) => match encoding_rs::Encoding::for_label(label.as_bytes()) {
            Some(enc) => enc.decode(bytes).0.into_owned(),
            None => String::from_utf8_lossy(bytes).into_owned(),
        },
    }
}

pub fn render(
    hl: &Highlighter,
    path: &Path,
    head: Vec<u8>,
    encoding: Encoding,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    let file_size = std::fs::metadata(path)?.len();
    let byte_truncated = (head.len() as u64) < file_size && head.len() >= opts.max_bytes;

    // The detection head may be smaller than max_bytes; re-read up to the cap.
    let text = if (head.len() as u64) < file_size.min(opts.max_bytes as u64) {
        let mut buf = Vec::with_capacity(opts.max_bytes);
        let file = std::fs::File::open(path)?;
        BufReader::new(file)
            .take(opts.max_bytes as u64)
            .read_to_end(&mut buf)?;
        decode(&buf, encoding)
    } else {
        decode(&head, encoding)
    };

    let syntax = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| hl.syntaxes.find_syntax_by_extension(e))
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|l| hl.syntaxes.find_syntax_by_first_line(l))
        })
        .unwrap_or_else(|| hl.syntaxes.find_syntax_plain_text());

    let mut language = syntax.name.clone();
    let mut highlighter = HighlightLines::new(syntax, &hl.theme);
    let mut lines = Vec::new();
    let mut line_truncated = false;
    let started = Instant::now();
    let mut gave_up = false;

    for (i, line) in text.lines().enumerate() {
        if i >= opts.max_lines {
            line_truncated = true;
            break;
        }
        if i % 64 == 0 {
            cancel.check()?;
            // Some syntax definitions are pathologically slow on some inputs
            // (the "log" grammar costs ~250us/line, ~700x plain text). Caps on
            // bytes and lines don't bound that, so bound the time too: past the
            // budget, emit the rest unstyled rather than stalling the preview.
            if !gave_up && started.elapsed() > HIGHLIGHT_BUDGET {
                gave_up = true;
            }
        }

        if gave_up {
            lines.push(StyledLine {
                spans: vec![Span {
                    text: line.to_string(),
                    fg: None,
                    bold: false,
                    italic: false,
                }],
            });
            continue;
        }

        let regions = highlighter
            .highlight_line(line, &hl.syntaxes)
            .unwrap_or_else(|_| vec![(Style::default(), line)]);
        lines.push(StyledLine {
            spans: regions
                .into_iter()
                .map(|(style, part)| Span {
                    text: part.to_string(),
                    fg: Some((style.foreground.r, style.foreground.g, style.foreground.b)),
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                })
                .collect(),
        });
    }

    if gave_up {
        // Be honest about it rather than silently showing half-styled text.
        language.push_str(" (highlighting timed out)");
    }

    Ok(Preview {
        content: PreviewContent::Text { lines, language },
        truncated: byte_truncated || line_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, body: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sekio-text-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create dir");
            let path = dir.join(name);
            std::fs::write(&path, body).expect("write");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn render_path(file: &TempFile, max_lines: usize) -> Preview {
        let hl = Highlighter::new();
        let opts = PreviewOptions {
            max_lines,
            ..Default::default()
        };
        render(
            &hl,
            &file.0,
            Vec::new(),
            Encoding::Utf8,
            &opts,
            &CancelToken::new(),
        )
        .expect("render")
    }

    #[test]
    fn extended_syntaxes_are_available() {
        // These three are absent from syntect's bundled set and are the reason
        // two-face is a dependency. Losing them is a silent quality regression.
        let syntaxes = load_syntaxes();
        for name in ["TOML", "TypeScript", "Dockerfile"] {
            assert!(
                syntaxes.find_syntax_by_name(name).is_some(),
                "missing syntax: {name}"
            );
        }
    }

    #[test]
    fn known_theme_loads_and_unknown_theme_is_rejected() {
        assert!(Highlighter::with_theme(DEFAULT_THEME).is_some());
        assert!(Highlighter::with_theme("no-such-theme-exists").is_none());
        let names = Highlighter::theme_names();
        assert!(names.contains(&DEFAULT_THEME.to_string()));
        assert!(names.len() > 1, "expected several themes, got {names:?}");
    }

    #[test]
    fn pathological_syntax_stays_within_a_sane_time_bound() {
        // The "log" grammar costs ~250us/line, so 2000 lines would be ~500ms
        // without the budget. Assert a loose ceiling: this is here to catch a
        // catastrophic regression, not to measure performance precisely.
        let body = "2026-08-21T10:00:00Z ERROR failed to connect to 10.0.0.1:5432\n".repeat(2000);
        let file = TempFile::new("huge.log", &body);

        let start = Instant::now();
        let preview = render_path(&file, 2000);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "highlighting took {elapsed:?}, budget is {HIGHLIGHT_BUDGET:?}"
        );
        match preview.content {
            PreviewContent::Text { lines, .. } => assert_eq!(lines.len(), 2000),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn cheap_input_is_fully_highlighted_and_unlabelled() {
        let file = TempFile::new("small.rs", "fn main() {\n    let x = 1;\n}\n");
        let preview = render_path(&file, 100);
        match preview.content {
            PreviewContent::Text { lines, language } => {
                assert_eq!(language, "Rust", "a small file must not hit the budget");
                assert!(lines[0].spans.iter().any(|s| s.fg.is_some()));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
