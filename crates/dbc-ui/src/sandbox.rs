//! G5 Task 2: sandbox edit model + SQL generation for the Apply dialog.
//! Pure model — no GPUI, no I/O — so the exact strings the dialog shows can
//! be unit-tested directly (quoting is CRITICAL: this is the app's only
//! write path).
//!
//! G5 Task 3 wires `EditState`/`Editable` into `grid.rs` (staging + diff
//! rendering) — every EditState method that drives staging itself
//! (`stage_cell`, `toggle_delete`, `add_insert_row`, `stage_insert_cell`,
//! `remove_insert_row`, `is_dirty`) is exercised by the UI now, so the
//! module-level `#![allow(dead_code)]` this file used to carry is gone.
//!
//! G5 Task 4 wires the rest: `main.rs::on_open_apply_dialog` builds a
//! `TableMeta` from the active tab's `ResultGrid::editable`/`column_names`/
//! `table_name`/`preview_identity` and calls `generate_statements` (reading
//! `original` straight off the grid's `ResultBuffer`) to populate the Apply
//! dialog; `EditState::clear` runs on a successful apply
//! (`ResultGrid::clear_edits`); `change_count` drives both the apply bar's
//! "{n} změn" label and the dirty-guard confirm prompt's count. Every item
//! here now has a real UI caller, in addition to its existing full test
//! coverage.

use std::collections::{HashMap, HashSet};

use dbc_core::{quote_ident_d, quote_qualified_d, Dialect};

/// G5 Task 3: editability facts for one PREVIEW tab's grid — computed once
/// per `Started` event by `main.rs`'s `detect_editable_pk` (mapping the
/// previewed table's catalog PK onto this result's actual columns) and
/// handed to `ResultGrid::set_editable`. `None` on a `ResultGrid` means "not
/// editable" (ad-hoc tab, read-only connection, MSSQL, PK-less table, or no
/// connection at all) — see `detect_editable_pk`'s doc comment for the full
/// decision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Editable {
    /// RESULT-column indices (same indexing as `ResultBuffer`/`ResultGrid`'s
    /// own columns, i.e. `EditState::cells`' `col`) that are part of the
    /// previewed table's primary key. Never empty when `Editable` exists —
    /// a table with no PK column mapped onto the result is NOT editable.
    pub pk_cols: Vec<usize>,
    /// By RESULT-column index — `true` when that column's Arrow type is
    /// numeric (`DataType::is_numeric`). Feeds `sql_value`'s bare-vs-quoted
    /// decision once the Apply dialog (a later task) calls
    /// `generate_statements`.
    pub numeric_cols: Vec<bool>,
}

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

    /// Stages a value into an inserted row's cell. Bounds-checked and a no-op
    /// when `ins_ix`/`col` are out of range: like `remove_insert_row`, the
    /// grid's cell-editor captures `ins_ix` at render time, and a concurrent
    /// `remove_insert_row` (its `Vec::remove` shifts later rows down) can land
    /// before a repaint — a stale target must never panic (crashing the app
    /// loses every staged edit in the tab) nor silently write into the wrong
    /// row. Callers should also re-validate `ins_ix` at click time; this is the
    /// belt-and-braces backstop. (T3 review issue 1.)
    pub fn stage_insert_cell(&mut self, ins_ix: usize, col: usize, v: Option<String>) {
        if let Some(row) = self.inserted_rows.get_mut(ins_ix) {
            if let Some(cell) = row.get_mut(col) {
                *cell = Some(v);
            }
        }
    }

    /// G5 Task 3: removes insert row `ins_ix` entirely (the grid's "␡" per
    /// inserted-row gutter affordance — brief contract #4). Unlike
    /// `toggle_delete` (a real row's delete is a flag that can be
    /// un-toggled, since the row still exists in the underlying table until
    /// Apply runs), an insert row has no underlying identity to preserve —
    /// removing it here is the only way to un-stage it, so this just drops
    /// it from `inserted_rows`. A no-op (rather than a panic) when `ins_ix`
    /// is out of range, since the grid's click handler captures `ins_ix` at
    /// render time and a second concurrent removal (unlikely, but not
    /// impossible with a stale render) shouldn't crash the app.
    pub fn remove_insert_row(&mut self, ins_ix: usize) {
        if ins_ix < self.inserted_rows.len() {
            self.inserted_rows.remove(ins_ix);
        }
    }

    /// Called by `ResultGrid::clear_edits` after a successful Apply.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.deleted_rows.clear();
        self.inserted_rows.clear();
    }
}

/// Built by `main.rs::on_open_apply_dialog` from the active tab's
/// `ResultGrid` state (`Editable`/`column_names`/`table_name`/
/// `preview_identity`) and fed to `generate_statements`.
pub struct TableMeta<'a> {
    pub schema: Option<&'a str>,
    pub table: &'a str,
    pub headers: &'a [String],
    pub pk_cols: &'a [usize],
    pub numeric_cols: &'a [bool],
    /// G15 §2b: threads through to every `quote_ident_d`/`quote_qualified_d`/
    /// `sql_value_d` call this module makes — `main.rs::on_open_apply_dialog`
    /// (the one production constructor) supplies `sql_dialect(engine)`.
    /// `Dialect::Mssql` is live and reachable since G15 T8's
    /// `detect_editable_pk` ON-flip — see that fn's doc comment for the
    /// live evidence (`mssql_sandbox_apply_bracket_quoted_weird_column_and_czech_diacritics_live`).
    pub dialect: dbc_core::Dialect,
}

/// G5 Task 3: display text for a staged CELL edit (`EditState::cells`
/// value), given the cell's staged entry if any. `None` (no entry — cell
/// isn't staged at all) is passed through as `None` so callers know to fall
/// back to the ORIGINAL committed text instead; a staged SQL NULL
/// (`Some(None)`) renders as the literal marker `"(NULL)"` rather than an
/// indistinguishable-from-untouched empty string; a staged value
/// (`Some(Some(s))`) renders as `s` itself.
pub fn staged_cell_display(staged: Option<&Option<String>>) -> Option<String> {
    staged.map(|v| match v {
        None => "(NULL)".to_string(),
        Some(s) => s.clone(),
    })
}

/// G5 Task 3: display text for one column of an INSERT row
/// (`EditState::inserted_rows[i][col]`'s outer-`Option` convention — see
/// `inserted_rows`' doc comment). Untouched (`None`) shows `"(výchozí)"`
/// (table default applies at Apply time); staged NULL (`Some(None)`) shows
/// `"(NULL)"`; a staged value (`Some(Some(s))`) shows `s`.
pub fn insert_cell_display(cell: &Option<Option<String>>) -> String {
    match cell {
        None => "(výchozí)".to_string(),
        Some(None) => "(NULL)".to_string(),
        Some(Some(s)) => s.clone(),
    }
}

/// Value emitter: staged None -> "NULL"; Some(s) with numeric col AND s
/// parses (after trimming) strictly as f64/i128 -> bare trimmed s;
/// otherwise a single-quoted string with `'` doubled. Thin pg-convention
/// wrapper over [`sql_value_d`] — byte-identical pre-G15 behavior.
pub fn sql_value(v: Option<&str>, numeric: bool) -> String {
    sql_value_d(v, numeric, Dialect::Postgres)
}

/// Dialect-aware sibling of [`sql_value`] (G15 §2b). Apply is the app's
/// ONLY user-data write path — quoting here is CRITICAL: a bare `'…'`
/// literal is `varchar` in T-SQL and transcodes through the database
/// collation's code page — Czech diacritics staged in the grid would
/// corrupt exactly the way `wide.rs` exists to prevent on the read side.
/// `N''` is harmless for ASCII and correct for everything else.
pub fn sql_value_d(v: Option<&str>, numeric: bool, dialect: Dialect) -> String {
    match v {
        None => "NULL".to_string(),
        Some(s) => {
            if numeric {
                let trimmed = s.trim();
                // f64 accepts "NaN"/"inf"/"infinity", which are NOT valid
                // SQL numeral tokens — only finite parses emit bare (Task 2
                // review issue 1); everything else falls through to the
                // quoted let-the-server-decide path.
                let finite_float = trimmed.parse::<f64>().map(|f| f.is_finite()).unwrap_or(false);
                if !trimmed.is_empty() && (trimmed.parse::<i128>().is_ok() || finite_float) {
                    return trimmed.to_string();
                }
            }
            let quoted = s.replace('\'', "''");
            match dialect {
                // §2b: non-finite floats keep the existing
                // quote-and-let-the-server-decide posture; MSSQL rejects
                // N'NaN' for a float column server-side, error surfaces
                // verbatim (documented, not special-cased).
                Dialect::Mssql => format!("N'{quoted}'"),
                _ => format!("'{quoted}'"),
            }
        }
    }
}

/// Builds a `pk = original` (or `pk IS NULL`) fragment for one pk column.
fn pk_where_fragment(meta: &TableMeta, row: usize, pk_col: usize, original: &mut dyn FnMut(usize, usize) -> Option<String>) -> String {
    let ident = quote_ident_d(meta.dialect, &meta.headers[pk_col]);
    match original(row, pk_col) {
        None => format!("{ident} IS NULL"),
        Some(v) => format!("{ident} = {}", sql_value_d(Some(&v), meta.numeric_cols[pk_col], meta.dialect)),
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
    // Brief invariant: editable tables always have a detected PK. An empty
    // slice would emit `WHERE ` (fails as a syntax error, not a mass
    // update, but must never be constructed) — Task 2 review issue 3.
    debug_assert!(
        !meta.pk_cols.is_empty(),
        "generate_statements requires a non-empty pk_cols"
    );
    let mut out = Vec::new();
    let table = quote_qualified_d(meta.dialect, meta.schema, meta.table);

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
                    quote_ident_d(meta.dialect, &meta.headers[c]),
                    sql_value_d(v.as_deref(), meta.numeric_cols[c], meta.dialect)
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
            let cols_sql = touched
                .iter()
                .map(|&c| quote_ident_d(meta.dialect, &meta.headers[c]))
                .collect::<Vec<_>>()
                .join(", ");
            let vals_sql = touched
                .iter()
                .map(|&c| {
                    let v = ins_row[c].as_ref().unwrap();
                    sql_value_d(v.as_deref(), meta.numeric_cols[c], meta.dialect)
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
        TableMeta { schema, table, headers, pk_cols, numeric_cols, dialect: Dialect::Postgres }
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

    // Task 2 review issue 1: f64 parses "NaN"/"inf"/"infinity" but they are
    // not SQL numerals — they must fall through to the quoted path.
    #[test]
    fn non_finite_floats_are_quoted_not_bare() {
        assert_eq!(sql_value(Some("NaN"), true), "'NaN'");
        assert_eq!(sql_value(Some("inf"), true), "'inf'");
        assert_eq!(sql_value(Some("-inf"), true), "'-inf'");
        assert_eq!(sql_value(Some("infinity"), true), "'infinity'");
        // Finite scientific notation stays bare (review issue 2: intended).
        assert_eq!(sql_value(Some("1e5"), true), "1e5");
    }

    // Task 2 review required addition: multi-column PK — every pk column
    // ANDed into the WHERE, in pk_cols order.
    #[test]
    fn multi_column_pk_where_is_anded() {
        let h = headers(&["a", "b", "v"]);
        let m = meta(None, "t", &h, &[0, 1], &[true, false, false]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 2, Some("x".into()));
        let mut original = |row: usize, col: usize| -> Option<String> {
            match (row, col) {
                (0, 0) => Some("7".into()),
                (0, 1) => Some("k".into()),
                _ => None,
            }
        };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(
            stmts[0].0,
            "UPDATE \"t\" SET \"v\" = 'x' WHERE \"a\" = 7 AND \"b\" = 'k'"
        );
    }

    // Task 2 review required addition: editing the PK column itself — the
    // WHERE must use the ORIGINAL value, the SET the staged one.
    #[test]
    fn editing_pk_column_uses_original_in_where() {
        let h = headers(&["id"]);
        let m = meta(None, "t", &h, &[0], &[true]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 0, Some("2".into()));
        let mut original = |row: usize, col: usize| -> Option<String> {
            if row == 0 && col == 0 { Some("1".into()) } else { None }
        };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(stmts[0].0, "UPDATE \"t\" SET \"id\" = 2 WHERE \"id\" = 1");
    }

    // Task 2 review required addition: an insert row with every cell
    // explicitly staged NULL must emit VALUES (NULL, ...), never DEFAULT
    // VALUES (which would take table defaults instead of NULLs).
    #[test]
    fn insert_all_explicit_nulls_emits_values_not_defaults() {
        let h = headers(&["a", "b"]);
        let m = meta(None, "t", &h, &[0], &[false, false]);
        let mut edits = EditState::default();
        let ix = edits.add_insert_row(2);
        edits.stage_insert_cell(ix, 0, None);
        edits.stage_insert_cell(ix, 1, None);
        let stmts = generate_statements(&m, &edits, &mut |_, _| None);
        assert_eq!(stmts[0].0, "INSERT INTO \"t\" (\"a\", \"b\") VALUES (NULL, NULL)");
        assert_eq!(stmts[0].1, None);
    }

    // G5 Task 3: `EditState::remove_insert_row` — the grid's per-inserted-row
    // "␡" gutter affordance.
    #[test]
    fn remove_insert_row_drops_the_row_and_shifts_later_indices() {
        let mut edits = EditState::default();
        let ix0 = edits.add_insert_row(1);
        edits.stage_insert_cell(ix0, 0, Some("first".into()));
        let ix1 = edits.add_insert_row(1);
        edits.stage_insert_cell(ix1, 0, Some("second".into()));
        assert_eq!(edits.inserted_rows.len(), 2);

        edits.remove_insert_row(ix0);
        assert_eq!(edits.inserted_rows.len(), 1);
        // The former ix1 row (now at index 0) survived with its own data.
        assert_eq!(edits.inserted_rows[0][0], Some(Some("second".to_string())));
    }

    #[test]
    fn remove_insert_row_out_of_range_is_a_noop() {
        let mut edits = EditState::default();
        edits.add_insert_row(1);
        edits.remove_insert_row(5);
        assert_eq!(edits.inserted_rows.len(), 1);
    }

    #[test]
    fn stage_insert_cell_out_of_range_is_a_noop() {
        // T3 review issue 1: a stale `ins_ix`/`col` (captured before a
        // concurrent remove_insert_row shifted the vec) must never panic nor
        // write into the wrong slot — it is a silent no-op.
        let mut edits = EditState::default();
        edits.add_insert_row(2);
        edits.stage_insert_cell(5, 0, Some("x".to_string())); // row OOB
        edits.stage_insert_cell(0, 9, Some("y".to_string())); // col OOB
        assert_eq!(edits.inserted_rows.len(), 1);
        assert_eq!(edits.inserted_rows[0], vec![None, None]);
        // A valid target still writes.
        edits.stage_insert_cell(0, 1, Some("ok".to_string()));
        assert_eq!(edits.inserted_rows[0][1], Some(Some("ok".to_string())));
    }

    #[test]
    fn remove_last_insert_row_clears_dirty() {
        let mut edits = EditState::default();
        edits.add_insert_row(1);
        assert!(edits.is_dirty());
        edits.remove_insert_row(0);
        assert!(!edits.is_dirty());
    }

    // G5 Task 3: pure staged-display resolution helpers — the exact text
    // the grid shows in a staged/inserted cell.
    #[test]
    fn staged_cell_display_distinguishes_untouched_null_and_value() {
        assert_eq!(staged_cell_display(None), None);
        assert_eq!(staged_cell_display(Some(&None)), Some("(NULL)".to_string()));
        assert_eq!(
            staged_cell_display(Some(&Some("x".to_string()))),
            Some("x".to_string())
        );
        // Empty string is a real staged value, distinct from staged NULL.
        assert_eq!(
            staged_cell_display(Some(&Some(String::new()))),
            Some(String::new())
        );
    }

    #[test]
    fn insert_cell_display_distinguishes_default_null_and_value() {
        assert_eq!(insert_cell_display(&None), "(výchozí)".to_string());
        assert_eq!(insert_cell_display(&Some(None)), "(NULL)".to_string());
        assert_eq!(
            insert_cell_display(&Some(Some("y".to_string()))),
            "y".to_string()
        );
    }

    // -- G15 T4: dialect-aware value/statement emission --------------------

    #[test]
    fn sql_value_d_mssql_uses_nchar_literals() {
        assert_eq!(
            sql_value_d(Some("Příliš žluťoučký"), false, Dialect::Mssql),
            "N'Příliš žluťoučký'".to_string()
        );
        // `'` doubling inside N''.
        assert_eq!(sql_value_d(Some("O'Reilly"), false, Dialect::Mssql), "N'O''Reilly'".to_string());
        // Numeric passthrough unchanged — no N prefix on a bare numeral.
        assert_eq!(sql_value_d(Some(" 42 "), true, Dialect::Mssql), "42".to_string());
        // None -> NULL regardless of dialect.
        assert_eq!(sql_value_d(None, false, Dialect::Mssql), "NULL".to_string());
        // pg/sqlite unaffected.
        assert_eq!(sql_value_d(Some("x"), false, Dialect::Postgres), "'x'".to_string());
    }

    #[test]
    fn generate_statements_mssql_brackets_and_nchar() {
        let h = headers(&["id", "we]ird"]);
        let m = TableMeta {
            schema: Some("s"),
            table: "t",
            headers: &h,
            pk_cols: &[0],
            numeric_cols: &[true, false],
            dialect: Dialect::Mssql,
        };
        let mut edits = EditState::default();
        edits.stage_cell(0, 1, Some("Příliš".into()));
        let mut original = |row: usize, col: usize| if row == 0 && col == 0 { Some("1".into()) } else { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(
            stmts[0].0,
            "UPDATE [s].[t] SET [we]]ird] = N'Příliš' WHERE [id] = 1"
        );
    }

    #[test]
    fn generate_statements_pg_output_is_byte_identical_to_before() {
        // Same fixture as `update_single_cell_quoted_string_pk_where` —
        // proves the default `Dialect::Postgres` `TableMeta` produces
        // exactly the pre-G15 string.
        let h = headers(&["id", "name"]);
        let m = meta(Some("public"), "users", &h, &[0], &[true, false]);
        let mut edits = EditState::default();
        edits.stage_cell(0, 1, Some("Alice".into()));
        let mut original = |row: usize, col: usize| if row == 0 && col == 0 { Some("1".into()) } else { None };
        let stmts = generate_statements(&m, &edits, &mut original);
        assert_eq!(
            stmts[0].0,
            "UPDATE \"public\".\"users\" SET \"name\" = 'Alice' WHERE \"id\" = 1"
        );
    }
}
