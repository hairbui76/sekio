//! One sharp downscaler, shared by every renderer that hands a bitmap to a
//! frontend.
//!
//! `image`'s own `resize` is deliberately not used, for two measured reasons.
//! On a 24 MP JPEG scaled to a 1024 px long edge it costs ~320 ms — more than
//! decoding the file — and its cheapest usable filter, `Triangle`, is bilinear,
//! which leaves photographs visibly soft. `fast_image_resize` does Lanczos3 in
//! ~90 ms at that size and ~165 ms at 3840 px, so asking for a preview the size
//! of the window it will be painted in is now *cheaper* than the 1024 px one
//! was, and sharper at both.
//!
//! It is pure Rust — `std::arch` SIMD, no `-sys` crate — so the no-C-toolchain
//! rule that keeps the Windows cross-build working still holds.
//!
//! Sizing is the caller's business: every renderer passes
//! [`PreviewOptions::image_max_dim`](crate::PreviewOptions::image_max_dim),
//! which frontends set to what they can actually paint.

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::DynamicImage;

/// Scale `img` down so neither side exceeds `max`.
///
/// Never upscales: an image already within `max` is returned untouched, so a
/// frontend asking for more pixels than the file has costs nothing.
pub fn downscale(img: DynamicImage, max: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let max = max.max(1);
    if w == 0 || h == 0 || (w <= max && h <= max) {
        return img;
    }

    // Round rather than truncate, or a 1999x1000 source capped at 1000 would
    // come back 1000x499 and lean by half a pixel.
    let scale = f64::from(max) / f64::from(w.max(h));
    let dw = ((f64::from(w) * scale).round() as u32).clamp(1, max);
    let dh = ((f64::from(h) * scale).round() as u32).clamp(1, max);

    let mut dst = DynamicImage::new(dw, dh, img.color());
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    match Resizer::new().resize(&img, &mut dst, &opts) {
        Ok(()) => dst,
        // A pixel layout the fast path does not speak. Decoders do produce
        // 16-bit and float images, and a slow correct preview beats none, so
        // fall back to `image`'s own resampler rather than failing.
        Err(_) => img.resize(dw, dh, image::imageops::FilterType::Lanczos3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(w: u32, h: u32) -> DynamicImage {
        let mut img = image::RgbImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            // Content the size tests do not look at; it is here so the
            // resampler has real pixels to work on rather than a blank buffer.
            let v = if (x + y) % 2 == 0 { 255 } else { 0 };
            *p = image::Rgb([v, v, v]);
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn the_long_edge_lands_on_the_cap_and_the_shape_is_kept() {
        let out = downscale(photo(4000, 2000), 1000);
        assert_eq!((out.width(), out.height()), (1000, 500));
        let tall = downscale(photo(1000, 4000), 500);
        assert_eq!((tall.width(), tall.height()), (125, 500));
    }

    /// The cap is a ceiling, not a target: blowing a thumbnail up to fill a 4K
    /// window would be worse than showing it small.
    #[test]
    fn an_image_already_small_enough_is_left_alone() {
        let out = downscale(photo(64, 48), 4096);
        assert_eq!((out.width(), out.height()), (64, 48));
        let exact = downscale(photo(1024, 1024), 1024);
        assert_eq!((exact.width(), exact.height()), (1024, 1024));
    }

    /// Nothing here may panic or produce a zero-sized buffer, however absurd
    /// the numbers: the dispatcher's hexdump fallback cannot catch a panic.
    #[test]
    fn absurd_sizes_still_produce_a_usable_image() {
        let sliver = downscale(photo(10_000, 3), 100);
        assert_eq!(sliver.width(), 100);
        assert!(sliver.height() >= 1, "a row must survive: {sliver:?}");
        let zero = downscale(photo(200, 200), 0);
        assert_eq!((zero.width(), zero.height()), (1, 1));
    }

    /// Detail coarser than the new pixel grid has to come through it: a
    /// resampler that got its strides or its filter wrong produces a flat
    /// field, a smear, or noise, and all three look like "the preview is
    /// broken" rather than "the preview is soft".
    ///
    /// (Detail *finer* than the grid — a one-pixel checkerboard halved — is
    /// supposed to average to grey. That is antialiasing, not blur, so it is
    /// deliberately not what this asserts.)
    #[test]
    fn coarse_detail_comes_through_at_full_contrast() {
        // 16 px blocks halved are still 8 px blocks: far above the new grid.
        let mut img = image::RgbImage::new(512, 512);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = if (x / 16 + y / 16) % 2 == 0 { 255 } else { 0 };
            *p = image::Rgb([v, v, v]);
        }
        let out = downscale(DynamicImage::ImageRgb8(img), 256).to_rgb8();

        // Block centres, well away from any edge the filter softens.
        assert!(out.get_pixel(4, 4)[0] > 240, "a light block went dark");
        assert!(out.get_pixel(12, 4)[0] < 15, "a dark block went light");
        assert!(out.get_pixel(4, 12)[0] < 15, "the pattern lost its rows");
    }

    /// Layout, not filtering: a wrong stride or a swapped axis still produces a
    /// plausible-looking image, so pin which half of the picture ends up where.
    #[test]
    fn the_picture_keeps_its_arrangement() {
        let mut img = image::RgbImage::new(400, 400);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = if x < 200 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        let out = downscale(DynamicImage::ImageRgb8(img), 100).to_rgb8();
        assert!(out.get_pixel(10, 50)[0] < 15, "the dark half moved");
        assert!(out.get_pixel(90, 50)[0] > 240, "the light half moved");
    }

    /// Preserving alpha matters for the icons and logos that get previewed:
    /// resampling straight (non-premultiplied) RGBA drags colour out of the
    /// transparent pixels and haloes the edges.
    #[test]
    fn a_transparent_image_keeps_its_transparency() {
        let mut img = image::RgbaImage::new(400, 400);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            let a = if x < 200 { 255 } else { 0 };
            *p = image::Rgba([255, 0, 0, a]);
        }
        let out = downscale(DynamicImage::ImageRgba8(img), 100).to_rgba8();
        assert_eq!((out.width(), out.height()), (100, 100));
        assert_eq!(out.get_pixel(5, 50)[3], 255, "the opaque half lost alpha");
        assert_eq!(out.get_pixel(95, 50)[3], 0, "the clear half gained alpha");
    }
}
