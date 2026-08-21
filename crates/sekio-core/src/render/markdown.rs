//! Markdown renderer.
//!
//! This renders Markdown *for reading*, not as highlighted source: headings
//! become bold coloured lines, emphasis becomes italic text, links show their
//! label with the URL trailing in dim parens, quotes grow a `│ ` gutter. The
//! output is the same `PreviewContent::Text` IR the syntect path produces, so
//! every frontend paints it without knowing markdown exists.
//!
//! Feature-gated inside the module (see `render/mod.rs`): with `markdown` off,
//! `render` still exists and returns `PreviewError::Format`, and the dispatcher
//! degrades to the hexdump.

use std::path::Path;

use crate::{CancelToken, Preview, PreviewError, PreviewOptions};

#[cfg(feature = "markdown")]
use crate::{PreviewContent, Span, StyledLine};

/// Palette, harmonised with the syntect theme used by the text renderer
/// (base16-ocean.dark) so a markdown preview and a source preview sit next to
/// each other without clashing. Comments name the base16 slot.
#[cfg(feature = "markdown")]
mod palette {
    pub type Rgb = (u8, u8, u8);

    /// Body text. base05
    pub const TEXT: Rgb = (0xc0, 0xc5, 0xce);
    /// Quoted body text — a step back from `TEXT`. base04
    pub const MUTED: Rgb = (0xa7, 0xad, 0xba);
    /// URLs, raw HTML, code-fence info strings. base03
    pub const DIM: Rgb = (0x65, 0x73, 0x7e);
    /// Horizontal rules and table gutters. base02
    pub const RULE: Rgb = (0x4f, 0x5b, 0x66);
    /// Inline code and fenced code blocks. base09
    pub const CODE: Rgb = (0xd0, 0x87, 0x70);
    /// Link labels. base0D
    pub const LINK: Rgb = (0x8f, 0xa1, 0xb3);
    /// List bullets and ordinals. base0B
    pub const MARKER: Rgb = (0xa3, 0xbe, 0x8c);
    /// One colour per heading level, h1..h6.
    pub const HEADING: [Rgb; 6] = [
        (0xb4, 0x8e, 0xad), // base0E
        (0x8f, 0xa1, 0xb3), // base0D
        (0x96, 0xb5, 0xb4), // base0C
        (0xa3, 0xbe, 0x8c), // base0B
        (0xeb, 0xcb, 0x8b), // base0A
        (0xbf, 0x61, 0x6a), // base08
    ];
}

/// Poll the cancel token every this many nodes/lines of work.
#[cfg(feature = "markdown")]
const CANCEL_INTERVAL: usize = 64;
/// Depth cap on the AST walk: recursion on hostile input must not blow the
/// stack, and nobody reads 32-deep nesting in a preview pane anyway.
#[cfg(feature = "markdown")]
const MAX_DEPTH: usize = 32;
/// Columns of `─` drawn for a thematic break.
#[cfg(feature = "markdown")]
const RULE_WIDTH: usize = 40;
/// Spaces a fenced/indented code block is inset by.
#[cfg(feature = "markdown")]
const CODE_INDENT: usize = 2;

#[cfg(feature = "markdown")]
pub fn render(
    path: &Path,
    head: Vec<u8>,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    use std::io::{BufReader, Read};

    let file_size = std::fs::metadata(path)?.len();
    let byte_truncated = (head.len() as u64) < file_size && head.len() >= opts.max_bytes;

    // The detection head may be smaller than max_bytes; re-read up to the cap.
    // Never past it: a 4 GB "markdown" file must not be parsed whole.
    let source = if (head.len() as u64) < file_size.min(opts.max_bytes as u64) {
        let mut buf = Vec::with_capacity(opts.max_bytes);
        let file = std::fs::File::open(path)?;
        BufReader::new(file)
            .take(opts.max_bytes as u64)
            .read_to_end(&mut buf)?;
        crate::render::text::decode(&buf, crate::detect::Encoding::Utf8)
    } else {
        crate::render::text::decode(&head, crate::detect::Encoding::Utf8)
    };
    cancel.check()?;

    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    let (lines, line_truncated) = build(source, opts, cancel)?;

    Ok(Preview {
        content: PreviewContent::Text {
            lines,
            language: "Markdown".to_string(),
        },
        truncated: byte_truncated || line_truncated,
    })
}

#[cfg(not(feature = "markdown"))]
pub fn render(
    _path: &Path,
    _head: Vec<u8>,
    _opts: &PreviewOptions,
    _cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    Err(PreviewError::Format(
        "markdown support not compiled in".into(),
    ))
}

/// Parse and walk. A panic anywhere in the parser is turned into a `Format`
/// error so a malformed document degrades to the hexdump instead of taking the
/// process down; cancellation is passed through untouched.
#[cfg(feature = "markdown")]
fn build(
    source: &str,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<(Vec<StyledLine>, bool), PreviewError> {
    let mut ctx = Ctx::new(opts, cancel);
    let walked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let arena = comrak::Arena::new();
        let mut options = comrak::Options::default();
        // GFM-flavoured reading: these are parser options, not cargo features.
        options.extension.strikethrough = true;
        options.extension.table = true;
        options.extension.tasklist = true;
        options.extension.autolink = true;
        options.extension.footnotes = true;
        let root = comrak::parse_document(&arena, source, &options);
        ctx.block(root, 0)
    }));

    match walked {
        Ok(Ok(())) => {
            ctx.flush();
            Ok((ctx.lines, ctx.truncated))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(PreviewError::Format("malformed markdown".into())),
    }
}

/// Style carried down the inline tree; children add to it (emphasis inside a
/// heading is bold *and* italic) rather than replacing it.
#[cfg(feature = "markdown")]
#[derive(Clone, Copy)]
struct Sty {
    fg: Option<palette::Rgb>,
    bold: bool,
    italic: bool,
}

#[cfg(feature = "markdown")]
impl Sty {
    fn new(fg: palette::Rgb) -> Self {
        Self {
            fg: Some(fg),
            bold: false,
            italic: false,
        }
    }
    fn plain() -> Self {
        Self {
            fg: None,
            bold: false,
            italic: false,
        }
    }
    fn fg(mut self, fg: palette::Rgb) -> Self {
        self.fg = Some(fg);
        self
    }
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
}

#[cfg(feature = "markdown")]
fn span(text: impl Into<String>, sty: Sty) -> Span {
    Span {
        text: text.into(),
        fg: sty.fg,
        bold: sty.bold,
        italic: sty.italic,
    }
}

/// Line-assembling walker. Blocks push lines; inlines push spans onto the line
/// currently being built. Every line starts with the block prefix (quote bars,
/// indent, a pending list marker) so wrapping state never leaks between blocks.
#[cfg(feature = "markdown")]
struct Ctx<'a> {
    opts: &'a PreviewOptions,
    cancel: &'a CancelToken,
    lines: Vec<StyledLine>,
    /// Spans of the line under construction.
    cur: Vec<Span>,
    /// Whether `cur` has already been given its prefix.
    open: bool,
    quote: usize,
    indent: usize,
    list_depth: usize,
    /// Marker for the next line opened (a list bullet or ordinal).
    marker: Option<Span>,
    truncated: bool,
    work: usize,
    last_blank: bool,
}

#[cfg(feature = "markdown")]
impl<'a> Ctx<'a> {
    fn new(opts: &'a PreviewOptions, cancel: &'a CancelToken) -> Self {
        Self {
            opts,
            cancel,
            lines: Vec::new(),
            cur: Vec::new(),
            open: false,
            quote: 0,
            indent: 0,
            list_depth: 0,
            marker: None,
            truncated: false,
            work: 0,
            last_blank: true,
        }
    }

    fn full(&self) -> bool {
        self.lines.len() >= self.opts.max_lines
    }

    fn tick(&mut self) -> Result<(), PreviewError> {
        self.work += 1;
        if self.work.is_multiple_of(CANCEL_INTERVAL) {
            self.cancel.check()?;
        }
        Ok(())
    }

    /// Body style: quoted text reads a shade back from ordinary prose.
    fn body(&self) -> Sty {
        if self.quote > 0 {
            Sty::new(palette::MUTED)
        } else {
            Sty::new(palette::TEXT)
        }
    }

    fn open_line(&mut self) {
        if self.open {
            return;
        }
        self.open = true;
        for _ in 0..self.quote {
            self.cur.push(span("│ ", Sty::new(palette::DIM)));
        }
        match self.marker.take() {
            Some(m) => {
                // `indent` already includes the marker's width so continuation
                // lines line up under the text, not under the bullet.
                let pad = self.indent.saturating_sub(m.text.chars().count());
                if pad > 0 {
                    self.cur.push(span(" ".repeat(pad), Sty::plain()));
                }
                self.cur.push(m);
            }
            None => {
                if self.indent > 0 {
                    self.cur.push(span(" ".repeat(self.indent), Sty::plain()));
                }
            }
        }
    }

    /// Append text to the current line. `\r` is dropped here so CRLF input
    /// never leaves a stray carriage return inside a span.
    fn text(&mut self, s: &str, sty: Sty) {
        if s.is_empty() || self.full() {
            return;
        }
        let cleaned: String = if s.contains('\r') {
            s.chars().filter(|c| *c != '\r').collect()
        } else {
            s.to_string()
        };
        if cleaned.is_empty() {
            return;
        }
        self.open_line();
        self.cur.push(span(cleaned, sty));
    }

    fn finish_line(&mut self) {
        if self.full() {
            self.truncated = true;
            self.cur.clear();
            self.open = false;
            return;
        }
        let spans = std::mem::take(&mut self.cur);
        self.open = false;
        self.last_blank = spans.iter().all(|s| s.text.trim().is_empty());
        self.lines.push(StyledLine { spans });
    }

    /// A blank separator line. Inside a quote it keeps the gutter so the block
    /// reads as one quotation.
    fn blank_line(&mut self) {
        if self.full() {
            self.truncated = true;
            return;
        }
        let mut spans = Vec::new();
        for _ in 0..self.quote {
            spans.push(span("│", Sty::new(palette::DIM)));
        }
        self.last_blank = true;
        self.lines.push(StyledLine { spans });
    }

    /// Separate this block from the previous one. Suppressed while a list
    /// marker is pending: the item's first line belongs next to its bullet.
    fn blank_between(&mut self) {
        if self.lines.is_empty() || self.last_blank || self.marker.is_some() {
            return;
        }
        if self.open {
            self.finish_line();
            return;
        }
        self.blank_line();
    }

    fn flush(&mut self) {
        if self.open {
            self.finish_line();
        }
    }

    fn children(&mut self, node: Node<'_>, depth: usize) -> Result<(), PreviewError> {
        for child in node.children() {
            self.block(child, depth)?;
        }
        Ok(())
    }

    fn block(&mut self, node: Node<'_>, depth: usize) -> Result<(), PreviewError> {
        use comrak::nodes::NodeValue;

        self.tick()?;
        if self.full() {
            // Something is left to render but the cap already bit.
            self.truncated = true;
            return Ok(());
        }
        if depth > MAX_DEPTH {
            return Ok(());
        }

        match &node.data.borrow().value {
            NodeValue::Document => self.children(node, depth + 1)?,

            NodeValue::Heading(h) => {
                self.blank_between();
                let level = (h.level.clamp(1, 6) - 1) as usize;
                let sty = Sty::new(palette::HEADING[level]).bold();
                self.inlines(node, sty, depth + 1)?;
                self.finish_line();
            }

            NodeValue::Paragraph => {
                self.blank_between();
                let sty = self.body();
                self.inlines(node, sty, depth + 1)?;
                self.finish_line();
            }

            NodeValue::CodeBlock(cb) => {
                self.blank_between();
                let info = cb.info.trim();
                if !info.is_empty() {
                    self.text(info, Sty::new(palette::DIM).italic());
                    self.finish_line();
                }
                self.indent += CODE_INDENT;
                // Content is verbatim: the indent is its own span so the code
                // span itself still holds exactly the source line.
                let literal = cb.literal.strip_suffix('\n').unwrap_or(&cb.literal);
                for line in literal.split('\n') {
                    self.tick()?;
                    if self.full() {
                        self.truncated = true;
                        break;
                    }
                    self.text(line, Sty::new(palette::CODE));
                    self.finish_line();
                }
                self.indent -= CODE_INDENT;
            }

            NodeValue::BlockQuote | NodeValue::MultilineBlockQuote(_) | NodeValue::Alert(_) => {
                self.blank_between();
                self.quote += 1;
                self.children(node, depth + 1)?;
                self.quote -= 1;
            }

            NodeValue::List(list) => {
                // A nested list continues its parent item; only a top-level
                // list is separated from the block above it.
                if self.list_depth == 0 {
                    self.blank_between();
                }
                self.list_depth += 1;
                let mut ordinal = list.start.max(1);
                for (i, item) in node.children().enumerate() {
                    self.tick()?;
                    if self.full() {
                        self.truncated = true;
                        break;
                    }
                    if i > 0 && !list.tight {
                        self.blank_between();
                    }
                    let marker = self.item_marker(item, list, &mut ordinal);
                    let width = marker.chars().count();
                    self.marker = Some(span(marker, Sty::new(palette::MARKER)));
                    self.indent += width;
                    self.children(item, depth + 1)?;
                    self.indent -= width;
                    // Dropped if the item produced nothing at all.
                    self.marker = None;
                }
                self.list_depth -= 1;
            }

            NodeValue::ThematicBreak => {
                self.blank_between();
                self.text(&"─".repeat(RULE_WIDTH), Sty::new(palette::RULE));
                self.finish_line();
            }

            NodeValue::Table(_) => {
                self.blank_between();
                self.children(node, depth + 1)?;
            }

            NodeValue::TableRow(header) => {
                let mut sty = self.body();
                if *header {
                    sty = sty.bold();
                }
                for (i, cell) in node.children().enumerate() {
                    if i > 0 {
                        self.text(" │ ", Sty::new(palette::RULE));
                    }
                    self.inlines(cell, sty, depth + 1)?;
                }
                self.finish_line();
            }

            NodeValue::FootnoteDefinition(fd) => {
                self.blank_between();
                self.marker = Some(span(format!("[^{}] ", fd.name), Sty::new(palette::MARKER)));
                self.children(node, depth + 1)?;
                self.marker = None;
            }

            NodeValue::HtmlBlock(hb) => {
                self.blank_between();
                self.raw_lines(&hb.literal, Sty::new(palette::DIM))?;
            }

            NodeValue::FrontMatter(fm) => {
                self.raw_lines(fm, Sty::new(palette::DIM))?;
            }

            // Containers we have no special shape for (description lists,
            // task items, extension blocks): keep their content, drop the box.
            _ => self.children(node, depth + 1)?,
        }
        Ok(())
    }

    /// Bullet or ordinal for one list item. Task items keep their checkbox.
    fn item_marker(
        &self,
        item: Node<'_>,
        list: &comrak::nodes::NodeList,
        ordinal: &mut usize,
    ) -> String {
        use comrak::nodes::{ListDelimType, ListType, NodeValue};

        if let NodeValue::TaskItem(task) = &item.data.borrow().value {
            return if task.symbol.is_some() {
                "[x] ".to_string()
            } else {
                "[ ] ".to_string()
            };
        }
        match list.list_type {
            ListType::Ordered => {
                let delim = match list.delimiter {
                    ListDelimType::Paren => ')',
                    ListDelimType::Period => '.',
                };
                let marker = format!("{}{} ", ordinal, delim);
                *ordinal = ordinal.saturating_add(1);
                marker
            }
            // Alternate so nested lists stay readable without colour.
            ListType::Bullet => {
                if self.list_depth % 2 == 1 {
                    "• ".to_string()
                } else {
                    "- ".to_string()
                }
            }
        }
    }

    /// Emit pre-formatted text one line per source line (CRLF tolerant).
    fn raw_lines(&mut self, literal: &str, sty: Sty) -> Result<(), PreviewError> {
        let literal = literal.strip_suffix('\n').unwrap_or(literal);
        for line in literal.split('\n') {
            self.tick()?;
            if self.full() {
                self.truncated = true;
                break;
            }
            self.text(line, sty);
            self.finish_line();
        }
        Ok(())
    }

    fn inlines(&mut self, node: Node<'_>, sty: Sty, depth: usize) -> Result<(), PreviewError> {
        for child in node.children() {
            self.inline(child, sty, depth)?;
        }
        Ok(())
    }

    fn inline(&mut self, node: Node<'_>, sty: Sty, depth: usize) -> Result<(), PreviewError> {
        use comrak::nodes::NodeValue;

        self.tick()?;
        if self.full() {
            self.truncated = true;
            return Ok(());
        }
        if depth > MAX_DEPTH {
            return Ok(());
        }

        match &node.data.borrow().value {
            NodeValue::Text(t) => self.text(t, sty),
            NodeValue::Code(c) => self.text(&c.literal, Sty::new(palette::CODE)),
            NodeValue::Math(m) => self.text(&m.literal, Sty::new(palette::CODE)),
            // A preview pane is not a paragraph reflower: soft breaks stay
            // breaks, so the preview mirrors the file's own line structure.
            NodeValue::SoftBreak | NodeValue::LineBreak => self.finish_line(),
            NodeValue::Emph => self.inlines(node, sty.italic(), depth + 1)?,
            NodeValue::Strong => self.inlines(node, sty.bold(), depth + 1)?,
            NodeValue::Strikethrough => {
                // No strike attribute in the IR; dim it instead.
                self.inlines(node, sty.fg(palette::DIM), depth + 1)?
            }
            NodeValue::Link(l) => {
                self.inlines(node, sty.fg(palette::LINK), depth + 1)?;
                self.link_url(&l.url);
            }
            NodeValue::Image(l) => {
                self.text("[image] ", Sty::new(palette::DIM));
                self.inlines(node, sty.fg(palette::LINK), depth + 1)?;
                self.link_url(&l.url);
            }
            NodeValue::WikiLink(w) => {
                self.inlines(node, sty.fg(palette::LINK), depth + 1)?;
                self.link_url(&w.url);
            }
            NodeValue::FootnoteReference(fr) => {
                self.text(&format!("[^{}]", fr.name), Sty::new(palette::DIM));
            }
            NodeValue::HtmlInline(h) => self.text(h, Sty::new(palette::DIM)),
            NodeValue::Raw(r) => self.text(r, sty),
            // Superscript, underline, spoilers, escapes: no IR attribute of
            // their own, so keep the text and the surrounding style.
            _ => self.inlines(node, sty, depth + 1)?,
        }
        Ok(())
    }

    fn link_url(&mut self, url: &str) {
        if url.is_empty() {
            return;
        }
        self.text(&format!(" ({})", url), Sty::new(palette::DIM));
    }
}

#[cfg(feature = "markdown")]
type Node<'a> = &'a comrak::nodes::AstNode<'a>;

#[cfg(all(test, feature = "markdown"))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Scratch file next to the test binary — no tempfile dependency, and it
    /// works the same on Windows.
    fn write_md(name: &str, body: &str) -> PathBuf {
        let mut dir = std::env::current_exe().expect("test exe path");
        dir.pop(); // deps/
        dir.pop(); // debug/
        dir.push("markdown-render-tests");
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write scratch file");
        path
    }

    fn preview(path: &std::path::Path, opts: &PreviewOptions) -> Preview {
        let head = std::fs::read(path).expect("read back");
        render(path, head, opts, &CancelToken::new()).expect("render")
    }

    fn lines(p: &Preview) -> &[StyledLine] {
        match &p.content {
            PreviewContent::Text { lines, language } => {
                assert_eq!(language, "Markdown");
                lines
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    fn plain(line: &StyledLine) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn headings_are_bold_and_coloured_by_level() {
        let path = write_md("headings.md", "# Title\n\n## Section\n\ntext\n");
        let p = preview(&path, &PreviewOptions::default());
        let lines = lines(&p);

        let h1 = lines.iter().find(|l| plain(l) == "Title").expect("h1");
        assert!(h1.spans.iter().all(|s| s.bold));
        assert_eq!(h1.spans[0].fg, Some(palette::HEADING[0]));

        let h2 = lines.iter().find(|l| plain(l) == "Section").expect("h2");
        assert!(h2.spans.iter().all(|s| s.bold));
        assert_eq!(h2.spans[0].fg, Some(palette::HEADING[1]));

        let body = lines.iter().find(|l| plain(l) == "text").expect("body");
        assert!(!body.spans[0].bold);
    }

    #[test]
    fn emphasis_and_inline_code_carry_style() {
        let path = write_md("inline.md", "*soft* **hard** `code` [docs](http://x.y)\n");
        let p = preview(&path, &PreviewOptions::default());
        let spans: Vec<_> = lines(&p).iter().flat_map(|l| l.spans.iter()).collect();

        let soft = spans.iter().find(|s| s.text == "soft").expect("emph");
        assert!(soft.italic && !soft.bold);
        let hard = spans.iter().find(|s| s.text == "hard").expect("strong");
        assert!(hard.bold);
        let code = spans.iter().find(|s| s.text == "code").expect("code span");
        assert_eq!(code.fg, Some(palette::CODE));
        let link = spans.iter().find(|s| s.text == "docs").expect("link text");
        assert_eq!(link.fg, Some(palette::LINK));
        let url = spans
            .iter()
            .find(|s| s.text == " (http://x.y)")
            .expect("dim url");
        assert_eq!(url.fg, Some(palette::DIM));
    }

    #[test]
    fn fenced_code_block_is_verbatim() {
        let src = "intro\n\n```rust\nfn main() {\n    let x = *y;  // **not** bold\n}\n```\n";
        let path = write_md("code.md", src);
        let p = preview(&path, &PreviewOptions::default());
        let texts: Vec<String> = lines(&p)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.text.clone())
            .collect();

        assert!(texts.iter().any(|t| t == "fn main() {"));
        assert!(texts
            .iter()
            .any(|t| t == "    let x = *y;  // **not** bold"));
        assert!(texts.iter().any(|t| t == "}"));
        // And the info string is kept as a caption.
        assert!(texts.iter().any(|t| t == "rust"));
    }

    #[test]
    fn list_items_get_bullets_and_quotes_get_a_gutter() {
        let path = write_md("blocks.md", "- one\n- two\n\n> quoted\n\n---\n");
        let p = preview(&path, &PreviewOptions::default());
        let rendered: Vec<String> = lines(&p).iter().map(plain).collect();

        assert!(rendered.iter().any(|l| l == "• one"));
        assert!(rendered.iter().any(|l| l == "• two"));
        assert!(rendered.iter().any(|l| l == "│ quoted"));
        assert!(rendered.iter().any(|l| l.starts_with("────")));
    }

    #[test]
    fn max_lines_truncates_and_flags() {
        let mut src = String::new();
        for i in 0..200 {
            src.push_str(&format!("line {}\n\n", i));
        }
        let path = write_md("long.md", &src);
        let opts = PreviewOptions {
            max_lines: 10,
            ..PreviewOptions::default()
        };
        let p = preview(&path, &opts);
        assert!(p.truncated);
        assert_eq!(lines(&p).len(), 10);
    }

    #[test]
    fn crlf_leaves_no_carriage_returns() {
        let src = "# Title\r\n\r\nsome *text* here\r\n\r\n```\r\nraw line\r\n```\r\n\r\n- item\r\n";
        let path = write_md("crlf.md", src);
        let p = preview(&path, &PreviewOptions::default());
        let rendered = lines(&p);
        assert!(rendered
            .iter()
            .flat_map(|l| l.spans.iter())
            .all(|s| !s.text.contains('\r')));
        let joined: Vec<String> = rendered.iter().map(plain).collect();
        assert!(joined.iter().any(|l| l == "Title"));
        assert!(joined.iter().any(|l| l == "  raw line"));
    }

    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let mut src = String::new();
        for i in 0..500 {
            src.push_str(&format!("para {}\n\n", i));
        }
        let path = write_md("cancel.md", &src);
        let cancel = CancelToken::new();
        cancel.cancel();
        let head = std::fs::read(&path).expect("read back");
        let err =
            render(&path, head, &PreviewOptions::default(), &cancel).expect_err("should cancel");
        assert!(matches!(err, PreviewError::Cancelled));
    }
}
