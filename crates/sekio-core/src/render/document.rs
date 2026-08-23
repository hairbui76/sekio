//! Word and PowerPoint previews: docx and pptx.
//!
//! Both are OOXML — a zip of XML parts — so this reads the one part that holds
//! the prose (`word/document.xml`, `ppt/slides/slideN.xml`) with `zip` and
//! pulls the text runs out with `quick-xml`. No DOM is built: the parser is
//! driven event by event straight off the inflating stream, so hitting
//! `max_lines` stops the *decompression*, not just the output.
//!
//! A document is prose that may *contain* tables, so the output stays
//! `PreviewContent::Text` and tables are laid out inline as styled spans. The
//! conventions are `render/markdown.rs`'s, deliberately: headings bold and
//! coloured per level, list bullets indented by depth, table cells joined by a
//! dim `│` with the first row bold. A `.docx` and a `.md` of the same content
//! should look like siblings.
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

    use quick_xml::events::BytesStart;
    use quick_xml::events::Event;
    use quick_xml::Reader as XmlReader;

    use crate::{
        CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, Span, StyledLine,
    };

    /// Palette, harmonised with `render/markdown.rs` and the base16-ocean.dark
    /// syntect theme, so prose from a docx and prose from a `.md` look alike.
    /// The slot names and values are markdown's; keep them in step.
    pub(super) mod palette {
        pub type Rgb = (u8, u8, u8);

        /// Body text. base05
        pub const TEXT: Rgb = (0xc0, 0xc5, 0xce);
        /// Slide separators, elision marks and the "no text" note. base03
        pub const DIM: Rgb = (0x65, 0x73, 0x7e);
        /// Table column gutters. base02
        pub const RULE: Rgb = (0x4f, 0x5b, 0x66);
        /// List bullets. base0B
        pub const MARKER: Rgb = (0xa3, 0xbe, 0x8c);
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

    /// Poll the cancel token every this many paragraphs / table rows.
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
    /// Columns one list level indents by. Markdown's bullet markers are two
    /// characters wide ("• ") and its nested lists indent by the enclosing
    /// marker's width, so two per level reproduces exactly what a `.md` of the
    /// same list looks like.
    const LIST_INDENT: usize = 2;
    /// Deepest list level given its own indent. OOXML allows `w:ilvl` 0..=8;
    /// past that the indent would eat the pane.
    const MAX_LIST_LEVEL: u8 = 8;
    /// Shape-nesting depth tracked for pptx placeholders. Groups nest, hostile
    /// input nests without end, and the stack must not grow with it.
    const MAX_SHAPE_DEPTH: usize = 32;
    /// Columns of a table laid out at all, whatever the width budget says. A
    /// 200-column table is not readable in a preview pane at any width.
    const MAX_TABLE_COLS: usize = 16;
    /// Characters buffered from one table cell. Bounds the memory a pathological
    /// table can cost: rows are bounded by `max_lines`, columns by
    /// `MAX_TABLE_COLS`, and each cell by this.
    const MAX_CELL_CHARS: usize = 256;
    /// Narrowest a laid-out column gets. Below this a column shows an ellipsis
    /// and nothing else, so a table narrower than this drops columns instead.
    const MIN_COL_CHARS: usize = 6;
    /// Drawn between table columns, exactly as `render/markdown.rs` draws it.
    const COL_SEP: &str = " │ ";
    /// Display width of [`COL_SEP`].
    const COL_SEP_WIDTH: usize = 3;

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

    /// Parser state for one part: everything the event loop has to remember
    /// that is not already line-assembly (which is [`Out`]'s job).
    #[derive(Default)]
    struct State {
        /// Depth inside a `*Pr` properties element (`w:pPr`, `a:rPr`, ...).
        /// Everything in there describes formatting, not content — and `w:tabs`
        /// in a `w:pPr` is full of `w:tab` elements that are tab *stops*, not
        /// tabs to print.
        props: usize,
        in_text: bool,
        /// Depth inside a run (`w:r` / `a:r`). Run formatting is only believed
        /// inside one: `<w:pPr><w:rPr><w:b/></w:rPr></w:pPr>` describes the
        /// paragraph *mark*, not the text, and would bold a whole paragraph
        /// that Word shows unbolded.
        run_depth: usize,
        bold: bool,
        italic: bool,
        /// Outline depth of the pptx paragraph being read (`<a:pPr lvl="N">`),
        /// remembered so a `<a:buChar/>` that follows knows how far to indent.
        lvl_hint: u8,
        /// Placeholder role per enclosing pptx shape — `Some(level)` for a
        /// title, so its paragraphs are styled as a heading.
        shapes: Vec<Option<u8>>,
        /// Shapes past `MAX_SHAPE_DEPTH`, so `pop` still mirrors `push`.
        shapes_skipped: usize,
    }

    impl State {
        fn push_shape(&mut self) {
            if self.shapes.len() < MAX_SHAPE_DEPTH {
                self.shapes.push(None);
            } else {
                self.shapes_skipped += 1;
            }
        }

        fn pop_shape(&mut self) {
            if self.shapes_skipped > 0 {
                self.shapes_skipped -= 1;
            } else {
                self.shapes.pop();
            }
        }

        fn placeholder(&self) -> Option<u8> {
            self.shapes.last().copied().flatten()
        }
    }

    /// Pull the text out of one OOXML part.
    ///
    /// docx and pptx use different namespaces (`w:` and `a:`) but the same
    /// local names — `p` for a paragraph, `r` for a run, `t` for text,
    /// `tbl`/`tr`/`tc` for a table — so one walker serves both. Matching on the
    /// local name also means a package that binds those namespaces to different
    /// prefixes still reads correctly.
    fn walk<R: Read>(part: R, out: &mut Out, cancel: &CancelToken) -> Result<(), PreviewError> {
        let mut reader = xml_reader(part);
        // The reader borrows this across calls; reused so one buffer serves the
        // whole part.
        let mut buf = Vec::new();
        let mut st = State::default();
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

                Event::Start(e) => open_element(&e, false, &mut st, out),

                // An empty element opens and closes at once, so it never moves
                // the properties depth.
                Event::Empty(e) => open_element(&e, true, &mut st, out),

                Event::End(e) => {
                    let raw = e.local_name();
                    let name = raw.as_ref();
                    if is_props(name) {
                        st.props = st.props.saturating_sub(1);
                    } else if name == b"t" {
                        st.in_text = false;
                    } else if st.props == 0 {
                        match name {
                            b"r" => {
                                st.run_depth = st.run_depth.saturating_sub(1);
                                st.bold = false;
                                st.italic = false;
                            }
                            b"sp" => st.pop_shape(),
                            b"tc" => out.end_cell(),
                            b"tr" => {
                                out.end_row();
                                work += 1;
                            }
                            b"tbl" => out.end_table(),
                            b"p" => {
                                out.close_paragraph();
                                work += 1;
                            }
                            _ => {}
                        }
                        if matches!(name, b"p" | b"tr" | b"tbl") {
                            if work.is_multiple_of(CANCEL_INTERVAL) {
                                cancel.check()?;
                            }
                            if out.full() {
                                out.truncated = true;
                                break;
                            }
                        }
                    }
                }

                Event::Text(e) if st.in_text => {
                    let text = e.decode().map_err(decode_err)?;
                    out.push(&text, st.bold, st.italic);
                }
                Event::CData(e) if st.in_text => {
                    let text = e.decode().map_err(decode_err)?;
                    out.push(&text, st.bold, st.italic);
                }
                // quick-xml reports `&amp;` / `&#233;` as events of their own
                // rather than folding them into the surrounding text.
                Event::GeneralRef(e) if st.in_text => {
                    if let Some(text) = resolve_ref(&e) {
                        out.push(&text, st.bold, st.italic);
                    }
                }

                _ => {}
            }
        }

        // A part that ended inside a table still has rows to lay out.
        out.close_blocks();
        Ok(())
    }

    /// One `Start` or `Empty` element.
    fn open_element(e: &BytesStart<'_>, empty: bool, st: &mut State, out: &mut Out<'_>) {
        let raw = e.local_name();
        let name = raw.as_ref();

        // Formatting lives *inside* `*Pr` elements, so it is read before the
        // properties gate below rejects everything else in there.
        match name {
            b"pStyle" => {
                if let Some(level) = val_attr(e).as_deref().and_then(heading_level) {
                    out.set_heading(level);
                }
            }
            // Direct outline level, for producers that format headings by hand
            // instead of by style. Word writes `w:val="9"` to mean "body text",
            // so only 0..=5 are believed.
            b"outlineLvl" => {
                if let Some(level) = val_attr(e).and_then(|v| v.trim().parse::<u8>().ok()) {
                    if level <= 5 {
                        out.set_heading_fallback(level + 1);
                    }
                }
            }
            // `<w:numPr>` is what makes a docx paragraph a list item at all;
            // `<w:ilvl>` inside it gives the depth and `<w:numId w:val="0"/>`
            // takes the membership away again.
            b"numPr" => out.set_list(Some(0)),
            b"ilvl" => {
                if let Some(level) = val_attr(e).and_then(|v| v.trim().parse::<u8>().ok()) {
                    out.set_list(Some(level.min(MAX_LIST_LEVEL)));
                }
            }
            b"numId" => {
                if val_attr(e).as_deref() == Some("0") {
                    out.set_list(None);
                }
            }
            // DrawingML: `<a:pPr lvl="1">` is the outline depth of a slide
            // bullet. Level 0 is left alone — an ordinary text box is level 0
            // too, and its bullet (if any) is inherited from the layout.
            b"pPr" => {
                if let Some(level) = attr(e, b"lvl").and_then(|v| v.trim().parse::<u8>().ok()) {
                    let level = level.min(MAX_LIST_LEVEL);
                    st.lvl_hint = level;
                    if level >= 1 {
                        out.set_list(Some(level));
                    }
                }
            }
            // An explicit bullet on a slide paragraph, which settles level 0.
            b"buChar" | b"buAutoNum" => out.set_list(Some(st.lvl_hint)),
            b"buNone" => out.set_list(None),
            b"ph" => {
                if let Some(top) = st.shapes.last_mut() {
                    *top = attr(e, b"type")
                        .as_deref()
                        .and_then(placeholder_level)
                        .or(*top);
                }
            }
            // WordprocessingML run formatting. `<w:b/>` with no `w:val` is on;
            // `<w:b w:val="0"/>` is explicitly *off*, so the tag's presence is
            // not the truth.
            b"b" | b"bCs" => {
                if st.run_depth > 0 {
                    st.bold = on_off(val_attr(e).as_deref());
                }
            }
            b"i" | b"iCs" => {
                if st.run_depth > 0 {
                    st.italic = on_off(val_attr(e).as_deref());
                }
            }
            // DrawingML carries the same two as attributes on the run's own
            // `a:rPr` rather than as child elements.
            b"rPr" if st.run_depth > 0 => {
                if let Some(v) = attr(e, b"b") {
                    st.bold = on_off(Some(&v));
                }
                if let Some(v) = attr(e, b"i") {
                    st.italic = on_off(Some(&v));
                }
            }
            _ => {}
        }

        if st.props == 0 {
            match name {
                b"p" => {
                    // A stale outline depth would indent the next paragraph
                    // to wherever the last one happened to sit.
                    st.lvl_hint = 0;
                    out.open_paragraph(st.placeholder());
                    if empty {
                        out.close_paragraph();
                    }
                }
                b"r" => {
                    st.bold = false;
                    st.italic = false;
                    if !empty {
                        st.run_depth += 1;
                    }
                }
                b"t" if !empty => st.in_text = true,
                b"br" => out.line_break(),
                b"tab" => out.push(TAB, false, false),
                b"tbl" => out.start_table(),
                b"tr" => out.start_row(),
                b"tc" => out.start_cell(),
                b"sp" if !empty => st.push_shape(),
                _ => {}
            }
        }

        if !empty && is_props(name) {
            st.props += 1;
        }
    }

    fn decode_err(e: quick_xml::encoding::EncodingError) -> PreviewError {
        PreviewError::Format(format!("xml: {e}"))
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

    /// One attribute by local-ish name, trying the `w:`-prefixed spelling too.
    /// Style ids, levels and on/off flags are ASCII — no entities to unescape,
    /// and a lossy decode can never fail on a hostile file.
    fn attr(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
        let mut prefixed = Vec::with_capacity(name.len() + 2);
        prefixed.extend_from_slice(b"w:");
        prefixed.extend_from_slice(name);
        for candidate in [prefixed.as_slice(), name] {
            if let Ok(Some(found)) = e.try_get_attribute(candidate) {
                return Some(String::from_utf8_lossy(&found.value).into_owned());
            }
        }
        None
    }

    fn val_attr(e: &BytesStart<'_>) -> Option<String> {
        attr(e, b"val")
    }

    /// OOXML's on/off type: the attribute may be absent (which means on), or
    /// one of `0`/`false`/`off` (which means off).
    fn on_off(value: Option<&str>) -> bool {
        match value.map(str::trim) {
            None => true,
            Some(v) => !matches!(v, "0" | "false" | "off"),
        }
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

    /// A pptx shape's placeholder role, for the ones worth styling. Body,
    /// footer, slide-number and date placeholders are ordinary text.
    fn placeholder_level(kind: &str) -> Option<u8> {
        match kind.trim() {
            "title" | "ctrTitle" => Some(1),
            "subTitle" => Some(2),
            _ => None,
        }
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

    // --------------------------------------------------------------- widths

    /// Columns one character occupies in a monospaced pane.
    ///
    /// An approximation, and deliberately a small one: pulling in a full
    /// `unicode-width` table for a preview is not worth a dependency, but the
    /// two cases that visibly break a column layout — CJK/fullwidth characters
    /// that take two cells, and combining marks that take none — are worth the
    /// twenty lines. Everything else counts as one.
    fn char_width(c: char) -> usize {
        let cp = c as u32;
        const ZERO: [std::ops::RangeInclusive<u32>; 6] = [
            0x0300..=0x036F, // combining diacritics
            0x1AB0..=0x1AFF,
            0x1DC0..=0x1DFF,
            0x200B..=0x200F, // zero-width space .. RTL mark
            0x20D0..=0x20FF,
            0xFE00..=0xFE0F, // variation selectors
        ];
        const WIDE: [std::ops::RangeInclusive<u32>; 14] = [
            0x1100..=0x115F, // Hangul Jamo
            0x2E80..=0x303E, // CJK radicals, Kangxi, CJK symbols
            0x3041..=0x33FF, // kana, Hangul compat jamo, CJK compat
            0x3400..=0x4DBF, // CJK ext A
            0x4E00..=0x9FFF, // CJK unified
            0xA000..=0xA4CF, // Yi
            0xA960..=0xA97F, // Hangul Jamo ext A
            0xAC00..=0xD7A3, // Hangul syllables
            0xF900..=0xFAFF, // CJK compat ideographs
            0xFE10..=0xFE19,
            0xFE30..=0xFE6F,
            0xFF00..=0xFF60, // fullwidth forms
            0xFFE0..=0xFFE6,
            0x1F300..=0x1FAFF, // emoji
        ];
        if ZERO.iter().any(|r| r.contains(&cp)) {
            return 0;
        }
        if WIDE.iter().any(|r| r.contains(&cp)) {
            return 2;
        }
        1
    }

    fn width(s: &str) -> usize {
        s.chars().map(char_width).sum()
    }

    /// The longest prefix of `s` that fits in `max` columns.
    fn take_width(s: &str, max: usize) -> String {
        let mut used = 0;
        let mut out = String::new();
        for c in s.chars() {
            let w = char_width(c);
            if used + w > max {
                break;
            }
            used += w;
            out.push(c);
        }
        out
    }

    // -------------------------------------------------------------- output

    #[derive(Clone, Copy)]
    struct Sty {
        fg: Option<palette::Rgb>,
        bold: bool,
        italic: bool,
    }

    impl Sty {
        fn new(fg: palette::Rgb) -> Self {
            Self {
                fg: Some(fg),
                bold: false,
                italic: false,
            }
        }
        /// No colour of its own: indentation and padding, which must not paint
        /// a background of their own either.
        fn plain() -> Self {
            Self {
                fg: None,
                bold: false,
                italic: false,
            }
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

    fn span(text: impl Into<String>, sty: Sty) -> Span {
        Span {
            text: text.into(),
            fg: sty.fg,
            bold: sty.bold,
            italic: sty.italic,
        }
    }

    /// Block-level style of the paragraph being read.
    #[derive(Default, Clone, Copy)]
    struct Para {
        heading: Option<u8>,
        /// List depth, `Some(0)` being a top-level item.
        list: Option<u8>,
    }

    /// One table, buffered while it is read. A table cannot be emitted line by
    /// line: the column widths are only known once every row has been seen.
    /// The buffer is bounded on all three axes — rows by `max_lines` (see
    /// [`Out::full`]), columns by `MAX_TABLE_COLS`, cells by `MAX_CELL_CHARS`.
    #[derive(Default)]
    struct Table {
        rows: Vec<Vec<Vec<Span>>>,
        row: Vec<Vec<Span>>,
        cell: Option<Vec<Span>>,
        /// A cap dropped a cell or a row.
        clipped: bool,
    }

    /// Line assembler. Paragraphs push lines; runs push spans onto the line
    /// being built. Inside a table cell the same paragraphs land in the cell
    /// buffer instead, and the table lays itself out when it closes.
    struct Out<'a> {
        opts: &'a PreviewOptions,
        lines: Vec<StyledLine>,
        /// Spans of the line under construction.
        cur: Vec<Span>,
        /// Columns already on `cur`, against `MAX_LINE_CHARS`.
        cur_chars: usize,
        /// Whether `cur` has been given its indent/bullet prefix.
        open: bool,
        /// Whether nothing of this paragraph has been emitted yet.
        para_first: bool,
        /// Whether this paragraph's list bullet has already been drawn.
        list_marked: bool,
        para: Para,
        table: Option<Table>,
        /// Nesting depth of `<w:tbl>`. Only the outermost is laid out as a
        /// table; a nested one's cells flow into the enclosing cell, which
        /// keeps every word but does not try to draw a grid inside a grid.
        tbl_depth: usize,
        truncated: bool,
    }

    impl<'a> Out<'a> {
        fn new(opts: &'a PreviewOptions) -> Self {
            Self {
                opts,
                lines: Vec::new(),
                cur: Vec::new(),
                cur_chars: 0,
                open: false,
                para_first: true,
                list_marked: false,
                para: Para::default(),
                table: None,
                tbl_depth: 0,
                truncated: false,
            }
        }

        /// Rows waiting to be laid out. They are lines-to-be, so they count
        /// against `max_lines` before they are emitted — that is what stops a
        /// million-row table being buffered whole.
        fn pending_rows(&self) -> usize {
            self.table.as_ref().map_or(0, |t| t.rows.len())
        }

        fn full(&self) -> bool {
            self.lines.len() + self.pending_rows() >= self.opts.max_lines
        }

        fn in_cell(&self) -> bool {
            self.table.as_ref().is_some_and(|t| t.cell.is_some())
        }

        // ------------------------------------------------------- paragraphs

        fn open_paragraph(&mut self, placeholder: Option<u8>) {
            self.open = true;
            self.para_first = true;
            self.list_marked = false;
            self.para = Para {
                heading: placeholder,
                list: None,
            };
        }

        fn set_heading(&mut self, level: u8) {
            self.para.heading = Some(level);
        }

        fn set_heading_fallback(&mut self, level: u8) {
            if self.para.heading.is_none() {
                self.para.heading = Some(level);
            }
        }

        fn set_list(&mut self, level: Option<u8>) {
            self.para.list = level;
        }

        fn para_style(&self) -> Sty {
            match self.para.heading {
                Some(level) => {
                    let index = (level.clamp(1, 6) - 1) as usize;
                    Sty::new(palette::HEADING[index]).bold()
                }
                None => Sty::new(palette::TEXT),
            }
        }

        /// Indent and bullet, laid down before the first run of a line. Inside
        /// a table cell there is no room for either.
        fn open_line(&mut self) {
            self.open = true;
            if !self.cur.is_empty() || self.in_cell() {
                return;
            }
            let Some(level) = self.para.list else {
                return;
            };
            let level = level.min(MAX_LIST_LEVEL) as usize;
            let indent = level * LIST_INDENT;
            // Alternate so nested levels stay apart without colour —
            // markdown.rs alternates by list depth for the same reason.
            let bullet = if level.is_multiple_of(2) {
                "• "
            } else {
                "- "
            };
            let width = indent + bullet.chars().count();
            if self.list_marked {
                // A `<w:br/>` inside the item: the continuation lines up under
                // the item's text rather than repeating its bullet.
                self.cur.push(span(" ".repeat(width), Sty::plain()));
            } else {
                if indent > 0 {
                    self.cur.push(span(" ".repeat(indent), Sty::plain()));
                }
                self.cur.push(span(bullet, Sty::new(palette::MARKER)));
                self.list_marked = true;
            }
            self.cur_chars += width;
        }

        fn push(&mut self, text: &str, bold: bool, italic: bool) {
            if text.is_empty() || (self.full() && !self.in_cell()) {
                return;
            }
            let mut sty = self.para_style();
            if bold {
                sty = sty.bold();
            }
            if italic {
                sty = sty.italic();
            }
            self.open_line();
            let mut cleaned = String::with_capacity(text.len());
            for c in text.chars() {
                if self.cur_chars >= MAX_LINE_CHARS {
                    self.truncated = true;
                    break;
                }
                self.cur_chars += 1;
                // A stray newline inside a run would desynchronise the line
                // structure from the document's own paragraphs.
                cleaned.push(match c {
                    '\n' | '\r' => ' ',
                    other => other,
                });
            }
            if cleaned.is_empty() {
                return;
            }
            // docx splits prose into runs at spell-check and revision
            // boundaries, so merging equal styles keeps the span count down.
            if let Some(last) = self.cur.last_mut() {
                if last.fg == sty.fg && last.bold == sty.bold && last.italic == sty.italic {
                    last.text.push_str(&cleaned);
                    return;
                }
            }
            self.cur.push(span(cleaned, sty));
        }

        /// `<w:br/>`: a break *within* a paragraph. The line ends but the
        /// paragraph's style carries on.
        fn line_break(&mut self) {
            self.emit();
            self.open = true;
        }

        fn close_paragraph(&mut self) {
            if self.open || !self.cur.is_empty() {
                self.emit();
            }
            self.open = false;
            self.para = Para::default();
        }

        fn emit(&mut self) {
            let spans = trim_end(std::mem::take(&mut self.cur));
            self.cur_chars = 0;
            self.open = false;

            if self.tbl_depth > 0 {
                if let Some(table) = &mut self.table {
                    if let Some(cell) = &mut table.cell {
                        append_cell(cell, spans);
                    }
                }
                // Text between `<w:tr>`s belongs to no cell; there is nowhere
                // sensible to put it.
                return;
            }

            if self.full() {
                self.truncated = true;
                return;
            }
            // A heading is set apart from the prose above it, the way
            // markdown.rs separates its blocks.
            if self.para.heading.is_some() && self.para_first {
                self.blank_before();
            }
            self.para_first = false;
            if self.full() {
                self.truncated = true;
                return;
            }
            // Collapse runs of blank paragraphs: documents are full of them and
            // a preview pane is not.
            if spans.is_empty()
                && (self.lines.is_empty() || self.lines.last().is_some_and(is_blank))
            {
                return;
            }
            self.lines.push(StyledLine { spans });
        }

        fn blank_before(&mut self) {
            if self.lines.is_empty() || self.lines.last().is_some_and(is_blank) {
                return;
            }
            if self.full() {
                self.truncated = true;
                return;
            }
            self.lines.push(StyledLine::default());
        }

        // ----------------------------------------------------------- tables

        fn start_table(&mut self) {
            self.tbl_depth += 1;
            if self.tbl_depth == 1 {
                self.close_paragraph();
                self.table = Some(Table::default());
            }
        }

        fn start_row(&mut self) {
            if self.tbl_depth != 1 {
                return;
            }
            self.end_cell();
            if let Some(table) = &mut self.table {
                table.row.clear();
            }
        }

        fn start_cell(&mut self) {
            if self.tbl_depth != 1 {
                return;
            }
            self.end_cell();
            if let Some(table) = &mut self.table {
                table.cell = Some(Vec::new());
            }
        }

        fn end_cell(&mut self) {
            if self.tbl_depth != 1 {
                return;
            }
            self.close_paragraph();
            if let Some(table) = &mut self.table {
                if let Some(cell) = table.cell.take() {
                    if table.row.len() < MAX_TABLE_COLS {
                        table.row.push(cell);
                    } else {
                        table.clipped = true;
                    }
                }
            }
        }

        fn end_row(&mut self) {
            if self.tbl_depth != 1 {
                return;
            }
            self.end_cell();
            let full = self.full();
            if let Some(table) = &mut self.table {
                let row = std::mem::take(&mut table.row);
                if row.iter().all(Vec::is_empty) {
                    return;
                }
                if full {
                    table.clipped = true;
                    return;
                }
                table.rows.push(row);
            }
        }

        fn end_table(&mut self) {
            if self.tbl_depth == 0 {
                return;
            }
            if self.tbl_depth > 1 {
                self.tbl_depth -= 1;
                return;
            }
            self.end_row();
            self.tbl_depth = 0;
            self.flush_table();
        }

        /// Close anything still open at the end of a part.
        fn close_blocks(&mut self) {
            while self.tbl_depth > 0 {
                self.end_table();
            }
            self.close_paragraph();
        }

        /// Lay the buffered rows out in aligned columns and push them as lines.
        fn flush_table(&mut self) {
            let Some(table) = self.table.take() else {
                return;
            };
            if table.clipped {
                self.truncated = true;
            }
            if table.rows.is_empty() {
                return;
            }
            let layout = column_widths(&table.rows, self.opts.line_width());
            if layout.dropped {
                self.truncated = true;
            }
            self.blank_before();

            for (r, row) in table.rows.into_iter().enumerate() {
                if self.full() {
                    self.truncated = true;
                    break;
                }
                // Stop after the row's last non-empty cell: a trailing `│` with
                // nothing behind it only makes the row look cut off.
                let last = row
                    .iter()
                    .rposition(|cell| !cell.is_empty())
                    .map_or(0, |i| i + 1)
                    .min(layout.widths.len());
                let mut spans: Vec<Span> = Vec::new();
                for (c, mut cell) in row.into_iter().enumerate().take(last) {
                    let Some(&col) = layout.widths.get(c) else {
                        break;
                    };
                    if c > 0 {
                        spans.push(span(COL_SEP, Sty::new(palette::RULE)));
                    }
                    // Markdown bolds a table's header row; a docx table has no
                    // header flag most producers bother to set, so the first
                    // row is it by the same convention.
                    if r == 0 {
                        for s in &mut cell {
                            s.bold = true;
                        }
                    }
                    if elide(&mut cell, col) {
                        self.truncated = true;
                    }
                    let used: usize = cell.iter().map(|s| width(&s.text)).sum();
                    spans.append(&mut cell);
                    // Never pad past the row's last cell: trailing blanks are
                    // invisible but real, and a frontend would paint them.
                    if c + 1 < last && used < col {
                        spans.push(span(" ".repeat(col - used), Sty::plain()));
                    }
                }
                self.lines.push(StyledLine { spans });
            }
            // The next block separates itself with `blank_before`; a trailing
            // one is trimmed in `finish`.
            self.blank_before();
        }

        /// The `── Slide 3 ──` marker between pptx slides.
        fn separator(&mut self, text: String) {
            self.close_blocks();
            if self.full() {
                self.truncated = true;
                return;
            }
            self.blank_before();
            if self.full() {
                self.truncated = true;
                return;
            }
            self.lines.push(StyledLine {
                spans: vec![span(text, Sty::new(palette::DIM).bold())],
            });
        }

        fn finish(mut self, language: &str) -> Result<Preview, PreviewError> {
            self.close_blocks();
            while self.lines.last().is_some_and(is_blank) {
                self.lines.pop();
            }
            if self.lines.is_empty() {
                // A valid package that simply holds no prose (a deck of images,
                // say). Say so rather than erroring into a hexdump, which would
                // tell the reader even less.
                self.lines = vec![StyledLine {
                    spans: vec![span("(no text in this document)", Sty::new(palette::DIM))],
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

    fn trim_end(mut spans: Vec<Span>) -> Vec<Span> {
        while let Some(last) = spans.last_mut() {
            let keep = last.text.trim_end().len();
            if keep == 0 {
                spans.pop();
                continue;
            }
            last.text.truncate(keep);
            break;
        }
        spans
    }

    /// Fold one paragraph into the cell it belongs to. A cell is one line of
    /// the laid-out table, so its paragraphs run together with a space rather
    /// than pushing the row's other columns out of alignment.
    fn append_cell(cell: &mut Vec<Span>, spans: Vec<Span>) {
        if spans.is_empty() {
            return;
        }
        let used: usize = cell.iter().map(|s| width(&s.text)).sum();
        let mut room = MAX_CELL_CHARS.saturating_sub(used);
        if room == 0 {
            return;
        }
        if !cell.is_empty() {
            cell.push(span(" ", Sty::plain()));
            room -= 1;
        }
        for mut s in spans {
            if room == 0 {
                break;
            }
            let w = width(&s.text);
            if w > room {
                s.text = take_width(&s.text, room);
            }
            room -= width(&s.text);
            cell.push(s);
        }
    }

    struct Layout {
        widths: Vec<usize>,
        /// Columns were dropped, or would have been past `MAX_TABLE_COLS`.
        dropped: bool,
    }

    /// Column widths for one table.
    ///
    /// The budget is `PreviewOptions::line_width()` — the frontend's pane width
    /// when it gave one, `DEFAULT_TEXT_WIDTH` when it did not. Prose lines keep
    /// their own length and are scrolled sideways by the frontend, but a table
    /// row is a line *we* construct, so its length is our decision and an
    /// unbounded one would be built out of memory we own. Using the same hint
    /// the spreadsheet renderer uses also means a docx table and an xlsx sheet
    /// lay out to the same width in the same pane.
    fn column_widths(rows: &[Vec<Vec<Span>>], budget: usize) -> Layout {
        let mut natural: Vec<usize> = Vec::new();
        let mut widest_row = 0usize;
        for row in rows {
            widest_row = widest_row.max(row.len());
            for (c, cell) in row.iter().enumerate().take(MAX_TABLE_COLS) {
                let w: usize = cell.iter().map(|s| width(&s.text)).sum();
                match natural.get_mut(c) {
                    Some(slot) => *slot = (*slot).max(w),
                    None => natural.push(w),
                }
            }
        }
        let mut dropped = widest_row > natural.len();

        // Drop columns from the right until every survivor can hold at least
        // `MIN_COL_CHARS`: a column narrower than that shows an ellipsis and
        // nothing else, which is worse than admitting the column is not shown.
        let mut cols = natural.len();
        while cols > 1 && cols * MIN_COL_CHARS + COL_SEP_WIDTH * (cols - 1) > budget {
            cols -= 1;
        }
        if cols < natural.len() {
            natural.truncate(cols);
            dropped = true;
        }

        let content = budget.saturating_sub(COL_SEP_WIDTH * cols.saturating_sub(1));
        let total: usize = natural.iter().sum();
        if total <= content {
            return Layout {
                widths: natural,
                dropped,
            };
        }
        // Water-fill: cap every column at the widest cap that still fits, so
        // narrow columns keep their full width and only the wide ones give.
        let mut lo = MIN_COL_CHARS;
        let mut hi = natural.iter().copied().max().unwrap_or(MIN_COL_CHARS);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let fits: usize = natural.iter().map(|w| (*w).min(mid)).sum();
            if fits <= content {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Layout {
            widths: natural.iter().map(|w| (*w).min(lo)).collect(),
            dropped,
        }
    }

    /// Cut a cell down to `max` columns, marking the cut with an ellipsis.
    /// Returns whether anything was lost.
    fn elide(spans: &mut Vec<Span>, max: usize) -> bool {
        let total: usize = spans.iter().map(|s| width(&s.text)).sum();
        if total <= max {
            return false;
        }
        // One column is spent on the ellipsis itself.
        let keep = max.saturating_sub(1);
        let mut used = 0usize;
        let mut end = 0usize;
        for (i, s) in spans.iter_mut().enumerate() {
            let w = width(&s.text);
            if used + w <= keep {
                used += w;
                end = i + 1;
                continue;
            }
            s.text = take_width(&s.text, keep - used);
            end = if s.text.is_empty() { i } else { i + 1 };
            break;
        }
        spans.truncate(end);
        spans.push(span("…", Sty::new(palette::DIM)));
        true
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

        /// OOXML's on/off type: absent means on, and `0`/`false`/`off` mean off.
        #[test]
        fn on_off_reads_the_absent_attribute_as_on() {
            assert!(on_off(None));
            assert!(on_off(Some("1")));
            assert!(on_off(Some("true")));
            assert!(!on_off(Some("0")));
            assert!(!on_off(Some("false")));
            assert!(!on_off(Some("off")));
        }

        #[test]
        fn wide_and_combining_characters_are_measured_not_counted() {
            assert_eq!(width("abc"), 3);
            // Vietnamese, precomposed: one column each.
            assert_eq!(width("Trợ giảng"), 9);
            // Hangul and Han take two columns each.
            assert_eq!(width("한글"), 4);
            assert_eq!(width("漢字"), 4);
            // A combining mark rides on the character before it.
            assert_eq!(width("e\u{0301}"), 1);
        }

        #[test]
        fn columns_are_dropped_before_they_get_too_narrow_to_read() {
            let cell = |s: &str| vec![span(s, Sty::plain())];
            let rows = vec![(0..40).map(|i| cell(&format!("c{i}"))).collect::<Vec<_>>()];

            let layout = column_widths(&rows, 120);
            assert!(layout.dropped, "40 columns cannot fit in 120 chars");
            assert!(layout.widths.len() <= MAX_TABLE_COLS);
            let line: usize = layout.widths.iter().sum::<usize>()
                + COL_SEP_WIDTH * layout.widths.len().saturating_sub(1);
            assert!(line <= 120, "laid out {line} columns wide");
        }

        #[test]
        fn a_table_that_fits_keeps_its_natural_widths() {
            let cell = |s: &str| vec![span(s, Sty::plain())];
            let rows = vec![vec![cell("aa"), cell("bbbb")], vec![cell("c"), cell("d")]];
            let layout = column_widths(&rows, 120);
            assert_eq!(layout.widths, vec![2, 4]);
            assert!(!layout.dropped);
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

    fn heading(level: u8, text: &str) -> String {
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading{level}"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    }

    /// A list paragraph the way LibreOffice writes one: `w:numPr` with an
    /// `w:ilvl` depth inside the paragraph properties.
    fn item(level: u8, text: &str) -> String {
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Normal"/><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    }

    fn cell(text: &str) -> String {
        format!(
            r#"<w:tc><w:tcPr><w:tcW w:w="3000" w:type="dxa"/></w:tcPr>{}</w:tc>"#,
            para(text)
        )
    }

    fn table(rows: &[&[&str]]) -> String {
        let mut out =
            String::from(r#"<w:tbl><w:tblPr><w:tblW w:w="5000" w:type="pct"/></w:tblPr>"#);
        for row in rows {
            out.push_str("<w:tr><w:trPr></w:trPr>");
            for c in *row {
                out.push_str(&cell(c));
            }
            out.push_str("</w:tr>");
        }
        out.push_str("</w:tbl>");
        out
    }

    /// A pptx with one `<a:t>` run per slide, written to the zip in an order
    /// that is deliberately *not* the numeric one.
    fn pptx(slides: &[(u32, &str)]) -> Vec<u8> {
        let bodies: Vec<(u32, String)> = slides
            .iter()
            .map(|(n, text)| {
                (
                    *n,
                    format!(
                        r#"<p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"#
                    ),
                )
            })
            .collect();
        let borrowed: Vec<(u32, &str)> = bodies.iter().map(|(n, b)| (*n, b.as_str())).collect();
        pptx_raw(&borrowed)
    }

    /// A pptx whose slides hold exactly the given `<p:spTree>` markup.
    fn pptx_raw(slides: &[(u32, &str)]) -> Vec<u8> {
        let mut parts: Vec<(String, String)> = vec![
            ("[Content_Types].xml".to_string(), RELS.to_string()),
            ("_rels/.rels".to_string(), RELS.to_string()),
            // The part detection keys on.
            (
                "ppt/presentation.xml".to_string(),
                r#"<?xml version="1.0" encoding="UTF-8"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>"#.to_string(),
            ),
        ];
        for (n, body) in slides {
            parts.push((
                format!("ppt/slides/slide{n}.xml"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>"#
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

    fn texts(p: &Preview, language: &str) -> Vec<String> {
        lines(p, language).iter().map(plain).collect()
    }

    fn find<'a>(p: &'a Preview, language: &str, needle: &str) -> &'a StyledLine {
        lines(p, language)
            .iter()
            .find(|l| plain(l).contains(needle))
            .unwrap_or_else(|| panic!("no line containing {needle:?} in {:?}", texts(p, language)))
    }

    #[test]
    fn docx_paragraphs_come_back_in_order() {
        let body = format!("{}{}{}", para("First"), para("Second"), para("Third"));
        let p = preview("a.docx", "docx", &docx(&body), &PreviewOptions::default());
        let rendered: Vec<String> = texts(&p, "Word Document")
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(rendered, ["First", "Second", "Third"]);
        assert!(!p.truncated);
    }

    /// Every level gets its own colour, and none of them looks like body text.
    #[test]
    fn every_heading_level_is_styled_apart_from_body_text() {
        let mut body = String::new();
        for level in 1..=6u8 {
            body.push_str(&heading(level, &format!("H{level}")));
        }
        body.push_str(&para("body"));
        let p = preview("h.docx", "docx", &docx(&body), &PreviewOptions::default());

        let mut seen = Vec::new();
        for level in 1..=6u8 {
            let line = find(&p, "Word Document", &format!("H{level}"));
            assert!(
                line.spans.iter().all(|s| s.bold),
                "H{level} must be bold: {line:?}"
            );
            let fg = line.spans[0].fg.expect("heading colour");
            assert_eq!(fg, super::imp::palette::HEADING[level as usize - 1]);
            seen.push(fg);
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 6, "each level needs its own colour");

        let body_line = find(&p, "Word Document", "body");
        assert!(!body_line.spans[0].bold);
        assert_eq!(body_line.spans[0].fg, Some(super::imp::palette::TEXT));
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
        let rendered = texts(&p, "Word Document");

        let heading = find(&p, "Word Document", "Chapter One");
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

    /// `<w:numPr>` makes a paragraph a list item; `<w:ilvl>` says how deep.
    #[test]
    fn list_items_get_markers_indented_by_depth() {
        let body = format!(
            "{}{}{}{}",
            item(0, "top"),
            item(1, "nested"),
            item(2, "deeper"),
            para("after"),
        );
        let p = preview("l.docx", "docx", &docx(&body), &PreviewOptions::default());
        let rendered = texts(&p, "Word Document");

        assert!(rendered.iter().any(|l| l == "• top"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "  - nested"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "    • deeper"), "{rendered:?}");
        // The list ends where the numbering does.
        assert!(rendered.iter().any(|l| l == "after"), "{rendered:?}");

        let bullet = &find(&p, "Word Document", "• top").spans;
        assert_eq!(bullet[0].text, "• ");
        assert_eq!(bullet[0].fg, Some(super::imp::palette::MARKER));
    }

    /// A `<w:br/>` inside a list item continues the item: the second line lines
    /// up under the text, and does not get a bullet of its own.
    #[test]
    fn a_wrapped_list_item_is_indented_not_re_bulleted() {
        let body = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>first</w:t><w:br/><w:t>second</w:t></w:r></w:p>"#;
        let p = preview("br.docx", "docx", &docx(body), &PreviewOptions::default());
        let rendered = texts(&p, "Word Document");
        assert!(rendered.iter().any(|l| l == "  - first"), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "    second"), "{rendered:?}");
    }

    /// `<w:numId w:val="0"/>` takes the list membership away again.
    #[test]
    fn a_zero_numbering_id_is_not_a_list() {
        let body = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="0"/></w:numPr></w:pPr><w:r><w:t>plain</w:t></w:r></w:p>"#;
        let p = preview("l0.docx", "docx", &docx(body), &PreviewOptions::default());
        let rendered = texts(&p, "Word Document");
        assert!(rendered.iter().any(|l| l == "plain"), "{rendered:?}");
        assert!(!rendered.iter().any(|l| l.contains('•')), "{rendered:?}");
    }

    #[test]
    fn table_rows_land_on_one_line_each_with_aligned_columns() {
        let body = format!(
            "{}{}",
            table(&[
                &["Hạng mục", "Số giờ", "Ghi chú"],
                &["Trợ giảng", "47.3", "Học kỳ 1"],
                &["Chấm bài", "12", ""],
            ]),
            para("after"),
        );
        let p = preview("t.docx", "docx", &docx(&body), &PreviewOptions::default());
        let rendered = texts(&p, "Word Document");

        let header = find(&p, "Word Document", "Hạng mục");
        assert!(
            header
                .spans
                .iter()
                .filter(|s| !matches!(s.text.trim(), "" | "│"))
                .all(|s| s.bold),
            "the first row is the header row: {header:?}"
        );
        // One line per row: all three cells of a row are on it.
        let row = plain(find(&p, "Word Document", "Trợ giảng"));
        assert!(row.contains("47.3") && row.contains("Học kỳ 1"), "{row}");
        assert_eq!(
            rendered.iter().filter(|l| l.contains('│')).count(),
            3,
            "{rendered:?}"
        );

        // Columns line up: the separators sit at the same column on every row.
        // Every character in this fixture is one cell wide, so a char index is
        // the column.
        let bars: Vec<Vec<usize>> = rendered
            .iter()
            .filter(|l| l.contains('│'))
            .map(|l| {
                l.chars()
                    .enumerate()
                    .filter(|(_, ch)| *ch == '│')
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        assert_eq!(bars[0], bars[1], "{rendered:?}");
        // The short last row still starts its columns in the same place.
        assert_eq!(bars[2][0], bars[0][0], "{rendered:?}");

        // The gutter is markdown's, and dim.
        let sep = header
            .spans
            .iter()
            .find(|s| s.text.contains('│'))
            .expect("separator span");
        assert_eq!(sep.text, " │ ");
        assert_eq!(sep.fg, Some(super::imp::palette::RULE));

        // A table is a block: blank lines set it off from the prose around it.
        let first = rendered
            .iter()
            .position(|l| l.contains('│'))
            .expect("table");
        assert!(rendered[first + 3].is_empty(), "{rendered:?}");
        assert!(rendered.iter().any(|l| l == "after"), "{rendered:?}");
    }

    #[test]
    fn a_table_with_many_columns_stays_within_the_line_budget() {
        let wide: Vec<String> = (0..60)
            .map(|i| format!("column value number {i}"))
            .collect();
        let refs: Vec<&str> = wide.iter().map(String::as_str).collect();
        let body = table(&[&refs, &refs]);
        let opts = PreviewOptions {
            text_width: Some(80),
            ..PreviewOptions::default()
        };
        let p = preview("wide.docx", "docx", &docx(&body), &opts);
        let rendered = texts(&p, "Word Document");
        for line in rendered.iter().filter(|l| l.contains('│')) {
            assert!(
                line.chars().count() <= 80,
                "{} chars: {line}",
                line.chars().count()
            );
        }
        assert!(p.truncated, "dropped columns must be reported");
    }

    #[test]
    fn bold_and_italic_runs_keep_their_attributes() {
        let body = r#"<w:p><w:r><w:t>plain </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t> </w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>slanted</w:t></w:r><w:r><w:rPr><w:b/><w:i/></w:rPr><w:t>both</w:t></w:r></w:p>"#;
        let p = preview("b.docx", "docx", &docx(body), &PreviewOptions::default());
        let spans: Vec<_> = lines(&p, "Word Document")
            .iter()
            .flat_map(|l| l.spans.iter())
            .collect();

        let plain_span = spans.iter().find(|s| s.text == "plain ").expect("plain");
        assert!(!plain_span.bold && !plain_span.italic);
        let bold = spans.iter().find(|s| s.text == "bold").expect("bold");
        assert!(bold.bold && !bold.italic);
        let ital = spans.iter().find(|s| s.text == "slanted").expect("italic");
        assert!(ital.italic && !ital.bold);
        let both = spans.iter().find(|s| s.text == "both").expect("both");
        assert!(both.bold && both.italic);
    }

    /// `<w:b w:val="0"/>` means *not* bold: the tag's presence is not the truth.
    #[test]
    fn an_explicitly_off_bold_does_not_bold() {
        let body = r#"<w:p><w:r><w:rPr><w:b w:val="0"/><w:i w:val="false"/></w:rPr><w:t>upright</w:t></w:r></w:p>"#;
        let p = preview("b0.docx", "docx", &docx(body), &PreviewOptions::default());
        let line = find(&p, "Word Document", "upright");
        assert!(line.spans.iter().all(|s| !s.bold && !s.italic), "{line:?}");
    }

    /// A `<w:rPr>` inside `<w:pPr>` describes the paragraph *mark*, not the
    /// text, and must not bold the paragraph.
    #[test]
    fn paragraph_mark_formatting_does_not_leak_into_the_text() {
        let body =
            r#"<w:p><w:pPr><w:rPr><w:b/></w:rPr></w:pPr><w:r><w:t>upright</w:t></w:r></w:p>"#;
        let p = preview("pm.docx", "docx", &docx(body), &PreviewOptions::default());
        let line = find(&p, "Word Document", "upright");
        assert!(line.spans.iter().all(|s| !s.bold), "{line:?}");
    }

    #[test]
    fn docx_entities_are_resolved() {
        let body = para("A &amp; B &#233;");
        let p = preview("e.docx", "docx", &docx(&body), &PreviewOptions::default());
        let rendered = texts(&p, "Word Document");
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

    /// A table's rows are lines-to-be, so `max_lines` has to bound the *buffer*
    /// as well as the output — otherwise a million-row table is read whole.
    #[test]
    fn a_long_table_stops_at_max_lines() {
        let rows: Vec<Vec<&str>> = (0..400).map(|_| vec!["a", "b"]).collect();
        let refs: Vec<&[&str]> = rows.iter().map(Vec::as_slice).collect();
        let opts = PreviewOptions {
            max_lines: 12,
            ..PreviewOptions::default()
        };
        let p = preview("tt.docx", "docx", &docx(&table(&refs)), &opts);
        assert!(p.truncated);
        assert!(lines(&p, "Word Document").len() <= 12);
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
        let rendered: Vec<String> = texts(&p, "PowerPoint Presentation")
            .into_iter()
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

    /// A slide's title placeholder is styled like a heading; its body is not.
    #[test]
    fn pptx_titles_are_distinguishable_from_body_text() {
        let slide = r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Quarterly review</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>Opening remark</a:t></a:r></a:p><a:p><a:pPr lvl="1"/><a:r><a:t>Sub point</a:t></a:r></a:p><a:p><a:pPr marL="216000"><a:buChar char="-"/></a:pPr><a:r><a:t>Back out</a:t></a:r></a:p></p:txBody></p:sp>"#;
        let bytes = pptx_raw(&[(1, slide)]);
        let p = preview("titled.pptx", "pptx", &bytes, &PreviewOptions::default());

        let title = find(&p, "PowerPoint Presentation", "Quarterly review");
        assert!(title.spans.iter().all(|s| s.bold), "{title:?}");
        assert_eq!(title.spans[0].fg, Some(super::imp::palette::HEADING[0]));

        let body = find(&p, "PowerPoint Presentation", "Opening remark");
        assert!(!body.spans[0].bold, "{body:?}");
        assert_eq!(body.spans[0].fg, Some(super::imp::palette::TEXT));

        // An outline level of its own is a nested bullet.
        let rendered = texts(&p, "PowerPoint Presentation");
        assert!(
            rendered.iter().any(|l| l == "  - Sub point"),
            "{rendered:?}"
        );
        // And the depth does not leak into the paragraph after it, which
        // carries a bullet but no `lvl` of its own.
        assert!(rendered.iter().any(|l| l == "• Back out"), "{rendered:?}");
    }

    /// DrawingML puts bold and italic on the run's `a:rPr` as attributes, and
    /// slide tables use the same `tbl`/`tr`/`tc` names a docx table does.
    #[test]
    fn pptx_slide_tables_and_run_attributes_are_read() {
        let slide = r#"<p:graphicFrame><a:graphic><a:graphicData><a:tbl><a:tblGrid><a:gridCol w="100"/><a:gridCol w="100"/></a:tblGrid><a:tr h="100"><a:tc><a:txBody><a:p><a:r><a:rPr b="1"/><a:t>Region</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>Total</a:t></a:r></a:p></a:txBody></a:tc></a:tr><a:tr h="100"><a:tc><a:txBody><a:p><a:r><a:t>North</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:rPr i="1"/><a:t>42</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl></a:graphicData></a:graphic></p:graphicFrame>"#;
        let bytes = pptx_raw(&[(1, slide)]);
        let p = preview("grid.pptx", "pptx", &bytes, &PreviewOptions::default());
        let rendered = texts(&p, "PowerPoint Presentation");

        let row = plain(find(&p, "PowerPoint Presentation", "North"));
        assert!(row.contains('│') && row.contains("42"), "{rendered:?}");
        let italic = lines(&p, "PowerPoint Presentation")
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.text == "42")
            .expect("italic cell");
        assert!(italic.italic, "{italic:?}");
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
        let rendered = texts(&p, "PowerPoint Presentation");
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

    /// Tags that never close, and a table cut off mid-row: a preview is not a
    /// validator, so what did arrive is still shown.
    #[test]
    fn an_unterminated_table_still_lays_out_what_it_read() {
        let body = r#"<w:tbl><w:tr><w:tc><w:p><w:r><w:t>alpha</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>beta</w:t></w:r></w:p></w:tc></w:tr>"#;
        let p = preview("cut.docx", "docx", &docx(body), &PreviewOptions::default());
        let row = plain(find(&p, "Word Document", "alpha"));
        assert!(row.contains("beta") && row.contains('│'), "{row}");
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
