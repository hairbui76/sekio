use std::path::Path;

use crate::{Preview, PreviewContent, PreviewError, PreviewOptions};

const HEX_CAP: usize = 4 * 1024;

/// Degrade to a hexdump when a format renderer fails on a malformed file.
/// Re-reads the head because the original sample was consumed by the renderer.
pub fn fallback(path: &Path, opts: &PreviewOptions) -> Result<Preview, PreviewError> {
    use std::io::Read;
    let mut head = vec![0u8; HEX_CAP.min(opts.max_bytes)];
    let mut file = std::fs::File::open(path)?;
    let n = file.read(&mut head)?;
    head.truncate(n);
    let mime = infer::get(&head).map(|k| k.mime_type().to_string());
    render(path, mime, head, opts)
}

pub fn render(
    path: &Path,
    mime: Option<String>,
    mut head: Vec<u8>,
    _opts: &PreviewOptions,
) -> Result<Preview, PreviewError> {
    let file_size = std::fs::metadata(path)?.len();
    head.truncate(HEX_CAP);
    let truncated = (head.len() as u64) < file_size;

    Ok(Preview {
        content: PreviewContent::HexDump {
            data: head,
            file_size,
            mime,
        },
        truncated,
    })
}
