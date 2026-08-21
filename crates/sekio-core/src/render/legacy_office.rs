//! Legacy binary Office previews: pre-2007 `.doc` (Word) and `.ppt`
//! (PowerPoint), by shelling out to LibreOffice.
//!
//! These are OLE2 compound files, not OOXML — `render/document.rs` reads the
//! modern zip-of-XML packages and deliberately declines these. There is no
//! viable pure-Rust reader for the BIFF-era record streams, and linking one
//! would be a C dependency, so this module follows the pattern
//! `render/video.rs` already established for ffmpeg: find a converter on PATH,
//! run it under a hard deadline while watching the cancel token, kill *and
//! reap* the child on timeout or cancellation, and treat a missing binary as a
//! normal outcome that degrades to a `Metadata` preview.
//!
//! Two details are specific to LibreOffice and worth stating loudly:
//!
//! * **`-env:UserInstallation` is mandatory.** Without a private profile
//!   directory, headless LibreOffice refuses to start whenever the user already
//!   has LibreOffice open, and can disturb that running session. Every
//!   conversion here gets a throwaway profile under the OS temp dir, removed by
//!   the same RAII guard that removes the output directory.
//! * **The two formats need different filters.** Writer exports plain text, so
//!   `.doc` goes through `txt:Text` and is read back directly. Impress has no
//!   plain-text export filter at all (`--convert-to txt` on a `.ppt` fails with
//!   "no export filter ... aborting"), so `.ppt` is converted to `.pptx` and
//!   handed to the existing OOXML reader in `render/document.rs`, which already
//!   knows how to lay slides out. That reuse is why the `office-legacy` feature
//!   enables `office`.
//!
//! The compound-file sniffer at the top of this module is not tied to
//! `office-legacy`: `detect.rs` needs it under plain `office` too, to tell a
//! legacy Word/PowerPoint file from a legacy Excel workbook (which `calamine`
//! reads natively in `render/spreadsheet.rs` and which must keep going there).
//! It is a few hundred bytes of reads and no new dependency.

#[cfg(feature = "office")]
pub use ole::ole_format;

#[cfg(feature = "office-legacy")]
pub use imp::render;

// ---------------------------------------------------------------- OLE sniffer

/// Reads just enough of an OLE2 / Compound File Binary container to name the
/// application that wrote it.
///
/// The 8-byte OLE header is identical for Word, Excel and PowerPoint, so
/// detection by magic alone can only say "Office-ish". The root storage's
/// directory, however, is not ambiguous: each application stores its document
/// in a stream with a fixed, well-known name. Reading it costs a handful of
/// seeks even on a 200 MB file, and unlike the extension it cannot be wrong.
mod ole {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::path::Path;

    const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    const HEADER_LEN: usize = 512;
    /// Every directory entry is exactly this wide, in every CFB version.
    const DIR_ENTRY_LEN: usize = 128;
    /// The header carries 109 DIFAT entries inline. A file needing more than
    /// that has a FAT larger than ~7 MB of sector table; rather than chase the
    /// DIFAT chain we give up and let the caller fall back to the extension.
    const HEADER_DIFAT_ENTRIES: usize = 109;
    /// Stop after this many directory sectors: a preview needs the root
    /// storage's own children, which live at the very front of the directory.
    const MAX_DIR_SECTORS: usize = 64;
    /// Cycle guard for the red-black sibling walk. A malformed file may point
    /// entries at each other in a loop, and this must never spin.
    const MAX_VISITED: usize = 4096;

    /// Sector chain terminators. Anything >= `MAXREGSECT` is not a real sector.
    const MAXREGSECT: u32 = 0xFFFF_FFFA;
    /// Directory entry id meaning "no such sibling/child".
    const NOSTREAM: u32 = 0xFFFF_FFFF;

    const TYPE_STREAM: u8 = 2;
    const TYPE_ROOT: u8 = 5;

    /// `true` when `head` begins with the OLE2 compound-file signature. Says
    /// "some pre-2007 Office container or an encrypted OOXML package" and no
    /// more — use [`ole_format`] to find out which.
    pub fn is_ole(head: &[u8]) -> bool {
        head.starts_with(&MAGIC)
    }

    /// `"doc"`, `"ppt"` or `"xls"` — the format string the dispatcher routes
    /// on — or `None` when this is not a compound file we recognise (an
    /// encrypted OOXML package, an installer, a truncated file, garbage).
    ///
    /// Never panics and never allocates more than one sector at a time.
    #[cfg_attr(not(feature = "office"), allow(dead_code))]
    pub fn ole_format(path: &Path) -> Option<&'static str> {
        let mut file = File::open(path).ok()?;

        let mut header = [0u8; HEADER_LEN];
        read_at(&mut file, 0, &mut header)?;
        if !is_ole(&header) {
            return None;
        }
        // 0xFFFE is the only byte order the format defines; anything else is
        // not a compound file we should be guessing about.
        if u16_at(&header, 28)? != 0xFFFE {
            return None;
        }
        // Only 512-byte (v3) and 4096-byte (v4) sectors exist. Refusing the
        // rest is also what keeps a hostile shift from asking for a 16 MB
        // buffer below.
        let sector_size = match u16_at(&header, 30)? {
            9 => 512usize,
            12 => 4096usize,
            _ => return None,
        };

        let directory = read_directory(&mut file, &header, sector_size)?;
        root_children(&directory)
    }

    /// Concatenate the directory chain into one buffer, bounded by
    /// `MAX_DIR_SECTORS`.
    fn read_directory(file: &mut File, header: &[u8], sector_size: usize) -> Option<Vec<u8>> {
        let mut sector = u32_at(header, 48)?;
        let mut directory = Vec::new();
        let mut buf = vec![0u8; sector_size];

        for _ in 0..MAX_DIR_SECTORS {
            if sector >= MAXREGSECT {
                break;
            }
            // A sector we cannot read ends the chain rather than failing the
            // whole sniff: whatever we already have may well name the format.
            if read_at(file, sector_offset(sector, sector_size)?, &mut buf).is_none() {
                break;
            }
            directory.extend_from_slice(&buf);
            match next_in_chain(file, header, sector_size, sector) {
                Some(next) => sector = next,
                None => break,
            }
        }

        if directory.is_empty() {
            return None;
        }
        Some(directory)
    }

    /// Follow the FAT one link. Only the header's inline DIFAT is consulted —
    /// see `HEADER_DIFAT_ENTRIES`.
    fn next_in_chain(
        file: &mut File,
        header: &[u8],
        sector_size: usize,
        sector: u32,
    ) -> Option<u32> {
        let per_sector = sector_size / 4;
        let difat_index = (sector as usize) / per_sector;
        if difat_index >= HEADER_DIFAT_ENTRIES {
            return None;
        }
        let fat_sector = u32_at(header, 76 + difat_index * 4)?;
        if fat_sector >= MAXREGSECT {
            return None;
        }
        let within = ((sector as usize) % per_sector) * 4;
        let offset = sector_offset(fat_sector, sector_size)?.checked_add(within as u64)?;
        let mut entry = [0u8; 4];
        read_at(file, offset, &mut entry)?;
        Some(u32::from_le_bytes(entry))
    }

    /// Walk the root storage's *direct* children and name the format.
    ///
    /// Only direct children count: an embedded OLE object (a spreadsheet
    /// pasted into a Word document, say) has its own `Workbook` stream, but it
    /// hangs off a sub-storage, not off the root. Scanning the directory array
    /// flat would let that embedded object outvote the real document.
    fn root_children(directory: &[u8]) -> Option<&'static str> {
        let root = entry_at(directory, 0)?;
        if root[66] != TYPE_ROOT {
            return None;
        }

        let (mut word, mut power_point, mut workbook) = (false, false, false);
        let mut stack = vec![u32_at(root, 76)?];
        let mut visited = 0usize;

        while let Some(id) = stack.pop() {
            if id == NOSTREAM {
                continue;
            }
            visited += 1;
            if visited > MAX_VISITED {
                break;
            }
            let Some(entry) = entry_at(directory, id) else {
                continue;
            };
            if matches!(entry[66], TYPE_STREAM | TYPE_ROOT) {
                match entry_name(entry).as_deref() {
                    Some("WordDocument") => word = true,
                    Some("PowerPoint Document") => power_point = true,
                    // "Workbook" is BIFF8, "Book" is BIFF5.
                    Some("Workbook" | "Book") => workbook = true,
                    _ => {}
                }
            }
            // Siblings only — descending into `child` would reach embedded
            // objects, which are not what this file *is*.
            if let (Some(left), Some(right)) = (u32_at(entry, 68), u32_at(entry, 72)) {
                stack.push(left);
                stack.push(right);
            }
        }

        // A dual-format oddity is vanishingly rare, but if one turns up the
        // richer document wins over the workbook rather than the other way
        // round: `render/spreadsheet.rs` would show a Word file as an empty
        // grid, whereas the text conversion still reads a stray table.
        if word {
            Some("doc")
        } else if power_point {
            Some("ppt")
        } else if workbook {
            Some("xls")
        } else {
            None
        }
    }

    fn entry_at(directory: &[u8], id: u32) -> Option<&[u8]> {
        let start = (id as usize).checked_mul(DIR_ENTRY_LEN)?;
        directory.get(start..start.checked_add(DIR_ENTRY_LEN)?)
    }

    /// Entry names are UTF-16LE in the first 64 bytes, with the length in
    /// *bytes* (including the terminating NUL) at offset 64.
    fn entry_name(entry: &[u8]) -> Option<String> {
        let len = u16_at(entry, 64)? as usize;
        if !(2..=64).contains(&len) || !len.is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = (0..len / 2 - 1)
            .map(|i| u16::from_le_bytes([entry[i * 2], entry[i * 2 + 1]]))
            .collect();
        String::from_utf16(&units).ok()
    }

    /// Sector `n` starts one sector-sized region in: the header occupies the
    /// first, padded out to `sector_size` in v4 files.
    fn sector_offset(sector: u32, sector_size: usize) -> Option<u64> {
        (sector as u64)
            .checked_add(1)?
            .checked_mul(sector_size as u64)
    }

    fn read_at(file: &mut File, offset: u64, buf: &mut [u8]) -> Option<()> {
        file.seek(SeekFrom::Start(offset)).ok()?;
        file.read_exact(buf).ok()
    }

    fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
        let bytes = buf.get(at..at + 2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
        let bytes = buf.get(at..at + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::render::legacy_office::testing::{cfb, temp_file, Entry};

        #[test]
        fn ole_magic_is_recognised_and_nothing_else_is() {
            assert!(is_ole(&MAGIC));
            assert!(is_ole(&cfb(&[Entry::stream("WordDocument")])));
            assert!(!is_ole(b"PK\x03\x04not an ole file"));
            assert!(!is_ole(b""));
        }

        #[test]
        fn word_and_powerpoint_streams_name_the_format() {
            let doc = temp_file("word.bin", &cfb(&[Entry::stream("WordDocument")]));
            assert_eq!(ole_format(doc.path()), Some("doc"));

            let ppt = temp_file("deck.bin", &cfb(&[Entry::stream("PowerPoint Document")]));
            assert_eq!(ole_format(ppt.path()), Some("ppt"));
        }

        /// The whole point of sniffing: a legacy workbook must stay a
        /// spreadsheet, whatever it is called, so `render/spreadsheet.rs`
        /// keeps reading it with calamine.
        #[test]
        fn legacy_excel_is_not_claimed_by_the_legacy_office_renderer() {
            for name in ["Workbook", "Book"] {
                let xls = temp_file("sheet.doc", &cfb(&[Entry::stream(name)]));
                assert_eq!(ole_format(xls.path()), Some("xls"), "{name}");
            }
        }

        /// Sibling links are followed, so the stream we want is found wherever
        /// the red-black tree happens to have put it.
        #[test]
        fn siblings_are_walked_not_just_the_first_child() {
            let bytes = cfb(&[
                Entry::stream("\u{5}SummaryInformation").with_siblings(2, 3),
                Entry::stream("Pictures"),
                Entry::stream("PowerPoint Document"),
            ]);
            let file = temp_file("tree.bin", &bytes);
            assert_eq!(ole_format(file.path()), Some("ppt"));
        }

        /// Everything a hostile or broken file can be is `None`, never a panic
        /// and never a hang.
        #[test]
        fn malformed_compound_files_are_declined_without_panicking() {
            let mut header_only = MAGIC.to_vec();
            header_only.extend(std::iter::repeat_n(0u8, 504));
            let cases: Vec<(&str, Vec<u8>)> = vec![
                ("empty", Vec::new()),
                ("magic only", MAGIC.to_vec()),
                ("zeroed header", header_only),
                ("wrong magic", vec![0u8; 2048]),
                (
                    "truncated",
                    cfb(&[Entry::stream("WordDocument")])[..600].to_vec(),
                ),
                ("no known stream", cfb(&[Entry::stream("Nothing")])),
            ];
            for (label, bytes) in cases {
                let file = temp_file("bad.bin", &bytes);
                assert_eq!(ole_format(file.path()), None, "{label}");
            }
        }

        #[test]
        fn a_directory_cycle_terminates() {
            // Two entries pointing at each other as siblings.
            let bytes = cfb(&[
                Entry::stream("A").with_siblings(2, 2),
                Entry::stream("B").with_siblings(1, 1),
            ]);
            let file = temp_file("cycle.bin", &bytes);
            assert_eq!(ole_format(file.path()), None);
        }

        #[test]
        fn a_missing_file_is_none_not_an_error() {
            assert_eq!(
                ole_format(Path::new("sekio-no-such-compound-file-xyzzy.doc")),
                None
            );
        }
    }
}

// ------------------------------------------------------------------ renderer

#[cfg(feature = "office-legacy")]
mod imp {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use crate::{
        CancelToken, MetaField, Preview, PreviewContent, PreviewError, PreviewOptions, Span,
        StyledLine,
    };

    /// Wall-clock budget for one conversion. Measured on this machine a
    /// `.doc` -> `.txt` round trip is 1.5-2.0 s including LibreOffice's cold
    /// start against a fresh profile, so this is generous — its job is only to
    /// make sure a wedged `soffice` is killed rather than waited on.
    const CONVERT_TIMEOUT: Duration = Duration::from_secs(10);
    /// How often the child is polled. Short enough that cancelling a preview
    /// feels instant, long enough not to spin a core.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    /// Characters kept from one converted line, matching `render/document.rs`.
    const MAX_LINE_CHARS: usize = 4096;
    /// Poll the cancel token every this many lines while reading the output.
    const CANCEL_INTERVAL: usize = 64;
    /// A tab becomes this many spaces: frontends paint spans, not tab stops.
    const TAB: &str = "    ";

    /// Harmonised with `render/document.rs` and the base16-ocean.dark syntect
    /// theme, so a converted `.doc` looks like a `.docx`.
    mod palette {
        pub type Rgb = (u8, u8, u8);
        /// Body text. base05
        pub const TEXT: Rgb = (0xc0, 0xc5, 0xce);
        /// The "no text" note. base03
        pub const DIM: Rgb = (0x65, 0x73, 0x7e);
    }

    // ---------------------------------------------------------------- render

    pub fn render(
        path: &Path,
        format: &str,
        _head: Vec<u8>,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        cancel.check()?;

        // `_head` is the detection sample and is deliberately unused: the
        // converter reads the file itself, from disk, by path.
        let kind = Kind::from_format(format)?;

        let Some(bin) = find_libreoffice() else {
            // Not installed. That is a normal outcome, not a failure: describe
            // the file and say what would light a text preview up.
            return Ok(metadata_preview(path, kind));
        };

        // Both temp directories are created here and removed when `work` drops
        // — on success, on every error below, and on cancellation.
        let work = Workspace::create()?;
        let converted = convert(&bin, path, kind, &work, cancel)?;
        cancel.check()?;

        let Some(converted) = converted else {
            // The tool ran but wrote nothing usable. A hexdump of the original
            // bytes tells the reader more than an empty page would, so this is
            // a `Format` error for the dispatcher to fall back on.
            return Err(PreviewError::Format(format!(
                "LibreOffice could not convert this {} (the file may be corrupt)",
                kind.label()
            )));
        };

        match kind {
            Kind::Doc => text_preview(&converted, kind, opts, cancel),
            Kind::Ppt => ooxml_preview(&converted, kind, opts, cancel),
        }
    }

    /// Which legacy format we were handed, and everything that differs
    /// between them.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Doc,
        Ppt,
    }

    impl Kind {
        fn from_format(format: &str) -> Result<Self, PreviewError> {
            match format {
                "doc" => Ok(Self::Doc),
                "ppt" => Ok(Self::Ppt),
                other => Err(PreviewError::Format(format!(
                    "not a legacy binary Office format: {other}"
                ))),
            }
        }

        /// The `--convert-to` argument, and the extension LibreOffice writes.
        ///
        /// Writer owns the plain-text filter, so `.doc` converts straight to
        /// text. Impress has no text export at all — `--convert-to txt` on a
        /// `.ppt` fails outright — so `.ppt` goes to OOXML and is read back by
        /// the pptx path in `render/document.rs`, which lays slides out
        /// properly. The filter is left unnamed there on purpose: LibreOffice
        /// picks "Impress Office Open XML" itself, and that name has moved
        /// between releases while `Text` has not.
        fn target(self) -> (&'static str, &'static str) {
            match self {
                Self::Doc => ("txt:Text", "txt"),
                Self::Ppt => ("pptx", "pptx"),
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Doc => "Word document",
                Self::Ppt => "PowerPoint presentation",
            }
        }

        /// The `language` string frontends show. "(converted)" is there so a
        /// reader knows this text came through LibreOffice and may differ in
        /// layout from what Word or PowerPoint would draw.
        fn language(self) -> &'static str {
            match self {
                Self::Doc => "Word Document (converted)",
                Self::Ppt => "PowerPoint Presentation (converted)",
            }
        }

        fn metadata_label(self) -> &'static str {
            match self {
                Self::Doc => "Word document (binary, pre-2007)",
                Self::Ppt => "PowerPoint presentation (binary, pre-2007)",
            }
        }
    }

    // ------------------------------------------------------------ conversion

    /// Run LibreOffice once. `Ok(None)` means it ran but produced nothing
    /// usable — a soft failure the caller turns into a hexdump.
    fn convert(
        bin: &Path,
        input: &Path,
        kind: Kind,
        work: &Workspace,
        cancel: &CancelToken,
    ) -> Result<Option<PathBuf>, PreviewError> {
        let (filter, extension) = kind.target();

        let mut cmd = base_command(bin);
        cmd.arg("--headless")
            // Not optional. A shared profile makes headless LibreOffice refuse
            // to start while the user has LibreOffice open, and can disturb
            // that session; a throwaway profile makes this preview invisible
            // to the desktop.
            .arg(format!(
                "-env:UserInstallation={}",
                file_url(work.profile())
            ))
            .arg("--convert-to")
            .arg(filter)
            .arg("--outdir")
            .arg(work.out())
            // The only positional argument, absolutised by `arg_path` so a
            // file named `--help.doc` cannot be read as a flag. Passed as its
            // own argv entry: no shell, no quoting.
            .arg(arg_path(input));

        if !run_bounded(cmd, CONVERT_TIMEOUT, cancel)? {
            return Ok(None);
        }
        cancel.check()?;
        Ok(first_output(work.out(), extension))
    }

    /// The converted file, found by extension rather than by guessing at the
    /// name LibreOffice derived from the input.
    fn first_output(dir: &Path, extension: &str) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            let matches = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case(extension));
            if matches
                && std::fs::metadata(&path)
                    .map(|m| m.len() > 0)
                    .unwrap_or(false)
            {
                return Some(path);
            }
        }
        None
    }

    // ---------------------------------------------------------------- output

    /// Read the converted plain text back.
    ///
    /// Reading is bounded by `max_bytes` and stops at `max_lines`: a 200 MB
    /// `.doc` converts to a 200 MB `.txt`, and a preview wants the first few
    /// hundred lines of it, not all of it in memory.
    fn text_preview(
        converted: &Path,
        kind: Kind,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        let mut reader = BufReader::new(File::open(converted)?.take(opts.max_bytes as u64));

        let mut lines: Vec<StyledLine> = Vec::new();
        let mut truncated = false;
        let mut raw = Vec::new();
        let mut first = true;
        let mut read_lines = 0usize;

        while lines.len() < opts.max_lines {
            raw.clear();
            if reader.read_until(b'\n', &mut raw)? == 0 {
                break;
            }
            read_lines += 1;
            if read_lines.is_multiple_of(CANCEL_INTERVAL) {
                cancel.check()?;
            }

            // LibreOffice's Text filter always writes UTF-8, but a lossy
            // decode costs nothing and can never fail on a hostile file.
            let decoded = String::from_utf8_lossy(&raw);
            let mut text = decoded.trim_end_matches(['\n', '\r']);
            if first {
                first = false;
                // The Text filter writes a UTF-8 BOM.
                text = text.trim_start_matches('\u{feff}');
            }

            let trimmed = text.trim_end();
            if trimmed.is_empty() {
                // Converted documents are full of blank runs; a preview pane
                // is not.
                if lines.last().is_none_or(is_blank) {
                    continue;
                }
                lines.push(StyledLine::default());
                continue;
            }

            let clipped: String = trimmed
                .replace('\t', TAB)
                .chars()
                .take(MAX_LINE_CHARS)
                .collect();
            if clipped.chars().count() == MAX_LINE_CHARS {
                truncated = true;
            }
            lines.push(StyledLine {
                spans: vec![span(clipped, palette::TEXT, false)],
            });
        }

        cancel.check()?;

        // Only claim truncation at the line cap if there really was more.
        if lines.len() >= opts.max_lines {
            let mut probe = [0u8; 1];
            truncated |= reader.read(&mut probe).map(|n| n > 0).unwrap_or(false);
        }

        if lines.iter().all(is_blank) {
            // A document that genuinely holds no prose. Saying so beats
            // erroring into a hexdump, which would tell the reader less.
            lines = vec![StyledLine {
                spans: vec![span("(no text in this document)", palette::DIM, false)],
            }];
        }

        Ok(Preview {
            content: PreviewContent::Text {
                lines,
                language: kind.language().to_string(),
            },
            truncated,
        })
    }

    /// Hand the converted OOXML package to the reader that already exists for
    /// it, then relabel the language so the preview is honest about having
    /// gone through a converter.
    fn ooxml_preview(
        converted: &Path,
        kind: Kind,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        let mut preview =
            crate::render::document::render(converted, "pptx", Vec::new(), opts, cancel)?;
        if let PreviewContent::Text { language, .. } = &mut preview.content {
            *language = kind.language().to_string();
        }
        Ok(preview)
    }

    fn is_blank(line: &StyledLine) -> bool {
        line.spans.iter().all(|s| s.text.trim().is_empty())
    }

    fn span(text: impl Into<String>, fg: palette::Rgb, bold: bool) -> Span {
        Span {
            text: text.into(),
            fg: Some(fg),
            bold,
            italic: false,
        }
    }

    /// What a preview looks like with no LibreOffice installed: the facts we
    /// can state without one, plus how to get the rest. Mirrors the missing
    /// ffmpeg path in `render/video.rs`.
    fn metadata_preview(path: &Path, kind: Kind) -> Preview {
        let mut fields = vec![MetaField::new("Format", kind.metadata_label())];
        if let Ok(meta) = std::fs::metadata(path) {
            fields.push(MetaField::new("Size", human_size(meta.len())));
        }
        fields.push(MetaField::new(
            "Text preview",
            "unavailable — install LibreOffice to enable text previews for legacy Office files",
        ));

        Preview {
            content: PreviewContent::Metadata {
                fields,
                thumbnail: None,
            },
            // Nothing was cut short by a `PreviewOptions` cap; a missing
            // external tool is not truncation.
            truncated: false,
        }
    }

    fn human_size(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} B")
        } else {
            format!("{value:.1} {} ({bytes} bytes)", UNITS[unit])
        }
    }

    // ------------------------------------------------- child process control

    /// Detached from our own stdio in every direction: a preview must not
    /// print to the terminal a frontend owns, nor read its stdin.
    fn base_command(bin: &Path) -> Command {
        let mut cmd = Command::new(bin);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    enum Waited {
        Exited(ExitStatus),
        TimedOut,
    }

    /// Spawn and wait under a deadline. Returns whether the child exited
    /// successfully in time; a timeout is a `false`, not an error, because the
    /// caller always has a fallback. Never `Command::output`, which waits
    /// unboundedly.
    fn run_bounded(
        mut cmd: Command,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<bool, PreviewError> {
        cancel.check()?; // boundary: before we spawn anything
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            // On PATH a moment ago and not runnable now (removed mid-flight,
            // permissions). Treat it as "no tool".
            Err(_) => return Ok(false),
        };
        let outcome = wait_bounded(&mut child, timeout, cancel)?;
        cancel.check()?; // boundary: after the child returns
        Ok(matches!(outcome, Waited::Exited(status) if status.success()))
    }

    /// Poll `try_wait` until the child exits, the deadline passes, or the
    /// preview is cancelled. The child is killed and reaped in the latter two
    /// cases — this is the guarantee that a wedged `soffice` cannot hang sekio.
    fn wait_bounded(
        child: &mut Child,
        timeout: Duration,
        cancel: &CancelToken,
    ) -> Result<Waited, PreviewError> {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(Waited::Exited(status)),
                Ok(None) => {}
                Err(e) => {
                    kill_and_reap(child);
                    return Err(PreviewError::Io(e));
                }
            }
            if cancel.is_cancelled() {
                kill_and_reap(child);
                return Err(PreviewError::Cancelled);
            }
            if Instant::now() >= deadline {
                kill_and_reap(child);
                return Ok(Waited::TimedOut);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Always both: `kill` only sends the signal, `wait` is what stops the
    /// child lingering as a zombie for the lifetime of the process.
    fn kill_and_reap(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    // ----------------------------------------------------------- path lookup

    /// `libreoffice` is the wrapper distributions install; `soffice` is the
    /// binary's own name and is what a manual install puts on PATH.
    fn find_libreoffice() -> Option<PathBuf> {
        find_on_path("libreoffice").or_else(|| find_on_path("soffice"))
    }

    /// Resolve `name` against PATH ourselves — a `which` dependency would buy
    /// nothing over `env::split_paths`, which already handles the platform's
    /// separator and Windows quoting rules.
    fn find_on_path(name: &str) -> Option<PathBuf> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            // An empty PATH entry means "the current directory" on some
            // shells; resolving relative to cwd is a well-known foot-gun, so
            // skip it rather than run whatever happens to sit there.
            if dir.as_os_str().is_empty() {
                continue;
            }
            for candidate in candidate_names(name) {
                let full = dir.join(candidate);
                if std::fs::metadata(&full)
                    .map(|m| is_executable(&m))
                    .unwrap_or(false)
                {
                    return Some(full);
                }
            }
        }
        None
    }

    #[cfg(windows)]
    fn candidate_names(name: &str) -> Vec<String> {
        // `.com` first, and this is not pedantry: LibreOffice ships both
        // `soffice.exe` and `soffice.com`, and only the `.com` console front
        // end waits for the conversion to finish. Launching the `.exe` would
        // have us look for an output file the moment it detaches.
        vec![
            format!("{name}.com"),
            format!("{name}.exe"),
            name.to_string(),
        ]
    }

    #[cfg(not(windows))]
    fn candidate_names(name: &str) -> Vec<String> {
        vec![name.to_string()]
    }

    #[cfg(unix)]
    fn is_executable(meta: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    fn is_executable(meta: &std::fs::Metadata) -> bool {
        meta.is_file()
    }

    /// Absolutise a path before handing it to a child process. `soffice` has
    /// no `--` end-of-options marker, so this is what keeps a file named
    /// `--convert-to.doc` from being parsed as a flag: an absolute path can
    /// never begin with `-`.
    fn arg_path(path: &Path) -> PathBuf {
        if let Ok(abs) = std::path::absolute(path) {
            return abs;
        }
        if path.to_string_lossy().starts_with('-') {
            return Path::new(".").join(path);
        }
        path.to_path_buf()
    }

    /// `-env:UserInstallation` takes a URL, not a path. Percent-encoding
    /// matters: the default Windows temp directory sits under a user profile
    /// name that very often contains a space.
    fn file_url(path: &Path) -> String {
        let text = path.to_string_lossy().replace('\\', "/");
        let mut url = String::from("file://");
        // A Windows path starts `C:/...`, so it needs the root slash adding;
        // a Unix path already has one.
        if !text.starts_with('/') {
            url.push('/');
        }
        for byte in text.bytes() {
            match byte {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'.'
                | b'_'
                | b'~'
                | b'/'
                | b':' => url.push(byte as char),
                other => url.push_str(&format!("%{other:02X}")),
            }
        }
        url
    }

    // ------------------------------------------------------ temp directories

    /// The two throwaway directories one conversion needs: somewhere for
    /// LibreOffice to write its output, and a private user profile so this
    /// never touches the user's own LibreOffice session. Both are removed when
    /// the guard drops — on the success path, on every error path, and on
    /// cancellation.
    struct Workspace {
        out: PathBuf,
        profile: PathBuf,
    }

    impl Workspace {
        fn create() -> Result<Self, PreviewError> {
            // The pid separates concurrent sekio processes and the counter
            // separates concurrent previews inside one — no randomness needed.
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir();
            let stem = format!("sekio-lo-{}-{n}", std::process::id());

            let work = Self {
                out: base.join(format!("{stem}-out")),
                profile: base.join(format!("{stem}-profile")),
            };
            // Built after `work` exists so a half-created workspace is still
            // cleaned up by `Drop`.
            std::fs::create_dir_all(&work.out)?;
            std::fs::create_dir_all(&work.profile)?;
            Ok(work)
        }

        fn out(&self) -> &Path {
            &self.out
        }

        fn profile(&self) -> &Path {
            &self.profile
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            // Best-effort: a temp directory we cannot remove must not panic a
            // preview, and the OS will reap it eventually.
            let _ = std::fs::remove_dir_all(&self.out);
            let _ = std::fs::remove_dir_all(&self.profile);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::render::legacy_office::testing::{cfb, temp_file, Entry};

        /// Both directories exist while the guard lives and are gone after it
        /// drops, contents and all.
        #[test]
        fn workspace_removes_both_directories_on_drop() {
            let (out, profile) = {
                let work = Workspace::create().expect("create workspace");
                assert!(work.out().is_dir());
                assert!(work.profile().is_dir());
                // A populated profile is what LibreOffice actually leaves
                // behind, so prove the guard removes a non-empty tree.
                std::fs::create_dir_all(work.profile().join("user/config")).expect("nest");
                std::fs::write(work.profile().join("user/config/registry"), b"x").expect("write");
                std::fs::write(work.out().join("converted.txt"), b"text").expect("write");
                (work.out().to_path_buf(), work.profile().to_path_buf())
            };
            assert!(!out.exists(), "output dir survived: {}", out.display());
            assert!(
                !profile.exists(),
                "profile dir survived: {}",
                profile.display()
            );
        }

        #[test]
        fn workspaces_do_not_collide() {
            let a = Workspace::create().expect("a");
            let b = Workspace::create().expect("b");
            assert_ne!(a.out(), b.out());
            assert_ne!(a.profile(), b.profile());
            assert!(a.out().starts_with(std::env::temp_dir()));
        }

        /// A private profile is worthless if the URL is malformed, and a space
        /// in the path is the case that bites on Windows.
        #[test]
        fn profile_urls_are_absolute_and_encoded() {
            let url = file_url(Path::new("/tmp/sekio lo/profile"));
            assert_eq!(url, "file:///tmp/sekio%20lo/profile");

            let windows = file_url(Path::new(r"C:\Users\Jo Blogs\Temp\p"));
            assert_eq!(windows, "file:///C:/Users/Jo%20Blogs/Temp/p");

            // Whatever the platform, the real thing must come out as a URL.
            let real = file_url(&std::env::temp_dir());
            assert!(real.starts_with("file:///"), "{real}");
            assert!(!real.contains(' '), "{real}");
        }

        #[test]
        fn arg_path_never_starts_with_a_dash() {
            let arg = arg_path(Path::new("--convert-to.doc"));
            assert!(!arg.to_string_lossy().starts_with('-'), "{arg:?}");
        }

        #[test]
        fn only_the_two_legacy_formats_are_accepted() {
            assert_eq!(Kind::from_format("doc").ok(), Some(Kind::Doc));
            assert_eq!(Kind::from_format("ppt").ok(), Some(Kind::Ppt));
            for other in ["docx", "pptx", "xls", ""] {
                assert!(
                    matches!(Kind::from_format(other), Err(PreviewError::Format(_))),
                    "{other} should be refused"
                );
            }
        }

        /// The missing-LibreOffice path, exercised directly because we cannot
        /// uninstall it for the length of a test. It must name the format, the
        /// size, and how to enable text previews.
        #[test]
        fn missing_libreoffice_yields_a_metadata_preview() {
            let file = temp_file("legacy.doc", &cfb(&[Entry::stream("WordDocument")]));
            let preview = metadata_preview(file.path(), Kind::Doc);
            assert!(!preview.truncated);

            let PreviewContent::Metadata { fields, thumbnail } = &preview.content else {
                panic!("expected metadata, got {:?}", preview.content);
            };
            assert!(thumbnail.is_none());
            let rendered: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: {}", f.key, f.value))
                .collect();
            let all = rendered.join("\n");
            assert!(all.contains("Word document (binary, pre-2007)"), "{all}");
            assert!(all.contains("Size"), "{all}");
            assert!(all.contains("install LibreOffice"), "{all}");
        }

        #[test]
        fn a_child_that_never_exits_is_killed_at_the_deadline() {
            let sleeper = if cfg!(windows) { "timeout" } else { "sleep" };
            let Some(bin) = find_on_path(sleeper) else {
                eprintln!("skipping: no {sleeper} binary");
                return;
            };
            let mut cmd = base_command(&bin);
            cmd.arg("30");

            let started = Instant::now();
            let ran = run_bounded(cmd, Duration::from_millis(300), &CancelToken::new());
            let elapsed = started.elapsed();

            assert!(matches!(ran, Ok(false)), "a timeout must not be an error");
            assert!(
                elapsed < Duration::from_secs(5),
                "waited {elapsed:?} on a 300ms deadline — the child was not killed"
            );
        }

        #[test]
        fn cancellation_mid_wait_kills_the_child() {
            let sleeper = if cfg!(windows) { "timeout" } else { "sleep" };
            let Some(bin) = find_on_path(sleeper) else {
                eprintln!("skipping: no {sleeper} binary");
                return;
            };
            let mut cmd = base_command(&bin);
            cmd.arg("30");

            let cancel = CancelToken::new();
            let flag = cancel.clone();
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                flag.cancel();
            });

            let started = Instant::now();
            // A deadline far longer than the cancellation, so only the token
            // can be what ended this wait.
            let result = run_bounded(cmd, Duration::from_secs(30), &cancel);
            let elapsed = started.elapsed();
            let _ = canceller.join();

            assert!(matches!(result, Err(PreviewError::Cancelled)));
            assert!(elapsed < Duration::from_secs(5), "waited {elapsed:?}");
        }

        #[test]
        fn output_is_found_by_extension_and_empty_files_are_ignored() {
            let work = Workspace::create().expect("workspace");
            assert_eq!(first_output(work.out(), "txt"), None);
            std::fs::write(work.out().join("empty.txt"), b"").expect("write");
            assert_eq!(first_output(work.out(), "txt"), None);
            std::fs::write(work.out().join("real.txt"), b"hello").expect("write");
            assert_eq!(
                first_output(work.out(), "txt"),
                Some(work.out().join("real.txt"))
            );
        }

        #[test]
        fn converted_text_is_read_back_without_its_bom() {
            let work = Workspace::create().expect("workspace");
            let file = work.out().join("c.txt");
            std::fs::write(
                &file,
                "\u{feff}Title\n\n\n\nBody\ttabbed\r\n\nEnd\n".as_bytes(),
            )
            .expect("write");

            let preview = text_preview(
                &file,
                Kind::Doc,
                &PreviewOptions::default(),
                &CancelToken::new(),
            )
            .expect("read back");

            let PreviewContent::Text { lines, language } = &preview.content else {
                panic!("expected text");
            };
            assert_eq!(language, "Word Document (converted)");
            let rendered: Vec<String> = lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
                .collect();
            // BOM stripped, blank runs collapsed to one, tab expanded.
            assert_eq!(rendered, ["Title", "", "Body    tabbed", "", "End"]);
            assert!(!preview.truncated);
        }

        #[test]
        fn converted_text_stops_at_max_lines_and_flags_it() {
            let work = Workspace::create().expect("workspace");
            let file = work.out().join("long.txt");
            let body: String = (0..500).map(|i| format!("line {i}\n")).collect();
            std::fs::write(&file, body).expect("write");

            let opts = PreviewOptions {
                max_lines: 10,
                ..PreviewOptions::default()
            };
            let preview =
                text_preview(&file, Kind::Doc, &opts, &CancelToken::new()).expect("read back");
            assert!(preview.truncated);
            let PreviewContent::Text { lines, .. } = &preview.content else {
                panic!("expected text");
            };
            assert_eq!(lines.len(), 10);
        }

        #[test]
        fn an_empty_conversion_says_so_rather_than_erroring() {
            let work = Workspace::create().expect("workspace");
            let file = work.out().join("blank.txt");
            std::fs::write(&file, "\u{feff}\n\n\n").expect("write");
            let preview = text_preview(
                &file,
                Kind::Doc,
                &PreviewOptions::default(),
                &CancelToken::new(),
            )
            .expect("read back");
            let PreviewContent::Text { lines, .. } = &preview.content else {
                panic!("expected text");
            };
            assert_eq!(lines.len(), 1);
            assert!(lines[0].spans[0].text.contains("no text"));
        }
    }
}

// -------------------------------------------------- compiled-out replacement

/// With `office-legacy` off this still exists and fails, so the dispatcher
/// degrades to the hexdump — the convention every feature-gated renderer here
/// follows.
#[cfg(not(feature = "office-legacy"))]
pub fn render(
    _path: &std::path::Path,
    _format: &str,
    _head: Vec<u8>,
    _opts: &crate::PreviewOptions,
    _cancel: &crate::CancelToken,
) -> Result<crate::Preview, crate::PreviewError> {
    Err(crate::PreviewError::Format(
        "legacy office support not compiled in".into(),
    ))
}

// ------------------------------------------------------------------ fixtures

/// Hand-built compound files, so the detection tests hold whether or not this
/// machine has LibreOffice — or any legacy Office file — on it.
#[cfg(test)]
mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const SECTOR: usize = 512;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const NOSTREAM: u32 = 0xFFFF_FFFF;

    /// One directory entry to place in the built file's root storage.
    pub struct Entry {
        name: &'static str,
        left: u32,
        right: u32,
    }

    impl Entry {
        pub fn stream(name: &'static str) -> Self {
            Self {
                name,
                left: NOSTREAM,
                right: NOSTREAM,
            }
        }

        /// Ids are 1-based here: entry 0 is always the root storage.
        pub fn with_siblings(mut self, left: u32, right: u32) -> Self {
            self.left = left;
            self.right = right;
            self
        }
    }

    /// A minimal but structurally valid v3 compound file: header, one FAT
    /// sector, one directory sector holding the root storage plus `entries`.
    /// The root's first child is entry 1.
    pub fn cfb(entries: &[Entry]) -> Vec<u8> {
        let mut buf = vec![0u8; SECTOR * 3];

        buf[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
        put_u16(&mut buf, 26, 3); // major version 3
        put_u16(&mut buf, 28, 0xFFFE); // little-endian
        put_u16(&mut buf, 30, 9); // 512-byte sectors
        put_u16(&mut buf, 32, 6); // 64-byte mini sectors
        put_u32(&mut buf, 44, 1); // one FAT sector
        put_u32(&mut buf, 48, 1); // directory starts at sector 1
        put_u32(&mut buf, 60, ENDOFCHAIN); // no mini FAT
        put_u32(&mut buf, 68, ENDOFCHAIN); // no extra DIFAT
        put_u32(&mut buf, 76, 0); // DIFAT[0]: the FAT is sector 0
        for i in 1..109 {
            put_u32(&mut buf, 76 + i * 4, FREESECT);
        }

        // Sector 0 (file offset 512) is the FAT itself.
        let fat = SECTOR;
        put_u32(&mut buf, fat, FATSECT);
        put_u32(&mut buf, fat + 4, ENDOFCHAIN); // the directory is one sector
        for i in 2..SECTOR / 4 {
            put_u32(&mut buf, fat + i * 4, FREESECT);
        }

        // Sector 1 (file offset 1024) is the directory. Unwritten entries stay
        // zeroed, which is object type 0 — "unused".
        let dir = SECTOR * 2;
        put_entry(&mut buf, dir, "Root Entry", 5, NOSTREAM, NOSTREAM, 1);
        for (i, entry) in entries.iter().enumerate() {
            let at = dir + (i + 1) * 128;
            if at + 128 > buf.len() {
                break;
            }
            put_entry(
                &mut buf,
                at,
                entry.name,
                2,
                entry.left,
                entry.right,
                NOSTREAM,
            );
        }
        buf
    }

    fn put_entry(
        buf: &mut [u8],
        at: usize,
        name: &str,
        object_type: u8,
        left: u32,
        right: u32,
        child: u32,
    ) {
        let units: Vec<u16> = name.encode_utf16().collect();
        for (i, unit) in units.iter().enumerate() {
            put_u16(buf, at + i * 2, *unit);
        }
        // Length in bytes, including the terminating NUL.
        put_u16(buf, at + 64, (units.len() as u16 + 1) * 2);
        buf[at + 66] = object_type;
        buf[at + 67] = 1; // black
        put_u32(buf, at + 68, left);
        put_u32(buf, at + 72, right);
        put_u32(buf, at + 76, child);
    }

    fn put_u16(buf: &mut [u8], at: usize, value: u16) {
        buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(buf: &mut [u8], at: usize, value: u32) {
        buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// A temp file removed when the guard drops.
    pub struct TempFile(PathBuf);

    impl TempFile {
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub fn temp_file(name: &str, bytes: &[u8]) -> TempFile {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("sekio-legacy-{}-{n}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write fixture");
        TempFile(path)
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use super::testing::{cfb, temp_file, Entry};
    use crate::{CancelToken, PreviewContent, PreviewError, PreviewOptions};

    /// An already-cancelled token must surface `Cancelled`, never a swallowed
    /// success or a different error — and never after doing real work.
    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let file = temp_file("cancelled.doc", &cfb(&[Entry::stream("WordDocument")]));
        let cancel = CancelToken::new();
        cancel.cancel();
        let result = render(
            file.path(),
            "doc",
            Vec::new(),
            &PreviewOptions::default(),
            &cancel,
        );
        if cfg!(feature = "office-legacy") {
            assert!(matches!(result, Err(PreviewError::Cancelled)), "{result:?}");
        } else {
            assert!(matches!(result, Err(PreviewError::Format(_))), "{result:?}");
        }
    }

    /// A compound file that is structurally sound but holds no real Word
    /// document. LibreOffice will refuse it; with LibreOffice absent we get the
    /// metadata fallback. Either way: an error or a preview, never a panic and
    /// never a bogus `Cancelled`.
    #[test]
    fn garbage_input_errors_rather_than_panicking() {
        let mut bytes = cfb(&[Entry::stream("WordDocument")]);
        bytes.extend(std::iter::repeat_n(0xABu8, 8192));
        let file = temp_file("garbage.doc", &bytes);

        let result = render(
            file.path(),
            "doc",
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        );
        match result {
            Ok(preview) => assert!(
                matches!(preview.content, PreviewContent::Metadata { .. }),
                "unreadable input must not produce text"
            ),
            Err(PreviewError::Cancelled) => panic!("nothing cancelled this preview"),
            Err(PreviewError::Format(_)) => {}
            Err(other) => panic!("expected a format error, got {other:?}"),
        }
    }

    /// The dispatcher only ever hands us the two legacy binary formats; being
    /// handed anything else is a bug, and must be an error rather than a panic
    /// or a wrong-looking preview.
    #[test]
    fn modern_formats_are_refused() {
        let file = temp_file("modern.docx", b"PK\x03\x04");
        for format in ["docx", "pptx", "xls"] {
            let err = render(
                file.path(),
                format,
                Vec::new(),
                &PreviewOptions::default(),
                &CancelToken::new(),
            )
            .expect_err("should refuse");
            assert!(matches!(err, PreviewError::Format(_)), "{format}: {err:?}");
        }
    }

    /// Detection routes by what is *inside* the compound file, so a legacy
    /// workbook keeps going to `render/spreadsheet.rs` (calamine reads it
    /// natively) and never reaches this renderer — even when it is misnamed.
    #[cfg(feature = "office")]
    #[test]
    fn legacy_excel_is_routed_to_the_spreadsheet_renderer() {
        use crate::detect::{detect, Detected};
        let opts = PreviewOptions::default();

        // Named `.doc`, but a workbook inside.
        let misnamed = temp_file("liar.doc", &cfb(&[Entry::stream("Workbook")]));
        let detected = detect(misnamed.path(), &opts).expect("detect");
        assert!(
            matches!(&detected, Detected::Spreadsheet { format, .. } if format == "xls"),
            "got {detected:?}"
        );
    }

    /// ...and the mirror image: a Word document named `.xls` still comes here.
    #[cfg(feature = "office")]
    #[test]
    fn legacy_word_and_powerpoint_are_routed_to_this_renderer() {
        use crate::detect::{detect, Detected};
        let opts = PreviewOptions::default();

        let word = temp_file("liar.xls", &cfb(&[Entry::stream("WordDocument")]));
        let detected = detect(word.path(), &opts).expect("detect");
        assert!(
            matches!(&detected, Detected::Document { format, .. } if format == "doc"),
            "got {detected:?}"
        );

        let deck = temp_file("mystery.bin", &cfb(&[Entry::stream("PowerPoint Document")]));
        let detected = detect(deck.path(), &opts).expect("detect");
        assert!(
            matches!(&detected, Detected::Document { format, .. } if format == "ppt"),
            "got {detected:?}"
        );
    }

    // ------------------------------------------------- end-to-end, if present

    /// Build a real legacy file with LibreOffice, then preview it. Skipped
    /// with a note when LibreOffice is not installed, because that is exactly
    /// the situation this feature is designed to survive.
    #[cfg(feature = "office-legacy")]
    struct Fixture {
        root: std::path::PathBuf,
        file: std::path::PathBuf,
    }

    #[cfg(feature = "office-legacy")]
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Convert `source` with whatever LibreOffice is on PATH, using a private
    /// profile of its own so building a fixture cannot disturb the developer's
    /// session either. `None` means "no LibreOffice here".
    #[cfg(feature = "office-legacy")]
    fn convert_with_libreoffice(source: &std::path::Path, filter: &str) -> Option<Fixture> {
        use std::process::{Command, Stdio};

        let path_var = std::env::var_os("PATH")?;
        let bin = std::env::split_paths(&path_var)
            .filter(|dir| !dir.as_os_str().is_empty())
            .flat_map(|dir| ["libreoffice", "soffice"].map(|name| dir.join(name)))
            .find(|candidate| candidate.is_file())?;

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("sekio-legacy-fixture-{}-{n}", std::process::id()));
        // Created before anything can fail, so every early return below still
        // takes the directory with it.
        let mut fixture = Fixture {
            root: root.clone(),
            file: std::path::PathBuf::new(),
        };
        let profile = root.join("profile");
        let target = root.join("out");
        std::fs::create_dir_all(&profile).ok()?;
        std::fs::create_dir_all(&target).ok()?;

        let status = Command::new(&bin)
            .arg("--headless")
            .arg(format!(
                "-env:UserInstallation=file://{}",
                profile.display()
            ))
            .arg("--convert-to")
            .arg(filter)
            .arg("--outdir")
            .arg(&target)
            .arg(source)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        fixture.file = std::fs::read_dir(&target)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.is_file())?;
        Some(fixture)
    }

    #[cfg(feature = "office-legacy")]
    #[test]
    fn a_real_doc_previews_as_its_own_text() {
        let source = temp_file(
            "source.txt",
            b"Quarterly Report\n\nRevenue grew 12% this quarter.\n",
        );
        let Some(doc) = convert_with_libreoffice(source.path(), "doc:MS Word 97") else {
            eprintln!("skipping: LibreOffice not available to build a .doc fixture");
            return;
        };

        let preview = render(
            &doc.file,
            "doc",
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect("preview a real .doc");

        let PreviewContent::Text { lines, language } = &preview.content else {
            panic!("expected text, got {:?}", preview.content);
        };
        assert_eq!(language, "Word Document (converted)");
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert!(text.iter().any(|l| l == "Quarterly Report"), "{text:?}");
        assert!(
            text.iter().any(|l| l.contains("Revenue grew 12%")),
            "{text:?}"
        );
    }

    #[cfg(feature = "office-legacy")]
    #[test]
    fn a_real_ppt_previews_as_its_slides() {
        // Flat ODF: a single XML file LibreOffice reads reliably, so the
        // fixture does not depend on any binary blob checked into the repo.
        let fodp = concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0""#,
            r#" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0""#,
            r#" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0""#,
            r#" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""#,
            r#" office:version="1.2""#,
            r#" office:mimetype="application/vnd.oasis.opendocument.presentation">"#,
            r#"<office:body><office:presentation>"#,
            r#"<draw:page draw:name="one"><draw:frame svg:width="20cm" svg:height="3cm""#,
            r#" svg:x="2cm" svg:y="2cm"><draw:text-box>"#,
            r#"<text:p>Opening slide</text:p></draw:text-box></draw:frame></draw:page>"#,
            r#"<draw:page draw:name="two"><draw:frame svg:width="20cm" svg:height="3cm""#,
            r#" svg:x="2cm" svg:y="2cm"><draw:text-box>"#,
            r#"<text:p>Closing slide</text:p></draw:text-box></draw:frame></draw:page>"#,
            r#"</office:presentation></office:body></office:document>"#,
        );
        let source = temp_file("source.fodp", fodp.as_bytes());
        let Some(ppt) = convert_with_libreoffice(source.path(), "ppt:MS PowerPoint 97") else {
            eprintln!("skipping: LibreOffice not available to build a .ppt fixture");
            return;
        };

        let preview = render(
            &ppt.file,
            "ppt",
            Vec::new(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect("preview a real .ppt");

        let PreviewContent::Text { lines, language } = &preview.content else {
            panic!("expected text, got {:?}", preview.content);
        };
        assert_eq!(language, "PowerPoint Presentation (converted)");
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert!(text.iter().any(|l| l.contains("Opening slide")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("Closing slide")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("Slide 1")), "{text:?}");
    }
}
