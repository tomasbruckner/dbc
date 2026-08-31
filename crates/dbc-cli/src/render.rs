//! Turning a table of strings into stdout — pure, and unaware of where the
//! rows came from.
//!
//! Everything this binary prints is a table: connections, databases,
//! tables, query results. So the whole output layer is one shape
//! ([`Table`]) and three renderers, which is what keeps `--format json`
//! from meaning something subtly different per command.

use crate::args::Format;

/// A finished result: column names, and rows of already-stringified cells.
///
/// `None` is a NULL and prints differently from an empty string in every
/// format — a distinction a `String` cell could not carry, and the one a
/// person reading query output actually needs.
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// A row cap cut this short. Reported in every format, because a
    /// truncated result that looks complete is worse than no result.
    pub truncated: bool,
}

impl Table {
    pub fn new(columns: Vec<String>) -> Table {
        Table { columns, rows: Vec::new(), truncated: false }
    }

    /// Convenience for the metadata commands, whose cells are never NULL.
    pub fn push_str_row(&mut self, cells: Vec<String>) {
        self.rows.push(cells.into_iter().map(Some).collect());
    }
}

/// What a NULL looks like in the aligned table. Distinct from an empty
/// cell on purpose; the braces make it unmistakable against a literal
/// string that happens to read „NULL".
const NULL_TABLE: &str = "‹null›";

pub fn render(table: &Table, format: Format) -> String {
    match format {
        Format::Table => render_table(table),
        Format::Json => render_json(table),
        Format::Csv => render_csv(table),
    }
}

/// Display width in terminal cells.
///
/// Counting `char`s rather than bytes, so a column of Czech names lines up
/// (`š` is two bytes and one column). Not grapheme- or width-aware beyond
/// that: combining marks and CJK will still drift, which is a cosmetic
/// misalignment in one format and never a wrong value.
fn width(s: &str) -> usize {
    s.chars().count()
}

fn render_table(table: &Table) -> String {
    let ncols = table.columns.len();
    let mut widths: Vec<usize> = table.columns.iter().map(|c| width(c)).collect();
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            let w = width(cell.as_deref().unwrap_or(NULL_TABLE));
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }

    let mut out = String::new();
    let pad = |out: &mut String, text: &str, w: usize, last: bool| {
        out.push_str(text);
        if !last {
            for _ in width(text)..w {
                out.push(' ');
            }
            out.push_str("  ");
        }
    };

    for (i, c) in table.columns.iter().enumerate() {
        pad(&mut out, c, widths[i], i + 1 == ncols);
    }
    out.push('\n');
    for (i, w) in widths.iter().enumerate() {
        let rule = "-".repeat(*w);
        pad(&mut out, &rule, *w, i + 1 == ncols);
    }
    out.push('\n');

    for row in &table.rows {
        for (i, w) in widths.iter().enumerate() {
            let cell = row.get(i).and_then(|c| c.as_deref()).unwrap_or(NULL_TABLE);
            pad(&mut out, cell, *w, i + 1 == ncols);
        }
        out.push('\n');
    }

    out.push_str(&format!("\n({} řádků)\n", table.rows.len()));
    if table.truncated {
        out.push_str("výsledek je oříznutý — zvyš --limit\n");
    }
    out
}

fn render_json(table: &Table) -> String {
    let rows: Vec<serde_json::Value> = table
        .rows
        .iter()
        .map(|r| {
            serde_json::Value::Array(
                r.iter()
                    .map(|c| match c {
                        Some(s) => serde_json::Value::String(s.clone()),
                        None => serde_json::Value::Null,
                    })
                    .collect(),
            )
        })
        .collect();
    let value = serde_json::json!({
        "columns": table.columns,
        "rows": rows,
        "row_count": table.rows.len(),
        "truncated": table.truncated,
    });
    // Pretty-printed: a CLI's JSON is read by people at least as often as
    // by programs, and `jq` does not care either way.
    format!("{}\n", serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into()))
}

/// RFC 4180: quote when the value contains a comma, a quote, or a newline;
/// double the quotes inside.
///
/// A NULL is written as an EMPTY, UNQUOTED field, which is the one
/// convention CSV has for it — and the reason `--format csv` cannot round
/// trip a NULL apart from an empty string. Anything that needs that
/// distinction wants `--format json`.
fn csv_field(cell: Option<&str>) -> String {
    let Some(s) = cell else { return String::new() };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn render_csv(table: &Table) -> String {
    let mut out = String::new();
    let head: Vec<String> = table.columns.iter().map(|c| csv_field(Some(c))).collect();
    out.push_str(&head.join(","));
    out.push('\n');
    for row in &table.rows {
        let cells: Vec<String> =
            row.iter().map(|c| csv_field(c.as_deref())).collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        let mut t = Table::new(vec!["id".into(), "jméno".into()]);
        t.rows.push(vec![Some("1".into()), Some("Alice".into())]);
        t.rows.push(vec![Some("2".into()), None]);
        t
    }

    /// Alignment is a COLUMN property, so the assertion has to count
    /// columns too: `dlouhá` is 6 characters and 7 bytes, and comparing
    /// byte offsets would fail on correctly aligned output.
    fn column_of(line: &str, needle: char) -> Option<usize> {
        line.chars().position(|c| c == needle)
    }

    #[test]
    fn the_table_aligns_on_the_widest_cell() {
        let mut t = Table::new(vec!["a".into(), "b".into()]);
        t.push_str_row(vec!["dlouhá hodnota".into(), "x".into()]);
        t.push_str_row(vec!["z".into(), "y".into()]);
        let out = render(&t, Format::Table);
        let lines: Vec<&str> = out.lines().collect();
        // header, rule, then the two rows — the second column starts at
        // the same character offset on every one of them.
        let head = column_of(lines[0], 'b').expect("header");
        assert_eq!(column_of(lines[2], 'x'), Some(head));
        assert_eq!(column_of(lines[3], 'y'), Some(head));
    }

    /// Non-ASCII must not throw the alignment off: `jméno` is 5 columns and
    /// 6 bytes, and a byte-counting pad would indent the row under it.
    #[test]
    fn a_multibyte_header_still_lines_up() {
        let out = render(&sample(), Format::Table);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(column_of(lines[0], 'j'), column_of(lines[2], 'A'));
    }

    #[test]
    fn a_null_is_visibly_not_an_empty_string() {
        let out = render(&sample(), Format::Table);
        assert!(out.contains(NULL_TABLE), "{out}");
    }

    #[test]
    fn the_table_reports_its_row_count_and_says_when_it_was_cut() {
        let out = render(&sample(), Format::Table);
        assert!(out.contains("(2 řádků)"), "{out}");
        assert!(!out.contains("oříznutý"));
        let mut t = sample();
        t.truncated = true;
        assert!(render(&t, Format::Table).contains("--limit"));
    }

    #[test]
    fn json_keeps_null_as_null_and_reports_truncation() {
        let mut t = sample();
        t.truncated = true;
        let v: serde_json::Value = serde_json::from_str(&render(&t, Format::Json)).unwrap();
        assert_eq!(v["rows"][1][1], serde_json::Value::Null);
        assert_eq!(v["rows"][0][1], serde_json::json!("Alice"));
        assert_eq!(v["row_count"], 2);
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn csv_quotes_only_what_has_to_be_quoted() {
        assert_eq!(csv_field(Some("plain")), "plain");
        assert_eq!(csv_field(Some("a,b")), "\"a,b\"");
        assert_eq!(csv_field(Some("say \"hi\"")), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field(Some("line\nbreak")), "\"line\nbreak\"");
        assert_eq!(csv_field(None), "");
    }

    /// The header row goes through the same escaping as the data. A column
    /// literally named `a,b` would otherwise silently become two columns.
    #[test]
    fn a_comma_in_a_column_name_is_quoted_too() {
        let t = Table::new(vec!["a,b".into()]);
        assert!(render(&t, Format::Csv).starts_with("\"a,b\""));
    }

    #[test]
    fn csv_round_trips_through_a_parser() {
        let mut t = Table::new(vec!["x".into()]);
        t.push_str_row(vec!["has,comma".into()]);
        t.push_str_row(vec!["has\"quote".into()]);
        let out = render(&t, Format::Csv);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "x");
        assert_eq!(lines[1], "\"has,comma\"");
        assert_eq!(lines[2], "\"has\"\"quote\"");
    }

    #[test]
    fn an_empty_result_still_prints_its_header() {
        let t = Table::new(vec!["a".into(), "b".into()]);
        let out = render(&t, Format::Table);
        assert!(out.contains("(0 řádků)"), "{out}");
        assert!(out.starts_with('a'), "{out}");
        assert_eq!(render(&t, Format::Csv), "a,b\n");
    }
}
