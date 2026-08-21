//! Word and PowerPoint previews: docx and pptx.
//!
//! Both are OOXML — a zip of XML parts — so this reads the one part that holds
//! the prose (`word/document.xml`, `ppt/slides/slideN.xml`) with `zip` and
//! pulls the text runs out with `quick-xml`. No DOM is built: the parser is
//! driven event by event straight off the inflating stream, so hitting
//! `max_lines` stops the *decompression*, not just the output.
//!
//! Legacy binary `.doc`/`.ppt` (OLE compound files, not OOXML) are declined —
//! see `render` for why, and for the shape a future LibreOffice shell-out would
//! take.
//!
//! Feature-gated inside the module (see `render/mod.rs`): with `office` off,
//! `render` still exists and returns `PreviewError::Format`, and the dispatcher
//! degrades to the hexdump.

#[cfg(feature = "office")]
pub use imp::render;

#[cfg(feature = "office")]
mod imp {
    use std::fs::File;
    use std::io::{BufReader, Read, Take};
    use std::path::Path;

    use quick_xml::events::Event;
    use quick_xml::Reader as XmlReader;

    use crate::{
        CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, Span, StyledLine,
    };

    /// Palette, harmonised with `render/markdown.rs` and the base16-ocean.dark
    /// syntect theme, so prose from a docx and prose from a `.md` look alike.
    pub(super) mod palette {
        pub type Rgb = (u8, u8, u8);

        /// Body text. base05
        pub const TEXT: Rgb = (0xc0, 0xc5, 0xce);
        /// Slide separators and the "no text" note. base03
        pub const DIM: Rgb = (0x65, 0x73, 0x7e);
        /// One colour per heading level, h1..h6 — same order as markdown's.
        pub const HEADING: [Rgb; 6] = [
            (0xb4, 0x8e, 0xad), // base0E
            (0x8f, 0xa1, 0xb3), // base0D
            (0x96, 0xb5, 0xb4), // base0C
            (0xa3, 0xbe, 0x8c), // base0B
            (0xeb, 0xcb, 0x8b), // base0A
            (0xbf, 0x61, 0x6a), // base08
        ];
    }

    /// Poll the cancel token every this many paragraphs.
    const CANCEL_INTERVAL: usize = 64;
    /// Hard bound on what we inflate out of any single zip member. A preview
    /// needs a few hundred lines; this is the zip-bomb guard, so a member that
    /// claims to expand to 4 GB simply stops here.
    const MAX_PART_BYTES: u64 = 4 * 1024 * 1024;
    /// Characters kept from one paragraph.
    const MAX_LINE_CHARS: usize = 4096;
    /// A `<w:tab/>` becomes this many spaces — frontends paint spans, not tab
    /// stops, so a literal tab would land wherever the terminal felt like.
    const TAB: &str = "    ";

    pub fn render(
        path: &Path,
        format: &str,
        _head: Vec<u8>,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        cancel.check()?;

        // `_head` is the detection sample and is deliberately unused: a zip's
        // central directory lives at the end of the file, so a 64 KB prefix
        // can't be opened as an archive.

        // Third-party XML and zip data: an unwind must degrade to the hexdump
        // rather than take the process down. Cancellation passes through.
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
            "docx" => docx(path, opts, cancel),
            "pptx" => pptx(path, opts, cancel),

            // Legacy binary Word/PowerPoint, and OOXML that turned out to be an
            // encrypted OLE package. There is no pure-Rust reader for the
            // BIFF-era record streams, and guessing would print mojibake, so we
            // decline and let the dispatcher fall back to the hexdump.
            //
            // The future option is a LibreOffice shell-out behind its own
            // feature (`soffice --headless --convert-to txt`), following the
            // pattern `render/video.rs` already uses for ffmpeg: spawn detached
            // from our stdio, poll `try_wait` against a deadline while watching
            // the cancel token, kill *and reap* the child on either. That is a
            // change of its own, not something to smuggle in here.
            "doc" | "ppt" => Err(PreviewError::Format(format!(
                "legacy or encrypted binary Office document ({format}): no pure-Rust reader"
            ))),

            other => Err(PreviewError::Format(format!(
                "no document reader for {other}"
            ))),
        }));

        match built {
            Ok(result) => result,
            Err(_) => Err(PreviewError::Format("malformed document".into())),
        }
    }

    // ----------------------------------------------------------------- docx

    fn docx(
        path: &Path,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        let mut archive = open_zip(path)?;
        let mut out = Out::new(opts);
        {
            let part = archive
                .by_name("word/document.xml")
                .map_err(|e| PreviewError::Format(format!("docx: {e}")))?;
            walk(part, &mut out, cancel)?;
        }
        out.finish("Word Document")
    }

    // ----------------------------------------------------------------- pptx

    fn pptx(
        path: &Path,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        let mut archive = open_zip(path)?;

        let mut slides: Vec<(u32, String)> = archive
            .file_names()
            .filter_map(|name| slide_number(name).map(|n| (n, name.to_string())))
            .collect();
        if slides.is_empty() {
            return Err(PreviewError::Format("pptx: no slides".into()));
        }
        // Numeric, not lexicographic: slide2 comes before slide10.
        slides.sort_by_key(|(n, _)| *n);

        let mut out = Out::new(opts);
        if slides.len() > opts.max_entries {
            slides.truncate(opts.max_entries);
            out.truncated = true;
        }

        for (number, name) in slides {
            cancel.check()?;
            if out.full() {
                out.truncated = true;
                break;
            }
            out.separator(format!("── Slide {number} ──"));
            let part = archive
                .by_name(&name)
                .map_err(|e| PreviewError::Format(format!("pptx: {e}")))?;
            walk(part, &mut out, cancel)?;
        }

        out.finish("PowerPoint Presentation")
    }

    /// `ppt/slides/slide12.xml` -> `Some(12)`. Everything else — the `_rels`
    /// sidecars, slide layouts, masters — is `None`. Zip entry names are always
    /// `/`-separated, on Windows as much as on Linux, so this needs no `Path`.
    fn slide_number(name: &str) -> Option<u32> {
        let digits = name
            .strip_prefix("ppt/slides/slide")?
            .strip_suffix(".xml")?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }

    // -------------------------------------------------------------- xml walk

    fn open_zip(path: &Path) -> Result<zip::ZipArchive<BufReader<File>>, PreviewError> {
        // Parses the central directory only; part data stays on disk until
        // `by_name` asks for it.
        zip::ZipArchive::new(BufReader::new(File::open(path)?))
            .map_err(|e| PreviewError::Format(format!("not a readable OOXML package: {e}")))
    }

    /// Pull the text out of one OOXML part.
    ///
    /// docx and pptx use different namespaces (`w:` and `a:`) but the same
    /// local names — `p` for a paragraph, `t` for a text run, `br`/`tab` for
    /// breaks — so one walker serves both. Matching on the local name also
    /// means a package that binds those namespaces to different prefixes still
    /// reads correctly.
    fn walk<R: Read>(part: R, out: &mut Out, cancel: &CancelToken) -> Result<(), PreviewError> {
        let mut reader = xml_reader(part);
        // The reader borrows this across calls; reused so one buffer serves the
        // whole part.
        let mut buf = Vec::new();
        // Depth inside a `*Pr` properties element (`w:pPr`, `a:rPr`, ...).
        // Everything in there describes formatting, not content — and `w:tabs`
        // in a `w:pPr` is full of `w:tab` elements that are tab *stops*, not
        // tabs to print.
        let mut props = 0usize;
        let mut in_text = false;
        let mut work = 0usize;
        let produced_before = out.lines.len();

        loop {
            buf.clear();
            let event = match reader.read_event_into(&mut buf) {
                Ok(event) => event,
                // Malformed or cut short. If we already have prose, keep it and
                // say so; if we have nothing, this really is a broken file.
                Err(e) => {
                    if out.lines.len() > produced_before {
                        out.truncated = true;
                        break;
                    }
                    return Err(PreviewError::Format(format!("xml: {e}")));
                }
            };

            match event {
                Event::Eof => break,

                Event::Start(e) => {
                    let raw = e.local_name();
                    let name = raw.as_ref();
                    // `w:pStyle` lives *inside* `w:pPr`, so it is read before
                    // the properties gate rejects everything else in there.
                    if name == b"pStyle" {
                        apply_style(&e, out);
                    } else if props == 0 {
                        match name {
                            b"p" => out.open_paragraph(),
                            b"t" => in_text = true,
                            b"br" => out.line_break(),
                            b"tab" => out.push(TAB),
                            _ => {}
                        }
                    }
                    if is_props(name) {
                        props += 1;
                    }
                }

                // An empty element opens and closes at once, so it never moves
                // the properties depth.
                Event::Empty(e) => {
                    let raw = e.local_name();
                    let name = raw.as_ref();
                    if name == b"pStyle" {
                        apply_style(&e, out);
                    } else if props == 0 {
                        match name {
                            b"p" => {
                                out.open_paragraph();
                                out.close_paragraph();
                            }
                            b"br" => out.line_break(),
                            b"tab" => out.push(TAB),
                            _ => {}
                        }
                    }
                }

                Event::End(e) => {
                    let raw = e.local_name();
                    let name = raw.as_ref();
                    if is_props(name) {
                        props = props.saturating_sub(1);
                    } else if name == b"t" {
                        in_text = false;
                    } else if props == 0 && name == b"p" {
                        out.close_paragraph();
                        work += 1;
                        if work.is_multiple_of(CANCEL_INTERVAL) {
                            cancel.check()?;
                        }
                        if out.full() {
                            out.truncated = true;
                            break;
                        }
                    }
                }

                Event::Text(e) if in_text => {
                    let text = e.decode().map_err(decode_err)?;
                    out.push(&text);
                }
                Event::CData(e) if in_text => {
                    let text = e.decode().map_err(decode_err)?;
                    out.push(&text);
                }
                // quick-xml reports `&amp;` / `&#233;` as events of their own
                // rather than folding them into the surrounding text.
                Event::GeneralRef(e) if in_text => {
                    if let Some(text) = resolve_ref(&e) {
                        out.push(&text);
                    }
                }

                _ => {}
            }
        }

        Ok(())
    }

    fn decode_err(e: quick_xml::encoding::EncodingError) -> PreviewError {
        PreviewError::Format(format!("xml: {e}"))
    }

    fn apply_style(e: &quick_xml::events::BytesStart<'_>, out: &mut Out<'_>) {
        if let Some(level) = style_attr(e).as_deref().and_then(heading_level) {
            out.set_heading(level);
        }
    }

    /// OOXML names every formatting-properties element `<something>Pr`
    /// (`w:pPr`, `w:rPr`, `a:bodyPr`, ...). What is inside them describes
    /// layout, not content — and `w:tabs` in a `w:pPr` is full of `w:tab`
    /// elements that are tab *stops*, not tabs to print.
    fn is_props(name: &[u8]) -> bool {
        name.ends_with(b"Pr")
    }

    /// Build the parser over a *bounded* slice of the member. `take` is the
    /// zip-bomb guard: a member whose header claims 4 GB simply ends here, and
    /// because the reader pulls lazily, stopping early stops the inflater too.
    fn xml_reader<R: Read>(part: R) -> XmlReader<BufReader<Take<R>>> {
        let mut reader = XmlReader::from_reader(BufReader::new(part.take(MAX_PART_BYTES)));
        let config = reader.config_mut();
        // A preview is not a validator: mismatched tags in a file some other
        // tool wrote should still show their text.
        config.check_end_names = false;
        config.allow_unmatched_ends = true;
        reader
    }

    fn style_attr(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
        for name in [b"w:val".as_slice(), b"val".as_slice()] {
            if let Ok(Some(attr)) = e.try_get_attribute(name) {
                // Style ids are ASCII names like `Heading1` — no entities to
                // unescape, and a lossy decode can never fail on a hostile file.
                return Some(String::from_utf8_lossy(&attr.value).into_owned());
            }
        }
        None
    }

    /// `Heading1`, `heading 2`, `Title`, `Subtitle` — the style ids Word and
    /// LibreOffice actually write.
    fn heading_level(value: &str) -> Option<u8> {
        let lower = value.trim().to_ascii_lowercase();
        match lower.as_str() {
            "title" => return Some(1),
            "subtitle" => return Some(2),
            _ => {}
        }
        let digits = lower
            .strip_prefix("heading")?
            .trim_start_matches([' ', '-', '_']);
        digits.parse::<u8>().ok().map(|n| n.clamp(1, 6))
    }

    fn resolve_ref(e: &quick_xml::events::BytesRef<'_>) -> Option<String> {
        if let Ok(Some(c)) = e.resolve_char_ref() {
            return Some(c.to_string());
        }
        match e.decode().ok()?.as_ref() {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" => Some("'".to_string()),
            // An entity we cannot resolve is dropped rather than guessed at.
            _ => None,
        }
    }

    // -------------------------------------------------------------- output

    /// Line assembler. A paragraph is styled as a whole (body or heading), so
    /// one span per line is all the IR needs.
    struct Out<'a> {
        opts: &'a PreviewOptions,
        lines: Vec<StyledLine>,
        current: String,
        heading: Option<u8>,
        open: bool,
        truncated: bool,
    }

    impl<'a> Out<'a> {
        fn new(opts: &'a PreviewOptions) -> Self {
            Self {
                opts,
                lines: Vec::new(),
                current: String::new(),
                heading: None,
                open: false,
                truncated: false,
            }
        }

        fn full(&self) -> bool {
            self.lines.len() >= self.opts.max_lines
        }

        fn open_paragraph(&mut self) {
            self.open = true;
        }

        fn set_heading(&mut self, level: u8) {
            self.heading = Some(level);
        }

        fn push(&mut self, text: &str) {
            if self.full() {
                return;
            }
            for c in text.chars() {
                if self.current.chars().count() >= MAX_LINE_CHARS {
                    self.truncated = true;
                    return;
                }
                // A stray newline inside a run would desynchronise the line
                // structure from the document's own paragraphs.
                self.current.push(match c {
                    '\n' | '\r' => ' ',
                    other => other,
                });
            }
        }

        /// `<w:br/>`: a break *within* a paragraph. The line ends but the
        /// paragraph's style carries on.
        fn line_break(&mut self) {
            self.emit();
            self.open = true;
        }

        fn close_paragraph(&mut self) {
            if self.open || !self.current.is_empty() {
                self.emit();
            }
            self.open = false;
            self.heading = None;
        }

        fn emit(&mut self) {
            if self.full() {
                self.truncated = true;
                self.current.clear();
                return;
            }
            let text = std::mem::take(&mut self.current);
            let trimmed = text.trim_end();
            // Collapse runs of blank paragraphs: documents are full of them and
            // a preview pane is not.
            if trimmed.is_empty() && self.lines.last().is_some_and(is_blank) {
                return;
            }
            let style = match self.heading {
                Some(level) => {
                    let index = (level.clamp(1, 6) - 1) as usize;
                    Sty {
                        fg: Some(palette::HEADING[index]),
                        bold: true,
                    }
                }
                None => Sty {
                    fg: Some(palette::TEXT),
                    bold: false,
                },
            };
            let spans = if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![span(trimmed.to_string(), style)]
            };
            self.lines.push(StyledLine { spans });
        }

        /// The `── Slide 3 ──` marker between pptx slides.
        fn separator(&mut self, text: String) {
            self.close_paragraph();
            if self.full() {
                self.truncated = true;
                return;
            }
            if !self.lines.is_empty() && !self.lines.last().is_some_and(is_blank) {
                self.lines.push(StyledLine::default());
            }
            if self.full() {
                self.truncated = true;
                return;
            }
            self.lines.push(StyledLine {
                spans: vec![span(
                    text,
                    Sty {
                        fg: Some(palette::DIM),
                        bold: true,
                    },
                )],
            });
        }

        fn finish(mut self, language: &str) -> Result<Preview, PreviewError> {
            self.close_paragraph();
            if self.lines.iter().all(|l| l.spans.is_empty()) {
                // A valid package that simply holds no prose (a deck of images,
                // say). Say so rather than erroring into a hexdump, which would
                // tell the reader even less.
                self.lines = vec![StyledLine {
                    spans: vec![span(
                        "(no text in this document)",
                        Sty {
                            fg: Some(palette::DIM),
                            bold: false,
                        },
                    )],
                }];
            }
            if self.lines.len() > self.opts.max_lines {
                self.lines.truncate(self.opts.max_lines);
                self.truncated = true;
            }
            Ok(Preview {
                content: PreviewContent::Text {
                    lines: self.lines,
                    language: language.to_string(),
                },
                truncated: self.truncated,
            })
        }
    }

    fn is_blank(line: &StyledLine) -> bool {
        line.spans.iter().all(|s| s.text.trim().is_empty())
    }

    #[derive(Clone, Copy)]
    struct Sty {
        fg: Option<palette::Rgb>,
        bold: bool,
    }

    fn span(text: impl Into<String>, sty: Sty) -> Span {
        Span {
            text: text.into(),
            fg: sty.fg,
            bold: sty.bold,
            italic: false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn slide_numbers_are_parsed_only_from_slide_parts() {
            assert_eq!(slide_number("ppt/slides/slide1.xml"), Some(1));
            assert_eq!(slide_number("ppt/slides/slide10.xml"), Some(10));
            assert_eq!(slide_number("ppt/slides/_rels/slide1.xml.rels"), None);
            assert_eq!(slide_number("ppt/slideLayouts/slideLayout1.xml"), None);
            assert_eq!(slide_number("ppt/slides/slide.xml"), None);
        }

        #[test]
        fn heading_styles_are_recognised() {
            assert_eq!(heading_level("Heading1"), Some(1));
            assert_eq!(heading_level("heading 3"), Some(3));
            assert_eq!(heading_level("Title"), Some(1));
            assert_eq!(heading_level("Normal"), None);
            assert_eq!(heading_level("Heading99"), Some(6));
        }

        #[test]
        fn only_properties_elements_open_a_properties_scope() {
            assert!(is_props(b"pPr"));
            assert!(is_props(b"rPr"));
            assert!(is_props(b"bodyPr"));
            assert!(!is_props(b"p"));
            assert!(!is_props(b"tabs"));
            assert!(!is_props(b"t"));
        }
    }
}

#[cfg(not(feature = "office"))]
pub fn render(
    _path: &std::path::Path,
    _format: &str,
    _head: Vec<u8>,
    _opts: &crate::PreviewOptions,
    _cancel: &crate::CancelToken,
) -> Result<crate::Preview, crate::PreviewError> {
    Err(crate::PreviewError::Format(
        "office support not compiled in".into(),
    ))
}

#[cfg(all(test, feature = "office"))]
mod tests {
    use super::render;
    use crate::{CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, StyledLine};

    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_file(name: &str, bytes: &[u8]) -> TempFile {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("sekio-doc-{}-{n}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write fixture");
        TempFile(path)
    }

    fn zip_of(parts: &[(&str, String)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            writer.start_file(*name, options).expect("start part");
            writer.write_all(body.as_bytes()).expect("write part");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#;

    /// A docx whose body is exactly `body` (raw `<w:p>` markup).
    fn docx(body: &str) -> Vec<u8> {
        let document = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
        );
        zip_of(&[
            ("[Content_Types].xml", RELS.to_string()),
            ("_rels/.rels", RELS.to_string()),
            ("word/document.xml", document),
        ])
    }

    fn para(text: &str) -> String {
        format!(r#"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"#)
    }

    /// A pptx with one `<a:t>` run per slide, written to the zip in an order
    /// that is deliberately *not* the numeric one.
    fn pptx(slides: &[(u32, &str)]) -> Vec<u8> {
        let mut parts: Vec<(String, String)> = vec![
            ("[Content_Types].xml".to_string(), RELS.to_string()),
            ("_rels/.rels".to_string(), RELS.to_string()),
            // The part detection keys on.
            (
                "ppt/presentation.xml".to_string(),
                r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>"#.to_string(),
            ),
        ];
        for (n, text) in slides {
            parts.push((
                format!("ppt/slides/slide{n}.xml"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
                ),
            ));
            // The rels sidecar sits next to each slide and must not be mistaken
            // for one.
            parts.push((
                format!("ppt/slides/_rels/slide{n}.xml.rels"),
                RELS.to_string(),
            ));
        }
        let borrowed: Vec<(&str, String)> =
            parts.iter().map(|(n, b)| (n.as_str(), b.clone())).collect();
        zip_of(&borrowed)
    }

    fn preview(name: &str, format: &str, bytes: &[u8], opts: &PreviewOptions) -> Preview {
        let fixture = temp_file(name, bytes);
        render(
            &fixture.0,
            format,
            bytes.to_vec(),
            opts,
            &CancelToken::new(),
        )
        .expect("render")
    }

    fn lines<'a>(p: &'a Preview, language: &str) -> &'a [StyledLine] {
        match &p.content {
            PreviewContent::Text {
                lines,
                language: got,
            } => {
                assert_eq!(got, language);
                lines
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn plain(line: &StyledLine) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn docx_paragraphs_come_back_in_order() {
        let body = format!("{}{}{}", para("First"), para("Second"), para("Third"));
        let p = preview("a.docx", "docx", &docx(&body), &PreviewOptions::default());
        let rendered: Vec<String> = lines(&p, "Word Document")
            .iter()
            .map(plain)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(rendered, ["First", "Second", "Third"]);
        assert!(!p.truncated);
    }

    #[test]
    fn docx_headings_breaks_and_tabs_are_honoured() {
        let body = format!(
            "{}{}{}",
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr><w:r><w:t>Chapter One</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>one</w:t><w:br/><w:t>two</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:tab/><w:t>indented</w:t></w:r></w:p>"#,
        );
        let p = preview("h.docx", "docx", &docx(&body), &PreviewOptions::default());
        let all = lines(&p, "Word Document");
        let rendered: Vec<String> = all.iter().map(plain).collect();

        let heading = all
            .iter()
            .find(|l| plain(l) == "Chapter One")
            .expect("heading");
        assert!(heading.spans.iter().all(|s| s.bold), "heading must be bold");
        assert_eq!(heading.spans[0].fg, Some(super::imp::palette::HEADING[0]));

        // The `w:tab` inside `w:tabs` is a tab *stop* and must not be printed.
        assert!(
            !rendered.iter().any(|l| l.starts_with("    Chapter")),
            "{rendered:?}"
        );
        // `<w:br/>` splits the paragraph into two lines.
        assert!(rendered.iter().any(|l| l == "one"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "two"), "{rendered:?}");
        // A real `<w:tab/>` in a run does indent.
        assert!(rendered.iter().any(|l| l == "    indented"), "{rendered:?}");
    }

    #[test]
    fn docx_entities_are_resolved() {
        let body = para("A &amp; B &#233;");
        let p = preview("e.docx", "docx", &docx(&body), &PreviewOptions::default());
        let rendered: Vec<String> = lines(&p, "Word Document").iter().map(plain).collect();
        assert!(rendered.iter().any(|l| l == "A & B é"), "{rendered:?}");
    }

    #[test]
    fn docx_max_lines_truncates_and_flags() {
        let body: String = (0..300).map(|i| para(&format!("line {i}"))).collect();
        let opts = PreviewOptions {
            max_lines: 10,
            ..PreviewOptions::default()
        };
        let p = preview("long.docx", "docx", &docx(&body), &opts);
        assert!(p.truncated);
        assert_eq!(lines(&p, "Word Document").len(), 10);
    }

    #[test]
    fn pptx_slides_are_previewed_in_numeric_order() {
        // Written to the zip out of order, and including slide10 — the case a
        // lexicographic sort gets wrong ("slide10" < "slide2").
        let bytes = pptx(&[
            (10, "Tenth slide"),
            (2, "Second slide"),
            (1, "First slide"),
            (3, "Third slide"),
        ]);
        let p = preview("deck.pptx", "pptx", &bytes, &PreviewOptions::default());
        let rendered: Vec<String> = lines(&p, "PowerPoint Presentation")
            .iter()
            .map(plain)
            .filter(|l| !l.is_empty())
            .collect();

        assert_eq!(
            rendered,
            [
                "── Slide 1 ──",
                "First slide",
                "── Slide 2 ──",
                "Second slide",
                "── Slide 3 ──",
                "Third slide",
                "── Slide 10 ──",
                "Tenth slide",
            ]
        );
    }

    #[test]
    fn pptx_stops_at_max_entries() {
        let slides: Vec<(u32, &str)> = (1..=8).map(|n| (n, "text")).collect();
        let opts = PreviewOptions {
            max_entries: 3,
            ..PreviewOptions::default()
        };
        let p = preview("many.pptx", "pptx", &pptx(&slides), &opts);
        assert!(p.truncated);
        let rendered: Vec<String> = lines(&p, "PowerPoint Presentation")
            .iter()
            .map(plain)
            .collect();
        assert!(rendered.iter().any(|l| l == "── Slide 3 ──"));
        assert!(!rendered.iter().any(|l| l == "── Slide 4 ──"));
    }

    #[test]
    fn corrupt_package_is_a_format_error_not_a_panic() {
        let good = docx(&para("hello"));
        let half = good[..good.len() / 2].to_vec();
        let fixture = temp_file("half.docx", &half);
        let err = render(
            &fixture.0,
            "docx",
            half,
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("should fail");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn docx_without_a_document_part_is_a_format_error() {
        let bytes = zip_of(&[("readme.txt", "not a docx".to_string())]);
        let fixture = temp_file("empty.docx", &bytes);
        let err = render(
            &fixture.0,
            "docx",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("should fail");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    /// Legacy binary Word is declined on purpose, so the dispatcher hexdumps.
    #[test]
    fn legacy_doc_is_declined_rather_than_guessed_at() {
        let bytes = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1".to_vec();
        let fixture = temp_file("old.doc", &bytes);
        let err = render(
            &fixture.0,
            "doc",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("legacy .doc has no reader");
        match err {
            PreviewError::Format(msg) => assert!(msg.contains("no pure-Rust reader"), "{msg}"),
            other => panic!("got {other:?}"),
        }
    }

    /// The point of sniffing the zip's central directory: OOXML is recognised
    /// by the parts it contains, and a plain zip is left alone.
    #[test]
    fn ooxml_is_detected_by_content_and_a_plain_zip_still_is_not() {
        use crate::detect::{detect, Detected};
        let opts = PreviewOptions::default();

        let plain = zip_of(&[
            ("a.txt", "hello".to_string()),
            ("b/c.txt", "world".to_string()),
        ]);
        let zipped = temp_file("plain.zip", &plain);
        let detected = detect(&zipped.0, &opts).expect("detect");
        assert!(
            matches!(detected, Detected::Archive { .. }),
            "a plain zip must still list as an archive, got {detected:?}"
        );

        // Extensions deliberately wrong: only the parts inside decide.
        let doc = temp_file("mystery.bin", &docx(&para("hello")));
        let detected = detect(&doc.0, &opts).expect("detect");
        assert!(
            matches!(&detected, Detected::Document { format, .. } if format == "docx"),
            "got {detected:?}"
        );

        let deck = temp_file("mystery.dat", &pptx(&[(1, "hi")]));
        let detected = detect(&deck.0, &opts).expect("detect");
        assert!(
            matches!(&detected, Detected::Document { format, .. } if format == "pptx"),
            "got {detected:?}"
        );
    }

    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let bytes = docx(&para("hello"));
        let fixture = temp_file("cancelled.docx", &bytes);
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = render(
            &fixture.0,
            "docx",
            bytes,
            &PreviewOptions::default(),
            &cancel,
        )
        .expect_err("should cancel");
        assert!(matches!(err, PreviewError::Cancelled), "got {err:?}");
    }
}
