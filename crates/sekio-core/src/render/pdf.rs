//! PDF previews, in two tiers with a graceful chain between them.
//!
//! * **`pdf` (default, pure Rust).** `pdf-extract` pulls the document's text
//!   out and it renders as `PreviewContent::Text`. No C toolchain at build
//!   time and no library at run time, which is why it can be on by default:
//!   a PDF is one of the most common things anyone quick-looks, so it has to
//!   work out of the box.
//! * **`pdf-render` (opt-in at build time, on in every package).** pdfium
//!   draws page 1 to a `PreviewContent::Image`. pdfium is a *dynamically
//!   loaded* native library, so it cannot be a default for `cargo install` —
//!   but the `.deb`, `.rpm` and `.msi` are built with this feature and ship a
//!   copy of the library, so an installed sekio renders pages out of the box.
//!   `bind()` finds that copy beside the executable or in `../lib/sekio/`
//!   without any configuration; `SEKIO_PDFIUM_PATH` overrides it.
//!
//!   This matters most for the file the text tier cannot help with at all: a
//!   scan is images all the way down, so "here is the page" is the only real
//!   preview it has.
//!
//! The chain, in order, and the reason for each step:
//!
//! 1. `pdf-render` compiled in *and* pdfium loadable → the page image.
//! 2. Otherwise `pdf` → the document's text. This includes "pdfium is missing
//!    at run time": showing the prose beats a metadata card that only says the
//!    library is absent.
//! 3. Nothing usable came out — a scan is all images, so extraction honestly
//!    returns nothing — → a `Metadata` card naming the file, its page count and
//!    size, and how to get the page image.
//! 4. Only a genuinely malformed document returns `PreviewError::Format`, which
//!    is what makes the dispatcher fall back to the hexdump.
//!
//! A failure in one tier never escapes as an error while a lower tier can still
//! say something useful. `PreviewError::Cancelled` is the single exception: it
//! always propagates, and is polled on both sides of extraction.
//!
//! Only page 1 is ever drawn, and text extraction stops at the first page that
//! fills `max_lines`/`max_bytes`: a 900-page report must preview as fast as a
//! one-page one.

#[cfg(any(feature = "pdf", feature = "pdf-render"))]
mod imp {
    use std::path::Path;

    use crate::{CancelToken, MetaField, Preview, PreviewContent, PreviewError, PreviewOptions};

    /// Env var pointing at a pdfium shared library (a file, or a directory
    /// containing the platform-named library). Checked before the system paths.
    /// Named in the hint text even in builds without `pdf-render`, so it lives
    /// out here rather than in the page-image module.
    const LIB_PATH_VAR: &str = "SEKIO_PDFIUM_PATH";

    /// How far into the file we look for the `%PDF-` header. The spec puts it at
    /// byte 0; readers in practice tolerate a little leading junk.
    const HEADER_SCAN: usize = 1024;

    pub fn render(
        path: &Path,
        head: Vec<u8>,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        // Cheap structural check first, so a garbage file is rejected as
        // malformed before either tier spends real work on it.
        let head = if head.is_empty() {
            read_head(path)
        } else {
            head
        };
        let version = pdf_version(&head)
            .ok_or_else(|| PreviewError::Format("not a PDF: missing %PDF- header".into()))?;
        cancel.check()?;

        // Tier 2: the page image, when it is both compiled in and loadable.
        match page_image(path, &version, opts, cancel)? {
            #[cfg(feature = "pdf-render")]
            Tier2::Rendered(preview) => Ok(preview),
            Tier2::Unavailable(why) => text_tier(path, &version, why, opts, cancel),
        }
    }

    /// What tier 2 had to say. `Rendered` is the happy path; the rest describe
    /// why there is no image, which tier 3 repeats to the reader.
    ///
    /// The variants are feature-gated rather than merely unused without
    /// `pdf-render`: a build that cannot draw a page can never reach them, and
    /// the Windows cross-check treats an unreachable variant as dead code.
    enum Tier2 {
        #[cfg(feature = "pdf-render")]
        Rendered(Preview),
        Unavailable(NoImage),
    }

    enum NoImage {
        /// The `pdf-render` feature is not compiled in.
        #[cfg(not(feature = "pdf-render"))]
        NotCompiled,
        /// pdfium itself could not be loaded — not the document's fault, so a
        /// degraded preview is the right answer.
        #[cfg(feature = "pdf-render")]
        Library(String),
        /// pdfium loaded and rejected the document. Genuinely malformed, so
        /// with no lower tier to try this becomes `PreviewError::Format`.
        #[cfg(feature = "pdf-render")]
        Document(String),
    }

    // ------------------------------------------------------------ tier 1: text

    /// Text extraction, or the `Metadata` card when there is no text to show.
    #[cfg(feature = "pdf")]
    fn text_tier(
        path: &Path,
        version: &str,
        why: NoImage,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        match text::extract(path, opts, cancel) {
            Ok(text::Extracted::Lines { lines, truncated }) => Ok(Preview {
                content: PreviewContent::Text {
                    lines,
                    language: "PDF".into(),
                },
                truncated,
            }),
            Ok(text::Extracted::NoText { pages, note }) => {
                Ok(metadata(path, version, pages, Some(&note), why))
            }
            Err(PreviewError::Cancelled) => Err(PreviewError::Cancelled),
            // A document even lopdf cannot open is malformed for real: let the
            // dispatcher show the bytes.
            Err(e) => Err(e),
        }
    }

    /// Without `pdf` there is no lower tier, so a broken document is an error
    /// and a merely un-renderable one gets the metadata card. Only reachable
    /// with `pdf-render` on: with both features off this module is not built.
    #[cfg(not(feature = "pdf"))]
    fn text_tier(
        path: &Path,
        version: &str,
        why: NoImage,
        _opts: &PreviewOptions,
        _cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        if let NoImage::Document(e) = &why {
            return Err(PreviewError::Format(e.clone()));
        }
        Ok(metadata(path, version, None, None, why))
    }

    /// The file-size ceiling text extraction refuses to go past, re-exported
    /// for the test that checks the guard actually short-circuits.
    #[cfg(all(test, feature = "pdf"))]
    pub(super) const MAX_EXTRACT_BYTES: u64 = text::MAX_EXTRACT_BYTES;

    #[cfg(feature = "pdf")]
    mod text {
        use std::path::Path;

        use crate::{CancelToken, PreviewError, PreviewOptions, Span, StyledLine};

        /// Palette, harmonised with `render/document.rs` and `render/markdown.rs`
        /// so prose out of a PDF looks like prose out of a docx.
        mod palette {
            pub type Rgb = (u8, u8, u8);

            /// Body text. base05
            pub const TEXT: Rgb = (0xc0, 0xc5, 0xce);
            /// Page separators. base03
            pub const DIM: Rgb = (0x65, 0x73, 0x7e);
        }

        /// Hard ceiling on a file we will run extraction over.
        ///
        /// lopdf parses the *whole* object graph into memory before a single
        /// page can be asked for, so unlike the streaming renderers there is no
        /// way to bound the work from inside — the bound has to be the file
        /// itself. Above this the document is described rather than read, which
        /// is still a useful preview and costs nothing.
        ///
        /// Measured on a release build, worst case (text on every page, so the
        /// load is all real work):
        ///
        /// | file                    | wall  | peak RSS |
        /// |-------------------------|-------|----------|
        /// | 900 pages, 3.0 MB       |  20 ms|    18 MB |
        /// | 8900 pages, 30.3 MiB    | 200 ms|   106 MB |
        /// | 12000 pages, 41.0 MiB   |   0 ms|     9 MB |  <- refused here
        ///
        /// So the ceiling buys a bounded ~0.2 s and ~110 MB, and anything past
        /// it is answered instantly from `stat` alone.
        pub const MAX_EXTRACT_BYTES: u64 = 32 * 1024 * 1024;

        /// Characters kept from one extracted line. Nothing readable is this
        /// wide; a PDF whose text has no line breaks at all would otherwise
        /// hand a frontend one enormous span.
        const MAX_LINE_CHARS: usize = 4096;

        pub enum Extracted {
            Lines {
                lines: Vec<StyledLine>,
                truncated: bool,
            },
            /// Nothing usable came out, with the reason to show the reader.
            /// `pages` is `Some` when the page count was learned on the way.
            NoText { pages: Option<usize>, note: String },
        }

        const NOTE_NO_TEXT: &str =
            "none — the pages hold no extractable text (a scan is images all the way down)";
        const NOTE_ENCRYPTED: &str = "not extracted — the document is password-protected";
        const NOTE_NO_PAGES: &str = "none — the document contains no pages";

        pub fn extract(
            path: &Path,
            opts: &PreviewOptions,
            cancel: &CancelToken,
        ) -> Result<Extracted, PreviewError> {
            let size = std::fs::metadata(path)?.len();
            if size > MAX_EXTRACT_BYTES {
                return Ok(Extracted::NoText {
                    pages: None,
                    note: format!(
                        "not extracted — the file is too large to parse for a preview \
                         (over {} MiB)",
                        MAX_EXTRACT_BYTES / (1024 * 1024)
                    ),
                });
            }
            cancel.check()?;

            // pdf-extract is known to panic on some malformed input (it
            // `unwrap`s a page dictionary and `expect`s a MediaBox), and this
            // runs inside a resident daemon: an unwind must become a `Format`
            // error, never a dead process. Cancellation passes through.
            let extracted = {
                let _quiet = Quiet::new();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(path, opts, cancel)))
            };
            cancel.check()?;

            match extracted {
                Ok(result) => result,
                Err(_) => Err(PreviewError::Format(
                    "malformed PDF: text extraction failed".into(),
                )),
            }
        }

        fn run(
            path: &Path,
            opts: &PreviewOptions,
            cancel: &CancelToken,
        ) -> Result<Extracted, PreviewError> {
            // `Document` is lopdf's, re-exported by pdf-extract.
            let mut doc = pdf_extract::Document::load(path)
                .map_err(|e| PreviewError::Format(format!("malformed PDF: {e}")))?;
            cancel.check()?;

            if doc.is_encrypted() {
                // The common "owner-locked" case has an empty user password.
                // A real one we cannot supply, so describe the file instead.
                if doc.decrypt("").is_err() {
                    return Ok(Extracted::NoText {
                        pages: None,
                        note: NOTE_ENCRYPTED.to_string(),
                    });
                }
            }

            let pages = doc.get_pages();
            let page_count = pages.len();
            if page_count == 0 {
                return Ok(Extracted::NoText {
                    pages: Some(0),
                    note: NOTE_NO_PAGES.to_string(),
                });
            }
            cancel.check()?;

            let mut out = Out::new(opts);
            let mut read = 0usize;
            let mut readable = 0usize;
            // Only as many pages as it takes to fill the caps: this is what
            // keeps a 900-page report as cheap to preview as a one-pager.
            for page_number in pages.keys().copied() {
                cancel.check()?;
                if out.full() {
                    break;
                }
                read += 1;
                let Some(raw) = page_text(&doc, page_number) else {
                    // One unreadable page must not cost us the whole document.
                    continue;
                };
                readable += 1;
                if !raw.trim().is_empty() && page_count > 1 {
                    out.separator(format!("── Page {page_number} ──"));
                }
                out.page(&raw);
            }

            // Every page we opened refused to parse: the object graph loaded but
            // is not a document anyone can read. Show the bytes instead. Guarded
            // on `read` so a zero-line cap is not mistaken for a broken file.
            if read > 0 && readable == 0 {
                return Err(PreviewError::Format(
                    "malformed PDF: no page could be read".into(),
                ));
            }
            cancel.check()?;

            if out.is_blank() {
                return Ok(Extracted::NoText {
                    pages: Some(page_count),
                    note: NOTE_NO_TEXT.to_string(),
                });
            }
            // Pages we never opened are content the reader is not seeing.
            let truncated = out.truncated || read < page_count;
            Ok(Extracted::Lines {
                lines: out.lines,
                truncated,
            })
        }

        // ------------------------------------------------- keeping stderr clean
        //
        // `catch_unwind` above stops a pdf-extract panic from killing us, but by
        // the time it runs the *default panic hook* has already printed
        // "thread 'main' panicked at ...: MediaBox" to stderr. sekio writes
        // previews to stdout and a preview pane shows both, so that is noise
        // about a file we handle perfectly well — and in `--daemon` mode it is
        // noise once per bad PDF, forever.
        //
        // So: install one process-wide hook, the first time extraction runs,
        // that says nothing for a panic raised on a thread currently *inside*
        // extraction and chains to the previous hook for every other panic.
        // The flag is thread-local because the hook runs on the panicking
        // thread, so a genuine panic elsewhere in the process still reports
        // itself in full.
        //
        // pdf-extract writes nothing of its own: it logs through the `log`
        // crate, which is silent with no logger installed, and contains no
        // `print!`/`println!` at all. Verified by running the CLI over a real
        // PDF — zero bytes on stderr.

        thread_local! {
            static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }

        /// Silences panic reporting on this thread for as long as it is alive.
        struct Quiet(bool);

        impl Quiet {
            fn new() -> Self {
                static HOOK: std::sync::Once = std::sync::Once::new();
                HOOK.call_once(|| {
                    let previous = std::panic::take_hook();
                    std::panic::set_hook(Box::new(move |info| {
                        if QUIET.with(|q| q.get()) {
                            return;
                        }
                        previous(info);
                    }));
                });
                // Nested calls must not un-silence the outer one on drop.
                Self(QUIET.with(|q| q.replace(true)))
            }
        }

        impl Drop for Quiet {
            fn drop(&mut self) {
                QUIET.with(|q| q.set(self.0));
            }
        }

        /// The text of one page, or `None` when that page cannot be read.
        fn page_text(doc: &pdf_extract::Document, page_number: u32) -> Option<String> {
            let mut buf = String::new();
            {
                let mut sink = pdf_extract::PlainTextOutput::new(&mut buf);
                pdf_extract::output_doc_page(doc, &mut sink, page_number).ok()?;
            }
            Some(buf)
        }

        /// Line assembler. Bounded by `max_lines` *and* `max_bytes`, so neither
        /// a document with a million short lines nor one with a few enormous
        /// ones can hand a frontend more than the caps allow.
        struct Out<'a> {
            opts: &'a PreviewOptions,
            lines: Vec<StyledLine>,
            bytes: usize,
            truncated: bool,
        }

        impl<'a> Out<'a> {
            fn new(opts: &'a PreviewOptions) -> Self {
                Self {
                    opts,
                    lines: Vec::new(),
                    bytes: 0,
                    truncated: false,
                }
            }

            fn full(&self) -> bool {
                self.lines.len() >= self.opts.max_lines || self.bytes >= self.opts.max_bytes
            }

            /// True while nothing but whitespace has been collected.
            fn is_blank(&self) -> bool {
                self.lines
                    .iter()
                    .all(|l| l.spans.iter().all(|s| s.text.trim().is_empty()))
            }

            fn push(&mut self, text: String, sty: (Option<palette::Rgb>, bool)) {
                if self.full() {
                    self.truncated = true;
                    return;
                }
                self.bytes = self.bytes.saturating_add(text.len());
                let spans = if text.is_empty() {
                    Vec::new()
                } else {
                    vec![Span {
                        text,
                        fg: sty.0,
                        bold: sty.1,
                        italic: false,
                    }]
                };
                self.lines.push(StyledLine { spans });
            }

            /// The `── Page 3 ──` marker between pages of a multi-page document.
            fn separator(&mut self, text: String) {
                if self.full() {
                    self.truncated = true;
                    return;
                }
                if !self.lines.is_empty() && !self.lines.last().is_some_and(is_blank) {
                    self.push(String::new(), (None, false));
                }
                self.push(text, (Some(palette::DIM), true));
            }

            /// Fold one page of extracted text into lines.
            fn page(&mut self, raw: &str) {
                for line in raw.split('\n') {
                    if self.full() {
                        self.truncated = true;
                        return;
                    }
                    let mut text: String = line
                        .chars()
                        .filter(|c| *c != '\r')
                        .take(MAX_LINE_CHARS)
                        .collect();
                    // `take` above may have cut mid-line; only say so if it did.
                    if line.chars().filter(|c| *c != '\r').count() > MAX_LINE_CHARS {
                        self.truncated = true;
                    }
                    while text.ends_with(char::is_whitespace) {
                        text.pop();
                    }
                    // PDF text layers are full of blank runs; a preview pane is
                    // not, and every one of them costs a line of the cap. The
                    // `is_empty` case also trims the leading blank a content
                    // stream that opens with a newline would otherwise produce.
                    if text.is_empty()
                        && (self.lines.is_empty() || self.lines.last().is_some_and(is_blank))
                    {
                        continue;
                    }
                    self.push(text, (Some(palette::TEXT), false));
                }
            }
        }

        fn is_blank(line: &StyledLine) -> bool {
            line.spans.iter().all(|s| s.text.trim().is_empty())
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn blank_runs_collapse_and_lines_are_capped() {
                let opts = PreviewOptions::default();
                let mut out = Out::new(&opts);
                out.page("one\n\n\n\n   \ntwo\n");
                let rendered: Vec<String> = out
                    .lines
                    .iter()
                    .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
                    .collect();
                assert_eq!(rendered, ["one", "", "two", ""]);
            }

            #[test]
            fn a_single_enormous_line_is_bounded() {
                let opts = PreviewOptions::default();
                let mut out = Out::new(&opts);
                out.page(&"x".repeat(MAX_LINE_CHARS * 3));
                assert!(out.truncated);
                assert_eq!(out.lines[0].spans[0].text.chars().count(), MAX_LINE_CHARS);
            }

            #[test]
            fn max_bytes_stops_collection() {
                let opts = PreviewOptions {
                    max_bytes: 64,
                    ..PreviewOptions::default()
                };
                let mut out = Out::new(&opts);
                for _ in 0..50 {
                    out.page("0123456789abcdef\n");
                }
                assert!(out.truncated);
                assert!(out.bytes < 64 + 32, "bytes ran away: {}", out.bytes);
            }
        }
    }

    // ------------------------------------------------------ tier 2: page image

    #[cfg(feature = "pdf-render")]
    fn page_image(
        path: &Path,
        version: &str,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Tier2, PreviewError> {
        image::render(path, version, opts, cancel)
    }

    #[cfg(not(feature = "pdf-render"))]
    fn page_image(
        _path: &Path,
        _version: &str,
        _opts: &PreviewOptions,
        _cancel: &CancelToken,
    ) -> Result<Tier2, PreviewError> {
        Ok(Tier2::Unavailable(NoImage::NotCompiled))
    }

    #[cfg(feature = "pdf-render")]
    mod image {
        use std::path::Path;

        use pdfium_render::prelude::{
            PdfDocumentMetadataTagType, PdfRenderConfig, Pdfium, PdfiumError,
            PdfiumLibraryBindings, Pixels,
        };

        use super::{NoImage, Tier2, LIB_PATH_VAR};
        use crate::{
            CancelToken, MetaField, Preview, PreviewContent, PreviewError, PreviewOptions,
        };

        /// Draw page 1. Only cancellation comes back as an `Err`; everything
        /// else is a `Tier2::Unavailable` for the caller to fall through on.
        pub fn render(
            path: &Path,
            version: &str,
            opts: &PreviewOptions,
            cancel: &CancelToken,
        ) -> Result<Tier2, PreviewError> {
            let bindings = match bind() {
                Ok(bindings) => bindings,
                Err(why) => {
                    return Ok(Tier2::Unavailable(NoImage::Library(format!(
                        "the pdfium library could not be loaded ({})",
                        // `PdfiumError`'s Display is a pretty-printed Debug of
                        // the dlopen error, several lines tall. A metadata field
                        // is one line.
                        one_line(&why.to_string())
                    ))));
                }
            };
            cancel.check()?;

            match draw(&Pdfium::new(bindings), path, version, opts, cancel) {
                Ok(preview) => Ok(Tier2::Rendered(preview)),
                Err(PreviewError::Cancelled) => Err(PreviewError::Cancelled),
                Err(e) => Ok(Tier2::Unavailable(NoImage::Document(one_line(
                    &e.to_string(),
                )))),
            }
        }

        /// Squash a multi-line error into one line: metadata fields are rows in
        /// a table, not paragraphs.
        fn one_line(text: &str) -> String {
            let mut out = String::with_capacity(text.len());
            let mut space = false;
            for c in text.chars() {
                if c.is_whitespace() {
                    space = !out.is_empty();
                } else {
                    if space {
                        out.push(' ');
                        space = false;
                    }
                    out.push(c);
                }
            }
            out
        }

        fn draw(
            pdfium: &Pdfium,
            path: &Path,
            version: &str,
            opts: &PreviewOptions,
            cancel: &CancelToken,
        ) -> Result<Preview, PreviewError> {
            // Not "malformed PDF": pdfium refuses a password-protected file
            // here too, and that document is perfectly well-formed.
            let document = pdfium
                .load_pdf_from_file(path, None)
                .map_err(|e| PreviewError::Format(format!("could not open the document: {e}")))?;
            cancel.check()?;

            // Facts worth showing next to the page image. `metadata()` is a
            // handful of dictionary lookups and `pages().len()` is a single
            // Pdfium call — neither walks the page tree.
            let mut fields = Vec::new();
            for (label, tag) in [
                ("title", PdfDocumentMetadataTagType::Title),
                ("author", PdfDocumentMetadataTagType::Author),
            ] {
                if let Some(value) = document
                    .metadata()
                    .get(tag)
                    .map(|t| t.value().trim().to_string())
                    .filter(|v| !v.is_empty())
                {
                    fields.push(MetaField::new(label, value));
                }
            }
            fields.push(MetaField::new("version", version));

            let pages = document.pages();
            let page_count = pages.len();
            fields.push(MetaField::new("pages", page_count.to_string()));
            // Never iterate the collection: one page, fetched by index, no
            // matter how long the document is. The index is clamped rather
            // than refused — a frontend paging past the end should land on the
            // last page, not on an error.
            let wanted = opts
                .page
                .min(page_count.saturating_sub(1) as usize)
                .min(u16::MAX as usize) as u16;
            let page = pages
                .get(wanted)
                .map_err(|e| PreviewError::Format(format!("malformed PDF: {e}")))?;

            // Which page this is, so a frontend can label it and know when it
            // has reached the end without counting for itself.
            if page_count > 1 {
                fields.push(MetaField::new(
                    "page",
                    format!("{} of {page_count}", wanted as usize + 1),
                ));
            }

            let (ow, oh) = (page.width().value, page.height().value);
            if !(ow.is_finite() && oh.is_finite()) || ow <= 0.0 || oh <= 0.0 {
                return Err(PreviewError::Format(
                    "malformed PDF: page has no usable dimensions".into(),
                ));
            }

            // Ask pdfium for the size we actually want; don't render at full
            // resolution and shrink afterwards.
            // `Pixels` is i32, so clamp before casting: a wrapped negative cap
            // would ask Pdfium for a nonsensical bitmap.
            let max = opts.image_max_dim.clamp(1, i32::MAX as u32) as Pixels;
            let scale = (max as f32 / ow.max(oh)).min(1.0);
            let target_w = ((ow * scale).round() as i64).clamp(1, max as i64) as Pixels;
            let config = PdfRenderConfig::new()
                // Width alone: pdfium keeps the page's aspect ratio.
                .set_target_width(target_w)
                .set_maximum_width(max)
                .set_maximum_height(max)
                .render_form_data(false);

            let bitmap = page
                .render_with_config(&config)
                .map_err(|e| PreviewError::Format(format!("PDF page render failed: {e}")))?;
            cancel.check()?;

            Ok(Preview {
                content: PreviewContent::Image {
                    image: bitmap.as_image().to_rgba8(),
                    original_width: ow.round().max(1.0) as u32,
                    original_height: oh.round().max(1.0) as u32,
                    format: "application/pdf".to_string(),
                    fields,
                },
                // One page of many is a partial view of the document; so is a
                // page we had to scale down to fit `image_max_dim`.
                truncated: page_count > 1 || scale < 1.0,
            })
        }

        /// Bind to pdfium at runtime, in four steps that fail soft into one
        /// another — a candidate that is absent or will not load is simply the
        /// next one's turn, and only the last step's error is ever reported:
        ///
        /// 1. `SEKIO_PDFIUM_PATH`, which always wins.
        /// 2. The directory the running program sits in. This is the Windows
        ///    package layout: `pdfium.dll` beside `sekio-gui.exe`.
        /// 3. `../lib/sekio/` relative to that directory — the `.deb`/`.rpm`
        ///    layout, see [`bundled_candidates`].
        /// 4. The system library.
        ///
        /// Steps 2 and 3 are what make a packaged install render pages with no
        /// configuration at all. The filename is never hardcoded: pdfium-render
        /// derives it per platform (`libpdfium.so`, `pdfium.dll`, ...).
        fn bind() -> Result<Box<dyn PdfiumLibraryBindings>, PdfiumError> {
            if let Some(dir_or_file) = std::env::var_os(LIB_PATH_VAR) {
                if !dir_or_file.is_empty() {
                    let as_given = std::path::PathBuf::from(&dir_or_file);
                    // Accept either the library itself or the directory holding it.
                    let candidates = [
                        Pdfium::pdfium_platform_library_name_at_path(&as_given),
                        as_given,
                    ];
                    for candidate in candidates {
                        if let Ok(bindings) = Pdfium::bind_to_library(&candidate) {
                            return Ok(bindings);
                        }
                    }
                }
            }

            // `current_exe` can fail (a deleted or unreadable image); that is
            // not fatal, it just means there is no bundled copy to find.
            if let Ok(exe) = std::env::current_exe() {
                for candidate in bundled_candidates(&exe) {
                    if let Ok(bindings) = Pdfium::bind_to_library(&candidate) {
                        return Ok(bindings);
                    }
                }
            }

            Pdfium::bind_to_system_library()
        }

        /// Where a pdfium shipped with sekio may sit, in search order, for a
        /// program whose executable is `exe`.
        ///
        /// Two layouts, because the two package formats cannot agree on one:
        ///
        /// * beside the executable — `C:\Program Files\sekio\bin\pdfium.dll`,
        ///   which is how the `.msi` installs it and how a private library is
        ///   normally shipped on Windows;
        /// * `<prefix>/lib/sekio/` — `/usr/bin/sekio` → `/usr/lib/sekio/`,
        ///   because the FHS forbids a shared library under a `bin` directory,
        ///   so the `.deb`/`.rpm` cannot use the first layout.
        ///
        /// Pure, and takes the executable path rather than reading it, so both
        /// layouts are testable from either platform.
        fn bundled_candidates(exe: &Path) -> Vec<std::path::PathBuf> {
            let dir = match exe.parent() {
                // A bare `sekio` has an empty parent rather than none: a
                // candidate built from it would be a relative path resolved
                // against the working directory, which is nobody's layout.
                Some(dir) if !dir.as_os_str().is_empty() => dir,
                _ => return Vec::new(),
            };
            let mut dirs = vec![dir.to_path_buf()];
            if let Some(prefix) = dir.parent() {
                dirs.push(prefix.join("lib").join(LIBDIR));
            }
            dirs.iter()
                .map(Pdfium::pdfium_platform_library_name_at_path)
                .collect()
        }

        /// The private-library directory name under `<prefix>/lib`. Matches the
        /// `assets` entries in `crates/sekio-cli/Cargo.toml`.
        const LIBDIR: &str = "sekio";

        #[cfg(test)]
        mod tests {
            use super::*;

            /// The name pdfium-render expects on *this* platform, so the
            /// assertions below read the same on Linux and Windows.
            fn lib_name() -> String {
                Pdfium::pdfium_platform_library_name()
                    .to_string_lossy()
                    .into_owned()
            }

            #[test]
            fn the_executables_own_directory_comes_first() {
                let candidates = bundled_candidates(Path::new("/opt/sekio/bin/sekio-gui"));
                assert_eq!(
                    candidates.first().map(|p| p.to_string_lossy().into_owned()),
                    Some(format!("/opt/sekio/bin/{}", lib_name())),
                    "{candidates:?}"
                );
            }

            /// The `.deb`/`.rpm` layout: the library lives in `/usr/lib/sekio`
            /// because it may not live in `/usr/bin`.
            #[test]
            fn a_libdir_beside_the_bindir_is_searched_too() {
                let candidates = bundled_candidates(Path::new("/usr/bin/sekio"));
                let rendered: Vec<String> = candidates
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                assert_eq!(
                    rendered,
                    [
                        format!("/usr/bin/{}", lib_name()),
                        format!("/usr/lib/sekio/{}", lib_name()),
                    ]
                );
            }

            /// A bare name has no parent directory to search: the list is empty
            /// rather than a panic or a lookup at the filesystem root.
            #[test]
            fn an_exe_path_with_no_directory_yields_nothing_to_try() {
                assert!(bundled_candidates(Path::new("sekio")).is_empty());
            }
        }
    }

    // ------------------------------------------------------- tier 3: metadata

    /// The last useful thing we can say: this *is* a PDF, here is how big it is
    /// and why you are not looking at its contents.
    fn metadata(
        path: &Path,
        version: &str,
        pages: Option<usize>,
        text_note: Option<&str>,
        why: NoImage,
    ) -> Preview {
        let mut fields = vec![MetaField::new("Type", "PDF document")];
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            fields.push(MetaField::new("Name", name));
        }
        fields.push(MetaField::new("MIME", "application/pdf"));
        fields.push(MetaField::new("PDF version", version));
        if let Some(pages) = pages {
            fields.push(MetaField::new("Pages", pages.to_string()));
        }
        if let Ok(len) = std::fs::metadata(path).map(|m| m.len()) {
            fields.push(MetaField::new("Size", human_size(len)));
        }
        if let Some(note) = text_note {
            fields.push(MetaField::new("Text", note));
        }
        // The hints below assume the normal case is that pdfium *is* there: the
        // packages ship it beside the program. So a missing library is
        // described as the exception it now is, and the way out leads with the
        // install that fits — never "recompile this yourself" first.
        match why {
            #[cfg(feature = "pdf-render")]
            NoImage::Library(e) => {
                fields.push(MetaField::new(
                    "Page preview",
                    format!("unavailable: {e}, so only metadata is shown"),
                ));
                fields.push(MetaField::new(
                    "Hint",
                    format!(
                        "the .deb, .rpm and .msi packages ship pdfium beside the program; \
                         in a build from source, point {LIB_PATH_VAR} at a libpdfium.so / \
                         pdfium.dll or install one on the library path"
                    ),
                ));
            }
            // pdfium is loaded and working — the document is the problem, and
            // there is nothing for the reader to install.
            #[cfg(feature = "pdf-render")]
            NoImage::Document(e) => {
                fields.push(MetaField::new(
                    "Page preview",
                    format!("unavailable: pdfium loaded but could not draw page 1 ({e})"),
                ));
            }
            #[cfg(not(feature = "pdf-render"))]
            NoImage::NotCompiled => {
                fields.push(MetaField::new(
                    "Page preview",
                    "unavailable: this build has no page renderer compiled in",
                ));
                fields.push(MetaField::new(
                    "Hint",
                    format!(
                        "install the .deb, .rpm or .msi package — each enables the page \
                         renderer and ships pdfium — or rebuild with \
                         `--features sekio-core/pdf-render` and point {LIB_PATH_VAR} at a \
                         libpdfium.so / pdfium.dll"
                    ),
                ));
            }
        }

        Preview {
            content: PreviewContent::Metadata {
                fields,
                thumbnail: None,
            },
            truncated: false,
        }
    }

    // ---------------------------------------------------------------- helpers

    /// `%PDF-1.7` -> `1.7`. `None` when no header is present at all.
    fn pdf_version(head: &[u8]) -> Option<String> {
        const MAGIC: &[u8] = b"%PDF-";
        let window = &head[..head.len().min(HEADER_SCAN)];
        let at = window
            .windows(MAGIC.len())
            .position(|w| w == MAGIC)?
            .saturating_add(MAGIC.len());
        let digits: Vec<u8> = window[at..]
            .iter()
            .copied()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .collect();
        Some(match String::from_utf8(digits) {
            Ok(v) if !v.is_empty() => v,
            _ => "unknown".to_string(),
        })
    }

    fn read_head(path: &Path) -> Vec<u8> {
        use std::io::Read;

        let mut buf = Vec::new();
        if let Ok(file) = std::fs::File::open(path) {
            // Failure here just means an empty head: the caller reports it as
            // a malformed PDF, which is the right answer for an unreadable file.
            let _ = file.take(HEADER_SCAN as u64).read_to_end(&mut buf);
        }
        buf
    }

    fn human_size(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{value:.1} {} ({bytes} bytes)", UNITS[unit])
        }
    }
}

#[cfg(any(feature = "pdf", feature = "pdf-render"))]
pub use imp::render;

#[cfg(not(any(feature = "pdf", feature = "pdf-render")))]
pub fn render(
    _path: &std::path::Path,
    _head: Vec<u8>,
    _opts: &crate::PreviewOptions,
    _cancel: &crate::CancelToken,
) -> Result<crate::Preview, crate::PreviewError> {
    Err(crate::PreviewError::Format(
        "pdf support not compiled in".into(),
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{CancelToken, PreviewError, PreviewOptions};

    /// Writes `bytes` to a uniquely named file in the temp dir and removes it
    /// when the guard drops.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, bytes: &[u8]) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static SEQ: AtomicU32 = AtomicU32::new(0);

            let name = format!(
                "sekio-pdf-test-{}-{}-{}.pdf",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed),
                tag
            );
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, bytes).expect("temp file write");
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A real, structurally valid PDF with one page per entry in `pages`, each
    /// showing its lines of text. Built rather than pasted so the xref offsets
    /// and stream lengths are always correct — a hand-typed PDF that lopdf
    /// rejects would test nothing.
    fn build_pdf(pages: &[&[&str]]) -> Vec<u8> {
        // 1 catalog, 2 page tree, 3 font, then (page, contents) per page.
        let first_page_obj = 4usize;
        let kids: String = (0..pages.len())
            .map(|i| format!("{} 0 R ", first_page_obj + i * 2))
            .collect();

        let mut objects: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.trim_end(),
                pages.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];

        for (i, lines) in pages.iter().enumerate() {
            let contents_obj = first_page_obj + i * 2 + 1;
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {contents_obj} 0 R >>"
            ));

            let mut stream = String::from("BT /F1 12 Tf 12 TL 20 80 Td\n");
            for line in lines.iter() {
                // Escape the only two characters a literal string cares about.
                let escaped = line.replace('\\', r"\\").replace(['(', ')'], "");
                stream.push_str(&format!("({escaped}) Tj T*\n"));
            }
            stream.push_str("ET");
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }

        let mut out = Vec::from(&b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n"[..]);
        let mut offsets = Vec::with_capacity(objects.len());
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }

        let startxref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    /// One page reading "sekio pdf".
    fn minimal_pdf() -> Vec<u8> {
        build_pdf(&[&["sekio pdf"]])
    }

    /// A structurally loadable document whose page has no `MediaBox` anywhere
    /// in its tree. pdf-extract `expect`s that key, so this is the input that
    /// makes it panic — the case `catch_unwind` exists for.
    #[cfg(feature = "pdf")]
    fn pdf_that_panics_the_extractor() -> Vec<u8> {
        let mut out = build_pdf(&[&["no mediabox here"]]);
        // Blank the key rather than shortening the file: every xref offset
        // after it must stay correct or lopdf rejects the document before
        // pdf-extract ever sees a page.
        let needle = b"/MediaBox [0 0 200 100]";
        let at = out
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("fixture must contain a MediaBox to remove");
        out[at..at + needle.len()].fill(b' ');
        out
    }

    #[test]
    fn garbage_is_a_format_error_not_a_panic() {
        let junk = b"this is definitely not a pdf, not even a little bit".to_vec();
        let file = TempFile::new("junk", &junk);

        let err = super::render(
            &file.0,
            junk,
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("garbage must not preview");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn empty_file_is_a_format_error() {
        let file = TempFile::new("empty", b"");
        let err = super::render(
            &file.0,
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("an empty file is not a PDF");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    /// A file that gets past the `%PDF-` header check and then falls apart:
    /// this is the input that must reach `PreviewError::Format` — via
    /// `catch_unwind` if pdf-extract panics on it — rather than crashing.
    ///
    /// A build with neither parser compiled in *cannot* tell this from a good
    /// document, and its metadata card is the documented degradation, so the
    /// strict assertion only holds where something can actually read a PDF.
    #[test]
    fn header_without_a_document_is_a_format_error() {
        let bytes = b"%PDF-1.7\nnothing here is a real object\n%%EOF\n".to_vec();
        let file = TempFile::new("headeronly", &bytes);
        let result = super::render(
            &file.0,
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        );
        match result {
            Err(PreviewError::Format(_)) => {}
            // Only reachable with `pdf` off and pdfium not loadable.
            Ok(_) if cfg!(not(feature = "pdf")) => {}
            other => panic!("a header alone is not a document: got {other:?}"),
        }
    }

    /// The known pdf-extract panic must surface as a `Format` error — which is
    /// what sends the dispatcher to the hexdump — and must not take the
    /// process, or any resident daemon hosting it, down with it.
    #[cfg(feature = "pdf")]
    #[test]
    fn an_extractor_panic_becomes_a_format_error() {
        let bytes = pdf_that_panics_the_extractor();
        let file = TempFile::new("panics", &bytes);
        let result = super::render(
            &file.0,
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        );
        match result {
            // pdfium, if installed, renders this page perfectly well.
            Ok(_) => {}
            Err(PreviewError::Format(_)) => {}
            other => panic!("got {other:?}"),
        }
    }

    /// Truncating a valid PDF mid-object is the classic parser-panic input.
    #[test]
    fn truncated_pdf_never_panics() {
        let whole = minimal_pdf();
        for fraction in [2, 3, 4, 8] {
            let cut = whole[..whole.len() / fraction].to_vec();
            let file = TempFile::new("cut", &cut);
            match super::render(
                &file.0,
                cut,
                &PreviewOptions::default(),
                &CancelToken::new(),
            ) {
                Ok(_) => {}
                Err(PreviewError::Format(_)) => {}
                other => panic!("truncated PDF gave {other:?}"),
            }
        }
    }

    /// Cancellation outranks every other outcome — but only where there is a
    /// renderer to cancel: with both features off `render` is the stub.
    #[cfg(any(feature = "pdf", feature = "pdf-render"))]
    #[test]
    fn cancellation_is_never_swallowed() {
        let bytes = minimal_pdf();
        let file = TempFile::new("cancel", &bytes);

        let cancel = CancelToken::new();
        cancel.cancel();
        let err = super::render(&file.0, bytes, &PreviewOptions::default(), &cancel)
            .expect_err("an already-cancelled preview must bail out");
        assert!(matches!(err, PreviewError::Cancelled), "got {err:?}");
    }

    #[cfg(not(any(feature = "pdf", feature = "pdf-render")))]
    #[test]
    fn disabled_build_reports_missing_support() {
        let bytes = minimal_pdf();
        let file = TempFile::new("valid", &bytes);
        let err = super::render(
            &file.0,
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("both pdf features are off");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    // -------------------------------------------------------------- tier 1

    #[cfg(feature = "pdf")]
    mod text {
        use super::*;
        use crate::{Preview, PreviewContent, StyledLine};

        fn preview(bytes: Vec<u8>, opts: &PreviewOptions) -> (TempFile, Preview) {
            let file = TempFile::new("text", &bytes);
            let preview = super::super::render(&file.0, bytes, opts, &CancelToken::new())
                .expect("a valid PDF must preview");
            (file, preview)
        }

        fn lines(p: &Preview) -> &[StyledLine] {
            match &p.content {
                PreviewContent::Text { lines, language } => {
                    assert_eq!(language, "PDF");
                    lines
                }
                other => panic!("expected text content, got {other:?}"),
            }
        }

        fn plain(line: &StyledLine) -> String {
            line.spans.iter().map(|s| s.text.as_str()).collect()
        }

        fn fields(p: &Preview) -> String {
            match &p.content {
                PreviewContent::Metadata { fields, .. } => fields
                    .iter()
                    .map(|f| format!("{}={}", f.key, f.value))
                    .collect::<Vec<_>>()
                    .join("; "),
                other => panic!("expected metadata content, got {other:?}"),
            }
        }

        /// pdfium is not installed on CI, so tier 2 falls through and tier 1
        /// answers. Where it *is* installed the image is the correct answer.
        fn is_image(p: &Preview) -> bool {
            matches!(p.content, PreviewContent::Image { .. })
        }

        #[test]
        fn text_pdf_extracts_its_text() {
            let bytes = build_pdf(&[&["sekio pdf", "second line"]]);
            let (_file, p) = preview(bytes, &PreviewOptions::default());
            if is_image(&p) {
                return;
            }
            let rendered: Vec<String> = lines(&p).iter().map(plain).collect();
            let joined = rendered.join("\n");
            assert!(joined.contains("sekio pdf"), "{joined:?}");
            assert!(joined.contains("second line"), "{joined:?}");
            assert!(!p.truncated, "one short page is not truncated");
        }

        #[test]
        fn multi_page_text_is_separated_by_page() {
            let bytes = build_pdf(&[&["page one text"], &["page two text"]]);
            let (_file, p) = preview(bytes, &PreviewOptions::default());
            if is_image(&p) {
                return;
            }
            let rendered: Vec<String> = lines(&p).iter().map(plain).collect();
            let joined = rendered.join("\n");
            assert!(joined.contains("── Page 1 ──"), "{joined:?}");
            assert!(joined.contains("── Page 2 ──"), "{joined:?}");
            assert!(joined.contains("page two text"), "{joined:?}");
        }

        #[test]
        fn max_lines_truncates_and_flags() {
            // Twenty pages of twenty lines: far more than the cap below, so the
            // renderer must stop early *and* say it did.
            let page: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
            let borrowed: Vec<&str> = page.iter().map(String::as_str).collect();
            let pages: Vec<&[&str]> = (0..20).map(|_| borrowed.as_slice()).collect();
            let opts = PreviewOptions {
                max_lines: 10,
                ..PreviewOptions::default()
            };
            let (_file, p) = preview(build_pdf(&pages), &opts);
            if is_image(&p) {
                return;
            }
            assert!(p.truncated, "the cap bit but truncated was not set");
            assert!(
                lines(&p).len() <= 10,
                "got {} lines for max_lines=10",
                lines(&p).len()
            );
        }

        #[test]
        fn max_bytes_bounds_the_text_kept() {
            let page: Vec<String> = (0..40).map(|i| format!("a fairly long line {i}")).collect();
            let borrowed: Vec<&str> = page.iter().map(String::as_str).collect();
            let pages: Vec<&[&str]> = (0..10).map(|_| borrowed.as_slice()).collect();
            let opts = PreviewOptions {
                max_bytes: 128,
                ..PreviewOptions::default()
            };
            let (_file, p) = preview(build_pdf(&pages), &opts);
            if is_image(&p) {
                return;
            }
            assert!(p.truncated);
            let kept: usize = lines(&p).iter().map(|l| plain(l).len()).sum();
            assert!(kept < 128 + 4096, "kept {kept} bytes for max_bytes=128");
        }

        /// A scan: real pages, no text layer. The reader deserves a card that
        /// says so, not a hexdump and not an error.
        #[test]
        fn a_pdf_with_no_text_explains_itself() {
            let bytes = build_pdf(&[&[], &[]]);
            let (_file, p) = preview(bytes, &PreviewOptions::default());
            if is_image(&p) {
                return;
            }
            let dump = fields(&p);
            assert!(dump.contains("application/pdf"), "{dump}");
            assert!(dump.contains("Pages=2"), "{dump}");
            assert!(dump.contains("Size="), "{dump}");
            assert!(dump.contains("no extractable text"), "{dump}");
            // The way out is always spelled, and it leads with an install
            // rather than a recompile: the packages ship pdfium.
            assert!(dump.contains("pdfium"), "{dump}");
            #[cfg(not(feature = "pdf-render"))]
            {
                assert!(dump.contains(".msi package"), "{dump}");
                assert!(dump.contains("sekio-core/pdf-render"), "{dump}");
                let hint = dump.split("Hint=").nth(1).unwrap_or_default();
                assert!(
                    hint.find("package").unwrap_or(usize::MAX)
                        < hint.find("rebuild").unwrap_or(usize::MAX),
                    "the hint must lead with the install, not the rebuild: {hint}"
                );
            }
        }

        /// The file-size guard: a 900-page report must not be parsed whole just
        /// to fill a preview pane.
        #[test]
        fn oversized_files_are_described_not_parsed() {
            let mut bytes = build_pdf(&[&["sekio pdf"]]);
            bytes.resize(super::super::imp::MAX_EXTRACT_BYTES as usize + 1, b'\n');
            let file = TempFile::new("huge", &bytes);
            let started = std::time::Instant::now();
            let p = super::super::render(
                &file.0,
                bytes[..1024].to_vec(),
                &PreviewOptions::default(),
                &CancelToken::new(),
            )
            .expect("an oversized PDF must degrade, never fail");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "the size guard did not short-circuit"
            );
            if is_image(&p) {
                return;
            }
            let dump = fields(&p);
            assert!(dump.contains("too large"), "{dump}");
        }
    }

    // -------------------------------------------------------------- tier 2

    #[cfg(feature = "pdf-render")]
    mod render_tier {
        use super::*;
        use crate::PreviewContent;

        /// True when a pdfium shared library is actually loadable here. Most
        /// machines (CI included) will say `false`.
        fn pdfium_available() -> bool {
            pdfium_render::prelude::Pdfium::bind_to_system_library().is_ok()
        }

        #[test]
        fn the_page_image_wins_when_pdfium_is_there() {
            let bytes = minimal_pdf();
            let file = TempFile::new("valid", &bytes);

            let preview = super::super::render(
                &file.0,
                bytes,
                &PreviewOptions::default(),
                &CancelToken::new(),
            )
            .expect("a valid PDF must preview or degrade, never fail");

            match preview.content {
                PreviewContent::Image {
                    original_width,
                    original_height,
                    ref format,
                    ref image,
                    ref fields,
                } => {
                    assert!(pdfium_available(), "Image without pdfium?");
                    assert_eq!((original_width, original_height), (200, 100));
                    assert_eq!(format, "application/pdf");
                    let max = PreviewOptions::default().image_max_dim;
                    assert!(image.width() <= max && image.height() <= max);
                    assert!(
                        fields.iter().any(|f| f.key == "pages" && f.value == "1"),
                        "page count must be reported: {fields:?}"
                    );
                }
                // No pdfium: with `pdf` on this is the document's text, and
                // without it the metadata card.
                other => assert!(!pdfium_available(), "no image but pdfium loaded: {other:?}"),
            }
        }

        /// With pdfium unloadable the preview must still say something: the
        /// text when tier 1 is compiled in, the metadata card otherwise.
        #[test]
        fn a_bogus_library_path_falls_through_rather_than_failing() {
            let bytes = minimal_pdf();
            let file = TempFile::new("badlib", &bytes);

            // `SEKIO_PDFIUM_PATH` is process-global, but a value that cannot be
            // loaded is harmless to concurrent tests: binding falls through to
            // the system library exactly as if the var were unset.
            let previous = std::env::var_os("SEKIO_PDFIUM_PATH");
            std::env::set_var("SEKIO_PDFIUM_PATH", "/nonexistent/sekio/libpdfium.so");
            let preview = super::super::render(
                &file.0,
                bytes,
                &PreviewOptions::default(),
                &CancelToken::new(),
            );
            match previous {
                Some(value) => std::env::set_var("SEKIO_PDFIUM_PATH", value),
                None => std::env::remove_var("SEKIO_PDFIUM_PATH"),
            }

            assert!(
                preview.is_ok(),
                "an unloadable library path must degrade, not error: {preview:?}"
            );
        }
    }
}
