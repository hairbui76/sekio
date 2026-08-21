//! End-to-end tests for the public `Previewer` API: they exercise detection
//! and dispatch together, which the per-renderer unit tests do not.

use std::path::{Path, PathBuf};

use sekio_core::{CancelToken, PreviewContent, PreviewOptions, Previewer};

/// Test files live in a per-test directory that is removed on drop.
struct Fixtures {
    dir: PathBuf,
}

impl Fixtures {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sekio-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        Self { dir }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn preview(path: &Path) -> sekio_core::Preview {
    Previewer::new()
        .preview(path, &PreviewOptions::default(), &CancelToken::new())
        .expect("preview should succeed")
}

#[test]
fn source_file_is_highlighted_text() {
    let fx = Fixtures::new("text");
    let path = fx.write("main.rs", b"fn main() {\n    println!(\"hi\");\n}\n");

    match preview(&path).content {
        PreviewContent::Text { lines, language } => {
            assert_eq!(language, "Rust", "extension should pick the syntax");
            assert_eq!(lines.len(), 3);
            // Highlighting must produce more than one span on a code line.
            assert!(lines[1].spans.len() > 1, "expected highlighted spans");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn directory_lists_children_with_dirs_first() {
    let fx = Fixtures::new("dir");
    fx.write("zebra.txt", b"z");
    fx.write("apple.txt", b"a");
    fx.write("sub/nested.txt", b"n");

    match preview(&fx.dir).content {
        PreviewContent::Listing { entries } => {
            assert_eq!(entries.len(), 3);
            assert!(entries[0].is_dir, "directories sort first");
            assert_eq!(entries[0].name, "sub");
            let files: Vec<_> = entries[1..].iter().map(|e| e.name.as_str()).collect();
            assert_eq!(files, ["apple.txt", "zebra.txt"], "case-insensitive sort");
        }
        other => panic!("expected Listing, got {other:?}"),
    }
}

#[test]
fn unknown_binary_falls_back_to_hexdump() {
    let fx = Fixtures::new("binary");
    // NUL bytes with no recognizable magic.
    let path = fx.write("blob.bin", &[0x00, 0x01, 0x02, 0xFF, 0x00, 0xAB]);

    match preview(&path).content {
        PreviewContent::HexDump {
            data, file_size, ..
        } => {
            assert_eq!(file_size, 6);
            assert_eq!(data, [0x00, 0x01, 0x02, 0xFF, 0x00, 0xAB]);
        }
        other => panic!("expected HexDump, got {other:?}"),
    }
}

#[test]
fn png_is_decoded_as_an_image() {
    let fx = Fixtures::new("png");
    let mut bytes = Vec::new();
    let img = image::RgbaImage::from_pixel(8, 4, image::Rgba([10, 200, 30, 255]));
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("encode png");
    let path = fx.write("pixel.png", &bytes);

    match preview(&path).content {
        PreviewContent::Image {
            original_width,
            original_height,
            format,
            ..
        } => {
            assert_eq!((original_width, original_height), (8, 4));
            assert_eq!(format, "image/png");
        }
        other => panic!("expected Image, got {other:?}"),
    }
}

#[test]
fn extension_is_not_trusted_over_magic_bytes() {
    // A PNG named .txt must still preview as an image: detection is by content.
    let fx = Fixtures::new("liar");
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        2,
        image::Rgba([1, 2, 3, 255]),
    ))
    .write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .expect("encode png");
    let path = fx.write("actually-a-png.txt", &bytes);

    assert!(
        matches!(preview(&path).content, PreviewContent::Image { .. }),
        "magic bytes must win over the extension"
    );
}

#[test]
fn line_cap_truncates_and_reports_it() {
    let fx = Fixtures::new("cap");
    let body = "line\n".repeat(500);
    let path = fx.write("long.txt", body.as_bytes());

    let opts = PreviewOptions {
        max_lines: 10,
        ..Default::default()
    };
    let preview = Previewer::new()
        .preview(&path, &opts, &CancelToken::new())
        .expect("preview");

    assert!(preview.truncated, "hitting the cap must set truncated");
    match preview.content {
        PreviewContent::Text { lines, .. } => assert_eq!(lines.len(), 10),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn already_cancelled_token_aborts_before_work() {
    let fx = Fixtures::new("cancel");
    let path = fx.write("some.txt", b"content\n");

    let cancel = CancelToken::new();
    cancel.cancel();
    let err = Previewer::new()
        .preview(&path, &PreviewOptions::default(), &cancel)
        .expect_err("cancelled token must abort");

    assert!(matches!(err, sekio_core::PreviewError::Cancelled));
}

#[test]
fn missing_file_is_an_io_error_not_a_panic() {
    let err = Previewer::new()
        .preview(
            Path::new("/nonexistent/sekio/definitely-not-here"),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("missing file must error");

    assert!(matches!(err, sekio_core::PreviewError::Io(_)));
}

#[test]
fn previewer_is_reusable_across_files() {
    // The Previewer holds the syntax set; reusing it must not corrupt state
    // between previews (highlighting is stateful per file).
    let fx = Fixtures::new("reuse");
    let rust = fx.write("a.rs", b"fn a() {}\n");
    let text = fx.write("b.txt", b"plain\n");

    let previewer = Previewer::new();
    let opts = PreviewOptions::default();
    for _ in 0..3 {
        let a = previewer
            .preview(&rust, &opts, &CancelToken::new())
            .unwrap();
        let b = previewer
            .preview(&text, &opts, &CancelToken::new())
            .unwrap();
        match (a.content, b.content) {
            (
                PreviewContent::Text { language: la, .. },
                PreviewContent::Text { language: lb, .. },
            ) => {
                assert_eq!(la, "Rust");
                assert_ne!(lb, "Rust", "plain text must not inherit Rust syntax");
            }
            other => panic!("expected two Text previews, got {other:?}"),
        }
    }
}
