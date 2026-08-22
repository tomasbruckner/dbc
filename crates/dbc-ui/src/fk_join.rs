//! G4 Task 5: FK joined columns (spec choice "B") — pure SQL-building and
//! accessor helpers, no GPUI, no I/O. `grid.rs` drives the UI (☰ menu,
//! tinting, effective-column plumbing) and `main.rs` drives the actual
//! query dispatch (preview re-run / ad-hoc lookup); this module only builds
//! SQL strings and maps values, so all of it is unit-testable directly
//! against fixtures.
//!
//! Two independent mechanisms share this module:
//! - **PREVIEW tabs** (`build_join_sql`): the whole preview query is
//!   rewritten with `LEFT JOIN`s and re-run — the joined columns arrive as
//!   ordinary result columns aliased `"{ref_table}.{col}"`.
//! - **AD-HOC tabs** (`build_lookup_sql` + `VirtualCol`/`effective_cell_text`):
//!   no re-run. One batched `SELECT ... WHERE key IN (...)` fetches the
//!   referenced rows for the FK column's CURRENT DISTINCT VALUES, and the
//!   grid renders the result as VIRTUAL columns computed on the fly from a
//!   `HashMap<value, Option<value>>` per joined column — see
//!   `effective_cell_text`'s doc comment for the "sources 0..n, virtuals
//!   n..n+m" indexing this enables.

use std::collections::{HashMap, HashSet};

use dbc_core::{quote_ident, quote_qualified};

/// One FK column's requested joined columns — one `JoinSpec` per FK column
/// with at least one checked ref-column; `build_join_sql` turns each into
/// its own `LEFT JOIN`, aliased `j1`, `j2`, ... in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinSpec {
    pub fk_col: String,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_key: String,
    pub cols: Vec<String>,
}

/// `SELECT t.*, j1."col" AS "ref_table.col", ... FROM {qualified base} t
/// LEFT JOIN {qualified ref} j1 ON t."fk_col" = j1."ref_key" ... LIMIT 1000`.
///
/// Joins whose `cols` is empty are skipped entirely (nothing to select,
/// nothing worth joining for — this is what lets a checkbox-toggle-to-zero
/// silently drop a join rather than emitting a pointless `LEFT JOIN` with no
/// selected columns). All identifiers go through `dbc_core::quote_ident`/
/// `quote_qualified` — a table/column literally named `we"ird` can't break
/// out of the query, same guarantee `preview_sql` (main.rs) already gives
/// the un-joined preview path.
pub fn build_join_sql(schema: Option<&str>, table: &str, joins: &[JoinSpec]) -> String {
    let base = quote_qualified(schema, table);
    let mut select_extra = String::new();
    let mut from_extra = String::new();
    let mut alias_ix = 0usize;
    for j in joins {
        if j.cols.is_empty() {
            continue;
        }
        alias_ix += 1;
        let alias = format!("j{alias_ix}");
        for c in &j.cols {
            select_extra.push_str(&format!(
                ", {alias}.{col} AS {label}",
                col = quote_ident(c),
                label = quote_ident(&format!("{}.{}", j.ref_table, c)),
            ));
        }
        let ref_q = quote_qualified(j.ref_schema.as_deref(), &j.ref_table);
        from_extra.push_str(&format!(
            " LEFT JOIN {ref_q} {alias} ON t.{fk} = {alias}.{key}",
            fk = quote_ident(&j.fk_col),
            key = quote_ident(&j.ref_key),
        ));
    }
    format!("SELECT t.*{select_extra} FROM {base} t{from_extra} LIMIT 1000")
}

/// `SELECT "key", "col1", ... FROM {qualified ref} WHERE "key" IN ('v1',
/// 'v2', ...)` — `values` are assumed already deduped/capped by the caller
/// (see `collect_distinct_capped`); this only escapes and quotes each one as
/// a SQL string literal. Numeric-looking values are still quoted — both
/// Postgres and SQLite compare/cast a quoted numeric literal against a
/// numeric column without complaint, and quoting uniformly means this
/// function never has to guess a column's real type. Single quotes inside a
/// value are doubled (SAFETY: values originate from the DB itself via a
/// prior SELECT, but are escaped anyway — never trust a round-trip).
///
/// `values.is_empty()` still produces syntactically valid-looking SQL
/// (`IN ()`, which most engines reject) — callers must not invoke this with
/// an empty `values` list; `collect_distinct_capped` returning `Some(vec![])`
/// (nothing to look up) is the caller's signal to skip the query entirely.
pub fn build_lookup_sql(
    ref_schema: Option<&str>,
    ref_table: &str,
    key_col: &str,
    wanted_cols: &[String],
    values: &[String],
) -> String {
    let ref_q = quote_qualified(ref_schema, ref_table);
    let mut cols = vec![quote_ident(key_col)];
    cols.extend(wanted_cols.iter().map(|c| quote_ident(c)));
    let values_sql: Vec<String> =
        values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect();
    format!(
        "SELECT {cols} FROM {ref_q} WHERE {key} IN ({vals})",
        cols = cols.join(", "),
        key = quote_ident(key_col),
        vals = values_sql.join(", "),
    )
}

/// Distinct-value collection with a cap (brief contract #3): pure over
/// already-read cell values (grid.rs reads the CURRENT VIEW's fk column via
/// `RowView`/`ResultBuffer` and feeds the result here). Order of first
/// occurrence is preserved; `None` entries (SQL NULL) are skipped — nothing
/// to look up for a NULL fk value. Returns `None` when the distinct count
/// would exceed `cap`, so the caller can abort with "příliš mnoho hodnot pro
/// dočasný join" instead of silently truncating the `IN` list (which would
/// look up the wrong subset without any indication rows are missing).
pub fn collect_distinct_capped(
    values: impl IntoIterator<Item = Option<String>>,
    cap: usize,
) -> Option<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for v in values.into_iter().flatten() {
        if seen.insert(v.clone()) {
            out.push(v);
            if out.len() > cap {
                return None;
            }
        }
    }
    Some(out)
}

/// One ad-hoc-tab virtual (looked-up) column: `name` is the display header
/// (`"{ref_table}.{col}"`, same alias convention `build_join_sql` uses for
/// the preview path so both tint/identify the same way in `grid.rs`), `map`
/// is `fk value (as text) -> Option<joined value text>` (`None` = the
/// matched ref row's value is itself SQL NULL; a missing key = no matching
/// ref row at all — both render as an empty cell, see
/// `virtual_cell_text`), and `src_col` is the SOURCE column index (into the
/// grid's real result columns, NOT effective-column space) whose value is
/// the join key.
#[derive(Debug, Clone)]
pub struct VirtualCol {
    pub name: String,
    pub map: HashMap<String, Option<String>>,
    pub src_col: usize,
}

/// Looks up `fk_value` in `map`, collapsing both "no matching ref row"
/// (missing key) and "matched row, but the joined column is SQL NULL"
/// (`Some(None)`) to an empty string — same "NULL displays as empty string"
/// convention `ResultBuffer::cell_text` already uses for real columns, so a
/// virtual cell looks indistinguishable from a real one at the text level.
pub fn virtual_cell_text(fk_value: &str, map: &HashMap<String, Option<String>>) -> String {
    map.get(fk_value).cloned().flatten().unwrap_or_default()
}

/// Effective-column cell accessor — the unified indexing the brief asks
/// for: SOURCE columns occupy `0..ncols`, VIRTUAL columns occupy
/// `ncols..ncols+virtual_cols.len()` (in `virtual_cols`' own order). `col <
/// ncols` reads straight through `buf_cell` (the real column, exactly what
/// `RowView::rebuild`'s own closure convention already does); `col >=
/// ncols` resolves `virtual_cols[col - ncols]`, reads ITS `src_col` (a
/// SOURCE index, always `< ncols`) via the SAME `buf_cell`, and maps that
/// through `virtual_cell_text`.
///
/// Pure — `buf_cell` is a caller-supplied `FnMut(row, col) -> String`
/// closure exactly like `RowView::rebuild`'s, so this is unit-testable over
/// a fake `Vec<Vec<String>>` fixture instead of a real `ResultBuffer`, and
/// is exactly what makes sort/filter/find/export/copy in `grid.rs` "see"
/// virtual columns for free: every one of those already routes cell access
/// through a closure of this same shape — swapping the closure's inner call
/// for `effective_cell_text` is the entire extension.
pub fn effective_cell_text(
    ncols: usize,
    virtual_cols: &[VirtualCol],
    buf_cell: &mut dyn FnMut(usize, usize) -> String,
    source_row: usize,
    col: usize,
) -> String {
    if col < ncols {
        return buf_cell(source_row, col);
    }
    let Some(vcol) = virtual_cols.get(col - ncols) else {
        return String::new();
    };
    let fk_val = buf_cell(source_row, vcol.src_col);
    virtual_cell_text(&fk_val, &vcol.map)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- build_join_sql ---

    #[test]
    fn single_join_one_column() {
        let joins = vec![JoinSpec {
            fk_col: "customer_id".into(),
            ref_schema: Some("public".into()),
            ref_table: "customers".into(),
            ref_key: "id".into(),
            cols: vec!["name".into()],
        }];
        let sql = build_join_sql(Some("public"), "orders", &joins);
        assert_eq!(
            sql,
            "SELECT t.*, j1.\"name\" AS \"customers.name\" FROM \"public\".\"orders\" t \
             LEFT JOIN \"public\".\"customers\" j1 ON t.\"customer_id\" = j1.\"id\" LIMIT 1000"
        );
    }

    #[test]
    fn no_schema_qualifier_when_none() {
        let joins = vec![JoinSpec {
            fk_col: "cid".into(),
            ref_schema: None,
            ref_table: "customers".into(),
            ref_key: "id".into(),
            cols: vec!["name".into()],
        }];
        let sql = build_join_sql(None, "orders", &joins);
        assert_eq!(
            sql,
            "SELECT t.*, j1.\"name\" AS \"customers.name\" FROM \"orders\" t \
             LEFT JOIN \"customers\" j1 ON t.\"cid\" = j1.\"id\" LIMIT 1000"
        );
    }

    #[test]
    fn multiple_joins_get_sequential_aliases() {
        let joins = vec![
            JoinSpec {
                fk_col: "customer_id".into(),
                ref_schema: None,
                ref_table: "customers".into(),
                ref_key: "id".into(),
                cols: vec!["name".into()],
            },
            JoinSpec {
                fk_col: "product_id".into(),
                ref_schema: None,
                ref_table: "products".into(),
                ref_key: "id".into(),
                cols: vec!["sku".into(), "title".into()],
            },
        ];
        let sql = build_join_sql(None, "orders", &joins);
        assert_eq!(
            sql,
            "SELECT t.*, j1.\"name\" AS \"customers.name\", j2.\"sku\" AS \"products.sku\", \
             j2.\"title\" AS \"products.title\" FROM \"orders\" t \
             LEFT JOIN \"customers\" j1 ON t.\"customer_id\" = j1.\"id\" \
             LEFT JOIN \"products\" j2 ON t.\"product_id\" = j2.\"id\" LIMIT 1000"
        );
    }

    #[test]
    fn joins_with_no_selected_columns_are_skipped_entirely() {
        let joins = vec![
            JoinSpec {
                fk_col: "customer_id".into(),
                ref_schema: None,
                ref_table: "customers".into(),
                ref_key: "id".into(),
                cols: vec![],
            },
            JoinSpec {
                fk_col: "product_id".into(),
                ref_schema: None,
                ref_table: "products".into(),
                ref_key: "id".into(),
                cols: vec!["sku".into()],
            },
        ];
        let sql = build_join_sql(None, "orders", &joins);
        // The empty join is skipped, so the surviving join is still
        // aliased j1 (not j2) — alias numbering only counts EMITTED joins.
        assert_eq!(
            sql,
            "SELECT t.*, j1.\"sku\" AS \"products.sku\" FROM \"orders\" t \
             LEFT JOIN \"products\" j1 ON t.\"product_id\" = j1.\"id\" LIMIT 1000"
        );
    }

    #[test]
    fn no_joins_at_all_is_a_plain_select_star() {
        let sql = build_join_sql(Some("public"), "orders", &[]);
        assert_eq!(sql, "SELECT t.* FROM \"public\".\"orders\" t LIMIT 1000");
    }

    #[test]
    fn embedded_quote_in_identifiers_is_doubled_not_smuggled() {
        let joins = vec![JoinSpec {
            fk_col: "we\"ird_fk".into(),
            ref_schema: None,
            ref_table: "we\"ird".into(),
            ref_key: "id".into(),
            cols: vec!["na\"me".into()],
        }];
        let sql = build_join_sql(None, "t", &joins);
        assert_eq!(
            sql,
            "SELECT t.*, j1.\"na\"\"me\" AS \"we\"\"ird.na\"\"me\" FROM \"t\" t \
             LEFT JOIN \"we\"\"ird\" j1 ON t.\"we\"\"ird_fk\" = j1.\"id\" LIMIT 1000"
        );
    }

    // --- build_lookup_sql ---

    #[test]
    fn lookup_sql_basic_shape() {
        let sql = build_lookup_sql(
            Some("public"),
            "customers",
            "id",
            &["name".to_string(), "email".to_string()],
            &["1".to_string(), "2".to_string()],
        );
        assert_eq!(
            sql,
            "SELECT \"id\", \"name\", \"email\" FROM \"public\".\"customers\" \
             WHERE \"id\" IN ('1', '2')"
        );
    }

    #[test]
    fn lookup_sql_no_schema() {
        let sql = build_lookup_sql(None, "customers", "id", &["name".to_string()], &["1".to_string()]);
        assert_eq!(sql, "SELECT \"id\", \"name\" FROM \"customers\" WHERE \"id\" IN ('1')");
    }

    #[test]
    fn lookup_sql_escapes_single_quotes_in_values() {
        let sql = build_lookup_sql(
            None,
            "customers",
            "id",
            &["name".to_string()],
            &["o'brien".to_string()],
        );
        assert_eq!(sql, "SELECT \"id\", \"name\" FROM \"customers\" WHERE \"id\" IN ('o''brien')");
    }

    #[test]
    fn lookup_sql_quotes_numeric_looking_values() {
        let sql = build_lookup_sql(None, "t", "id", &[], &["42".to_string()]);
        assert_eq!(sql, "SELECT \"id\" FROM \"t\" WHERE \"id\" IN ('42')");
    }

    // --- collect_distinct_capped ---

    #[test]
    fn distinct_preserves_first_occurrence_order_and_drops_dupes() {
        let values = vec![Some("b".to_string()), Some("a".to_string()), Some("b".to_string())];
        assert_eq!(
            collect_distinct_capped(values, 10),
            Some(vec!["b".to_string(), "a".to_string()])
        );
    }

    #[test]
    fn distinct_skips_nulls() {
        let values = vec![Some("a".to_string()), None, Some("b".to_string()), None];
        assert_eq!(
            collect_distinct_capped(values, 10),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn distinct_within_cap_succeeds() {
        let values: Vec<Option<String>> = (0..5).map(|i| Some(i.to_string())).collect();
        assert_eq!(collect_distinct_capped(values, 5).map(|v| v.len()), Some(5));
    }

    #[test]
    fn distinct_over_cap_returns_none() {
        let values: Vec<Option<String>> = (0..1001).map(|i| Some(i.to_string())).collect();
        assert_eq!(collect_distinct_capped(values, 1000), None);
    }

    #[test]
    fn distinct_empty_input_yields_empty_vec_not_none() {
        assert_eq!(collect_distinct_capped(Vec::new(), 1000), Some(Vec::new()));
    }

    // --- virtual_cell_text / effective_cell_text ---

    #[test]
    fn virtual_cell_text_missing_key_is_empty() {
        let map = HashMap::new();
        assert_eq!(virtual_cell_text("1", &map), "");
    }

    #[test]
    fn virtual_cell_text_null_joined_value_is_empty() {
        let mut map = HashMap::new();
        map.insert("1".to_string(), None);
        assert_eq!(virtual_cell_text("1", &map), "");
    }

    #[test]
    fn virtual_cell_text_present_value() {
        let mut map = HashMap::new();
        map.insert("1".to_string(), Some("Alice".to_string()));
        assert_eq!(virtual_cell_text("1", &map), "Alice");
    }

    /// Fixture: 2 source columns (0: "id", 1: "customer_id"), 1 virtual
    /// column (index 2) mapping `customer_id` -> customer name.
    fn fixture() -> (usize, Vec<VirtualCol>, Vec<Vec<&'static str>>) {
        let rows = vec![vec!["10", "1"], vec!["11", "2"], vec!["12", "9"]];
        let mut map = HashMap::new();
        map.insert("1".to_string(), Some("Alice".to_string()));
        map.insert("2".to_string(), None); // matched ref row, NULL name
        // "9" deliberately absent — no matching ref row.
        let vcols = vec![VirtualCol { name: "customers.name".to_string(), map, src_col: 1 }];
        (2, vcols, rows)
    }

    #[test]
    fn effective_cell_text_reads_source_columns_unchanged() {
        let (ncols, vcols, rows) = fixture();
        let mut cell = |r: usize, c: usize| rows[r][c].to_string();
        assert_eq!(effective_cell_text(ncols, &vcols, &mut cell, 0, 0), "10");
        assert_eq!(effective_cell_text(ncols, &vcols, &mut cell, 1, 1), "2");
    }

    #[test]
    fn effective_cell_text_resolves_virtual_column_via_src_col() {
        let (ncols, vcols, rows) = fixture();
        let mut cell = |r: usize, c: usize| rows[r][c].to_string();
        assert_eq!(effective_cell_text(ncols, &vcols, &mut cell, 0, 2), "Alice");
    }

    #[test]
    fn effective_cell_text_virtual_column_null_and_missing_both_render_empty() {
        let (ncols, vcols, rows) = fixture();
        let mut cell = |r: usize, c: usize| rows[r][c].to_string();
        assert_eq!(effective_cell_text(ncols, &vcols, &mut cell, 1, 2), ""); // NULL match
        assert_eq!(effective_cell_text(ncols, &vcols, &mut cell, 2, 2), ""); // no match
    }

    #[test]
    fn effective_cell_text_out_of_range_virtual_index_is_empty() {
        let (ncols, vcols, rows) = fixture();
        let mut cell = |r: usize, c: usize| rows[r][c].to_string();
        assert_eq!(effective_cell_text(ncols, &vcols, &mut cell, 0, 99), "");
    }
}
