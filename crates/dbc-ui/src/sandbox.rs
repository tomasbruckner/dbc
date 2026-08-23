//! G5 Task 2: sandbox edit model + SQL generation for the Apply dialog.
//! Pure model — no GPUI, no I/O — so the exact strings the dialog shows can
//! be unit-tested directly (quoting is CRITICAL: this is the app's only
//! write path).
//!
//! Not yet wired to the UI — the Apply dialog and grid edit affordances are
//! a later G5 task — so the public surface is unused outside `#[cfg(test)]`
//! for now.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use dbc_core::{quote_ident, quote_qualified};

/// Staged, not-yet-applied edits for one editable preview tab.
#[derive(Default)]
pub struct EditState {
    /// (source_row, source_col) -> staged value (None = SQL NULL).
    pub cells: HashMap<(usize, usize), Option<String>>,
    pub deleted_rows: HashSet<usize>,
    /// Each entry: per visible source column an optional staged value;
    /// column set fixed at insert time (headers.len()). Outer `None` means
    /// "left untouched" (table default applies); `Some(None)` is a staged
    /// SQL NULL, `Some(Some(s))` is a staged value.
    pub inserted_rows: Vec<Vec<Option<Option<String>>>>,
}

impl EditState {
    pub fn is_dirty(&self) -> bool {
        !self.cells.is_empty() || !self.deleted_rows.is_empty() || !self.inserted_rows.is_empty()
    }

    /// Row-granular change count: edited (non-deleted) rows + deleted rows +
    /// inserted rows.
    pub fn change_count(&self) -> usize {
        let edited_rows: HashSet<usize> = self
            .cells
            .keys()
            .map(|(r, _)| *r)
            .filter(|r| !self.deleted_rows.contains(r))
            .collect();
        edited_rows.len() + self.deleted_rows.len() + self.inserted_rows.len()
    }

    pub fn stage_cell(&mut self, row: usize, col: usize, v: Option<String>) {
        self.cells.insert((row, col), v);
    }

    pub fn toggle_delete(&mut self, row: usize) {
        if !self.deleted_rows.remove(&row) {
            self.deleted_rows.insert(row);
        }
    }

    /// Appends a new blank insert row with `cols` untouched columns; returns
    /// its index in `inserted_rows` (insertion order).
    pub fn add_insert_row(&mut self, cols: usize) -> usize {
        self.inserted_rows.push(vec![None; cols]);
        self.inserted_rows.len() - 1
    }

    pub fn stage_insert_cell(&mut self, ins_ix: usize, col: usize, v: Option<String>) {
        self.inserted_rows[ins_ix][col] = Some(v);
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.deleted_rows.clear();
        self.inserted_rows.clear();
    }
}

pub struct TableMeta<'a> {
    pub schema: Option<&'a str>,
    pub table: &'a str,
    pub headers: &'a [String],
    pub pk_cols: &'a [usize],
    pub numeric_cols: &'a [bool],
}

/// Value emitter: staged None -> "NULL"; Some(s) with numeric col AND s
/// parses (after trimming) strictly as f64/i128 -> bare trimmed s;
/// otherwise a single-quoted string with `'` doubled.
pub fn sql_value(v: Option<&str>, numeric: bool) -> String {
    match v {
        None => "NULL".to_string(),
        Some(s) => {
            if numeric {
                let trimmed = s.trim();
                if !trimmed.is_empty()
                    && (trimmed.parse::<i128>().is_ok() || trimmed.parse::<f64>().is_ok())
                {
                    return trimmed.to_string();
                }
            }
            format!("'{}'", s.replace('\'', "''"))
        }
    }
}

/// Builds a `pk = original` (or `pk IS NULL`) fragment for one pk column.
fn pk_where_fragment(meta: &TableMeta, row: usize, pk_col: usize, original: &mut dyn FnMut(usize, usize) -> Option<String>) -> String {
    let ident = quote_ident(&meta.headers[pk_col]);
    match original(row, pk_col) {
        None => format!("{ident} IS NULL"),
        Some(v) => format!("{ident} = {}", sql_value(Some(&v), meta.numeric_cols[pk_col])),
    }
}

fn where_clause(meta: &TableMeta, row: usize, original: &mut dyn FnMut(usize, usize) -> Option<String>) -> String {
    meta.pk_cols
        .iter()
        .map(|&pc| pk_where_fragment(meta, row, pc, original))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Generates the exact statements the Apply dialog shows, in order: UPDATEs
/// (ascending source row), DELETEs (ascending source row), INSERTs
/// (insertion order). Deleted rows' staged cell edits are ignored (delete
/// wins). Every statement pairs with its expected affected-row count (1 for
/// UPDATE/DELETE; None for INSERT — the driver reports 1 but server
/// triggers may differ).
pub fn generate_statements(
    meta: &TableMeta,
    edits: &EditState,
    original: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> Vec<(String, Option<u64>)> {
    let mut out = Vec::new();
    let table = quote_qualified(meta.schema, meta.table);

    // UPDATEs: rows with staged cells, excluding deleted rows, ascending.
    let mut rows: Vec<usize> = edits
        .cells
        .keys()
        .map(|(r, _)| *r)
        .filter(|r| !edits.deleted_rows.contains(r))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    rows.sort_unstable();

    for row in rows {
        let mut cols: Vec<usize> =
            edits.cells.keys().filter(|(r, _)| *r == row).map(|(_, c)| *c).collect();
        cols.sort_unstable();

        let set_clause = cols
            .iter()
            .map(|&c| {
                let v = edits.cells.get(&(row, c)).unwrap();
                format!(
                    "{} = {}",
                    quote_ident(&meta.headers[c]),
                    sql_value(v.as_deref(), meta.numeric_cols[c])
                )
            })
            .collect::<Vec<_>>()
            .join(", ");

        let where_sql = where_clause(meta, row, original);
        out.push((format!("UPDATE {table} SET {set_clause} WHERE {where_sql}"), Some(1)));
    }

    // DELETEs: ascending source row.
    let mut del_rows: Vec<usize> = edits.deleted_rows.iter().copied().collect();
    del_rows.sort_unstable();
    for row in del_rows {
        let where_sql = where_clause(meta, row, original);
        out.push((format!("DELETE FROM {table} WHERE {where_sql}"), Some(1)));
    }

    // INSERTs: insertion order.
    for ins_row in &edits.inserted_rows {
        let touched: Vec<usize> =
            ins_row.iter().enumerate().filter(|(_, v)| v.is_some()).map(|(c, _)| c).collect();

        if touched.is_empty() {
            out.push((format!("INSERT INTO {table} DEFAULT VALUES"), None));
        } else {
            let cols_sql =
                touched.iter().map(|&c| quote_ident(&meta.headers[c])).collect::<Vec<_>>().join(", ");
            let vals_sql = touched
                .iter()
                .map(|&c| {
                    let v = ins_row[c].as_ref().unwrap();
                    sql_value(v.as_deref(), meta.numeric_cols[c])
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push((format!("INSERT INTO {table} ({cols_sql}) VALUES ({vals_sql})"), None));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta<'a>(
        schema: Option<&'a str>,
        table: &'a str,
        headers: &'a [String],
        pk_cols: &'a [usize],
        numeric_cols: &'a [bool],
    ) -> TableMeta<'a> {
        TableMeta { schema, table, headers, pk_cols, numeric_cols }
    }

    fn headers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // No original values by default; tests override per-cell as needed.
    fn no_originals(_row: usize, _col: usize) -> Option<String> {
        None
    }

    #[test]
    fn update_single_cell_quoted_string_pk_where() {
        let h = headers(&["id", "name"]);
        let m = meta(Some("public"), "users", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 1, Some("Alice".into()));

        let mut original = |row: usize, col: usize| -> Option<String> {
            if row == 0 && col == 0 {
                Some("1".into())
            } else {
                no_originals(row, col)
            }
        };

        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0].0,
            "UPDATE \"public\".\"users\" SET \"name\" = 'Alice' WHERE \"id\" = 1"
        );
        assert_eq!(stmts[0].1, Some(1));
    }

    #[test]
    fn null_staging_vs_empty_string() {
        let h = headers(&["id", "note"]);
        let m = meta(None, "t", &h, &[0], &[true, false]);

        // NULL staging.
        let mut edits_null = EditState::default();
        edits_null.stage_cell(0, 1, None);
        let mut original = |row: usize, col: usize| if row == 0 && col == 0 { Some("1".into()) } else { None };
        let stmts = generate_statements(&m, &edits_null, &mut original);
        assert_eq!(stmts[0].0, "UPDATE \"t\" SET \"note\" = NULL WHERE \"id\" = 1");

        // Empty string staging (distinct from NULL).
        let mut edits_empty = EditState::default();
        edits_empty.stage_cell(0, 1, Some(String::new()));
        let stmts = generate_statements(&m, &edits_empty, &mut original);
        assert_eq!(stmts[0].0, "UPDATE \"t\" SET \"note\" = '' WHERE \"id\" = 1");
    }

    #[test]
    fn numeric_unquoted_and_numeric_parse_failure_quoted() {
        // Numeric column, valid numeric value -> unquoted, trimmed.
        assert_eq!(sql_value(Some(" 42 "), true), "42");
        assert_eq!(sql_value(Some("-3.5"), true), "-3.5");
        // Numeric column, parse failure -> quoted (server decides).
        assert_eq!(sql_value(Some("abc"), true), "'abc'");
        // Numeric column, hex-looking string is rejected (not bare).
        assert_eq!(sql_value(Some("0x1A"), true), "'0x1A'");
        // Numeric column, empty/whitespace-only -> quoted, not bare.
        assert_eq!(sql_value(Some(""), true), "''");
        assert_eq!(sql_value(Some("   "), true), "'   '");
        // Non-numeric column, numeric-looking value -> still quoted.
        assert_eq!(sql_value(Some("42"), false), "'42'");
    }

    #[test]
    fn multi_cell_one_row_is_one_update() {
        let h = headers(&["id", "name", "age"]);
        let m = meta(None, "t", &h, &[0], &[true, false, true]);
        let mut edits = EditState::default();
        edits.stage_cell(2, 2, Some("30".into()));
        edits.stage_cell(2, 1, Some("Bob".into()));

        let mut original = |row: usize, col: usize| if row == 2 && col == 0 { Some("9".into()) } else { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0].0,
            "UPDATE \"t\" SET \"name\" = 'Bob', \"age\" = 30 WHERE \"id\" = 9"
        );
    }

    #[test]
    fn delete_row_ignores_its_staged_edits() {
        let h = headers(&["id", "name"]);
        let m = meta(None, "t", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        edits.stage_cell(1, 1, Some("ignored".into()));
        edits.toggle_delete(1);

        let mut original = |row: usize, col: usize| if row == 1 && col == 0 { Some("5".into()) } else { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].0, "DELETE FROM \"t\" WHERE \"id\" = 5");
        assert_eq!(stmts[0].1, Some(1));
    }

    #[test]
    fn insert_partial_columns() {
        let h = headers(&["id", "name", "age"]);
        let m = meta(None, "t", &h, &[0], &[true, false, true]);
        let mut edits = EditState::default();
        let ix = edits.add_insert_row(3);
        edits.stage_insert_cell(ix, 1, Some("Carl".into()));

        let mut original = no_originals;
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].0, "INSERT INTO \"t\" (\"name\") VALUES ('Carl')");
        assert_eq!(stmts[0].1, None);
    }

    #[test]
    fn insert_untouched_is_default_values() {
        let h = headers(&["id", "name"]);
        let m = meta(None, "t", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        edits.add_insert_row(2);

        let mut original = no_originals;
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].0, "INSERT INTO \"t\" DEFAULT VALUES");
        assert_eq!(stmts[0].1, None);
    }

    #[test]
    fn pk_null_uses_is_null() {
        let h = headers(&["id", "name"]);
        let m = meta(None, "t", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 1, Some("x".into()));

        let mut original = |_row: usize, _col: usize| -> Option<String> { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts[0].0, "UPDATE \"t\" SET \"name\" = 'x' WHERE \"id\" IS NULL");
    }

    #[test]
    fn weird_idents_quoted() {
        let h = headers(&["we\"ird", "na\"me"]);
        let m = meta(Some("pu\"blic"), "ta\"ble", &h, &[0], &[false, false]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 1, Some("v".into()));

        let mut original = |row: usize, col: usize| if row == 0 && col == 0 { Some("k".into()) } else { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(
            stmts[0].0,
            "UPDATE \"pu\"\"blic\".\"ta\"\"ble\" SET \"na\"\"me\" = 'v' WHERE \"we\"\"ird\" = 'k'"
        );
    }

    #[test]
    fn oreilly_values_escaped() {
        assert_eq!(sql_value(Some("O'Reilly"), false), "'O''Reilly'");

        let h = headers(&["id", "name"]);
        let m = meta(None, "t", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 1, Some("O'Reilly".into()));
        let mut original = |row: usize, col: usize| if row == 0 && col == 0 { Some("1".into()) } else { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts[0].0, "UPDATE \"t\" SET \"name\" = 'O''Reilly' WHERE \"id\" = 1");
    }

    #[test]
    fn statement_ordering_and_expectations() {
        let h = headers(&["id", "name"]);
        let m = meta(None, "t", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        // UPDATE rows 3 then 1 (staged out of order; must emit ascending).
        edits.stage_cell(3, 1, Some("three".into()));
        edits.stage_cell(1, 1, Some("one".into()));
        // DELETE rows 5 then 2 (staged out of order; must emit ascending).
        edits.toggle_delete(5);
        edits.toggle_delete(2);
        // INSERTs in call order.
        let ix0 = edits.add_insert_row(2);
        edits.stage_insert_cell(ix0, 1, Some("first".into()));
        let ix1 = edits.add_insert_row(2);
        edits.stage_insert_cell(ix1, 1, Some("second".into()));

        let mut original = |row: usize, col: usize| {
            if col == 0 {
                Some(row.to_string())
            } else {
                None
            }
        };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts.len(), 6);
        assert_eq!(stmts[0].0, "UPDATE \"t\" SET \"name\" = 'one' WHERE \"id\" = 1");
        assert_eq!(stmts[0].1, Some(1));
        assert_eq!(stmts[1].0, "UPDATE \"t\" SET \"name\" = 'three' WHERE \"id\" = 3");
        assert_eq!(stmts[1].1, Some(1));
        assert_eq!(stmts[2].0, "DELETE FROM \"t\" WHERE \"id\" = 2");
        assert_eq!(stmts[2].1, Some(1));
        assert_eq!(stmts[3].0, "DELETE FROM \"t\" WHERE \"id\" = 5");
        assert_eq!(stmts[3].1, Some(1));
        assert_eq!(stmts[4].0, "INSERT INTO \"t\" (\"name\") VALUES ('first')");
        assert_eq!(stmts[4].1, None);
        assert_eq!(stmts[5].0, "INSERT INTO \"t\" (\"name\") VALUES ('second')");
        assert_eq!(stmts[5].1, None);
    }

    #[test]
    fn change_count_counts_edits_deletes_inserts_row_granular() {
        let mut edits = EditState::default();
        assert!(!edits.is_dirty());
        assert_eq!(edits.change_count(), 0);

        edits.stage_cell(0, 0, Some("a".into()));
        edits.stage_cell(0, 1, Some("b".into())); // same row, 2nd cell.
        edits.stage_cell(1, 0, Some("c".into()));
        assert_eq!(edits.change_count(), 2); // 2 distinct edited rows.

        edits.toggle_delete(5);
        assert_eq!(edits.change_count(), 3);

        edits.add_insert_row(2);
        assert_eq!(edits.change_count(), 4);
        assert!(edits.is_dirty());

        // A row that's both edited and deleted counts once (as a delete),
        // not twice — delete wins in generate_statements too.
        edits.stage_cell(5, 0, Some("edited-then-deleted".into()));
        assert_eq!(edits.change_count(), 4);

        edits.clear();
        assert_eq!(edits.change_count(), 0);
        assert!(!edits.is_dirty());
    }

    #[test]
    fn staging_same_cell_twice_keeps_last() {
        let mut edits = EditState::default();
        edits.stage_cell(0, 0, Some("first".into()));
        edits.stage_cell(0, 0, Some("second".into()));
        assert_eq!(edits.cells.get(&(0, 0)), Some(&Some("second".to_string())));
        assert_eq!(edits.change_count(), 1);
    }

    #[test]
    fn toggle_delete_twice_undeletes() {
        let mut edits = EditState::default();
        edits.toggle_delete(3);
        assert!(edits.deleted_rows.contains(&3));
        edits.toggle_delete(3);
        assert!(!edits.deleted_rows.contains(&3));
        assert!(!edits.is_dirty());
    }
}
