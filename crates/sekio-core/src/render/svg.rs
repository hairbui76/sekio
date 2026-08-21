//! SVG rendering (feature `svg`).
//!
//! resvg is pure Rust, so this renderer needs no C toolchain and builds the
//! same on Windows. The `svg` feature gate lives inside this module: with the
//! feature off `render` still exists and returns `PreviewError::Format`, which
//! makes the dispatcher degrade to the hexdump.

use std::path::Path;

#[cfg(feature = "svg")]
use crate::PreviewContent;
use crate::{CancelToken, Preview, PreviewError, PreviewOptions};

/// An SVG has to be parsed whole, so we may have to read past the head sample.
/// Refuse anything absurd rather than slurping a multi-gigabyte "SVG".
#[cfg(feature = "svg")]
const MAX_SVG_BYTES: u64 = 16 * 1024 * 1024;

/// Hard ceiling on the rasterised pixmap, independent of `image_max_dim`.
/// An SVG declaring 100000x100000 must never turn into a giant allocation
/// even if a frontend asks for a huge `image_max_dim`.
#[cfg(feature = "svg")]
const RASTER_DIM_CAP: u32 = 4096;

#[cfg(feature = "svg")]
pub fn render(
    path: &Path,
    head: Vec<u8>,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    use resvg::{tiny_skia, usvg};

    let data = source_bytes(path, head)?;
    cancel.check()?;

    // Never unwrap a parse: malformed markup must surface as Format so the
    // dispatcher can fall back to a hexdump.
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .map_err(|e| PreviewError::Format(format!("svg parse failed: {e}")))?;
    drop(data);
    cancel.check()?;

    // Intrinsic size from viewBox/width/height, before any scaling.
    let size = tree.size();
    let (ow, oh) = (size.width(), size.height());
    let original_width = ow.round().max(1.0) as u32;
    let original_height = oh.round().max(1.0) as u32;

    // Cap the work, not just the output: scale down at raster time so the
    // pixmap is never bigger than the cap, whatever the SVG claims to be.
    let max = opts.image_max_dim.clamp(1, RASTER_DIM_CAP);
    let truncated = original_width > max || original_height > max;
    let scale = if truncated {
        max as f32 / ow.max(oh)
    } else {
        1.0
    };
    let target_w = ((ow * scale).round() as u32).clamp(1, max);
    let target_h = ((oh * scale).round() as u32).clamp(1, max);

    let mut pixmap = tiny_skia::Pixmap::new(target_w, target_h).ok_or_else(|| {
        PreviewError::Format(format!("cannot allocate {target_w}x{target_h} svg canvas"))
    })?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    cancel.check()?;

    Ok(Preview {
        content: PreviewContent::Image {
            image: to_rgba(&pixmap, target_w, target_h)?,
            original_width,
            original_height,
            format: "image/svg+xml".to_string(),
            fields: Vec::new(),
        },
        truncated,
    })
}

/// The head sample is capped at 64 KB, but an SVG only parses as a whole
/// document — re-read from disk when the sample doesn't cover the file.
#[cfg(feature = "svg")]
fn source_bytes(path: &Path, head: Vec<u8>) -> Result<Vec<u8>, PreviewError> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_SVG_BYTES {
        return Err(PreviewError::Format(format!(
            "svg too large to parse: {len} bytes"
        )));
    }
    if head.len() as u64 >= len {
        return Ok(head);
    }
    Ok(std::fs::read(path)?)
}

/// tiny-skia pixmaps hold *premultiplied* RGBA; `RgbaImage` is straight alpha.
/// Un-premultiply every pixel or semi-transparent artwork comes out too dark.
#[cfg(feature = "svg")]
fn to_rgba(
    pixmap: &resvg::tiny_skia::Pixmap,
    width: u32,
    height: u32,
) -> Result<image::RgbaImage, PreviewError> {
    let mut buf = Vec::with_capacity(width as usize * height as usize * 4);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        buf.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    image::RgbaImage::from_raw(width, height, buf)
        .ok_or_else(|| PreviewError::Format("svg raster buffer size mismatch".into()))
}

#[cfg(not(feature = "svg"))]
pub fn render(
    _path: &Path,
    _head: Vec<u8>,
    _opts: &PreviewOptions,
    _cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    Err(PreviewError::Format("svg support not compiled in".into()))
}

#[cfg(all(test, feature = "svg"))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Scratch directory next to the test binary, i.e. inside `target/`.
    fn write_svg(name: &str, body: &str) -> PathBuf {
        let mut dir = std::env::current_exe().expect("test exe path");
        dir.pop();
        dir.push("sekio-svg-tests");
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write svg");
        path
    }

    fn image_of(preview: &Preview) -> &image::RgbaImage {
        match &preview.content {
            PreviewContent::Image { image, .. } => image,
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn simple_rect_keeps_intrinsic_size() {
        let path = write_svg(
            "simple.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
                 <rect width="100" height="50" fill="#0000ff"/>
               </svg>"##,
        );
        let preview = render(
            &path,
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect("render");

        assert!(!preview.truncated);
        match &preview.content {
            PreviewContent::Image {
                image,
                original_width,
                original_height,
                format,
                ..
            } => {
                assert_eq!((*original_width, *original_height), (100, 50));
                assert_eq!(format, "image/svg+xml");
                assert_eq!(image.dimensions(), (100, 50));
                assert_eq!(image.get_pixel(50, 25).0, [0, 0, 255, 255]);
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn viewbox_only_svg_reports_viewbox_size() {
        let path = write_svg(
            "viewbox.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 80">
                 <rect width="200" height="80" fill="#00ff00"/>
               </svg>"##,
        );
        let preview = render(
            &path,
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect("render");
        match preview.content {
            PreviewContent::Image {
                original_width,
                original_height,
                ..
            } => assert_eq!((original_width, original_height), (200, 80)),
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn oversized_svg_is_clamped_and_marked_truncated() {
        let path = write_svg(
            "huge.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="50000">
                 <rect width="100000" height="50000" fill="#ff0000"/>
               </svg>"##,
        );
        let opts = PreviewOptions {
            image_max_dim: 64,
            ..PreviewOptions::default()
        };
        let preview = render(&path, Vec::new(), &opts, &CancelToken::new()).expect("render");

        assert!(preview.truncated);
        match &preview.content {
            PreviewContent::Image {
                image,
                original_width,
                original_height,
                ..
            } => {
                assert_eq!((*original_width, *original_height), (100_000, 50_000));
                assert_eq!(image.dimensions(), (64, 32));
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn raster_is_capped_even_with_a_huge_option() {
        // A frontend asking for an enormous `image_max_dim` must not make us
        // allocate an enormous pixmap for an enormous SVG.
        let path = write_svg(
            "uncapped.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="100000"/>"#,
        );
        let opts = PreviewOptions {
            image_max_dim: u32::MAX,
            ..PreviewOptions::default()
        };
        let preview = render(&path, Vec::new(), &opts, &CancelToken::new()).expect("render");

        assert!(preview.truncated);
        let (w, h) = image_of(&preview).dimensions();
        assert!(w <= RASTER_DIM_CAP && h <= RASTER_DIM_CAP, "{w}x{h}");
    }

    #[test]
    fn half_alpha_fill_is_unpremultiplied() {
        let path = write_svg(
            "alpha.svg",
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40">
                 <rect width="40" height="40" fill="#ff0000" fill-opacity="0.5"/>
               </svg>"##,
        );
        let preview = render(
            &path,
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect("render");

        let px = image_of(&preview).get_pixel(20, 20).0;
        // Premultiplied it would be ~128 red; un-premultiplied it must be ~255.
        assert!(px[0] >= 250, "red channel not un-premultiplied: {px:?}");
        assert!(px[1] < 5 && px[2] < 5, "unexpected colour: {px:?}");
        assert!((120..=136).contains(&px[3]), "unexpected alpha: {px:?}");
    }

    #[test]
    fn truncated_head_is_backfilled_from_disk() {
        let body = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="30" height="30">
                 <title>{}</title>
                 <rect width="30" height="30" fill="#123456"/>
               </svg>"##,
            "p".repeat(4096)
        );
        let path = write_svg("truncated-head.svg", &body);
        // Simulate the 64 KB head sample stopping mid-document.
        let head = body.as_bytes()[..200].to_vec();

        let preview =
            render(&path, head, &PreviewOptions::default(), &CancelToken::new()).expect("render");
        assert_eq!(
            image_of(&preview).get_pixel(15, 15).0,
            [0x12, 0x34, 0x56, 255]
        );
    }

    #[test]
    fn malformed_svg_is_a_format_error_not_a_panic() {
        let path = write_svg("broken.svg", "<svg><rect width=</svg  garbage");
        let err = render(
            &path,
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("malformed svg must error");
        assert!(matches!(err, PreviewError::Format(_)), "{err:?}");
    }

    #[test]
    fn not_an_svg_is_a_format_error() {
        let path = write_svg("notsvg.svg", "just some plain text, not markup at all");
        assert!(matches!(
            render(
                &path,
                Vec::new(),
                &PreviewOptions::default(),
                &CancelToken::new()
            ),
            Err(PreviewError::Format(_))
        ));
    }

    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let path = write_svg(
            "cancel.svg",
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#,
        );
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = render(&path, Vec::new(), &PreviewOptions::default(), &cancel)
            .expect_err("cancelled render must error");
        assert!(matches!(err, PreviewError::Cancelled), "{err:?}");
    }
}
