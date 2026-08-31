//! „Rozbal hvězdičku" — turn `SELECT *` into the actual column list.
//!
//! What DataGrip calls *Expand column list*, and the reason people want it
//! is not typing: `SELECT *` is a query whose result shape changes when
//! somebody adds a column, so the first thing you do with an exploratory
//! `SELECT *` is nail it down. Doing that by hand means reading the sidebar
//! and typing forty names.
//!
//! Pure and GPUI-free, like `fk_join`/`autocomplete` next door: it takes
//! SQL text, a cursor and a schema snapshot, and returns a byte range to
//! replace and the text to put there. `main.rs` owns the action, the
//! buffer edit and the status line.
//!
//! # What counts as a star
//!
//! Only a `*` in select-list position: the previous significant character
//! is `SELECT`, `DISTINCT`, a comma, or the `.` of `alias.*`. That single
//! rule is what keeps `COUNT(*)` (preceded by `(`) and multiplication
//! (`price * qty`, preceded by an identifier) out — both of which would be
//! actively destructive to rewrite.
//!
//! # What it refuses
//!
//! Everything it is not sure about, loudly, via [`Refusal`]. A star
//! expansion that guesses the wrong table does not produce a wrong answer
//! later, it produces a broken query now — but it also silently rewrites
//! the user's text, which is the one thing an editor must not do casually.

use std::ops::Range;

use dbc_core::{quote_ident_d, Dialect, SchemaSnapshot};

use crate::autocomplete::{self, Source, TableRef, KEYWORDS};

/// The edit to apply: replace `range` with `text`.
#[derive(Debug, Clone, PartialEq)]
pub struct Expansion {
    pub range: Range<usize>,
    pub text: String,
}

/// Why nothing was expanded. Each variant is a different sentence to the
/// user — „nic se nestalo" with no reason is the complaint this whole
/// codebase's status line exists to avoid.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// No `*` in select-list position anywhere in the statement.
    NoStar,
    /// The schema for this connection has not been loaded yet.
    NoSchema,
    /// A `FROM`/`JOIN` subquery, or an alias bound to two different
    /// tables — the source scan will not guess.
    Ambiguous,
    /// `x.*` where `x` is not a table or alias in this statement.
    UnknownQualifier(String),
    /// The table is named but the snapshot has no columns for it.
    UnknownTable(String),
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::NoStar => "Kurzor není u hvězdičky v seznamu sloupců.".into(),
            Refusal::NoSchema => "Schéma není načtené — rozbalit hvězdičku zatím nejde.".into(),
            Refusal::Ambiguous => {
                "Dotaz obsahuje poddotaz nebo nejednoznačný alias — nebudu hádat.".into()
            }
            Refusal::UnknownQualifier(q) => format!("`{q}` není tabulka ani alias v tomto dotazu."),
            Refusal::UnknownTable(t) => format!("Pro tabulku `{t}` nemám ve schématu sloupce."),
        }
    }
}

/// Byte range of the statement containing `cursor`, split on top-level `;`.
///
/// Masked first, so a semicolon inside a string literal or a comment is not
/// a statement boundary.
pub fn statement_span(sql: &str, cursor: usize) -> Range<usize> {
    let masked = autocomplete::mask_strings_and_comments(sql);
    // The mask is char-for-char, but `;` is ASCII either way; walk bytes so
    // the offsets are the ones the caller uses.
    let bytes = masked.as_bytes();
    let cursor = cursor.min(sql.len());
    let mut start = 0usize;
    let mut end = sql.len();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b';' {
            continue;
        }
        if i < cursor {
            start = i + 1;
        } else {
            end = i;
            break;
        }
    }
    start.min(sql.len())..end.min(sql.len())
}

/// A `*` in select-list position, plus the range that must be replaced.
#[derive(Debug, Clone, PartialEq)]
struct Star {
    /// The `*` itself.
    at: usize,
    /// `Some("a")` for `a.*` — the qualifier written before the dot.
    qualifier: Option<String>,
    /// From the start of the qualifier (or the `*`) through the `*`.
    range: Range<usize>,
}

/// Every expandable `*` inside `span`, in source order.
fn stars_in(sql: &str, span: &Range<usize>) -> Vec<Star> {
    let masked = autocomplete::mask_strings_and_comments(sql);
    let bytes = masked.as_bytes();
    let mut out = Vec::new();
    for at in span.start..span.end.min(bytes.len()) {
        if bytes[at] != b'*' {
            continue;
        }
        // The previous significant byte decides whether this is a select
        // list star or arithmetic.
        let mut p = at;
        while p > span.start && bytes[p - 1].is_ascii_whitespace() {
            p -= 1;
        }
        if p == span.start {
            continue; // nothing before it in this statement
        }
        let prev = bytes[p - 1];
        if prev == b'.' {
            // `qualifier.*` — read the identifier before the dot.
            let mut q = p - 1;
            while q > span.start && is_ident_byte(bytes[q - 1]) {
                q -= 1;
            }
            if q == p - 1 {
                continue; // a lone `.` is not a qualifier
            }
            out.push(Star {
                at,
                qualifier: Some(sql[q..p - 1].to_string()),
                range: q..at + 1,
            });
            continue;
        }
        if prev == b',' {
            out.push(Star { at, qualifier: None, range: at..at + 1 });
            continue;
        }
        // A bare `*` is only a select star right after SELECT or DISTINCT.
        let mut w = p;
        while w > span.start && is_ident_byte(bytes[w - 1]) {
            w -= 1;
        }
        let word = sql.get(w..p).unwrap_or("").to_ascii_uppercase();
        if word == "SELECT" || word == "DISTINCT" {
            out.push(Star { at, qualifier: None, range: at..at + 1 });
        }
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Identifiers that can be written bare. Anything else is quoted, because
/// Postgres folds an unquoted `MyCol` to `mycol` and would not find it.
fn needs_quoting(name: &str) -> bool {
    let mut chars = name.chars();
    let ok_start = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    !(ok_start && ok_rest) || KEYWORDS.contains(&name.to_ascii_uppercase().as_str())
}

fn ident(dialect: Dialect, name: &str) -> String {
    if needs_quoting(name) {
        quote_ident_d(dialect, name)
    } else {
        name.to_string()
    }
}

fn find_table<'a>(snapshot: &'a SchemaSnapshot, want: &TableRef) -> Option<&'a dbc_core::TableInfo> {
    snapshot.tables.iter().find(|t| {
        t.name.eq_ignore_ascii_case(&want.name)
            && match (&want.schema, &t.schema) {
                (Some(w), Some(have)) => w.eq_ignore_ascii_case(have),
                // The user did not qualify: any schema's table of that name
                // will do, which is the same latitude `resolve_aliases`
                // already gives bare names.
                (None, _) => true,
                (Some(_), None) => false,
            }
    })
}

/// The replacement column list for one source.
fn columns_of(
    dialect: Dialect,
    snapshot: &SchemaSnapshot,
    src: &Source,
    qualify: bool,
) -> Result<Vec<String>, Refusal> {
    let table =
        find_table(snapshot, &src.table).ok_or_else(|| Refusal::UnknownTable(src.table.name.clone()))?;
    if table.columns.is_empty() {
        return Err(Refusal::UnknownTable(src.table.name.clone()));
    }
    let prefix = if qualify { format!("{}.", ident(dialect, src.qualifier())) } else { String::new() };
    Ok(table.columns.iter().map(|c| format!("{prefix}{}", ident(dialect, &c.name))).collect())
}

/// Expand the `*` nearest the cursor.
///
/// „Nearest", not „the first": a two-statement buffer or a `SELECT a.*, b.*`
/// both have several, and the one the user means is the one they put the
/// caret next to. Ties go to the earlier star, so the rule is total.
pub fn expand_at(
    sql: &str,
    cursor: usize,
    snapshot: Option<&SchemaSnapshot>,
    dialect: Dialect,
) -> Result<Expansion, Refusal> {
    let span = statement_span(sql, cursor);
    let stars = stars_in(sql, &span);
    let star = stars
        .iter()
        .min_by_key(|s| s.at.abs_diff(cursor))
        .ok_or(Refusal::NoStar)?;

    let snapshot = snapshot.ok_or(Refusal::NoSchema)?;
    let sources = autocomplete::sources_in_order(&sql[span.clone()]).ok_or(Refusal::Ambiguous)?;
    if sources.is_empty() {
        return Err(Refusal::Ambiguous);
    }

    let columns: Vec<String> = match &star.qualifier {
        Some(q) => {
            let src = sources
                .iter()
                .find(|s| s.qualifier().eq_ignore_ascii_case(q))
                .ok_or_else(|| Refusal::UnknownQualifier(q.clone()))?;
            // `a.*` was already qualified by the user, so keep qualifying —
            // dropping it would change what the columns resolve to in a
            // multi-table query.
            columns_of(dialect, snapshot, src, true)?
        }
        None => {
            // One table: bare names read better and cannot be ambiguous.
            // Several: every column must say which table it came from.
            let qualify = sources.len() > 1;
            let mut all = Vec::new();
            for src in &sources {
                all.extend(columns_of(dialect, snapshot, src, qualify)?);
            }
            all
        }
    };

    Ok(Expansion { range: star.range.clone(), text: columns.join(", ") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{ColumnInfo, TableInfo};

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), ..Default::default() }
    }

    fn table(schema: Option<&str>, name: &str, cols: &[&str]) -> TableInfo {
        TableInfo {
            schema: schema.map(str::to_string),
            name: name.into(),
            columns: cols.iter().map(|c| col(c)).collect(),
            ..Default::default()
        }
    }

    fn snap() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![
                table(Some("dbo"), "C_Data_View", &["Id", "Name", "created_at"]),
                table(Some("public"), "orders", &["id", "customer_id", "total"]),
                table(Some("public"), "customers", &["id", "name"]),
            ],
            ..Default::default()
        }
    }

    fn expand(sql: &str, cursor: usize) -> Result<String, Refusal> {
        expand_at(sql, cursor, Some(&snap()), Dialect::Postgres).map(|e| {
            let mut out = sql.to_string();
            out.replace_range(e.range, &e.text);
            out
        })
    }

    #[test]
    fn a_single_table_gets_bare_column_names() {
        let sql = "SELECT * FROM orders";
        assert_eq!(expand(sql, 7).unwrap(), "SELECT id, customer_id, total FROM orders");
    }

    /// The user's own case: MSSQL-style PascalCase must come back QUOTED,
    /// because Postgres folds an unquoted `Id` to `id` and then cannot
    /// find it — an expansion that produces a broken query is worse than
    /// no expansion.
    #[test]
    fn mixed_case_columns_are_quoted() {
        let sql = "SELECT *\nFROM C_Data_View";
        assert_eq!(
            expand(sql, 7).unwrap(),
            "SELECT \"Id\", \"Name\", created_at\nFROM C_Data_View"
        );
    }

    #[test]
    fn several_tables_qualify_every_column() {
        let sql = "SELECT * FROM orders o JOIN customers c ON o.customer_id = c.id";
        assert_eq!(
            expand(sql, 7).unwrap(),
            "SELECT o.id, o.customer_id, o.total, c.id, c.name \
             FROM orders o JOIN customers c ON o.customer_id = c.id"
        );
    }

    #[test]
    fn a_qualified_star_expands_only_that_table() {
        let sql = "SELECT o.* FROM orders o JOIN customers c ON o.customer_id = c.id";
        assert_eq!(
            expand(sql, 9).unwrap(),
            "SELECT o.id, o.customer_id, o.total \
             FROM orders o JOIN customers c ON o.customer_id = c.id"
        );
    }

    #[test]
    fn an_unaliased_table_qualifies_by_its_own_name() {
        let sql = "SELECT * FROM orders JOIN customers ON orders.customer_id = customers.id";
        let out = expand(sql, 7).unwrap();
        assert!(out.starts_with("SELECT orders.id, orders.customer_id, orders.total, customers.id"));
    }

    /// The rule that keeps this from being destructive.
    #[test]
    fn count_star_and_multiplication_are_never_touched() {
        for sql in [
            "SELECT COUNT(*) FROM orders",
            "SELECT total * 2 FROM orders",
            "SELECT count( * ) FROM orders",
        ] {
            assert_eq!(expand(sql, 10), Err(Refusal::NoStar), "rewrote {sql:?}");
        }
    }

    #[test]
    fn a_star_after_a_comma_is_still_a_select_star() {
        let sql = "SELECT id, * FROM orders";
        assert_eq!(expand(sql, 11).unwrap(), "SELECT id, id, customer_id, total FROM orders");
    }

    #[test]
    fn distinct_star_expands() {
        let sql = "SELECT DISTINCT * FROM customers";
        assert_eq!(expand(sql, 16).unwrap(), "SELECT DISTINCT id, name FROM customers");
    }

    #[test]
    fn the_nearest_star_wins() {
        let sql = "SELECT o.*, c.* FROM orders o JOIN customers c ON o.customer_id = c.id";
        let near_c = sql.find("c.*").unwrap() + 2;
        assert!(expand(sql, near_c).unwrap().contains("SELECT o.*, c.id, c.name FROM"));
    }

    /// A star in one statement must not be expanded from another
    /// statement's tables.
    #[test]
    fn statements_are_separated_by_semicolons() {
        let sql = "SELECT * FROM orders;\nSELECT * FROM customers";
        let second = sql.rfind('*').unwrap();
        assert_eq!(
            expand(sql, second).unwrap(),
            "SELECT * FROM orders;\nSELECT id, name FROM customers"
        );
    }

    #[test]
    fn a_semicolon_inside_a_string_is_not_a_boundary() {
        let sql = "SELECT * FROM orders WHERE note = 'a;b'";
        assert!(expand(sql, 7).unwrap().starts_with("SELECT id, customer_id, total FROM"));
    }

    #[test]
    fn refusals_say_which_one_and_have_a_message() {
        assert_eq!(expand_at("SELECT 1", 0, Some(&snap()), Dialect::Postgres), Err(Refusal::NoStar));
        assert_eq!(
            expand_at("SELECT * FROM orders", 7, None, Dialect::Postgres),
            Err(Refusal::NoSchema)
        );
        assert_eq!(
            expand("SELECT * FROM (SELECT 1) x", 7),
            Err(Refusal::Ambiguous),
            "a subquery source is not guessed at"
        );
        assert_eq!(
            expand("SELECT zz.* FROM orders o", 10),
            Err(Refusal::UnknownQualifier("zz".into()))
        );
        assert_eq!(
            expand("SELECT * FROM nope", 7),
            Err(Refusal::UnknownTable("nope".into()))
        );
        for r in [
            Refusal::NoStar,
            Refusal::NoSchema,
            Refusal::Ambiguous,
            Refusal::UnknownQualifier("q".into()),
            Refusal::UnknownTable("t".into()),
        ] {
            assert!(!r.message().trim().is_empty(), "{r:?} has no message");
        }
    }

    #[test]
    fn mssql_quotes_with_brackets() {
        let sql = "SELECT * FROM C_Data_View";
        let e = expand_at(sql, 7, Some(&snap()), Dialect::Mssql).unwrap();
        assert_eq!(e.text, "[Id], [Name], created_at");
    }

    #[test]
    fn a_column_named_like_a_keyword_is_quoted() {
        let s = SchemaSnapshot {
            tables: vec![table(None, "t", &["order", "default", "total"])],
            ..Default::default()
        };
        let e = expand_at("SELECT * FROM t", 7, Some(&s), Dialect::Postgres).unwrap();
        assert_eq!(e.text, "\"order\", \"default\", total", "only the reserved ones get quotes");
    }
}
