//! Spreadsheet preview: xlsx/xlsm, xlsb, legacy xls (BIFF) and ods, all read
//! through `calamine` — pure Rust, so nothing here needs a C toolchain.
//!
//! The output is a real grid in the `PreviewContent::Table` IR: the column
//! letters, one row per spreadsheet row with its own row number as the label,
//! and a typed cell per column. Cells carry their **full** text and core picks
//! no widths at all — a frontend knows how much room it actually has, so it is
//! the one that decides what (if anything) has to be elided. `total_rows` and
//! `total_cols` carry the sheet's real extent so a frontend can say how much of
//! it is not shown.
//!
//! Only the *first* sheet is previewed, and only its first `max_lines` rows.
//! For xlsx that cap is a real read bound: `Xlsx::worksheet_cells_reader`
//! streams cells straight out of the sheet XML, so a 200 MB workbook costs the
//! rows we show and nothing more. xls/xlsb/ods have no streaming reader in
//! calamine, so those go through `worksheet_range`, which materialises one
//! sheet — still never the whole workbook.
//!
//! Feature-gated inside the module (see `render/mod.rs`): with `office` off,
//! `render` still exists and returns `PreviewError::Format`, and the dispatcher
//! degrades to the hexdump.

#[cfg(feature = "office")]
pub use imp::render;

#[cfg(feature = "office")]
mod imp {
    use std::fs::File;
    use std::io::BufReader;
    use std::path::Path;

    use calamine::{
        open_workbook, Data, DataRef, ExcelDateTime, Ods, Range, Reader, Xls, Xlsb, Xlsx,
        XlsxCellReader,
    };

    use crate::{
        CancelToken, CellKind, Preview, PreviewContent, PreviewError, PreviewOptions, TableCell,
        TableRow,
    };

    /// Poll the cancel token every this many rows / cells of work.
    const CANCEL_INTERVAL: usize = 64;
    /// Columns read at all. Bounds the work on a sheet with 16 384 populated
    /// columns; anything past it is counted in `total_cols` but not read.
    const SCAN_COLS: u32 = 512;
    /// Characters kept from one cell. A cell can legally hold 32 767 of them,
    /// and no pane will ever show that, so this is the sanity bound that stops
    /// one 4 000-character note (or a grid full of them) from costing real
    /// memory. It is not a layout decision: it is generous enough that an
    /// ordinary cell is never touched, and when it does bite the preview says
    /// so by setting `truncated`.
    const MAX_CELL_CHARS: usize = 256;
    /// Sheet names listed before the list gives up. `max_entries` can lower it.
    const MAX_SHEET_NAMES: usize = 24;

    pub fn render(
        path: &Path,
        format: &str,
        _head: Vec<u8>,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<Preview, PreviewError> {
        cancel.check()?;

        // `_head` is the detection sample and is deliberately unused: both the
        // zip central directory (xlsx/xlsb/ods) and the OLE FAT (xls) live at
        // the *end* of the file, so a 64 KB prefix can't be parsed. calamine
        // opens the path itself and seeks.

        // calamine walks XML and BIFF records written by other programs. A
        // malformed or hostile file must degrade to the hexdump, never take
        // the process down, so an unwind is turned into a `Format` error.
        // Cancellation is passed through untouched.
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build(path, format, opts, cancel)
        }));

        let (content, truncated) = match built {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(PreviewError::Format("malformed spreadsheet".into())),
        };

        Ok(Preview { content, truncated })
    }

    fn build(
        path: &Path,
        format: &str,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<(PreviewContent, bool), PreviewError> {
        // A row of the sheet is a row of the table, so `max_lines` bounds the
        // read directly — there are no header or summary lines to reserve room
        // for any more.
        let max_rows = opts.max_lines.max(1);

        let (names, sheet) = read(path, format, max_rows, cancel)?;
        cancel.check()?;
        Ok(table(names, sheet, opts))
    }

    // ------------------------------------------------------------- reading

    /// Rows are sparse — `(column, value)` in ascending column order, empty
    /// cells simply absent — so a sheet with one value in column ZZ costs one
    /// entry rather than 700. They are squared off into full rows in [`table`].
    type Row = (u32, Vec<(u32, TableCell)>);

    struct Sheet {
        rows: Vec<Row>,
        /// Rows the sheet really has, as far as we can tell.
        total_rows: u64,
        /// Columns the sheet really has, as far as we can tell.
        total_cols: u64,
        /// A cap stopped the read before the sheet ended.
        truncated: bool,
    }

    fn read(
        path: &Path,
        format: &str,
        max_rows: usize,
        cancel: &CancelToken,
    ) -> Result<(Vec<String>, Sheet), PreviewError> {
        match format {
            // xlsm is an xlsx container with macros; same reader.
            "xlsx" | "xlsm" => {
                let wb: Xlsx<_> = open_workbook(path).map_err(|e| fmt_err("xlsx", e))?;
                read_streaming(wb, max_rows, cancel)
            }
            "xlsb" => {
                let wb: Xlsb<_> = open_workbook(path).map_err(|e| fmt_err("xlsb", e))?;
                read_via_range(wb, max_rows, cancel)
            }
            "xls" => {
                let wb: Xls<_> = open_workbook(path).map_err(|e| fmt_err("xls", e))?;
                read_via_range(wb, max_rows, cancel)
            }
            "ods" => {
                let wb: Ods<_> = open_workbook(path).map_err(|e| fmt_err("ods", e))?;
                read_via_range(wb, max_rows, cancel)
            }
            other => Err(PreviewError::Format(format!(
                "no spreadsheet reader for {other}"
            ))),
        }
    }

    /// calamine's per-format errors only promise `Debug`, and their `Display`
    /// where they have one is no better, so `{e:?}` is the honest rendering.
    fn fmt_err<E: std::fmt::Debug>(what: &str, e: E) -> PreviewError {
        PreviewError::Format(format!("{what}: {e:?}"))
    }

    /// xlsx: stream cells out of the sheet XML and stop at the row cap, so the
    /// rest of the sheet is never inflated.
    fn read_streaming(
        mut wb: Xlsx<BufReader<File>>,
        max_rows: usize,
        cancel: &CancelToken,
    ) -> Result<(Vec<String>, Sheet), PreviewError> {
        let names = wb.sheet_names();
        let Some(first) = names.first().cloned() else {
            return Err(PreviewError::Format("workbook has no sheets".into()));
        };
        cancel.check()?;

        let mut reader = wb
            .worksheet_cells_reader(&first)
            .map_err(|e| fmt_err("xlsx", e))?;
        let sheet = stream_sheet(&mut reader, max_rows, cancel)?;
        Ok((names, sheet))
    }

    fn stream_sheet(
        reader: &mut XlsxCellReader<'_, BufReader<File>>,
        max_rows: usize,
        cancel: &CancelToken,
    ) -> Result<Sheet, PreviewError> {
        let dims = reader.dimensions();
        let mut rows: Vec<Row> = Vec::new();
        let mut base: Option<u32> = None;
        let mut last_row = 0u32;
        let mut max_col = 0u32;
        let mut truncated = false;
        let mut seen = 0usize;

        loop {
            seen += 1;
            if seen.is_multiple_of(CANCEL_INTERVAL) {
                cancel.check()?;
            }
            let raw = match reader.next_cell() {
                Ok(Some(cell)) => cell,
                Ok(None) => break,
                Err(e) => return Err(fmt_err("xlsx", e)),
            };
            if matches!(raw.get_value(), DataRef::Empty) {
                continue;
            }

            let (r, c) = raw.get_position();
            // Cells arrive in ascending row order; the first one anchors the
            // grid so a sheet whose data starts at row 40 doesn't cost 40
            // blank lines.
            let start = *base.get_or_insert(r);
            let Some(idx) = r.checked_sub(start).map(|d| d as usize) else {
                continue; // out-of-order row in a malformed file
            };
            if idx >= max_rows {
                // The row cap: stop reading here, don't read-then-truncate.
                truncated = true;
                break;
            }
            if c >= SCAN_COLS {
                truncated = true;
                continue;
            }

            while rows.len() <= idx {
                let n = start + rows.len() as u32;
                rows.push((n, Vec::new()));
            }
            let (cell, cut) = cell_of_ref(raw.get_value());
            truncated |= cut;
            rows[idx].1.push((c, cell));
            max_col = max_col.max(c);
            last_row = r;
        }

        // What we saw is exact for the part we read. Past the cap the sheet's
        // declared `<dimension>` is the only hint we have about the rest, so
        // take whichever is larger.
        let observed_rows = base.map_or(0, |b| u64::from(last_row.saturating_sub(b)) + 1);
        let observed_cols = if rows.iter().any(|(_, cells)| !cells.is_empty()) {
            u64::from(max_col.saturating_sub(min_col_of(&rows))) + 1
        } else {
            0
        };
        let (declared_rows, declared_cols) = if truncated {
            (
                u64::from(dims.end.0.saturating_sub(dims.start.0)) + 1,
                u64::from(dims.end.1.saturating_sub(dims.start.1)) + 1,
            )
        } else {
            (0, 0)
        };

        Ok(Sheet {
            rows,
            total_rows: observed_rows.max(declared_rows),
            total_cols: observed_cols.max(declared_cols),
            truncated,
        })
    }

    /// xls/xlsb/ods: calamine has no streaming cell reader for these, so one
    /// sheet is materialised. Still bounded — never the whole workbook — and
    /// the row cap still decides how much we walk.
    fn read_via_range<R>(
        mut wb: R,
        max_rows: usize,
        cancel: &CancelToken,
    ) -> Result<(Vec<String>, Sheet), PreviewError>
    where
        R: Reader<BufReader<File>>,
    {
        let names = wb.sheet_names();
        let Some(first) = names.first().cloned() else {
            return Err(PreviewError::Format("workbook has no sheets".into()));
        };
        cancel.check()?;

        let range = wb
            .worksheet_range(&first)
            .map_err(|e| fmt_err("spreadsheet", e))?;
        cancel.check()?;
        let sheet = sheet_from_range(&range, max_rows, cancel)?;
        Ok((names, sheet))
    }

    fn sheet_from_range(
        range: &Range<Data>,
        max_rows: usize,
        cancel: &CancelToken,
    ) -> Result<Sheet, PreviewError> {
        let (base_row, base_col) = range.start().unwrap_or((0, 0));
        let mut rows: Vec<Row> = Vec::new();
        let mut truncated = false;

        for (i, row) in range.rows().enumerate() {
            if i.is_multiple_of(CANCEL_INTERVAL) {
                cancel.check()?;
            }
            if i >= max_rows {
                truncated = true;
                break;
            }
            let mut cells = Vec::new();
            for (j, value) in row.iter().enumerate() {
                if j as u32 >= SCAN_COLS {
                    truncated = true;
                    break;
                }
                if matches!(value, Data::Empty) {
                    continue;
                }
                let (cell, cut) = cell_of_data(value);
                truncated |= cut;
                cells.push((base_col + j as u32, cell));
            }
            rows.push((base_row + i as u32, cells));
        }

        Ok(Sheet {
            rows,
            total_rows: range.height() as u64,
            total_cols: range.width() as u64,
            truncated,
        })
    }

    fn min_col_of(rows: &[Row]) -> u32 {
        rows.iter()
            .filter_map(|(_, cells)| cells.first().map(|(c, _)| *c))
            .min()
            .unwrap_or(0)
    }

    // --------------------------------------------------------------- table

    /// Square the sparse rows off into the IR: every row gets exactly
    /// `columns.len()` cells, absent ones empty, so a frontend indexing by
    /// column never has to cope with a ragged row.
    fn table(names: Vec<String>, sheet: Sheet, opts: &PreviewOptions) -> (PreviewContent, bool) {
        let min_col = min_col_of(&sheet.rows);
        let max_col = sheet
            .rows
            .iter()
            .filter_map(|(_, cells)| cells.last().map(|(c, _)| *c))
            .max();
        // Columns span the cells we actually read: from the leftmost populated
        // column to the rightmost. A sheet with nothing in it has none.
        let width = match max_col {
            Some(max) => (max.saturating_sub(min_col) as usize) + 1,
            None => 0,
        };

        let columns: Vec<String> = (0..width)
            .map(|i| column_label(min_col.saturating_add(i as u32)))
            .collect();

        let rows: Vec<TableRow> = sheet
            .rows
            .into_iter()
            .map(|(number, cells)| {
                let mut out = vec![empty_cell(); width];
                for (c, cell) in cells {
                    if let Some(slot) = c.checked_sub(min_col).and_then(|i| out.get_mut(i as usize))
                    {
                        *slot = cell;
                    }
                }
                TableRow {
                    // Row numbers are 1-based, like every spreadsheet UI.
                    label: number.saturating_add(1).to_string(),
                    cells: out,
                }
            })
            .collect();

        let cap = MAX_SHEET_NAMES.min(opts.max_entries.max(1));
        let names_truncated = names.len() > cap;
        let sheets: Vec<String> = names.into_iter().take(cap).map(|n| clean(&n).0).collect();

        let shown_rows = rows.len() as u64;
        let content = PreviewContent::Table {
            columns,
            rows,
            sheets,
            // The first sheet is the one we read, and it is never dropped from
            // the (capped) name list.
            active_sheet: 0,
            total_rows: sheet.total_rows.max(shown_rows),
            total_cols: sheet.total_cols.max(width as u64),
        };
        (content, sheet.truncated || names_truncated)
    }

    fn empty_cell() -> TableCell {
        TableCell {
            text: String::new(),
            kind: CellKind::Text,
        }
    }

    // ------------------------------------------------------------ cell text

    fn cell_of_data(value: &Data) -> (TableCell, bool) {
        match value {
            Data::Int(i) => (cell(i.to_string(), CellKind::Number), false),
            Data::Float(f) => (cell(number(*f), CellKind::Number), false),
            Data::String(s) => from_str(s, CellKind::Text),
            Data::Bool(b) => (cell(bool_text(*b), CellKind::Bool), false),
            Data::DateTime(dt) => (cell(datetime(dt), CellKind::Date), false),
            Data::DateTimeIso(s) | Data::DurationIso(s) => from_str(s, CellKind::Date),
            Data::Error(e) => from_str(&e.to_string(), CellKind::Error),
            Data::Empty => (empty_cell(), false),
        }
    }

    fn cell_of_ref(value: &DataRef<'_>) -> (TableCell, bool) {
        match value {
            DataRef::Int(i) => (cell(i.to_string(), CellKind::Number), false),
            DataRef::Float(f) => (cell(number(*f), CellKind::Number), false),
            DataRef::String(s) => from_str(s, CellKind::Text),
            DataRef::SharedString(s) => from_str(s, CellKind::Text),
            DataRef::Bool(b) => (cell(bool_text(*b), CellKind::Bool), false),
            DataRef::DateTime(dt) => (cell(datetime(dt), CellKind::Date), false),
            DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => from_str(s, CellKind::Date),
            DataRef::Error(e) => from_str(&e.to_string(), CellKind::Error),
            DataRef::Empty => (empty_cell(), false),
        }
    }

    fn cell(text: String, kind: CellKind) -> TableCell {
        TableCell { text, kind }
    }

    /// A string straight out of the file: flattened, and bounded by
    /// [`MAX_CELL_CHARS`]. The flag says whether that bound bit.
    fn from_str(s: &str, kind: CellKind) -> (TableCell, bool) {
        let (text, cut) = clean(s);
        (TableCell { text, kind }, cut)
    }

    fn bool_text(b: bool) -> String {
        if b { "TRUE" } else { "FALSE" }.to_string()
    }

    fn number(f: f64) -> String {
        if f.is_finite() {
            format!("{f}")
        } else {
            "#NUM!".to_string()
        }
    }

    /// Excel keeps dates as a serial number; show something a reader can
    /// actually parse. Durations and wild values fall back to the raw number
    /// rather than risking nonsense out of the calendar conversion.
    fn datetime(dt: &ExcelDateTime) -> String {
        let value = dt.as_f64();
        if dt.is_duration() || !value.is_finite() || !(0.0..3_000_000.0).contains(&value) {
            return number(value);
        }
        let (y, mo, d, h, mi, s, _ms) = dt.to_ymd_hms_milli();
        if value < 1.0 {
            format!("{h:02}:{mi:02}:{s:02}")
        } else if h == 0 && mi == 0 && s == 0 {
            format!("{y:04}-{mo:02}-{d:02}")
        } else {
            format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }
    }

    /// One cell must stay one table cell: newlines and tabs become spaces and
    /// other control characters are dropped, so a frontend can put the text in
    /// a grid cell without it breaking the row. Returns the (bounded) text and
    /// whether [`MAX_CELL_CHARS`] cut it short.
    fn clean(s: &str) -> (String, bool) {
        let mut chars = s
            .chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                other => other,
            })
            .filter(|c| !c.is_control());
        let text: String = chars.by_ref().take(MAX_CELL_CHARS).collect();
        let cut = chars.next().is_some();
        (text, cut)
    }

    /// 0 -> A, 25 -> Z, 26 -> AA, exactly like a spreadsheet's column headers.
    pub(super) fn column_label(mut index: u32) -> String {
        let mut letters = [0u8; 8];
        let mut n = 0;
        loop {
            letters[n] = b'A' + (index % 26) as u8;
            n += 1;
            if index < 26 || n == letters.len() {
                break;
            }
            index = index / 26 - 1;
        }
        letters[..n].iter().rev().map(|b| *b as char).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn column_labels_roll_over_at_z() {
            assert_eq!(column_label(0), "A");
            assert_eq!(column_label(25), "Z");
            assert_eq!(column_label(26), "AA");
            assert_eq!(column_label(27), "AB");
            assert_eq!(column_label(51), "AZ");
            assert_eq!(column_label(52), "BA");
            assert_eq!(column_label(701), "ZZ");
            assert_eq!(column_label(702), "AAA");
        }

        #[test]
        fn cells_are_flattened_to_one_line() {
            assert_eq!(clean("a\nb\tc"), ("a b c".to_string(), false));
        }

        /// The per-cell bound is the only thing core still shortens, and it
        /// owns up to it.
        #[test]
        fn a_pathological_cell_is_bounded_and_says_so() {
            let (text, cut) = clean(&"x".repeat(4000));
            assert_eq!(text.chars().count(), MAX_CELL_CHARS);
            assert!(cut);

            // An ordinary cell is nowhere near it and is passed through whole.
            let (text, cut) = clean("Hỗ trợ lễ bảo vệ khóa luận");
            assert_eq!(text, "Hỗ trợ lễ bảo vệ khóa luận");
            assert!(!cut);
        }
    }
}

#[cfg(not(feature = "office"))]
pub fn render(
    _path: &std::path::Path,
    _format: &str,
    _head: Vec<u8>,
    _opts: &crate::PreviewOptions,
    _cancel: &crate::CancelToken,
) -> Result<crate::Preview, crate::PreviewError> {
    Err(crate::PreviewError::Format(
        "office support not compiled in".into(),
    ))
}

#[cfg(all(test, feature = "office"))]
mod tests {
    use super::render;
    use crate::{
        CancelToken, CellKind, Preview, PreviewContent, PreviewError, PreviewOptions, TableRow,
    };

    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A file in the system temp dir that deletes itself — same trick the
    /// archive tests use, so no `tempfile` dev-dependency.
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
        path.push(format!("sekio-sheet-{}-{n}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write fixture");
        TempFile(path)
    }

    /// Build a minimal but real xlsx: the four parts calamine insists on plus
    /// one worksheet. Written with the `zip` crate so no binary fixture ever
    /// lands in the repo.
    ///
    /// A cell's text decides its type, so a fixture can exercise every
    /// `CellKind`: `""` is an absent cell, anything that parses as a number is
    /// numeric, `TRUE`/`FALSE` are booleans, a leading `#` is a formula error
    /// and everything else is an inline string.
    fn xlsx(rows: &[Vec<&str>]) -> Vec<u8> {
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
        let mut sheet = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:{}{}"/><sheetData>"#,
            super::imp::column_label(cols as u32 - 1),
            rows.len().max(1),
        );
        for (r, row) in rows.iter().enumerate() {
            sheet.push_str(&format!(r#"<row r="{}">"#, r + 1));
            for (c, value) in row.iter().enumerate() {
                if value.is_empty() {
                    continue;
                }
                let reference = format!("{}{}", super::imp::column_label(c as u32), r + 1);
                if value.parse::<f64>().is_ok() {
                    sheet.push_str(&format!(r#"<c r="{reference}"><v>{value}</v></c>"#));
                } else if *value == "TRUE" || *value == "FALSE" {
                    let bit = u8::from(*value == "TRUE");
                    sheet.push_str(&format!(r#"<c r="{reference}" t="b"><v>{bit}</v></c>"#));
                } else if value.starts_with('#') {
                    sheet.push_str(&format!(r#"<c r="{reference}" t="e"><v>{value}</v></c>"#));
                } else {
                    sheet.push_str(&format!(
                        r#"<c r="{reference}" t="inlineStr"><is><t>{value}</t></is></c>"#
                    ));
                }
            }
            sheet.push_str("</row>");
        }
        sheet.push_str("</sheetData></worksheet>");

        let parts: [(&str, String); 5] = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.to_string(),
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string(),
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Notes" sheetId="2" r:id="rId2"/></sheets></workbook>"#.to_string(),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#.to_string(),
            ),
            ("xl/worksheets/sheet1.xml", sheet),
        ];

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in parts {
            writer.start_file(name, options).expect("start part");
            writer.write_all(body.as_bytes()).expect("write part");
        }
        writer
            .start_file("xl/worksheets/sheet2.xml", options)
            .expect("start sheet2");
        writer
            .write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#,
            )
            .expect("write sheet2");
        writer.finish().expect("finish zip").into_inner()
    }

    fn preview(bytes: &[u8], opts: &PreviewOptions) -> Result<Preview, PreviewError> {
        let fixture = temp_file("book.xlsx", bytes);
        render(
            &fixture.0,
            "xlsx",
            bytes.to_vec(),
            opts,
            &CancelToken::new(),
        )
    }

    /// The table IR, borrowed out of a preview.
    struct Grid<'a> {
        columns: &'a [String],
        rows: &'a [TableRow],
        sheets: &'a [String],
        active_sheet: usize,
        total_rows: u64,
        total_cols: u64,
    }

    fn grid(p: &Preview) -> Grid<'_> {
        match &p.content {
            PreviewContent::Table {
                columns,
                rows,
                sheets,
                active_sheet,
                total_rows,
                total_cols,
            } => Grid {
                columns,
                rows,
                sheets,
                active_sheet: *active_sheet,
                total_rows: *total_rows,
                total_cols: *total_cols,
            },
            other => panic!("expected table content, got {other:?}"),
        }
    }

    fn texts(row: &TableRow) -> Vec<&str> {
        row.cells.iter().map(|c| c.text.as_str()).collect()
    }

    fn kinds(row: &TableRow) -> Vec<CellKind> {
        row.cells.iter().map(|c| c.kind).collect()
    }

    /// A minimal ODF spreadsheet: the `mimetype` member has to be stored
    /// uncompressed and written first, which is what detection keys on.
    fn ods() -> Vec<u8> {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet><table:table table:name="Budget"><table:table-row><table:table-cell office:value-type="string"><text:p>Item</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>Cost</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell office:value-type="string"><text:p>Server</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="1200.5"><text:p>1200.5</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "mimetype",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .expect("start mimetype");
        writer
            .write_all(b"application/vnd.oasis.opendocument.spreadsheet")
            .expect("write mimetype");
        writer
            .start_file("content.xml", zip::write::SimpleFileOptions::default())
            .expect("start content");
        writer.write_all(content.as_bytes()).expect("write content");
        writer
            .start_file(
                "META-INF/manifest.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start manifest");
        writer
            .write_all(
                br#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"/>"#,
            )
            .expect("write manifest");
        writer.finish().expect("finish zip").into_inner()
    }

    /// ods (like xls and xlsb) takes the `worksheet_range` path rather than the
    /// streaming one, so it needs its own coverage — and it is detected by its
    /// `mimetype` member, not by being called `.ods`.
    #[test]
    fn ods_renders_and_is_detected_by_its_mimetype_member() {
        use crate::detect::{detect, Detected};

        let bytes = ods();
        let fixture = temp_file("sheet.dat", &bytes);
        let detected = detect(&fixture.0, &PreviewOptions::default()).expect("detect");
        assert!(
            matches!(&detected, Detected::Spreadsheet { format, .. } if format == "ods"),
            "got {detected:?}"
        );

        let p = render(
            &fixture.0,
            "ods",
            bytes,
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect("render");
        let g = grid(&p);
        assert_eq!(g.sheets, ["Budget"]);
        assert_eq!(g.columns, ["A", "B"]);
        assert_eq!(texts(&g.rows[0]), ["Item", "Cost"]);
        assert_eq!(texts(&g.rows[1]), ["Server", "1200.5"]);
        assert_eq!(kinds(&g.rows[1]), [CellKind::Text, CellKind::Number]);
    }

    #[test]
    fn a_sheet_becomes_columns_rows_and_cells() {
        let bytes = xlsx(&[
            vec!["Name", "Qty", "Price"],
            vec!["Widget", "12", "3.5"],
            vec!["Sprocket", "7", "11.25"],
        ]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert_eq!(g.columns, ["A", "B", "C"]);
        assert_eq!(g.rows.len(), 3);
        // Labels are the spreadsheet's own 1-based row numbers.
        let labels: Vec<&str> = g.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["1", "2", "3"]);

        assert_eq!(texts(&g.rows[0]), ["Name", "Qty", "Price"]);
        assert_eq!(texts(&g.rows[1]), ["Widget", "12", "3.5"]);
        assert_eq!(texts(&g.rows[2]), ["Sprocket", "7", "11.25"]);

        assert_eq!(g.total_rows, 3);
        assert_eq!(g.total_cols, 3);
        assert!(!p.truncated);
    }

    #[test]
    fn every_cell_carries_the_kind_of_value_that_produced_it() {
        let bytes = xlsx(&[
            vec!["Name", "Qty", "Ok", "Calc"],
            vec!["Widget", "12", "TRUE", "#REF!"],
        ]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert_eq!(
            kinds(&g.rows[0]),
            [CellKind::Text; 4],
            "a header row is all text"
        );
        assert_eq!(
            kinds(&g.rows[1]),
            [
                CellKind::Text,
                CellKind::Number,
                CellKind::Bool,
                CellKind::Error
            ],
            "{:?}",
            texts(&g.rows[1])
        );
        assert_eq!(texts(&g.rows[1]), ["Widget", "12", "TRUE", "#REF!"]);
        // The IR's alignment rule is the frontends', not ours — but a number
        // has to be the thing it applies to.
        assert!(g.rows[1].cells[1].kind.align_right());
        assert!(!g.rows[1].cells[0].kind.align_right());
    }

    /// Sparse rows are the normal case in a real workbook: the gaps have to
    /// come out as empty cells in the right *positions*, not as a short row.
    #[test]
    fn a_sparse_row_gets_empty_cells_in_the_gaps() {
        let bytes = xlsx(&[
            vec!["A1", "B1", "C1", "D1"],
            vec!["", "B2", "", "D2"],
            vec![""; 4],
            vec!["A4", "", "", "D4"],
        ]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert_eq!(g.columns, ["A", "B", "C", "D"]);
        for row in g.rows {
            assert_eq!(
                row.cells.len(),
                g.columns.len(),
                "row {:?} is ragged",
                row.label
            );
        }
        assert_eq!(texts(&g.rows[1]), ["", "B2", "", "D2"]);
        // A blank row inside the data is still a row, of empty cells.
        assert_eq!(g.rows[2].label, "3");
        assert_eq!(texts(&g.rows[2]), ["", "", "", ""]);
        assert_eq!(kinds(&g.rows[2]), [CellKind::Text; 4]);
        assert_eq!(texts(&g.rows[3]), ["A4", "", "", "D4"]);
    }

    /// A sheet whose data starts in the middle of the grid is anchored on its
    /// first populated column, and the letters say which one that is.
    #[test]
    fn columns_are_labelled_from_the_first_populated_one() {
        let bytes = xlsx(&[vec!["", "", "C1", "D1"], vec!["", "", "C2", ""]]);
        let g_owner = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&g_owner);
        assert_eq!(g.columns, ["C", "D"]);
        assert_eq!(texts(&g.rows[0]), ["C1", "D1"]);
        assert_eq!(texts(&g.rows[1]), ["C2", ""]);
    }

    #[test]
    fn every_sheet_is_named_and_the_active_one_is_the_first() {
        let bytes = xlsx(&[vec!["a"]]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);
        assert_eq!(g.sheets, ["Data", "Notes"]);
        assert_eq!(g.active_sheet, 0);
        assert_eq!(g.sheets[g.active_sheet], "Data");
    }

    #[test]
    fn max_lines_bounds_the_rows_and_the_real_extent_is_still_reported() {
        let rows: Vec<Vec<&str>> = (0..200).map(|_| vec!["a", "b"]).collect();
        let bytes = xlsx(&rows);
        let opts = PreviewOptions {
            max_lines: 12,
            ..PreviewOptions::default()
        };
        let p = preview(&bytes, &opts).expect("render");
        let g = grid(&p);

        assert!(p.truncated);
        assert_eq!(g.rows.len(), 12);
        assert_eq!(g.rows[11].label, "12");
        // The sheet's declared extent survives the cap, so a frontend can say
        // "12 of 200".
        assert_eq!(g.total_rows, 200);
        assert_eq!(g.total_cols, 2);
    }

    /// Nothing is dropped for being far to the right any more: the frontend
    /// has the room, so it gets every column that was read.
    #[test]
    fn a_wide_sheet_keeps_all_the_columns_it_read() {
        let wide: Vec<&str> = (0..80).map(|_| "x").collect();
        let bytes = xlsx(&[wide]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert_eq!(g.columns.len(), 80);
        assert_eq!(g.columns.last().map(String::as_str), Some("CB"));
        assert_eq!(g.rows[0].cells.len(), 80);
        assert_eq!(g.total_cols, 80);
        assert!(!p.truncated, "nothing was hidden, so nothing to own up to");
    }

    /// `SCAN_COLS` still bounds the *read*, and when it bites the count of what
    /// is out there is still honest.
    #[test]
    fn columns_past_the_scan_cap_are_counted_but_not_read() {
        let wide: Vec<&str> = (0..513).map(|_| "x").collect();
        let bytes = xlsx(&[wide]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert!(p.truncated);
        assert_eq!(g.columns.len(), 512);
        assert_eq!(g.rows[0].cells.len(), 512);
        assert_eq!(g.total_cols, 513);
    }

    /// The bug report: a Vietnamese sheet whose cells were elided to fit a
    /// width core had guessed. Core now hands over the whole string.
    fn vietnamese() -> Vec<u8> {
        xlsx(&[
            vec!["STT", "Hoạt động", "Kết quả (giờ quy đổi)", "Ghi chú"],
            vec!["1.3", "Đứng lớp hướng dẫn thực hành", "47.3", ""],
            vec![
                "8",
                "Các hoạt động hỗ trợ khác",
                "12",
                "Hỗ trợ lễ bảo vệ khóa luận",
            ],
        ])
    }

    #[test]
    fn cells_carry_their_full_text_and_nothing_is_elided() {
        let p = preview(&vietnamese(), &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert_eq!(
            texts(&g.rows[0]),
            ["STT", "Hoạt động", "Kết quả (giờ quy đổi)", "Ghi chú"]
        );
        assert_eq!(
            texts(&g.rows[1]),
            ["1.3", "Đứng lớp hướng dẫn thực hành", "47.3", ""]
        );
        assert_eq!(
            texts(&g.rows[2]),
            [
                "8",
                "Các hoạt động hỗ trợ khác",
                "12",
                "Hỗ trợ lễ bảo vệ khóa luận"
            ]
        );
        assert!(
            !g.rows
                .iter()
                .flat_map(|r| r.cells.iter())
                .any(|c| c.text.contains('…')),
            "core must not elide anything"
        );
        assert!(!p.truncated);
        // "1.3" and "8" are numbers to Excel and stay numbers here.
        assert_eq!(g.rows[1].cells[0].kind, CellKind::Number);
        assert_eq!(g.rows[1].cells[2].kind, CellKind::Number);
    }

    /// Width is the frontend's business now: the same workbook produces the
    /// same table however wide the caller says it is.
    #[test]
    fn the_width_hint_no_longer_changes_anything() {
        let bytes = vietnamese();
        let at = |width: Option<usize>| {
            let opts = PreviewOptions {
                text_width: width,
                ..PreviewOptions::default()
            };
            let p = preview(&bytes, &opts).expect("render");
            let g = grid(&p);
            let rows: Vec<Vec<String>> = g
                .rows
                .iter()
                .map(|r| r.cells.iter().map(|c| c.text.clone()).collect())
                .collect();
            (g.columns.to_vec(), rows, p.truncated)
        };

        let reference = at(None);
        for width in [Some(0usize), Some(1), Some(40), Some(200), Some(4096)] {
            assert_eq!(at(width), reference, "width {width:?} changed the table");
        }
    }

    /// One 4 000-character note is bounded when it is read, and that is a cap
    /// biting, so the preview says so.
    #[test]
    fn one_pathological_cell_is_bounded_and_flagged() {
        let huge = "x".repeat(4000);
        let bytes = xlsx(&[
            vec!["id", "note", "qty", "ok"],
            vec!["7", &huge, "1234", "yes"],
        ]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let g = grid(&p);

        assert!(p.truncated);
        let note = &g.rows[1].cells[1];
        assert_eq!(note.text.chars().count(), 256);
        assert!(note.text.chars().all(|c| c == 'x'));
        // Its neighbours are untouched — one big cell costs only itself.
        assert_eq!(texts(&g.rows[1]), ["7", note.text.as_str(), "1234", "yes"]);
    }

    #[test]
    fn corrupt_workbook_is_a_format_error_not_a_panic() {
        let good = xlsx(&[vec!["a"]]);
        let half = good[..good.len() / 2].to_vec();
        let err = preview(&half, &PreviewOptions::default()).expect_err("should fail");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn garbage_bytes_are_a_format_error_not_a_panic() {
        let bytes = vec![0xA5u8; 4096];
        let err = preview(&bytes, &PreviewOptions::default()).expect_err("should fail");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }

    #[test]
    fn cancellation_is_reported_not_swallowed() {
        let bytes = xlsx(&[vec!["a", "b"]]);
        let fixture = temp_file("cancelled.xlsx", &bytes);
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = render(
            &fixture.0,
            "xlsx",
            bytes,
            &PreviewOptions::default(),
            &cancel,
        )
        .expect_err("should cancel");
        assert!(matches!(err, PreviewError::Cancelled), "got {err:?}");
    }

    /// Detection reads the zip's parts, not the file name: `xl/workbook.xml`
    /// is what makes this a spreadsheet.
    #[test]
    fn xlsx_is_detected_by_content_not_extension() {
        use crate::detect::{detect, Detected};

        let fixture = temp_file("mystery.dat", &xlsx(&[vec!["a"]]));
        let detected = detect(&fixture.0, &PreviewOptions::default()).expect("detect");
        assert!(
            matches!(&detected, Detected::Spreadsheet { format, .. } if format == "xlsx"),
            "got {detected:?}"
        );
    }

    #[test]
    fn unknown_format_reports_a_format_error() {
        let fixture = temp_file("thing.numbers", b"nope");
        let err = render(
            &fixture.0,
            "numbers",
            b"nope".to_vec(),
            &PreviewOptions::default(),
            &CancelToken::new(),
        )
        .expect_err("should fail");
        assert!(matches!(err, PreviewError::Format(_)), "got {err:?}");
    }
}
