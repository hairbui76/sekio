use std::path::Path;

use crate::{CancelToken, ListEntry, Preview, PreviewContent, PreviewError, PreviewOptions};

pub fn render(
    path: &Path,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    let mut entries = Vec::new();
    let mut truncated = false;

    for (i, entry) in std::fs::read_dir(path)?.enumerate() {
        if i % 128 == 0 {
            cancel.check()?;
        }
        if entries.len() >= opts.max_entries {
            truncated = true;
            break;
        }
        let entry = entry?;
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        entries.push(ListEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
            size: if is_dir { None } else { meta.map(|m| m.len()) },
        });
    }

    // Directories first, then case-insensitive by name.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(Preview {
        content: PreviewContent::Listing { entries },
        truncated,
    })
}
