//! The window icon — what the title bar, the taskbar and Alt-Tab show.
//!
//! Without this eframe falls back to its own bundled egui logo
//! (`load_default_egui_icon`), which is why sekio's window used to wear
//! somebody else's mark.
//!
//! **The source logo is never decoded here.** `assets/sekio_logo.png` is
//! 1254x1254 and 735 KB, and this runs *before the window exists*, on a path
//! whose whole budget is a few milliseconds (`--timing`, `--probe`, ROADMAP
//! "benchmarks"). Measured on this workspace, release build:
//!
//! | decoded | cost |
//! |---|---|
//! | `assets/icons/sekio-64.png` (what is embedded) | **58 us** |
//! | `assets/icons/sekio-256.png` | 730 us |
//! | `assets/sekio_logo.png`, the 1254x1254 source | 7.9 ms |
//!
//! The source would therefore have roughly doubled a ~5 ms cold start on its
//! own. A pre-scaled 64x64 PNG is compiled in instead — see [`PNG`] for why 64
//! and not 256.
//!
//! Nothing in this module may panic or propagate an error: an icon is a
//! decoration, and `sekio-gui --daemon` is a resident process that must outlive
//! anything this trivial going wrong. Every failure path returns `None` and the
//! window opens without one.
//!
//! **What the tests below can and cannot prove.** They run headlessly, so they
//! check that the embedded bytes decode to the dimensions and buffer shape
//! `egui::IconData` requires, and that malformed input degrades to `None`.
//! Nothing here — and nothing in `tests/render.rs` either, which drives the
//! egui `Context` and never a real window — can observe a title bar, a taskbar
//! button or an Alt-Tab entry. That the compositor actually paints this icon is
//! verified by looking at the running app, not by `cargo test`.

/// The embedded window icon: `assets/icons/sekio-64.png`, generated from the
/// source logo by `assets/generate.py`.
///
/// 64x64 rather than 256x256, deliberately:
///
/// * eframe does not display these pixels at their own size anywhere. On
///   Windows it rescales them to `SM_CXICON` (32) and `SM_CXSMICON` (16) to
///   build the two `HICON`s; on X11 the WM scales `_NET_WM_ICON` to whatever
///   the title bar and switcher use. 256x256 measures 13x the decode cost
///   (730 us against 58 us) and holds a 256 KB buffer, for pixels nobody ever
///   shows at that size.
/// * The two places a large icon *is* displayed at size — a Linux dock or app
///   menu, and Explorer / the Start Menu on Windows — do not read this at all.
///   They read `Icon=sekio` from `packaging/sekio.desktop` (hicolor sizes up to
///   256) and the `.ico` resource in `sekio-gui.exe` respectively, both of
///   which ship the larger sizes.
///
/// The width is a multiple of 4, which `egui::IconData` asks for.
pub const PNG: &[u8] = include_bytes!("../../../assets/icons/sekio-64.png");

/// The size [`PNG`] is expected to decode to.
pub const SIZE: u32 = 64;

/// No window icon is worth more than this. Only ever applied to input this
/// crate compiled in itself, but [`decode`] is a public entry point and a
/// bounded decoder is one less way for a resident daemon to be surprised.
const MAX_DIM: u32 = 1024;

/// The icon for `ViewportBuilder::with_icon`, or `None` if it cannot be
/// decoded — in which case the window simply opens without one.
pub fn load() -> Option<egui::IconData> {
    decode(PNG)
}

/// Decode PNG bytes into an `egui::IconData`.
///
/// Returns `None` for anything that is not a decodable PNG of a sane size,
/// including an empty or zero-dimension image, which `egui` would otherwise be
/// handed as a buffer it cannot use.
pub fn decode(bytes: &[u8]) -> Option<egui::IconData> {
    // `with_format` rather than `with_guessed_format`: this only ever loads the
    // PNG above, so there is nothing to sniff and no other decoder to reach.
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    reader.limits(limits);

    // `.ok()?` on every step: a corrupt or truncated icon is a missing icon,
    // never an error the caller has to think about and never a panic.
    let rgba = reader.decode().ok()?.into_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let rgba = rgba.into_raw();
    // A decoded RGBA8 image is always exactly this long; asserting it here
    // means `egui` is never handed a buffer that disagrees with its own
    // dimensions, whatever a future decoder change does.
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_is_a_png() {
        assert_eq!(&PNG[..8], b"\x89PNG\r\n\x1a\n", "not a PNG file");
    }

    #[test]
    fn embedded_icon_decodes_to_the_expected_size() {
        let icon = load().expect("the compiled-in icon must decode");
        assert_eq!((icon.width, icon.height), (SIZE, SIZE));
    }

    #[test]
    fn embedded_icon_has_a_full_rgba_buffer() {
        let icon = load().expect("the compiled-in icon must decode");
        assert!(!icon.rgba.is_empty());
        assert_eq!(icon.rgba.len(), (SIZE as usize) * (SIZE as usize) * 4);
    }

    /// The logo has a transparent background and opaque artwork, so a decode
    /// that produced a blank or fully-transparent buffer — which would still
    /// have the right length — is caught here rather than on screen.
    #[test]
    fn embedded_icon_has_both_opaque_and_transparent_pixels() {
        let icon = load().expect("the compiled-in icon must decode");
        let alpha: Vec<u8> = icon
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[3])
            .collect();
        assert!(alpha.contains(&0), "no transparent background");
        assert!(alpha.iter().any(|&a| a > 250), "nothing opaque was drawn");
    }

    /// `egui::IconData` documents width and height as multiples of 4.
    #[test]
    fn embedded_icon_size_is_a_multiple_of_four() {
        assert_eq!(SIZE % 4, 0);
    }

    #[test]
    fn garbage_decodes_to_none_without_panicking() {
        assert!(decode(b"not a png at all").is_none());
    }

    #[test]
    fn empty_input_decodes_to_none() {
        assert!(decode(&[]).is_none());
    }

    /// The realistic corruption: a valid header and a damaged body, which is
    /// what a truncated write or a bad checkout looks like.
    #[test]
    fn truncated_png_decodes_to_none() {
        let half = &PNG[..PNG.len() / 2];
        assert!(decode(half).is_none());
    }

    #[test]
    fn corrupt_pixel_data_decodes_to_none() {
        let mut corrupted = PNG.to_vec();
        // Past the 8-byte signature and the IHDR chunk, so the header still
        // parses and the failure happens in the compressed stream.
        for byte in corrupted.iter_mut().skip(64) {
            *byte = 0xff;
        }
        assert!(decode(&corrupted).is_none());
    }

    /// A PNG signature and nothing else: the decoder must not read past it.
    #[test]
    fn signature_only_decodes_to_none() {
        assert!(decode(b"\x89PNG\r\n\x1a\n").is_none());
    }
}
