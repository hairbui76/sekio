use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{PreviewError, PreviewOptions};

/// Bytes sniffed from the head of the file. Passed along to the renderer so
/// small files are never read twice.
pub type Head = Vec<u8>;

/// Character encoding a text file was detected as. Frontends never see this;
/// the text renderer uses it to decode before highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// A legacy encoding named for `encoding_rs::Encoding::for_label`.
    Legacy(&'static str),
}

#[derive(Debug)]
pub enum Detected {
    Directory,
    Archive {
        mime: String,
        head: Head,
    },
    /// xlsx/xlsm, xlsb, legacy xls, ods. `format` is the container we proved
    /// by looking inside, not the extension we were handed.
    Spreadsheet {
        format: String,
        head: Head,
    },
    /// docx/pptx, plus the legacy binary doc/ppt we decline to parse.
    Document {
        format: String,
        head: Head,
    },
    Image {
        mime: String,
        head: Head,
    },
    Svg {
        head: Head,
    },
    Markdown {
        head: Head,
    },
    Audio {
        mime: String,
        head: Head,
    },
    Video {
        mime: String,
        head: Head,
    },
    Pdf {
        head: Head,
    },
    Text {
        head: Head,
        encoding: Encoding,
    },
    Binary {
        mime: Option<String>,
        head: Head,
    },
}

/// How much of the file head is sniffed. Large enough for `infer`'s longest
/// magic offset and a decent encoding-detection sample.
const SNIFF_LEN: usize = 64 * 1024;

/// MIME types `infer` reports that we have a dedicated archive renderer for.
fn is_archive_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/zip"
            | "application/x-tar"
            | "application/gzip"
            | "application/x-bzip2"
            | "application/x-xz"
            | "application/zstd"
            | "application/x-7z-compressed"
            | "application/vnd.rar"
    )
}

/// OLE2 / Compound File Binary header. Shared by every pre-2007 Office format
/// *and* by an encrypted OOXML package, so it says "Office-ish" and no more.
const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Which renderer an office container belongs to, and the format string that
/// renderer dispatches on.
#[cfg_attr(not(feature = "office"), allow(dead_code))]
enum OfficeKind {
    Spreadsheet(&'static str),
    Document(&'static str),
}

/// Cheap gate on the sniff: only files that could plausibly be an office
/// container are worth opening a second time.
fn is_office_container(mime: &str, head: &[u8]) -> bool {
    head.starts_with(&OLE_MAGIC)
        || mime.starts_with("application/vnd.openxmlformats-officedocument.")
        || mime.starts_with("application/vnd.oasis.opendocument.")
        || matches!(
            mime,
            "application/zip"
                | "application/msword"
                | "application/vnd.ms-excel"
                | "application/vnd.ms-powerpoint"
                | "application/vnd.ms-office"
                | "application/x-cfb"
        )
}

/// Identify an office container by its *contents*.
///
/// OOXML and ODF are zips, so the answer is in the central directory: the
/// presence of `word/document.xml`, `xl/workbook.xml(.bin)` or
/// `ppt/presentation.xml` names the format regardless of what the file is
/// called. ODF carries its own `mimetype` member, stored uncompressed.
///
/// Legacy Office is settled the same way: an OLE compound file's *header* is
/// identical for Word, Excel and PowerPoint, but its root directory is not —
/// each application stores its document in a stream of a fixed name, and
/// `render/legacy_office.rs` reads that in a handful of seeks. The extension is
/// only the fallback, for a compound file whose directory says nothing.
#[cfg(feature = "office")]
fn office_kind(path: &Path, head: &[u8], ext: Option<&str>) -> Option<OfficeKind> {
    if head.starts_with(&OLE_MAGIC) {
        let format = crate::render::legacy_office::ole_format(path).or_else(|| ole_ext(ext))?;
        return Some(match format {
            "xls" => OfficeKind::Spreadsheet("xls"),
            other => OfficeKind::Document(other),
        });
    }
    zip_office_kind(path)
}

#[cfg(feature = "office")]
fn ole_ext(ext: Option<&str>) -> Option<&'static str> {
    match ext {
        Some("xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xla" | "xlw") => Some("xls"),
        Some("doc" | "docx" | "docm" | "dot" | "dotx") => Some("doc"),
        Some("ppt" | "pptx" | "pptm" | "pps" | "pot") => Some("ppt"),
        _ => None,
    }
}

#[cfg(not(feature = "office"))]
fn office_kind(_path: &Path, _head: &[u8], _ext: Option<&str>) -> Option<OfficeKind> {
    None
}

/// Read only the central directory — no member is decompressed, so this costs
/// a seek and a parse even on a 200 MB workbook.
#[cfg(feature = "office")]
fn zip_office_kind(path: &Path) -> Option<OfficeKind> {
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(File::open(path).ok()?)).ok()?;

    // Zip member names always use `/`, on Windows as much as on Linux.
    if archive.index_for_name("word/document.xml").is_some() {
        return Some(OfficeKind::Document("docx"));
    }
    if archive.index_for_name("ppt/presentation.xml").is_some() {
        return Some(OfficeKind::Document("pptx"));
    }
    if archive.index_for_name("xl/workbook.xml").is_some() {
        return Some(OfficeKind::Spreadsheet("xlsx"));
    }
    if archive.index_for_name("xl/workbook.bin").is_some() {
        return Some(OfficeKind::Spreadsheet("xlsb"));
    }

    // ODF puts its content type in a stored (uncompressed) `mimetype` member.
    let mut mimetype = String::new();
    let mut member = archive.by_name("mimetype").ok()?;
    Read::take(&mut member, 128)
        .read_to_string(&mut mimetype)
        .ok()?;
    if mimetype.trim() == "application/vnd.oasis.opendocument.spreadsheet" {
        return Some(OfficeKind::Spreadsheet("ods"));
    }
    // odt/odp have no reader here; let them fall through to the archive path.
    None
}

/// Detect by magic bytes first (`infer`), then text heuristics.
/// Extensions only disambiguate formats magic bytes cannot see (SVG and
/// Markdown are both plain text) and pick syntax highlighting later.
pub fn detect(path: &Path, opts: &PreviewOptions) -> Result<Detected, PreviewError> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Ok(Detected::Directory);
    }

    let mut head = vec![0u8; opts.max_bytes.min(SNIFF_LEN)];
    let mut file = File::open(path)?;
    let n = read_fully(&mut file, &mut head)?;
    head.truncate(n);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let ext = ext.as_deref();

    // Legacy Office, before `infer` gets a look in. `infer` can only name an
    // OLE compound file when its root CLSID is set *and* the whole container
    // fits in the sniff sample, so a 400 KB .ppt would otherwise fall through
    // to the hexdump. The magic alone is enough to justify one directory read.
    if head.starts_with(&OLE_MAGIC) {
        match office_kind(path, &head, ext) {
            Some(OfficeKind::Spreadsheet(format)) => {
                return Ok(Detected::Spreadsheet {
                    format: format.to_string(),
                    head,
                })
            }
            Some(OfficeKind::Document(format)) => {
                return Ok(Detected::Document {
                    format: format.to_string(),
                    head,
                })
            }
            // Not an Office container we know (an encrypted OOXML package, an
            // installer): carry on down the normal path.
            None => {}
        }
    }

    if let Some(kind) = infer::get(&head) {
        let mime = kind.mime_type().to_string();
        if mime.starts_with("image/") {
            return Ok(Detected::Image { mime, head });
        }
        if mime.starts_with("audio/") {
            return Ok(Detected::Audio { mime, head });
        }
        if mime.starts_with("video/") {
            return Ok(Detected::Video { mime, head });
        }
        if mime == "application/pdf" {
            return Ok(Detected::Pdf { head });
        }
        // Office documents hide inside generic containers: OOXML and ODF are
        // zips, legacy Office is an OLE compound file. Magic bytes get us as
        // far as "some zip" / "some OLE file", so look *inside* before
        // trusting either the mime or the name. A plain .zip finds nothing in
        // there and carries on to the archive listing below.
        if is_office_container(&mime, &head) {
            match office_kind(path, &head, ext) {
                Some(OfficeKind::Spreadsheet(format)) => {
                    return Ok(Detected::Spreadsheet {
                        format: format.to_string(),
                        head,
                    })
                }
                Some(OfficeKind::Document(format)) => {
                    return Ok(Detected::Document {
                        format: format.to_string(),
                        head,
                    })
                }
                None => {}
            }
        }
        if is_archive_mime(&mime) {
            return Ok(Detected::Archive { mime, head });
        }
        // Known non-previewable magic: binary until a renderer exists.
        return Ok(Detected::Binary {
            mime: Some(mime),
            head,
        });
    }

    // No magic matched. Text-family formats live here.
    match detect_encoding(&head) {
        Some(encoding) => {
            if ext == Some("svg") || looks_like_svg(&head) {
                return Ok(Detected::Svg { head });
            }
            if matches!(ext, Some("md" | "markdown" | "mdown" | "mkd")) {
                return Ok(Detected::Markdown { head });
            }
            Ok(Detected::Text { head, encoding })
        }
        None => Ok(Detected::Binary { mime: None, head }),
    }
}

fn read_fully(file: &mut File, buf: &mut [u8]) -> Result<usize, PreviewError> {
    let mut total = 0;
    loop {
        if total == buf.len() {
            return Ok(total);
        }
        let n = file.read(&mut buf[total..])?;
        if n == 0 {
            return Ok(total);
        }
        total += n;
    }
}

/// Returns the encoding if the buffer looks like text, `None` if it looks
/// binary. UTF-8 is checked directly; otherwise `chardetng` guesses a legacy
/// encoding (CJK codepages matter on Windows), and we reject the guess if the
/// decode still produces control-character noise.
fn detect_encoding(buf: &[u8]) -> Option<Encoding> {
    if buf.is_empty() {
        return Some(Encoding::Utf8);
    }
    // A NUL byte means binary — except in UTF-16, which we detect by BOM.
    if let Some(enc) = bom_encoding(buf) {
        return Some(enc);
    }
    if buf.contains(&0) {
        return None;
    }

    match std::str::from_utf8(buf) {
        Ok(_) => return Some(Encoding::Utf8),
        Err(e) if e.error_len().is_none() && e.valid_up_to() > 0 => {
            // Valid prefix, error only at the tail => truncated sample.
            return Some(Encoding::Utf8);
        }
        Err(_) => {}
    }

    // We're previewing local files, not web content, so ISO-2022-JP and UTF-8
    // guesses are both allowed (the browser-security caveats don't apply).
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(buf, true);
    let guess = detector.guess(None, chardetng::Utf8Detection::Allow);
    let (decoded, _, had_errors) = guess.decode(buf);
    if had_errors || is_control_noise(&decoded) {
        return None;
    }
    Some(Encoding::Legacy(guess.name()))
}

fn bom_encoding(buf: &[u8]) -> Option<Encoding> {
    if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some(Encoding::Utf8)
    } else if buf.starts_with(&[0xFF, 0xFE]) {
        Some(Encoding::Legacy("UTF-16LE"))
    } else if buf.starts_with(&[0xFE, 0xFF]) {
        Some(Encoding::Legacy("UTF-16BE"))
    } else {
        None
    }
}

/// Decoded legacy text that is mostly control characters was really binary
/// that `chardetng` mapped onto some codepage.
fn is_control_noise(s: &str) -> bool {
    let mut control = 0usize;
    let mut total = 0usize;
    for c in s.chars().take(4096) {
        total += 1;
        if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
            control += 1;
        }
    }
    total > 0 && control * 100 / total > 2
}

/// SVG without an `.svg` extension: look for an `<svg` tag near the start,
/// skipping any XML declaration, doctype, or comments.
fn looks_like_svg(buf: &[u8]) -> bool {
    let sample = &buf[..buf.len().min(1024)];
    let text = String::from_utf8_lossy(sample);
    text.contains("<svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_text_is_text() {
        assert_eq!(detect_encoding(b"hello world\n"), Some(Encoding::Utf8));
    }

    #[test]
    fn empty_is_text() {
        assert_eq!(detect_encoding(b""), Some(Encoding::Utf8));
    }

    #[test]
    fn nul_bytes_are_binary() {
        assert_eq!(detect_encoding(&[0x7f, 0x45, 0x4c, 0x46, 0x00, 0x01]), None);
    }

    #[test]
    fn truncated_multibyte_tail_is_text() {
        // "日本語" with the final byte cut off by the sniff window.
        let full = "日本語".as_bytes();
        let cut = &full[..full.len() - 1];
        assert_eq!(detect_encoding(cut), Some(Encoding::Utf8));
    }

    #[test]
    fn utf8_bom_is_utf8() {
        let mut buf = vec![0xEF, 0xBB, 0xBF];
        buf.extend_from_slice(b"hello");
        assert_eq!(detect_encoding(&buf), Some(Encoding::Utf8));
    }

    #[test]
    fn latin1_text_is_legacy_not_binary() {
        // "café" in windows-1252: invalid UTF-8, but valid text.
        let buf = b"caf\xe9 au lait, r\xe9sum\xe9, na\xefve".to_vec();
        assert!(matches!(detect_encoding(&buf), Some(Encoding::Legacy(_))));
    }

    #[test]
    fn svg_sniffed_without_extension() {
        assert!(looks_like_svg(
            br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg">"#
        ));
        assert!(!looks_like_svg(b"<html><body>no svg here</body></html>"));
    }
}
