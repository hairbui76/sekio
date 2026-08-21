//! Renderers turn a detected file into a `PreviewContent` variant.
//!
//! Every renderer must: poll the `CancelToken` at work boundaries, stop
//! reading/decoding at the `PreviewOptions` caps (never load-then-truncate),
//! and set `Preview.truncated` when a cap bites.
//!
//! Feature-gated renderers keep their `#[cfg]` inside their own module and
//! expose a `render` that returns `PreviewError::Format` when compiled out —
//! the dispatcher then degrades to the hexdump.

pub mod archive;
pub mod audio;
pub mod dir;
pub mod hex;
pub mod image;
pub mod markdown;
pub mod pdf;
pub mod svg;
pub mod text;
pub mod video;
