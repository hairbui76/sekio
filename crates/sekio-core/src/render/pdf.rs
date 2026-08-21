//! PDF previews: renders page 1 of a document to an image.
//!
//! Two things make this renderer unusual.
//!
//! * pdfium is a *dynamically loaded* native library, so it may simply not be
//!   installed. That is not an error worth failing a preview over: when the
//!   binding fails we fall back to a `PreviewContent::Metadata` card that at
//!   least describes the file and says why no page was drawn. Only a genuinely
//!   malformed document produces `PreviewError::Format`.
//! * Only page 1 is ever touched. A 900-page report must preview as fast as a
//!   one-page one, and the render is requested *at* the target size rather than
//!   drawn full-resolution and downscaled afterwards.

#[cfg(feature = "pdf")]
mod imp {
    use std::path::Path;

    use pdfium_render::prelude::{
        PdfDocumentMetadataTagType, PdfRenderConfig, Pdfium, PdfiumError, PdfiumLibraryBindings,
        Pixels,
    };

    use crate::{CancelToken, MetaField, Preview, PreviewContent, PreviewError, PreviewOptions};

    /// Env var pointing at a pdfium shared library (a file, or a directory
    /// containing the platform-named library). Checked before the system paths.
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
        // malformed whether or not pdfium happens to be installed.
        let head = if head.is_empty() {
            read_head(path)
        } else {
            head
        };
        let version = pdf_version(&head)
            .ok_or_else(|| PreviewError::Format("not a PDF: missing %PDF- header".into()))?;
        cancel.check()?;

        let bindings = match bind() {
            Ok(bindings) => bindings,
            Err(why) => return Ok(metadata_fallback(path, &version, &why)),
        };
        cancel.check()?;

        let pdfium = Pdfium::new(bindings);
        let document = pdfium
            .load_pdf_from_file(path, None)
            .map_err(|e| PreviewError::Format(format!("malformed PDF: {e}")))?;
        cancel.check()?;

        // Facts worth showing next to the page image. `metadata()` is a handful
        // of dictionary lookups and `pages().len()` is a single Pdfium call —
        // neither walks the page tree.
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
        // Never iterate the collection: page 1 only, no matter how long the doc.
        let page = pages
            .first()
            .map_err(|e| PreviewError::Format(format!("malformed PDF: {e}")))?;

        let (ow, oh) = (page.width().value, page.height().value);
        if !(ow.is_finite() && oh.is_finite()) || ow <= 0.0 || oh <= 0.0 {
            return Err(PreviewError::Format(
                "malformed PDF: page has no usable dimensions".into(),
            ));
        }

        // Ask pdfium for the size we actually want; don't render at full
        // resolution and shrink afterwards.
        // `Pixels` is i32, so clamp before casting: a wrapped negative cap would
        // ask Pdfium for a nonsensical bitmap.
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
            // Page 1 of many is a partial view of the document; so is a page we
            // had to scale down to fit `image_max_dim`.
            truncated: page_count > 1 || scale < 1.0,
        })
    }

    /// Bind to pdfium at runtime. `SEKIO_PDFIUM_PATH` wins if it resolves;
    /// otherwise the system library, whose filename pdfium-render derives per
    /// platform (`libpdfium.so`, `pdfium.dll`, ...) — never hardcoded here.
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
        Pdfium::bind_to_system_library()
    }

    /// A useful degraded preview: pdfium is missing, so describe the file
    /// instead of failing outright.
    fn metadata_fallback(path: &Path, version: &str, why: &PdfiumError) -> Preview {
        let mut fields = vec![MetaField::new("Type", "PDF document")];
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            fields.push(MetaField::new("Name", name));
        }
        fields.push(MetaField::new("MIME", "application/pdf"));
        fields.push(MetaField::new("PDF version", version));
        if let Ok(len) = std::fs::metadata(path).map(|m| m.len()) {
            fields.push(MetaField::new("Size", human_size(len)));
        }
        fields.push(MetaField::new(
            "Page preview",
            format!(
                "unavailable: the pdfium library could not be loaded ({why}), \
                 so only metadata is shown"
            ),
        ));
        fields.push(MetaField::new(
            "Hint",
            format!(
                "install pdfium (libpdfium.so / pdfium.dll) on the library path, \
                 or point {LIB_PATH_VAR} at it"
            ),
        ));

        Preview {
            content: PreviewContent::Metadata {
                fields,
                thumbnail: None,
            },
            truncated: false,
        }
    }

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

#[cfg(feature = "pdf")]
pub use imp::render;

#[cfg(not(feature = "pdf"))]
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

    /// A real, structurally valid one-page PDF (200x100 pt). Built rather than
    /// pasted so the xref offsets are always correct.
    fn minimal_pdf() -> Vec<u8> {
        let objects: [&str; 5] = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
            "<< /Length 44 >>\nstream\nBT /F1 24 Tf 20 40 Td (sekio pdf) Tj ET\nendstream",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];

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

    #[cfg(not(feature = "pdf"))]
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
        .expect_err("pdf feature is off");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[cfg(feature = "pdf")]
    mod enabled {
        use super::*;
        use crate::PreviewContent;

        /// True when a pdfium shared library is actually loadable here. Most
        /// machines (CI included) will say `false`.
        fn pdfium_available() -> bool {
            pdfium_render::prelude::Pdfium::bind_to_system_library().is_ok()
        }

        #[test]
        fn valid_pdf_previews_or_degrades_to_metadata() {
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
                PreviewContent::Metadata { ref fields, .. } => {
                    assert!(!pdfium_available(), "Metadata even though pdfium loaded?");
                    let dump = fields
                        .iter()
                        .map(|f| format!("{}={}", f.key, f.value))
                        .collect::<Vec<_>>()
                        .join("; ");
                    assert!(dump.contains("application/pdf"), "{dump}");
                    assert!(dump.contains("1.7"), "{dump}");
                    assert!(dump.contains("pdfium"), "{dump}");
                    assert!(
                        fields.iter().any(|f| f.key == "Size"),
                        "fallback must report file size: {dump}"
                    );
                }
                other => panic!("unexpected content: {other:?}"),
            }
        }

        #[test]
        fn cancellation_is_never_swallowed() {
            let bytes = minimal_pdf();
            let file = TempFile::new("cancel", &bytes);

            let cancel = CancelToken::new();
            cancel.cancel();
            let err = super::super::render(&file.0, bytes, &PreviewOptions::default(), &cancel)
                .expect_err("an already-cancelled preview must bail out");
            assert!(matches!(err, PreviewError::Cancelled), "got {err:?}");
        }

        #[test]
        fn bogus_library_path_still_degrades_gracefully() {
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
