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
    Archive { mime: String, head: Head },
    Image { mime: String, head: Head },
    Svg { head: Head },
    Markdown { head: Head },
    Audio { mime: String, head: Head },
    Video { mime: String, head: Head },
    Pdf { head: Head },
    Text { head: Head, encoding: Encoding },
    Binary { mime: Option<String>, head: Head },
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
