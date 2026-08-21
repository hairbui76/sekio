//! Archive listing: zip, tar, tar.gz/.tgz and plain .gz.
//!
//! Members are listed by walking the archive's directory/headers — the member
//! *data* is never decompressed, and the file is never slurped into memory, so
//! a 4 GB archive costs a seek per header and nothing else.
//!
//! 7z and rar have no pure-Rust reader in the dependency tree; they return
//! `PreviewError::Format` and the dispatcher degrades to a hexdump.

use std::path::Path;

use crate::{CancelToken, Preview, PreviewError, PreviewOptions};

#[cfg(feature = "archive")]
use std::fs::File;
#[cfg(feature = "archive")]
use std::io::{BufReader, Read, Seek, SeekFrom};

#[cfg(feature = "archive")]
use crate::{ListEntry, PreviewContent};

/// How often the cancel token is polled while walking entries.
#[cfg(feature = "archive")]
const CANCEL_INTERVAL: usize = 128;

/// A tar header block. The `ustar` magic lives at offset 257 within it.
#[cfg(feature = "archive")]
const TAR_BLOCK: usize = 512;

#[cfg(feature = "archive")]
pub fn render(
    path: &Path,
    mime: &str,
    head: Vec<u8>,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    match mime {
        "application/zip" => list_zip(path, opts, cancel),
        "application/x-tar" => list_tar(path, opts, cancel),
        // `application/gzip` is ambiguous: a bare .gz and a .tar.gz share the
        // same magic, so the decision is made by peeking at the decompressed
        // stream rather than by trusting the name.
        "application/gzip" => list_gzip(path, &head, opts, cancel),
        other => {
            // Magic we recognise but have no reader for (7z, rar, bzip2, xz,
            // zstd), or a mime that told us nothing. Last chance on extension.
            match extension(path).as_deref() {
                Some("zip") => list_zip(path, opts, cancel),
                Some("tar") => list_tar(path, opts, cancel),
                Some("gz" | "tgz") => list_gzip(path, &head, opts, cancel),
                _ => Err(PreviewError::Format(format!(
                    "no archive reader for {other}"
                ))),
            }
        }
    }
}

#[cfg(not(feature = "archive"))]
pub fn render(
    _path: &Path,
    _mime: &str,
    _head: Vec<u8>,
    _opts: &PreviewOptions,
    _cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    Err(PreviewError::Format(
        "archive support not compiled in".into(),
    ))
}

#[cfg(feature = "archive")]
fn list_zip(
    path: &Path,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    // Parses the central directory only; member data is left on disk.
    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(path)?))
        .map_err(|e| PreviewError::Format(format!("zip: {e}")))?;

    let mut entries = Vec::new();
    let mut truncated = false;
    for i in 0..archive.len() {
        if i % CANCEL_INTERVAL == 0 {
            cancel.check()?;
        }
        if entries.len() >= opts.max_entries {
            truncated = true;
            break;
        }
        // `by_index_raw` reads the local header without setting up a
        // decompressor, so encrypted members list fine without a password.
        let Ok(file) = archive.by_index_raw(i) else {
            continue;
        };
        let is_dir = file.is_dir();
        entries.push(ListEntry {
            // Zip stores `/`-separated names; keep them verbatim so a listing
            // reads the same on Windows and Linux.
            name: file.name().to_string(),
            is_dir,
            size: if is_dir { None } else { Some(file.size()) },
        });
    }

    Ok(finish(entries, truncated))
}

#[cfg(feature = "archive")]
fn list_tar(
    path: &Path,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    let mut archive = tar::Archive::new(BufReader::new(File::open(path)?));
    // Seek past member data instead of reading it — a big tar is then just a
    // walk over its headers.
    let entries = archive
        .entries_with_seek()
        .map_err(|e| PreviewError::Format(format!("tar: {e}")))?;
    collect_tar(entries, opts, cancel)
}

#[cfg(feature = "archive")]
fn list_tar_gz(
    path: &Path,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    // A gzip stream cannot seek, so member data is skipped by reading — but we
    // stop at `max_entries`, so only the prefix we show is ever inflated.
    let decoder = flate2::read::GzDecoder::new(BufReader::new(File::open(path)?));
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|e| PreviewError::Format(format!("tar.gz: {e}")))?;
    collect_tar(entries, opts, cancel)
}

#[cfg(feature = "archive")]
fn collect_tar<'a, R, I>(
    iter: I,
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError>
where
    R: 'a + Read,
    I: Iterator<Item = std::io::Result<tar::Entry<'a, R>>>,
{
    let mut entries = Vec::new();
    let mut truncated = false;
    for (i, entry) in iter.enumerate() {
        if i % CANCEL_INTERVAL == 0 {
            cancel.check()?;
        }
        if entries.len() >= opts.max_entries {
            truncated = true;
            break;
        }
        // Tar has no index to resync against, so a bad header ends the listing.
        let entry = entry.map_err(|e| PreviewError::Format(format!("tar: {e}")))?;
        let is_dir = entry.header().entry_type().is_dir();
        let size = entry.size();
        entries.push(ListEntry {
            // Member names are `/`-separated bytes; decode lossily rather than
            // going through `Path`, which would mean OS-specific separators.
            name: String::from_utf8_lossy(&entry.path_bytes()).into_owned(),
            is_dir,
            size: if is_dir { None } else { Some(size) },
        });
    }

    Ok(finish(entries, truncated))
}

/// `.gz` is either a tarball or a single compressed file. Decide by inflating
/// the first tar block's worth of bytes and looking for the `ustar` magic.
#[cfg(feature = "archive")]
fn list_gzip(
    path: &Path,
    head: &[u8],
    opts: &PreviewOptions,
    cancel: &CancelToken,
) -> Result<Preview, PreviewError> {
    // The head sample usually covers the first block already; only re-read from
    // disk when it inflated to less than a full tar header.
    let mut probe = gz_peek(std::io::Cursor::new(head), TAR_BLOCK);
    if probe.len() < TAR_BLOCK {
        probe = gz_peek(BufReader::new(File::open(path)?), TAR_BLOCK);
    }

    // Pre-POSIX tars carry no magic, so a `.tar.gz`/`.tgz` name is worth a try
    // as well; if the read then fails it really was a single file.
    if looks_like_tar(&probe) || has_tar_gz_name(path) {
        match list_tar_gz(path, opts, cancel) {
            Ok(preview) if !is_empty_listing(&preview) => return Ok(preview),
            Ok(_) => {}
            Err(PreviewError::Cancelled) => return Err(PreviewError::Cancelled),
            Err(_) => {}
        }
    }

    cancel.check()?;
    let name = gz_stored_name(head).unwrap_or_else(|| decompressed_name(path));
    let entry = ListEntry {
        name,
        is_dir: false,
        size: gz_uncompressed_size(path),
    };
    Ok(finish(vec![entry], false))
}

/// Inflate up to `n` bytes. A short read is expected, not an error: `src` may be
/// only the head sample of a much larger file.
#[cfg(feature = "archive")]
fn gz_peek<R: Read>(src: R, n: usize) -> Vec<u8> {
    let mut decoder = flate2::read::GzDecoder::new(src);
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match decoder.read(&mut buf[got..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => got += k,
        }
    }
    buf.truncate(got);
    buf
}

#[cfg(feature = "archive")]
fn looks_like_tar(block: &[u8]) -> bool {
    block.len() >= TAR_BLOCK && &block[257..262] == b"ustar"
}

#[cfg(feature = "archive")]
fn is_empty_listing(preview: &Preview) -> bool {
    matches!(&preview.content, PreviewContent::Listing { entries } if entries.is_empty())
}

/// The name gzip recorded for the file it compressed, if the FNAME field is set.
/// The gzip header sits at the very start, so the head sample always covers it.
#[cfg(feature = "archive")]
fn gz_stored_name(head: &[u8]) -> Option<String> {
    let mut decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(head));
    // The header is parsed lazily; one read is enough to populate it, and the
    // read failing on a truncated sample does not invalidate the header.
    let mut byte = [0u8; 1];
    let _ = decoder.read(&mut byte);
    let name = String::from_utf8_lossy(decoder.header()?.filename()?).into_owned();
    (!name.is_empty()).then_some(name)
}

/// Fallback name for a single-member `.gz`: our own file name without the
/// compression suffix (`foo.txt.gz` -> `foo.txt`, `foo.tgz` -> `foo.tar`).
#[cfg(feature = "archive")]
fn decompressed_name(path: &Path) -> String {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return "(contents)".to_string();
    };
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tgz") && name.len() > 4 {
        return format!("{}.tar", &name[..name.len() - 4]);
    }
    for suffix in [".gz", ".z"] {
        if lower.ends_with(suffix) && name.len() > suffix.len() {
            // Suffixes are ASCII, so the split point is always a char boundary.
            return name[..name.len() - suffix.len()].to_string();
        }
    }
    name
}

/// gzip records the uncompressed size (mod 2^32) in its last four bytes — the
/// same trailer `gzip -l` reads, so we never inflate a file just to measure it.
#[cfg(feature = "archive")]
fn gz_uncompressed_size(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let len = file.seek(SeekFrom::End(0)).ok()?;
    // 10-byte header plus 8-byte trailer is the smallest possible member.
    if len < 18 {
        return None;
    }
    file.seek(SeekFrom::Start(len - 4)).ok()?;
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf).ok()?;
    Some(u64::from(u32::from_le_bytes(buf)))
}

#[cfg(feature = "archive")]
fn has_tar_gz_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz")
}

#[cfg(feature = "archive")]
fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

#[cfg(feature = "archive")]
fn finish(mut entries: Vec<ListEntry>, truncated: bool) -> Preview {
    // Archive formats mark directories with a trailing slash; `dir` entries
    // never carry one. Strip it so frontends can add their own marker without
    // producing "demo//".
    for entry in &mut entries {
        while entry.name.ends_with('/') && entry.name.len() > 1 {
            entry.name.pop();
        }
    }

    // Directories first, then case-insensitive by name — same order as `dir`.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Preview {
        content: PreviewContent::Listing { entries },
        truncated,
    }
}

#[cfg(all(test, feature = "archive"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A file in the system temp dir that deletes itself. Avoids a `tempfile`
    /// dev-dependency for the handful of fixtures these tests need.
    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn temp_file(name: &str, bytes: &[u8]) -> TempFile {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("sekio-arc-{}-{n}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write fixture");
        TempFile(path)
    }

    fn entries_of(preview: &Preview) -> &[ListEntry] {
        match &preview.content {
            PreviewContent::Listing { entries } => entries,
            other => panic!("expected a listing, got {other:?}"),
        }
    }

    fn names(preview: &Preview) -> Vec<&str> {
        entries_of(preview)
            .iter()
            .map(|e| e.name.as_str())
            .collect()
    }

    fn sample_zip() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .add_directory("src", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .start_file("src/main.rs", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"fn main() {}").unwrap();
        writer
            .start_file("README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"hello").unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn sample_tar_gz() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        let mut dir = tar::Header::new_ustar();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_mode(0o755);
        dir.set_size(0);
        builder.append_data(&mut dir, "docs/", &[][..]).unwrap();

        let body = b"hello tar";
        let mut file = tar::Header::new_ustar();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_mode(0o644);
        file.set_size(body.len() as u64);
        builder
            .append_data(&mut file, "docs/readme.txt", &body[..])
            .unwrap();

        builder.into_inner().unwrap().finish().unwrap()
    }

    fn sample_gz(body: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn zip_lists_members_directories_first() {
        let bytes = sample_zip();
        let fixture = temp_file("sample.zip", &bytes);
        let preview = render(
            &fixture.0,
            "application/zip",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap();

        assert!(!preview.truncated);
        // Directory entries drop the archive's trailing slash (see `finish`).
        assert_eq!(names(&preview), ["src", "README.md", "src/main.rs"]);
        let entries = entries_of(&preview);
        assert!(entries[0].is_dir && entries[0].size.is_none());
        assert_eq!(entries[1].size, Some(5));
    }

    #[test]
    fn zip_stops_at_max_entries_and_reports_truncation() {
        let bytes = sample_zip();
        let fixture = temp_file("capped.zip", &bytes);
        let opts = PreviewOptions {
            max_entries: 2,
            ..PreviewOptions::default()
        };
        let preview = render(
            &fixture.0,
            "application/zip",
            bytes,
            &opts,
            &CancelToken::default(),
        )
        .unwrap();

        assert!(preview.truncated);
        assert_eq!(entries_of(&preview).len(), 2);
    }

    #[test]
    fn tar_gz_lists_members() {
        let bytes = sample_tar_gz();
        let fixture = temp_file("sample.tar.gz", &bytes);
        let preview = render(
            &fixture.0,
            "application/gzip",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap();

        assert_eq!(names(&preview), ["docs", "docs/readme.txt"]);
        let entries = entries_of(&preview);
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].size, Some(9));
    }

    #[test]
    fn tar_gz_truncates_at_max_entries() {
        let bytes = sample_tar_gz();
        let fixture = temp_file("capped.tar.gz", &bytes);
        let opts = PreviewOptions {
            max_entries: 1,
            ..PreviewOptions::default()
        };
        let preview = render(
            &fixture.0,
            "application/gzip",
            bytes,
            &opts,
            &CancelToken::default(),
        )
        .unwrap();

        assert!(preview.truncated);
        assert_eq!(entries_of(&preview).len(), 1);
    }

    #[test]
    fn plain_tar_lists_members() {
        // The same tarball, uncompressed, through the `application/x-tar` path.
        let mut builder = tar::Builder::new(Vec::new());
        let body = b"plain";
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(body.len() as u64);
        builder
            .append_data(&mut header, "a/b.txt", &body[..])
            .unwrap();
        let bytes = builder.into_inner().unwrap();

        let fixture = temp_file("sample.tar", &bytes);
        let preview = render(
            &fixture.0,
            "application/x-tar",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap();

        assert_eq!(names(&preview), ["a/b.txt"]);
        assert_eq!(entries_of(&preview)[0].size, Some(5));
    }

    #[test]
    fn plain_gz_lists_one_decompressed_member() {
        let bytes = sample_gz(b"not a tarball, just text\n");
        let fixture = temp_file("notes.txt.gz", &bytes);
        let preview = render(
            &fixture.0,
            "application/gzip",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap();

        let entries = entries_of(&preview);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].name.ends_with("notes.txt"));
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, Some(25));
    }

    #[test]
    fn corrupt_zip_is_a_format_error_not_a_panic() {
        let bytes = {
            let mut b = b"PK\x03\x04".to_vec();
            b.extend(std::iter::repeat_n(0xA5, 512));
            b
        };
        let fixture = temp_file("broken.zip", &bytes);
        let err = render(
            &fixture.0,
            "application/zip",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn corrupt_tar_is_a_format_error_not_a_panic() {
        let bytes = vec![0xA5u8; 4096];
        let fixture = temp_file("broken.tar", &bytes);
        let err = render(
            &fixture.0,
            "application/x-tar",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn truncated_gzip_does_not_panic() {
        let full = sample_tar_gz();
        let bytes = full[..full.len() / 2].to_vec();
        let fixture = temp_file("half.tar.gz", &bytes);
        // Either a partial listing or a Format error is fine; a panic is not.
        match render(
            &fixture.0,
            "application/gzip",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::default(),
        ) {
            Ok(preview) => {
                let _ = entries_of(&preview);
            }
            Err(e) => assert!(matches!(e, PreviewError::Format(_)), "got {e:?}"),
        }
    }

    #[test]
    fn unsupported_format_reports_format_error() {
        let fixture = temp_file("archive.7z", b"7z\xbc\xaf\x27\x1c");
        let err = render(
            &fixture.0,
            "application/x-7z-compressed",
            b"7z\xbc\xaf\x27\x1c".to_vec(),
            &PreviewOptions::default(),
            &CancelToken::default(),
        )
        .unwrap_err();
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn cancellation_is_not_swallowed() {
        let bytes = sample_zip();
        let fixture = temp_file("cancelled.zip", &bytes);
        let cancel = CancelToken::default();
        cancel.cancel();
        let err = render(
            &fixture.0,
            "application/zip",
            bytes,
            &PreviewOptions::default(),
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, PreviewError::Cancelled), "got {err:?}");
    }

    #[test]
    fn decompressed_name_strips_compression_suffix() {
        assert_eq!(decompressed_name(Path::new("/x/foo.txt.gz")), "foo.txt");
        assert_eq!(decompressed_name(Path::new("/x/foo.TGZ")), "foo.tar");
        assert_eq!(decompressed_name(Path::new("/x/foo")), "foo");
    }
}
