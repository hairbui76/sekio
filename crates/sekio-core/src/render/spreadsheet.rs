//! Spreadsheet preview: xlsx/xlsm, xlsb, legacy xls (BIFF) and ods, all read
//! through `calamine` — pure Rust, so nothing here needs a C toolchain.
//!
//! The output is an aligned text table in the ordinary `PreviewContent::Text`
//! IR: a header line naming every sheet (the previewed one bracketed), a row of
//! column letters, then the cells. Frontends paint it exactly like source code,
//! so no frontend has to learn what a spreadsheet is.
//!
//! Column widths are laid out for `PreviewOptions::text_width` — how many
//! characters the frontend says it has room for. Nothing is elided while the
//! columns' natural widths fit inside it; past that the shortfall comes out of
//! the widest columns first. See `plan` for the rule.
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
        CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, Span, StyledLine,
    };

    /// Palette, harmonised with the syntect theme the text renderer uses
    /// (base16-ocean.dark) so a sheet and a source file sit side by side
    /// without clashing. Comments name the base16 slot.
    pub(super) mod palette {
        pub type Rgb = (u8, u8, u8);

        /// Sheet-list label, column letters, row numbers. base0D
        pub const HEADER: Rgb = (0x8f, 0xa1, 0xb3);
        /// The sheet actually being previewed. base0B
        pub const ACTIVE: Rgb = (0xa3, 0xbe, 0x8c);
        /// Ordinary string cells. base05
        pub const TEXT: Rgb = (0xc0, 0xc5, 0xce);
        /// Numeric cells — deliberately distinct from text. base09
        pub const NUMBER: Rgb = (0xd0, 0x87, 0x70);
        /// Booleans. base0E
        pub const BOOL: Rgb = (0xb4, 0x8e, 0xad);
        /// Dates, times and durations. base0C
        pub const DATE: Rgb = (0x96, 0xb5, 0xb4);
        /// `#REF!` and friends. base08
        pub const ERROR: Rgb = (0xbf, 0x61, 0x6a);
        /// Empty cells and the trailing summary. base03
        pub const DIM: Rgb = (0x65, 0x73, 0x7e);
    }

    /// Poll the cancel token every this many rows / cells of work.
    const CANCEL_INTERVAL: usize = 64;
    /// Columns shown. Wider sheets are reported in the trailing summary.
    const MAX_COLS: usize = 32;
    /// Columns kept while collecting, before the visible window is chosen.
    /// Bounds memory on a sheet with 16 384 populated columns.
    const SCAN_COLS: u32 = 512;
    /// Characters kept from one cell. A cell can legally hold 32 767 of them;
    /// a preview pane can show a few dozen.
    const MAX_CELL_CHARS: usize = 64;
    /// Hard ceiling on one column's printed width, however much room the pane
    /// has. Without it a single 4 000-character note would be the only thing on
    /// the line.
    const MAX_COL_WIDTH: usize = 40;
    /// Narrowest a squeezed column is allowed to get: two characters and the
    /// `…` that says there were more. A column that cannot have this much is
    /// dropped instead, and reported in the summary.
    const MIN_COL_WIDTH: usize = 3;
    /// Spaces between two columns.
    const COL_GAP: &str = "  ";
    /// Lines spent on the sheet list and the column letters.
    const HEADER_LINES: usize = 2;
    /// Sheet names listed before the header line gives up and counts the rest.
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

        let (lines, truncated) = match built {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(PreviewError::Format("malformed spreadsheet".into())),
        };

        Ok(Preview {
            content: PreviewContent::Text {
                lines,
                language: language_of(format).to_string(),
            },
            truncated,
        })
    }

    fn language_of(format: &str) -> &'static str {
        match format {
            "xlsb" => "Excel Binary Workbook",
            "xls" => "Excel Spreadsheet (legacy)",
            "ods" => "OpenDocument Spreadsheet",
            _ => "Excel Spreadsheet",
        }
    }

    fn build(
        path: &Path,
        format: &str,
        opts: &PreviewOptions,
        cancel: &CancelToken,
    ) -> Result<(Vec<StyledLine>, bool), PreviewError> {
        // Reserve room for the sheet list, the column letters and the trailing
        // summary so the finished preview still fits inside `max_lines`.
        let max_rows = opts.max_lines.saturating_sub(HEADER_LINES + 1).max(1);

        let (names, sheet) = read(path, format, max_rows, cancel)?;
        cancel.check()?;
        Ok(layout(&names, &sheet, opts))
    }

    // ------------------------------------------------------------- reading

    /// One cell's rendered text plus what kind of value produced it, which is
    /// all the layout pass needs to colour and align it.
    #[derive(Clone)]
    struct CellText {
        text: String,
        kind: Kind,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Text,
        Number,
        Bool,
        Date,
        Error,
    }

    /// Rows are sparse — `(column, value)` in ascending column order, empty
    /// cells simply absent — so a sheet with one value in column ZZ costs one
    /// entry rather than 700.
    type Row = (u32, Vec<(u32, CellText)>);

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
            let cell = match reader.next_cell() {
                Ok(Some(cell)) => cell,
                Ok(None) => break,
                Err(e) => return Err(fmt_err("xlsx", e)),
            };
            if matches!(cell.get_value(), DataRef::Empty) {
                continue;
            }

            let (r, c) = cell.get_position();
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
            rows[idx].1.push((c, cell_of_ref(cell.get_value())));
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
                cells.push((base_col + j as u32, cell_of_data(value)));
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

    // ------------------------------------------------------------ cell text

    fn cell_of_data(value: &Data) -> CellText {
        match value {
            Data::Int(i) => CellText::new(i.to_string(), Kind::Number),
            Data::Float(f) => CellText::new(number(*f), Kind::Number),
            Data::String(s) => CellText::new(clean(s), Kind::Text),
            Data::Bool(b) => CellText::new(bool_text(*b), Kind::Bool),
            Data::DateTime(dt) => CellText::new(datetime(dt), Kind::Date),
            Data::DateTimeIso(s) | Data::DurationIso(s) => CellText::new(clean(s), Kind::Date),
            Data::Error(e) => CellText::new(clean(&e.to_string()), Kind::Error),
            Data::Empty => CellText::new(String::new(), Kind::Text),
        }
    }

    fn cell_of_ref(value: &DataRef<'_>) -> CellText {
        match value {
            DataRef::Int(i) => CellText::new(i.to_string(), Kind::Number),
            DataRef::Float(f) => CellText::new(number(*f), Kind::Number),
            DataRef::String(s) => CellText::new(clean(s), Kind::Text),
            DataRef::SharedString(s) => CellText::new(clean(s), Kind::Text),
            DataRef::Bool(b) => CellText::new(bool_text(*b), Kind::Bool),
            DataRef::DateTime(dt) => CellText::new(datetime(dt), Kind::Date),
            DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => {
                CellText::new(clean(s), Kind::Date)
            }
            DataRef::Error(e) => CellText::new(clean(&e.to_string()), Kind::Error),
            DataRef::Empty => CellText::new(String::new(), Kind::Text),
        }
    }

    impl CellText {
        fn new(text: String, kind: Kind) -> Self {
            Self { text, kind }
        }
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

    /// One cell must stay one table cell: newlines and tabs become spaces,
    /// other control characters are dropped, and the whole thing is capped.
    fn clean(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                other => other,
            })
            .filter(|c| !c.is_control())
            .take(MAX_CELL_CHARS)
            .collect()
    }

    // --------------------------------------------------------------- layout

    #[derive(Clone, Copy)]
    struct Sty {
        fg: Option<palette::Rgb>,
        bold: bool,
    }

    impl Sty {
        fn new(fg: palette::Rgb) -> Self {
            Self {
                fg: Some(fg),
                bold: false,
            }
        }
        fn plain() -> Self {
            Self {
                fg: None,
                bold: false,
            }
        }
        fn bold(mut self) -> Self {
            self.bold = true;
            self
        }
    }

    fn span(text: impl Into<String>, sty: Sty) -> Span {
        Span {
            text: text.into(),
            fg: sty.fg,
            bold: sty.bold,
            italic: false,
        }
    }

    fn style_of(kind: Kind) -> Sty {
        Sty::new(match kind {
            Kind::Text => palette::TEXT,
            Kind::Number => palette::NUMBER,
            Kind::Bool => palette::BOOL,
            Kind::Date => palette::DATE,
            Kind::Error => palette::ERROR,
        })
    }

    fn layout(names: &[String], sheet: &Sheet, opts: &PreviewOptions) -> (Vec<StyledLine>, bool) {
        let min_col = min_col_of(&sheet.rows);
        let max_col = sheet
            .rows
            .iter()
            .filter_map(|(_, cells)| cells.last().map(|(c, _)| *c))
            .max()
            .unwrap_or(min_col);

        let spread = (max_col.saturating_sub(min_col) as usize) + 1;
        let scanned_cols = spread.clamp(1, MAX_COLS);

        // Dense window over the sparse rows: only the columns we might paint.
        let grid: Vec<Vec<Option<&CellText>>> = sheet
            .rows
            .iter()
            .map(|(_, cells)| {
                let mut row: Vec<Option<&CellText>> = vec![None; scanned_cols];
                for (c, cell) in cells {
                    if let Some(i) = c.checked_sub(min_col).map(|d| d as usize) {
                        if i < scanned_cols {
                            row[i] = Some(cell);
                        }
                    }
                }
                row
            })
            .collect();

        // What each column would need to show everything in it, unelided.
        let mut natural: Vec<usize> = (0..scanned_cols)
            .map(|i| {
                column_label(min_col.saturating_add(i as u32))
                    .chars()
                    .count()
            })
            .collect();
        for row in &grid {
            for (width, cell) in natural.iter_mut().zip(row.iter()) {
                if let Some(cell) = cell {
                    *width = (*width).max(cell.text.chars().count().min(MAX_COL_WIDTH));
                }
            }
        }

        // Row numbers are 1-based like every spreadsheet UI.
        let last_row_number = sheet.rows.last().map_or(1, |(r, _)| r.saturating_add(1));
        let gutter = last_row_number.to_string().len().max(3);

        // Now spend the frontend's width on those columns.
        let widths = plan(&natural, opts.line_width(), gutter);
        let shown_cols = widths.len();
        let squeezed = widths
            .iter()
            .zip(natural.iter())
            .any(|(shown, wanted)| shown < wanted);
        let col_truncated = spread > shown_cols || sheet.total_cols > shown_cols as u64;

        let mut lines = Vec::with_capacity(sheet.rows.len() + HEADER_LINES + 1);
        let (sheet_line, names_truncated) = sheet_header(names, opts);
        lines.push(sheet_line);
        lines.push(column_header(min_col, &widths, gutter));

        for ((row_number, _), row) in sheet.rows.iter().zip(grid.iter()) {
            lines.push(data_line(
                row_number.saturating_add(1),
                row,
                &widths,
                gutter,
            ));
        }

        let shown_rows = sheet.rows.len() as u64;
        if sheet.truncated || sheet.total_rows > shown_rows || col_truncated {
            lines.push(summary(sheet, shown_rows, shown_cols));
        }

        // Safety net: the row cap already reserved room for the header and the
        // summary, but a pathological `max_lines` of 1 must still be honoured.
        let mut truncated = sheet.truncated || col_truncated || names_truncated || squeezed;
        if lines.len() > opts.max_lines {
            lines.truncate(opts.max_lines);
            truncated = true;
        }
        (lines, truncated)
    }

    /// Decide a printed width for every column, spending the `budget`
    /// characters the frontend says it has.
    ///
    /// Two rules, in order:
    ///
    /// 1. **A column is never padded past what it needs.** `natural[i]` is the
    ///    widest thing in column `i` (already ceilinged at [`MAX_COL_WIDTH`]),
    ///    and no column is ever given more than that — a `STT` column stays
    ///    three characters wide however much room is going spare. So when the
    ///    natural widths fit the budget, they are used unchanged and nothing is
    ///    elided at all.
    /// 2. **When they do not fit, the shortfall comes out of the widest
    ///    columns.** A single cap is chosen by water-filling: every column
    ///    narrower than the cap keeps its full width, and every column above it
    ///    is cut to the cap. A three-character number column therefore never
    ///    loses a character so a prose column can keep one — the width comes
    ///    out of whoever has the most of it, which is also where a `…` costs
    ///    the least of the meaning. The handful of characters the integer
    ///    division leaves over go to the cut columns, left to right, so the
    ///    line fills the pane exactly rather than stopping short of it.
    ///
    /// Returns *fewer* widths than `natural` when the budget cannot seat every
    /// column at [`MIN_COL_WIDTH`]: past that point another column would show
    /// nothing but an ellipsis. The caller reports the dropped ones in the
    /// trailing summary, exactly as it does for the [`MAX_COLS`] cap.
    fn plan(natural: &[usize], budget: usize, gutter: usize) -> Vec<usize> {
        // Everything to the right of the row-number gutter.
        let room = budget.saturating_sub(gutter);
        // A column costs its own width plus the gap in front of it, so that is
        // what one has to be worth before it is shown at all. At least one is
        // always shown: a table with no columns is not a preview.
        let seats = (room / (COL_GAP.len() + MIN_COL_WIDTH)).max(1);
        let shown = natural.len().min(seats);
        let natural = &natural[..shown];
        // `.max(shown)` keeps the pathological case (a pane narrower than
        // `MIN_TEXT_WIDTH` can be) off the zero-width path below; every column
        // still gets at least one character.
        let content = room.saturating_sub(COL_GAP.len() * shown).max(shown);

        // Water level: walk the columns narrowest first, letting each keep its
        // natural width for as long as everything still to come could be capped
        // at that width and fit.
        let mut sorted: Vec<usize> = natural.to_vec();
        sorted.sort_unstable();
        let mut spent = 0usize;
        let mut rest = shown;
        for &width in &sorted {
            if spent + rest * width > content {
                break;
            }
            spent += width;
            rest -= 1;
        }
        if rest == 0 {
            // Everything fits at its natural width: nothing is elided.
            return natural.to_vec();
        }

        let left = content - spent;
        let cap = (left / rest).max(1);
        let mut spare = left.saturating_sub(cap * rest);
        natural
            .iter()
            .map(|&width| {
                if width <= cap {
                    width
                } else if spare > 0 {
                    spare -= 1;
                    cap + 1
                } else {
                    cap
                }
            })
            .collect()
    }

    /// `Sheets: [Data]  Notes  Q3` — the previewed one bracketed and bold.
    fn sheet_header(names: &[String], opts: &PreviewOptions) -> (StyledLine, bool) {
        let mut spans = vec![span("Sheets: ", Sty::new(palette::HEADER))];
        let cap = MAX_SHEET_NAMES.min(opts.max_entries.max(1));
        let truncated = names.len() > cap;

        for (i, name) in names.iter().take(cap).enumerate() {
            if i > 0 {
                spans.push(span(COL_GAP, Sty::plain()));
            }
            if i == 0 {
                // The first sheet is the one we preview.
                spans.push(span(
                    format!("[{}]", clean(name)),
                    Sty::new(palette::ACTIVE).bold(),
                ));
            } else {
                spans.push(span(clean(name), Sty::new(palette::DIM)));
            }
        }
        if truncated {
            spans.push(span(
                format!("  +{} more", names.len() - cap),
                Sty::new(palette::DIM),
            ));
        }
        (StyledLine { spans }, truncated)
    }

    fn column_header(min_col: u32, widths: &[usize], gutter: usize) -> StyledLine {
        let mut spans = vec![span(" ".repeat(gutter), Sty::plain())];
        for (i, width) in widths.iter().enumerate() {
            spans.push(span(COL_GAP, Sty::plain()));
            let label = column_label(min_col.saturating_add(i as u32));
            spans.push(span(pad(&label, *width, false), Sty::new(palette::HEADER)));
        }
        finish(spans)
    }

    fn data_line(
        row_number: u32,
        row: &[Option<&CellText>],
        widths: &[usize],
        gutter: usize,
    ) -> StyledLine {
        let mut spans = vec![span(
            pad(&row_number.to_string(), gutter, true),
            Sty::new(palette::HEADER),
        )];
        for (cell, width) in row.iter().zip(widths.iter()) {
            spans.push(span(COL_GAP, Sty::plain()));
            match cell {
                Some(cell) => {
                    // Numbers right-align so decimal points line up; text
                    // left-aligns, exactly as a spreadsheet would show it.
                    let right = cell.kind == Kind::Number;
                    spans.push(span(pad(&cell.text, *width, right), style_of(cell.kind)));
                }
                // An empty cell still occupies its column, dimmed.
                None => spans.push(span(" ".repeat(*width), Sty::new(palette::DIM))),
            }
        }
        finish(spans)
    }

    fn summary(sheet: &Sheet, shown_rows: u64, shown_cols: usize) -> StyledLine {
        let total_rows = sheet.total_rows.max(shown_rows);
        let total_cols = sheet.total_cols.max(shown_cols as u64);
        let text = if total_rows > shown_rows || total_cols > shown_cols as u64 {
            format!(
                "{total_rows} rows × {total_cols} columns — showing {shown_rows} × {shown_cols}"
            )
        } else {
            // A cap bit, but the sheet never declared its size (xlsx written
            // without a `<dimension>`), so "there is more" is all we honestly
            // know. Better a vague note than a confident wrong number.
            format!("showing first {shown_rows} rows × {shown_cols} columns — more follow")
        };
        StyledLine {
            spans: vec![span(text, Sty::new(palette::DIM))],
        }
    }

    /// Drop trailing padding so a line carries no invisible whitespace tail.
    fn finish(mut spans: Vec<Span>) -> StyledLine {
        while spans
            .last()
            .is_some_and(|s| s.text.chars().all(|c| c == ' '))
        {
            spans.pop();
        }
        if let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end();
            if trimmed.len() != last.text.len() {
                last.text.truncate(trimmed.len());
            }
        }
        StyledLine { spans }
    }

    fn pad(text: &str, width: usize, right: bool) -> String {
        let shown = fit(text, width);
        let gap = width.saturating_sub(shown.chars().count());
        if right {
            format!("{}{}", " ".repeat(gap), shown)
        } else {
            format!("{}{}", shown, " ".repeat(gap))
        }
    }

    /// Elide with `…` when the value is wider than its column. Character
    /// counts, not display widths: core has no unicode-width dependency, so
    /// full-width CJK will run a little long. Everything still lines up on the
    /// character grid the frontends use.
    fn fit(text: &str, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let mut chars = text.chars();
        let head: String = chars.by_ref().take(width).collect();
        if chars.next().is_none() {
            return head;
        }
        let mut out: String = head.chars().take(width - 1).collect();
        out.push('…');
        out
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
        fn fit_elides_only_when_too_wide() {
            assert_eq!(fit("abc", 5), "abc");
            assert_eq!(fit("abc", 3), "abc");
            assert_eq!(fit("abcdef", 4), "abc…");
            assert_eq!(fit("abc", 0), "");
        }

        #[test]
        fn cells_are_flattened_to_one_line() {
            assert_eq!(clean("a\nb\tc"), "a b c");
            assert_eq!(clean(&"x".repeat(500)).chars().count(), MAX_CELL_CHARS);
        }

        // ------------------------------------------------------ width plan

        /// What one line of the table costs: the gutter plus a gap and a
        /// column for each width.
        fn line_cost(widths: &[usize], gutter: usize) -> usize {
            gutter + widths.iter().map(|w| w + COL_GAP.len()).sum::<usize>()
        }

        #[test]
        fn columns_that_fit_are_never_squeezed_or_padded() {
            // The user's sheet, roughly: a 3-char id, two prose columns and a
            // number column, in a 200-character window.
            let natural = [3, 20, 24, 30];
            let widths = plan(&natural, 200, 3);
            assert_eq!(
                widths,
                natural.to_vec(),
                "everything fits, so every column keeps exactly what it needs"
            );
            assert!(line_cost(&widths, 3) <= 200);
        }

        #[test]
        fn spare_room_is_not_handed_to_columns_that_do_not_want_it() {
            // 1 000 characters of pane and four narrow columns: the table stays
            // narrow rather than sprawling across the window.
            assert_eq!(plan(&[3, 4, 5, 6], 1000, 3), vec![3, 4, 5, 6]);
        }

        #[test]
        fn a_shortfall_comes_out_of_the_widest_columns_only() {
            // gutter 3 + 4 gaps of 2 leaves 40 characters of columns; the
            // natural widths want 78.
            let natural = [3, 5, 30, 40];
            let widths = plan(&natural, 51, 3);
            assert_eq!(line_cost(&widths, 3), 51, "the whole budget is spent");
            assert_eq!(
                &widths[..2],
                &[3, 5],
                "narrow columns must not lose a character to a prose column"
            );
            // The two wide ones share what is left equally: 40 - 3 - 5 = 32,
            // so 16 each.
            assert_eq!(&widths[2..], &[16, 16]);
        }

        /// Water-filling, not equal-shares: a column only loses width down to
        /// the level of the *next* widest, so a middling column that fits under
        /// the cap keeps everything while the giant beside it pays.
        #[test]
        fn only_the_columns_above_the_water_line_are_cut() {
            // Same budget as above, but the third column is 15 rather than 30 —
            // narrow enough to survive whole once the 40 has been cut back.
            let widths = plan(&[3, 5, 15, 40], 51, 3);
            assert_eq!(widths, vec![3, 5, 15, 17]);
            assert_eq!(line_cost(&widths, 3), 51);
        }

        #[test]
        fn the_leftover_characters_of_the_split_are_not_thrown_away() {
            // 3 + 5 = 8 spent, 33 left over two columns: 16 each with 1 spare,
            // which goes to the first column that was cut.
            let natural = [3, 5, 20, 40];
            let widths = plan(&natural, 52, 3);
            assert_eq!(widths, vec![3, 5, 17, 16]);
            assert_eq!(line_cost(&widths, 3), 52);
        }

        #[test]
        fn a_column_is_dropped_rather_than_shown_as_an_ellipsis() {
            // 40 characters of pane: gutter 3 leaves 37, and a column costs at
            // least 2 + 3, so seven columns is all that will seat.
            let natural = [8; 20];
            let widths = plan(&natural, 40, 3);
            assert_eq!(widths.len(), 7);
            assert!(
                widths.iter().all(|w| *w >= MIN_COL_WIDTH),
                "a shown column must be wide enough to say something: {widths:?}"
            );
            assert!(line_cost(&widths, 3) <= 40, "{widths:?}");
        }

        #[test]
        fn a_pane_too_narrow_for_even_one_column_still_lays_out() {
            for budget in 0..12usize {
                for gutter in [0usize, 3, 9] {
                    let widths = plan(&[3, 30, 7], budget, gutter);
                    assert_eq!(widths.len().min(1), 1, "at least one column survives");
                    assert!(
                        widths.iter().all(|w| *w >= 1),
                        "budget {budget}, gutter {gutter}: {widths:?} has a zero-width column"
                    );
                }
            }
        }

        #[test]
        fn one_column_never_swallows_the_whole_line() {
            // One column of 40 (the ceiling) beside three tiny ones, in a pane
            // with room to spare: the tiny ones keep every character.
            let widths = plan(&[40, 3, 3, 3], 80, 3);
            assert_eq!(&widths[1..], &[3, 3, 3]);
            assert!(widths[0] <= MAX_COL_WIDTH);
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
    use crate::{CancelToken, Preview, PreviewContent, PreviewError, PreviewOptions, StyledLine};

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
    /// one worksheet whose cells are inline strings and numbers. Written with
    /// the `zip` crate so no binary fixture ever lands in the repo.
    fn xlsx(rows: &[Vec<&str>]) -> Vec<u8> {
        let mut sheet = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
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

    fn lines(p: &Preview) -> &[StyledLine] {
        match &p.content {
            PreviewContent::Text { lines, language } => {
                assert_eq!(language, "Excel Spreadsheet");
                lines
            }
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn plain(line: &StyledLine) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
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
        let rendered: Vec<String> = match &p.content {
            PreviewContent::Text { lines, language } => {
                assert_eq!(language, "OpenDocument Spreadsheet");
                lines.iter().map(plain).collect()
            }
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(rendered[0].contains("[Budget]"), "{rendered:?}");
        assert!(
            rendered.iter().any(|l| l.contains("Server")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("1200.5")),
            "{rendered:?}"
        );
    }

    #[test]
    fn cell_values_appear_and_columns_align() {
        let bytes = xlsx(&[
            vec!["Name", "Qty", "Price"],
            vec!["Widget", "12", "3.5"],
            vec!["Sprocket", "7", "11.25"],
        ]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let rendered: Vec<String> = lines(&p).iter().map(plain).collect();

        // Sheet list names both sheets, previewed one bracketed.
        assert!(rendered[0].contains("[Data]"), "{:?}", rendered[0]);
        assert!(rendered[0].contains("Notes"), "{:?}", rendered[0]);
        // Column letters.
        assert!(rendered[1].contains(" A "), "{:?}", rendered[1]);
        assert!(rendered[1].contains('B') && rendered[1].contains('C'));

        let body = &rendered[2..];
        assert!(body.iter().any(|l| l.contains("Widget")));
        assert!(body.iter().any(|l| l.contains("Sprocket")));
        assert!(body.iter().any(|l| l.contains("11.25")));

        // Alignment: column B occupies the same character span on every line.
        // Its width is 3 — the widest of "B", "Qty", "12", "7".
        let header = &rendered[1];
        let start = header
            .chars()
            .position(|c| c == 'B')
            .expect("column B in header");
        let field = |line: &str| {
            line.chars()
                .skip(start)
                .take(3)
                .collect::<String>()
                .trim()
                .to_string()
        };

        assert_eq!(field(header), "B");
        assert_eq!(field(&body[0]), "Qty", "header row: {:?}", body[0]);
        let widget = body
            .iter()
            .find(|l| l.contains("Widget"))
            .expect("widget row");
        let sprocket = body
            .iter()
            .find(|l| l.contains("Sprocket"))
            .expect("sprocket row");
        assert_eq!(field(widget), "12", "row {widget:?} vs header {header:?}");
        assert_eq!(
            field(sprocket),
            "7",
            "row {sprocket:?} vs header {header:?}"
        );
    }

    #[test]
    fn numbers_text_and_empties_get_distinct_styles() {
        let bytes = xlsx(&[vec!["Name", "Qty"], vec!["Widget", ""], vec!["", "42"]]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        let spans: Vec<_> = lines(&p).iter().flat_map(|l| l.spans.iter()).collect();

        let text = spans
            .iter()
            .find(|s| s.text.trim() == "Widget")
            .expect("text cell");
        let number = spans
            .iter()
            .find(|s| s.text.trim() == "42")
            .expect("number cell");
        assert_ne!(text.fg, number.fg, "numbers must not look like text");
        assert_eq!(number.fg, Some(super::imp::palette::NUMBER));

        // The empty cell next to "Widget" is padding painted in the dim slot.
        assert!(
            spans.iter().any(|s| s.fg == Some(super::imp::palette::DIM)
                && s.text.chars().all(|c| c == ' ')
                && !s.text.is_empty()),
            "an empty cell should be dim"
        );
    }

    #[test]
    fn max_lines_truncates_and_flags() {
        let rows: Vec<Vec<&str>> = (0..200).map(|_| vec!["a", "b"]).collect();
        let bytes = xlsx(&rows);
        let opts = PreviewOptions {
            max_lines: 12,
            ..PreviewOptions::default()
        };
        let p = preview(&bytes, &opts).expect("render");
        assert!(p.truncated);
        assert!(lines(&p).len() <= 12, "got {}", lines(&p).len());
        // And the trailing summary owns up to what was left out.
        let last = plain(lines(&p).last().expect("a line"));
        assert!(last.contains("rows"), "{last:?}");
    }

    #[test]
    fn wide_sheets_report_the_columns_they_hid() {
        let wide: Vec<&str> = (0..80).map(|_| "x").collect();
        let bytes = xlsx(&[wide]);
        let p = preview(&bytes, &PreviewOptions::default()).expect("render");
        assert!(p.truncated);
        let last = plain(lines(&p).last().expect("a line"));
        assert!(last.contains("80 columns"), "{last:?}");
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

    // ----------------------------------------------------------- widths

    /// The shape of the sheet from the bug report: a short id, two prose
    /// columns, a number column and a note. Its natural width is around 90
    /// characters, which is what makes it interesting both above and below.
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

    fn preview_at(bytes: &[u8], width: Option<usize>) -> Preview {
        let opts = PreviewOptions {
            text_width: width,
            ..PreviewOptions::default()
        };
        preview(bytes, &opts).expect("render")
    }

    fn widest(p: &Preview) -> usize {
        lines(p)
            .iter()
            .map(|l| plain(l).chars().count())
            .max()
            .unwrap_or(0)
    }

    /// The user's actual complaint: a table that needs ~90 characters was cut
    /// to 62 in a window with room for 200.
    #[test]
    fn a_wide_pane_leaves_the_table_whole() {
        let p = preview_at(&vietnamese(), Some(200));
        let rendered: Vec<String> = lines(&p).iter().map(plain).collect();

        assert!(
            !rendered.iter().any(|l| l.contains('…')),
            "nothing should be elided with 200 characters to spend:\n{}",
            rendered.join("\n")
        );
        assert!(!p.truncated, "nothing was cut, so nothing to own up to");
        // Every cell, in full.
        for cell in [
            "Kết quả (giờ quy đổi)",
            "Đứng lớp hướng dẫn thực hành",
            "Hỗ trợ lễ bảo vệ khóa luận",
        ] {
            assert!(
                rendered.iter().any(|l| l.contains(cell)),
                "{cell:?} is missing from:\n{}",
                rendered.join("\n")
            );
        }
        // …and the table still stops where its content does rather than being
        // padded across the whole window.
        assert!(
            widest(&p) < 200,
            "the table was padded out to the full pane: {} characters",
            widest(&p)
        );
    }

    /// The same sheet in a pane that genuinely cannot hold it: the number
    /// column keeps every digit, the prose gives the width up.
    #[test]
    fn a_narrow_pane_takes_the_width_from_the_prose_not_the_numbers() {
        let p = preview_at(&vietnamese(), Some(60));
        let rendered: Vec<String> = lines(&p).iter().map(plain).collect();
        let body = rendered.join("\n");

        assert!(p.truncated, "eliding a cell is a cap biting");
        assert!(
            rendered.iter().all(|l| l.chars().count() <= 60),
            "a line ran past the 60-character pane:\n{body}"
        );
        assert!(body.contains('…'), "something had to give:\n{body}");
        // The numbers are three and four characters wide and must survive
        // whole — losing a digit is losing the value.
        for number in ["47.3", "12"] {
            assert!(
                rendered.iter().any(|l| l.contains(number)),
                "the number {number:?} was elided to make room for prose:\n{body}"
            );
        }
        // The id column, too.
        assert!(rendered.iter().any(|l| l.contains("1.3")), "{body}");
        // And the prose is what paid for it.
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("Đứng lớp") && l.contains('…')),
            "the prose column should be the one elided:\n{body}"
        );
    }

    /// One 64-character note beside three tiny columns: the note is ceilinged
    /// rather than being handed the entire line, and the tiny columns are
    /// untouched.
    #[test]
    fn one_pathological_cell_does_not_consume_the_line() {
        let huge = "x".repeat(4000);
        let bytes = xlsx(&[
            vec!["id", "note", "qty", "ok"],
            vec!["7", &huge, "1234", "yes"],
        ]);
        let p = preview_at(&bytes, Some(200));
        let rendered: Vec<String> = lines(&p).iter().map(plain).collect();
        let body = rendered.join("\n");

        // The cell is capped at MAX_CELL_CHARS when read and the column at
        // MAX_COL_WIDTH when laid out, so the whole line stays modest even in
        // a very wide pane.
        assert!(
            widest(&p) <= 60,
            "one cell took the line out to {} characters:\n{body}",
            widest(&p)
        );
        let xs = rendered
            .iter()
            .find(|l| l.contains("xxx"))
            .expect("the wide row must be painted");
        assert!(
            xs.matches('x').count() <= 40,
            "the wide column was not ceilinged: {xs:?}"
        );
        // The columns beside it are complete.
        assert!(xs.contains("1234"), "{xs:?}");
        assert!(xs.contains("yes"), "{xs:?}");
    }

    /// Forty characters is about as narrow as a preview pane ever gets. It has
    /// to stay a table: no panic, no zero-width columns, nothing past the edge.
    #[test]
    fn a_forty_character_pane_still_lays_out_a_table() {
        let bytes = xlsx(&[
            vec!["STT", "Hoạt động", "Kết quả", "Ghi chú", "Thêm"],
            vec!["1", "Trợ giảng lý thuyết", "47.3", "abc", "def"],
        ]);
        let p = preview_at(&bytes, Some(40));
        let rendered: Vec<String> = lines(&p).iter().map(plain).collect();
        let body = rendered.join("\n");

        // The sheet-name header is one long label and is not a column, so skip
        // it; every line of the table proper fits.
        for line in &rendered[1..] {
            assert!(
                line.chars().count() <= 40,
                "{line:?} runs past a 40-character pane:\n{body}"
            );
        }
        // Column letters are still aligned over real columns.
        assert!(
            rendered[1].contains('A') && rendered[1].contains('B'),
            "{body}"
        );
        assert!(rendered.iter().any(|l| l.contains("47.3")), "{body}");
        assert!(p.truncated);
    }

    /// Down to nothing at all: absurd widths must not panic or underflow.
    #[test]
    fn absurd_widths_do_not_panic() {
        let bytes = vietnamese();
        for width in [0usize, 1, 2, 5, 11, 19, 20, 21, 4096] {
            let p = preview_at(&bytes, Some(width));
            assert!(!lines(&p).is_empty(), "width {width} produced nothing");
        }
    }

    /// The hint is optional, and leaving it out has to behave like the default
    /// every existing caller already gets.
    #[test]
    fn no_hint_lays_out_exactly_like_the_default_width() {
        let bytes = vietnamese();
        let unhinted: Vec<String> = lines(&preview_at(&bytes, None)).iter().map(plain).collect();
        let defaulted: Vec<String> = lines(&preview_at(&bytes, Some(crate::DEFAULT_TEXT_WIDTH)))
            .iter()
            .map(plain)
            .collect();
        assert_eq!(unhinted, defaulted);
    }

    /// The hint really is what decides the layout: wider in, wider table out.
    #[test]
    fn a_wider_hint_produces_a_wider_table() {
        let bytes = vietnamese();
        let narrow = preview_at(&bytes, Some(50));
        let wide = preview_at(&bytes, Some(200));
        assert!(
            widest(&wide) > widest(&narrow),
            "50 chars gave {} and 200 gave {}",
            widest(&narrow),
            widest(&wide)
        );
        assert!(widest(&narrow) <= 50);
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
