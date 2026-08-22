//! G4 Task 4: pure serialization for grid exports (CSV/TSV/JSON/INSERT).
//!
//! Deliberately GPUI-free and writer-agnostic (`std::io::Write`) — same
//! "pure core, GPUI-free shell wires it up" split as `row_view.rs`: tests
//! here feed a `Vec<u8>` sink directly, no window/entity needed. `grid.rs`
//! is the only caller; it builds the `cell` accessor over `RowView` +
//! `hidden_cols` so this module never has to know about display order or
//! column visibility itself — same "closure does the mapping" shape
//! `row_view::find_matches` already uses.
//!
//! Null-awareness: `cell(row, col)` returns `Option<String>` — `None` means
//! the source Arrow value is SQL NULL (see `dbc_buffer::ResultBuffer::
//! cell_is_null`), `Some(text)` is the display text otherwise. The cell's
//! *text* alone can't disambiguate a real NULL from a literal string that
//! happens to read "NULL" or "" — that's exactly why the accessor is
//! `Option`-typed rather than reusing `cell_text`'s plain `String`.

use std::io::Write;

use dbc_core::quote_ident;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    Insert,
}

impl ExportFormat {
    /// File extension (no leading dot) used for the save-dialog's suggested
    /// filename and the Downloads-fallback filename (see `grid.rs`'s
    /// `start_export`).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::Insert => "sql",
        }
    }
}

/// Streams `rows` rows (already in the caller's desired — i.e. DISPLAY —
/// order) through `cell(row, col) -> Option<String>` into `w`, where `col`
/// indexes into `headers` (0..headers.len()); `headers` is the VISIBLE
/// column list only — this function has no notion of hidden columns, the
/// caller simply never mentions them. `table_name` is only used by
/// `ExportFormat::Insert`.
pub fn export<W: Write>(
    w: &mut W,
    format: ExportFormat,
    headers: &[String],
    table_name: &str,
    rows: usize,
    cell: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> Result<(), String> {
    let result = match format {
        ExportFormat::Csv => export_csv(w, headers, rows, cell),
        ExportFormat::Tsv => export_tsv(w, headers, rows, cell),
        ExportFormat::Json => export_json(w, headers, rows, cell),
        ExportFormat::Insert => export_insert(w, headers, table_name, rows, cell),
    };
    result.map_err(|e| e.to_string())
}

/// RFC4180: a field is quoted (with embedded `"` doubled) only when it
/// contains the delimiter, a quote, or a line break — fields that don't
/// need it are written bare, matching what most spreadsheet tools produce.
/// A `None` (SQL NULL) cell is written as a bare empty field, same as an
/// empty-string cell — CSV has no native NULL marker, so the two are
/// indistinguishable on round-trip; documented rather than solved (the
/// brief doesn't ask for a NULL sentinel in CSV/TSV, only in INSERT).
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn export_csv<W: Write>(
    w: &mut W,
    headers: &[String],
    rows: usize,
    cell: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> std::io::Result<()> {
    write_csv_row(w, headers.iter().map(|h| csv_field(h)))?;
    for r in 0..rows {
        write_csv_row(
            w,
            (0..headers.len()).map(|c| cell(r, c).map(|v| csv_field(&v)).unwrap_or_default()),
        )?;
    }
    Ok(())
}

fn write_csv_row<W: Write>(w: &mut W, fields: impl Iterator<Item = String>) -> std::io::Result<()> {
    let mut first = true;
    for f in fields {
        if !first {
            w.write_all(b",")?;
        }
        first = false;
        w.write_all(f.as_bytes())?;
    }
    // RFC4180 line ending.
    w.write_all(b"\r\n")
}

/// TSV has no quoting mechanism, so instead of RFC4180-style escaping, any
/// tab/CR/LF embedded in a cell is replaced with a single space (brief's
/// documented lossy behaviour) — a value round-tripped through TSV never
/// contains the delimiter or a line break, at the cost of that information.
/// Line endings are CRLF, same choice as CSV, for consistency between the
/// two delimited formats (TSV has no RFC to defer to either way).
fn tsv_sanitize(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

fn export_tsv<W: Write>(
    w: &mut W,
    headers: &[String],
    rows: usize,
    cell: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> std::io::Result<()> {
    write_tsv_row(w, headers.iter().map(|h| tsv_sanitize(h)))?;
    for r in 0..rows {
        write_tsv_row(
            w,
            (0..headers.len()).map(|c| cell(r, c).map(|v| tsv_sanitize(&v)).unwrap_or_default()),
        )?;
    }
    Ok(())
}

fn write_tsv_row<W: Write>(w: &mut W, fields: impl Iterator<Item = String>) -> std::io::Result<()> {
    let mut first = true;
    for f in fields {
        if !first {
            w.write_all(b"\t")?;
        }
        first = false;
        w.write_all(f.as_bytes())?;
    }
    w.write_all(b"\r\n")
}

/// Minimal JSON string escaper (quotes, backslash, the short C0 escapes,
/// and `\u00XX` for any other control character) — enough for arbitrary
/// database text without pulling in `serde_json` just for this.
fn write_json_string<W: Write>(w: &mut W, s: &str) -> std::io::Result<()> {
    w.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => w.write_all(b"\\\"")?,
            '\\' => w.write_all(b"\\\\")?,
            '\n' => w.write_all(b"\\n")?,
            '\r' => w.write_all(b"\\r")?,
            '\t' => w.write_all(b"\\t")?,
            c if (c as u32) < 0x20 => write!(w, "\\u{:04x}", c as u32)?,
            c => write!(w, "{c}")?,
        }
    }
    w.write_all(b"\"")
}

/// Streaming array-of-objects: one row is formatted and written at a time
/// (`cell` is called row-by-row, column-by-column) rather than building a
/// `serde_json::Value` tree for the whole result first — the brief calls
/// out "no full materialization", which matters once a result is large
/// enough to have partially spilled to disk. Values are strings (typed
/// JSON — numbers/bools as native JSON types — is left for a later task per
/// the interface comment); a `None` cell is the JSON literal `null`, not
/// the string `"null"`.
fn export_json<W: Write>(
    w: &mut W,
    headers: &[String],
    rows: usize,
    cell: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> std::io::Result<()> {
    w.write_all(b"[\n")?;
    for r in 0..rows {
        if r > 0 {
            w.write_all(b",\n")?;
        }
        w.write_all(b"  {")?;
        for (c, h) in headers.iter().enumerate() {
            if c > 0 {
                w.write_all(b",")?;
            }
            write_json_string(w, h)?;
            w.write_all(b":")?;
            match cell(r, c) {
                Some(v) => write_json_string(w, &v)?,
                None => w.write_all(b"null")?,
            }
        }
        w.write_all(b"}")?;
    }
    w.write_all(b"\n]\n")
}

/// `'` doubled — the standard SQL string-literal escape (same family as
/// `quote_ident`'s `"` doubling for identifiers, just a different quote
/// character and a different grammar position).
fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// One `INSERT INTO ... VALUES (...);` statement per row. Table and column
/// identifiers go through `quote_ident` (shared with `dbc_core::ddl`'s DDL
/// quoting) so a table/column literally named `we"ird` round-trips safely.
/// A `None` cell is the bare `NULL` keyword (unquoted — quoting it would
/// insert the four-character string `'NULL'` instead of the SQL NULL
/// value), everything else is a single-quoted, escaped string literal —
/// the brief doesn't ask for typed literals (numbers unquoted, etc.), so
/// every non-null value round-trips through a portable quoted string.
fn export_insert<W: Write>(
    w: &mut W,
    headers: &[String],
    table_name: &str,
    rows: usize,
    cell: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> std::io::Result<()> {
    let cols = headers.iter().map(|h| quote_ident(h)).collect::<Vec<_>>().join(", ");
    let table = quote_ident(table_name);
    for r in 0..rows {
        let values = (0..headers.len())
            .map(|c| match cell(r, c) {
                Some(v) => format!("'{}'", sql_escape(&v)),
                None => "NULL".to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(w, "INSERT INTO {table} ({cols}) VALUES ({values});")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn run(format: ExportFormat, headers: &[String], table: &str, rows: usize, cell: &mut dyn FnMut(usize, usize) -> Option<String>) -> String {
        let mut buf = Vec::new();
        export(&mut buf, format, headers, table, rows, cell).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // --- CSV ---

    #[test]
    fn csv_plain_fields_are_unquoted_with_crlf_line_ends() {
        let h = headers(&["a", "b"]);
        let data = vec![vec!["1".to_string(), "x".to_string()]];
        let out = run(ExportFormat::Csv, &h, "t", 1, &mut |r, c| Some(data[r][c].clone()));
        assert_eq!(out, "a,b\r\n1,x\r\n");
    }

    #[test]
    fn csv_quotes_fields_containing_comma_quote_or_newline() {
        let h = headers(&["a"]);
        let rows = vec!["has,comma", "has\"quote", "has\nnewline"];
        let out = run(ExportFormat::Csv, &h, "t", rows.len(), &mut |r, _c| Some(rows[r].to_string()));
        assert_eq!(out, "a\r\n\"has,comma\"\r\n\"has\"\"quote\"\r\n\"has\nnewline\"\r\n");
    }

    #[test]
    fn csv_null_cell_is_a_bare_empty_field() {
        let h = headers(&["a", "b"]);
        let out = run(ExportFormat::Csv, &h, "t", 1, &mut |_r, c| if c == 0 { None } else { Some("x".to_string()) });
        assert_eq!(out, "a,b\r\n,x\r\n");
    }

    // --- TSV ---

    #[test]
    fn tsv_replaces_tabs_and_newlines_with_spaces() {
        let h = headers(&["a"]);
        let out = run(ExportFormat::Tsv, &h, "t", 1, &mut |_r, _c| Some("has\ttab\nand\rnewline".to_string()));
        assert_eq!(out, "a\r\nhas tab and newline\r\n");
    }

    #[test]
    fn tsv_joins_columns_with_tab() {
        let h = headers(&["a", "b"]);
        let out = run(ExportFormat::Tsv, &h, "t", 1, &mut |_r, c| Some(if c == 0 { "1".to_string() } else { "2".to_string() }));
        assert_eq!(out, "a\tb\r\n1\t2\r\n");
    }

    // --- JSON ---

    #[test]
    fn json_escapes_quotes_backslashes_and_control_chars() {
        let h = headers(&["a"]);
        let out = run(ExportFormat::Json, &h, "t", 1, &mut |_r, _c| Some("quote\" back\\ tab\t nl\n".to_string()));
        assert_eq!(out, "[\n  {\"a\":\"quote\\\" back\\\\ tab\\t nl\\n\"}\n]\n");
    }

    #[test]
    fn json_null_cell_is_the_null_literal_not_a_string() {
        let h = headers(&["a", "b"]);
        let out = run(ExportFormat::Json, &h, "t", 1, &mut |_r, c| if c == 0 { None } else { Some("x".to_string()) });
        assert_eq!(out, "[\n  {\"a\":null,\"b\":\"x\"}\n]\n");
    }

    #[test]
    fn json_multiple_rows_are_comma_separated() {
        let h = headers(&["a"]);
        let rows = vec!["1", "2"];
        let out = run(ExportFormat::Json, &h, "t", rows.len(), &mut |r, _c| Some(rows[r].to_string()));
        assert_eq!(out, "[\n  {\"a\":\"1\"},\n  {\"a\":\"2\"}\n]\n");
    }

    // --- INSERT ---

    #[test]
    fn insert_quotes_table_and_columns_and_escapes_values() {
        let h = headers(&["id", "note"]);
        let out = run(ExportFormat::Insert, &h, "orders", 1, &mut |_r, c| {
            if c == 0 { Some("1".to_string()) } else { Some("it's fine".to_string()) }
        });
        assert_eq!(out, "INSERT INTO \"orders\" (\"id\", \"note\") VALUES ('1', 'it''s fine');\n");
    }

    #[test]
    fn insert_null_cell_is_the_bare_null_keyword() {
        let h = headers(&["id", "note"]);
        let out = run(ExportFormat::Insert, &h, "orders", 1, &mut |_r, c| if c == 0 { Some("1".to_string()) } else { None });
        assert_eq!(out, "INSERT INTO \"orders\" (\"id\", \"note\") VALUES ('1', NULL);\n");
    }

    #[test]
    fn insert_quotes_a_weird_table_and_column_name() {
        let h = headers(&["we\"ird"]);
        let out = run(ExportFormat::Insert, &h, "we\"ird", 1, &mut |_r, _c| Some("v".to_string()));
        assert_eq!(out, "INSERT INTO \"we\"\"ird\" (\"we\"\"ird\") VALUES ('v');\n");
    }

    #[test]
    fn insert_emits_one_statement_per_row_in_given_order() {
        let h = headers(&["id"]);
        let rows = vec!["1", "2", "3"];
        let out = run(ExportFormat::Insert, &h, "t", rows.len(), &mut |r, _c| Some(rows[r].to_string()));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("VALUES ('1')"));
        assert!(lines[2].contains("VALUES ('3')"));
    }

    // --- display order / visible-only correctness ---

    /// Simulates what `grid.rs` actually does: `headers` is a REORDERED,
    /// SUBSET view of some wider source schema (as if a middle column were
    /// hidden), and `cell`'s closure maps the given (display_row, display_col)
    /// through an arbitrary permutation — exactly the shape of
    /// `view.source_row` + a visible-column index list. `export` itself must
    /// not need to know anything about that mapping: it only ever asks for
    /// `0..rows` × `0..headers.len()`, in that order, and the output must
    /// reflect whatever the accessor returned for those coordinates.
    #[test]
    fn export_follows_caller_supplied_display_order_and_visible_columns_only() {
        // Source schema (never referenced by name below): id, secret, name.
        // "secret" (source col 1) is hidden — headers only mention id/name.
        let h = headers(&["name", "id"]); // also reordered vs. source
        let source_rows = vec![
            ("row-a", "hidden-a", "1"), // source row 0
            ("row-b", "hidden-b", "2"), // source row 1
        ];
        // Display order is REVERSED relative to source (as a sort would do):
        // display row 0 -> source row 1, display row 1 -> source row 0.
        let display_to_source = [1usize, 0usize];
        let out = run(ExportFormat::Csv, &h, "t", 2, &mut |r, c| {
            let src = display_to_source[r];
            let (name, _secret, id) = source_rows[src];
            Some(match c {
                0 => name.to_string(), // "name"
                1 => id.to_string(),   // "id"
                _ => unreachable!(),
            })
        });
        assert_eq!(out, "name,id\r\nrow-b,2\r\nrow-a,1\r\n");
        assert!(!out.contains("hidden"), "hidden column must never appear in output");
    }
}
