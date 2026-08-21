use std::path::Path;

use image::imageops::FilterType;
use image::GenericImageView;

use crate::{CancelToken, MetaField, Preview, PreviewContent, PreviewError, PreviewOptions};

pub fn render(
    path: &Path,
    mime: &str,
    _head: Vec<u8>,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    // Decode from disk (the head sample is usually not the whole file).
    // `image::open` picks the decoder from the file EXTENSION, which would
    // fail on a correctly-detected PNG named `.txt` — detection here is by
    // content, so sniff the format from the bytes instead.
    let img = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?;
    cancel.check()?;

    let exif = read_exif(path);
    cancel.check()?;

    // Phone photos are stored unrotated with an orientation tag; without this
    // they preview sideways.
    let img = apply_orientation(img, exif.orientation);

    let (ow, oh) = img.dimensions();
    let max = opts.image_max_dim;
    let truncated = ow > max || oh > max;
    let img = if truncated {
        img.resize(max, max, FilterType::Triangle)
    } else {
        img
    };
    cancel.check()?;

    Ok(Preview {
        content: PreviewContent::Image {
            image: img.to_rgba8(),
            original_width: ow,
            original_height: oh,
            format: mime.to_string(),
            fields: exif.fields,
        },
        truncated,
    })
}

struct Exif {
    fields: Vec<MetaField>,
    /// EXIF orientation tag, 1..=8. 1 (or absent) means no transform.
    orientation: u16,
}

impl Default for Exif {
    fn default() -> Self {
        Self {
            fields: Vec::new(),
            orientation: 1,
        }
    }
}

fn apply_orientation(img: image::DynamicImage, orientation: u16) -> image::DynamicImage {
    use image::DynamicImage as D;
    match orientation {
        2 => D::fliph(&img),
        3 => D::rotate180(&img),
        4 => D::flipv(&img),
        5 => D::rotate90(&D::fliph(&img)),
        6 => D::rotate90(&img),
        7 => D::rotate270(&D::fliph(&img)),
        8 => D::rotate270(&img),
        _ => img,
    }
}

#[cfg(feature = "exif")]
fn read_exif(path: &Path) -> Exif {
    use exif::{In, Tag};

    // EXIF is a nice-to-have: any failure yields no fields, never an error.
    let Ok(file) = std::fs::File::open(path) else {
        return Exif::default();
    };
    let mut reader = std::io::BufReader::new(file);
    let Ok(data) = exif::Reader::new().read_from_container(&mut reader) else {
        return Exif::default();
    };

    let get = |tag: Tag| -> Option<String> {
        data.get_field(tag, In::PRIMARY)
            .map(|f| f.display_value().with_unit(&data).to_string())
            .map(|s| s.trim_matches('"').trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let orientation = data
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(1) as u16;

    // Camera make and model usually repeat the make ("Canon" / "Canon EOS R").
    let camera = match (get(Tag::Make), get(Tag::Model)) {
        (Some(make), Some(model)) if model.starts_with(&make) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (make, model) => make.or(model),
    };

    let mut fields = Vec::new();
    let mut push = |key: &str, value: Option<String>| {
        if let Some(value) = value {
            fields.push(MetaField::new(key, value));
        }
    };
    push("camera", camera);
    push("lens", get(Tag::LensModel));
    push(
        "taken",
        get(Tag::DateTimeOriginal).or_else(|| get(Tag::DateTime)),
    );
    push("exposure", get(Tag::ExposureTime));
    push("aperture", get(Tag::FNumber));
    push("iso", get(Tag::PhotographicSensitivity));
    push("focal length", get(Tag::FocalLength));

    Exif {
        fields,
        orientation,
    }
}

#[cfg(not(feature = "exif"))]
fn read_exif(_path: &Path) -> Exif {
    Exif::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            w,
            h,
            image::Rgba([255, 0, 0, 255]),
        ))
    }

    #[test]
    fn orientation_1_and_unknown_are_identity() {
        let img = solid(4, 2);
        assert_eq!(apply_orientation(img.clone(), 1).dimensions(), (4, 2));
        assert_eq!(apply_orientation(img, 0).dimensions(), (4, 2));
    }

    #[test]
    fn quarter_turns_swap_dimensions() {
        for orientation in [5, 6, 7, 8] {
            let rotated = apply_orientation(solid(4, 2), orientation);
            assert_eq!(
                rotated.dimensions(),
                (2, 4),
                "orientation {orientation} should swap dimensions"
            );
        }
    }

    #[test]
    fn half_turns_and_flips_preserve_dimensions() {
        for orientation in [2, 3, 4] {
            let flipped = apply_orientation(solid(4, 2), orientation);
            assert_eq!(flipped.dimensions(), (4, 2));
        }
    }

    #[test]
    fn missing_exif_yields_no_fields() {
        // A file that is not an image at all must not panic the EXIF reader.
        let dir = std::env::temp_dir().join("sekio-exif-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-an-image.txt");
        std::fs::write(&path, b"plain text").unwrap();
        let exif = read_exif(&path);
        assert!(exif.fields.is_empty());
        assert_eq!(exif.orientation, 1);
        let _ = std::fs::remove_file(&path);
    }
}
