//! sekio-core: filetype detection + rendering into a frontend-neutral
//! `PreviewContent` IR. Frontends (CLI/TUI/GUI) only know how to paint the IR.

mod cancel;
mod detect;
mod render;

pub use cancel::CancelToken;
pub use detect::Detected;

use std::path::Path;

/// Limits baked into the core so every frontend inherits them.
/// A preview must never stall on a huge file: cap the work, not just the output.
#[derive(Debug, Clone)]
pub struct PreviewOptions {
    /// Max bytes read from a text/binary file.
    pub max_bytes: usize,
    /// Max lines produced for a text preview.
    pub max_lines: usize,
    /// Longest edge an image is downscaled to before handing to a frontend.
    pub image_max_dim: u32,
    /// Max entries in a directory/archive listing.
    pub max_entries: usize,
}

impl Default for PreviewOptions {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024,
            max_lines: 500,
            image_max_dim: 1024,
            max_entries: 1000,
        }
    }
}

/// A single styled run of text. Colors are 24-bit RGB from the syntect theme;
/// each frontend maps them to its own output (ANSI, ratatui Style, egui Color32).
#[derive(Debug, Clone)]
pub struct Span {
    pub text: String,
    pub fg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Default)]
pub struct StyledLine {
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone)]
pub struct ListEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

/// One labelled fact about a file, for the `Metadata` variant.
#[derive(Debug, Clone)]
pub struct MetaField {
    pub key: String,
    pub value: String,
}

impl MetaField {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// The frontend-neutral intermediate representation.
#[derive(Debug)]
pub enum PreviewContent {
    Text {
        lines: Vec<StyledLine>,
        /// Language name syntect matched, e.g. "Rust".
        language: String,
    },
    Image {
        /// Already downscaled to `image_max_dim`.
        image: image::RgbaImage,
        /// Dimensions of the original file, pre-downscale.
        original_width: u32,
        original_height: u32,
        format: String,
        /// Extra facts to show alongside the image (EXIF camera/date, page
        /// count, video duration). Empty when the format has none.
        fields: Vec<MetaField>,
    },
    Listing {
        entries: Vec<ListEntry>,
    },
    /// Key/value facts about a file we can describe but not render — audio
    /// tags, EXIF, a video's codec/duration. `thumbnail` carries a cover image
    /// or extracted frame when one is available.
    Metadata {
        fields: Vec<MetaField>,
        thumbnail: Option<image::RgbaImage>,
    },
    HexDump {
        data: Vec<u8>,
        file_size: u64,
        mime: Option<String>,
    },
}

#[derive(Debug)]
pub struct Preview {
    pub content: PreviewContent,
    /// True if any limit in `PreviewOptions` cut the content short.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("image decode failed: {0}")]
    Image(#[from] image::ImageError),
    #[error("{0}")]
    Format(String),
    #[error("preview cancelled")]
    Cancelled,
}

/// Reusable previewer. Holds the loaded syntax/theme sets (loading them is the
/// expensive part — construct once, preview many).
pub struct Previewer {
    highlighter: render::text::Highlighter,
}

impl Previewer {
    pub fn new() -> Self {
        Self {
            highlighter: render::text::Highlighter::new(),
        }
    }

    /// Build a previewer whose text renderer uses the named syntax theme.
    /// Returns `None` for an unknown name; `theme_names` lists the valid ones.
    pub fn with_theme(name: &str) -> Option<Self> {
        Some(Self {
            highlighter: render::text::Highlighter::with_theme(name)?,
        })
    }

    /// Every theme name `with_theme` accepts, sorted. Frontends use this to
    /// validate a config value and to offer a choice.
    pub fn theme_names() -> Vec<String> {
        render::text::Highlighter::theme_names()
    }

    /// The theme used when none is named.
    pub const DEFAULT_THEME: &'static str = render::text::DEFAULT_THEME;

    /// Render a preview for `path`. Checks `cancel` at work boundaries so a
    /// frontend can abort a stale request (user already moved to the next file).
    pub fn preview(
        &self,
        path: &Path,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        let detected = detect::detect(path, opts)?;
        cancel.check()?;

        // A format renderer that fails on a malformed file degrades to the
        // hexdump rather than failing the whole preview — a broken zip is
        // still worth showing bytes for. Cancellation is never swallowed.
        macro_rules! or_hex {
            ($e:expr) => {
                match $e {
                    Ok(p) => Ok(p),
                    Err(PreviewError::Cancelled) => Err(PreviewError::Cancelled),
                    Err(_) => render::hex::fallback(path, opts),
                }
            };
        }

        match detected {
            Detected::Directory => render::dir::render(path, opts, cancel),
            Detected::Image { mime, head } => {
                or_hex!(render::image::render(path, &mime, head, opts, cancel))
            }
            Detected::Svg { head } => or_hex!(render::svg::render(path, head, opts, cancel)),
            Detected::Archive { mime, head } => {
                or_hex!(render::archive::render(path, &mime, head, opts, cancel))
            }
            Detected::Spreadsheet { format, head } => {
                or_hex!(render::spreadsheet::render(
                    path, &format, head, opts, cancel
                ))
            }
            // Legacy binary Word/PowerPoint have no pure-Rust reader, so they
            // go to the LibreOffice shell-out instead of the OOXML reader.
            Detected::Document { format, head } if matches!(format.as_str(), "doc" | "ppt") => {
                or_hex!(render::legacy_office::render(
                    path, &format, head, opts, cancel
                ))
            }
            Detected::Document { format, head } => {
                or_hex!(render::document::render(path, &format, head, opts, cancel))
            }
            Detected::Markdown { head } => {
                or_hex!(render::markdown::render(path, head, opts, cancel))
            }
            Detected::Audio { mime, head } => {
                or_hex!(render::audio::render(path, &mime, head, opts, cancel))
            }
            Detected::Pdf { head } => or_hex!(render::pdf::render(path, head, opts, cancel)),
            Detected::Video { mime, head } => {
                or_hex!(render::video::render(path, &mime, head, opts, cancel))
            }
            Detected::Text { head, encoding } => or_hex!(render::text::render(
                &self.highlighter,
                path,
                head,
                encoding,
                opts,
                cancel
            )),
            Detected::Binary { mime, head } => render::hex::render(path, mime, head, opts),
        }
    }
}

impl Default for Previewer {
    fn default() -> Self {
        Self::new()
    }
}
