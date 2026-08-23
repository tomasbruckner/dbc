//! G12 Task 6: CSV import pure model -- header/target-column mapping and
//! batched INSERT SQL generation. Mirrors `sandbox.rs`'s "pure model,
//! GPUI-free, exhaustively tested" split (see the G12 design doc, §5): no
//! dependency on `dbc-core::split`, no filesystem/DB access, no `csv` crate
//! dependency (kept dependency-free per §6's T6 scope -- the UI task that
//! wires this in owns the actual CSV parsing and its quote-awareness).
//!
//! Value emission reuses `sandbox::sql_value` UNCHANGED (per the design
//! doc's binding constraint) and identifiers reuse `dbc_core::{quote_ident,
//! quote_qualified}` -- both already `pub`, so no visibility changes were
//! needed here.
//!
//! T7 (CSV import UI) is what actually calls into this module (file picker,
//! header peek, mapping modal, row pre-count, the runner method that drives
//! `generate_insert_batches` against a real connection) -- until then
//! nothing in the app calls these items, hence the module-level
//! `#![allow(dead_code)]`, matching the same convention `sandbox.rs` and
//! `tunnel.rs` use for not-yet-wired pure modules.

#![allow(dead_code)]

use crate::sandbox::sql_value;
use dbc_core::{quote_ident, quote_qualified};

/// Fixed batch size for generated multi-row `INSERT`s -- not user-tunable in
/// v1, same posture as `TAB_CAP`/`LOOKUP_ROW_CAP` elsewhere in this
/// codebase (G12 design doc, §5).
pub const CSV_IMPORT_BATCH_SIZE: usize = 500;

/// Classifies a catalog `ColumnInfo.data_type` string as numeric (bare
/// value emission) or not (quoted-as-text emission), feeding
/// `sandbox::sql_value`'s `numeric` flag. Case-insensitive SUBSTRING match
/// against known numeric type-name fragments covering both Postgres type
/// names (`int`, `serial`, `numeric`, `decimal`, `real`, `double`, `float`)
/// and SQLite's type-affinity names (`INTEGER`, `REAL`, `NUMERIC` -- all
/// already covered by the same fragment set: "integer" contains "int",
/// "real" and "numeric" are literal matches). Fail-closed like every other
/// guard in this codebase: an empty/unrecognized type name -> `false`
/// (quoted as text) -- always syntactically safe, worst case an
/// unnecessary quote the server coerces away (G12 design doc, §5).
pub fn is_numeric_type_name(data_type: &str) -> bool {
    const NUMERIC_FRAGMENTS: &[&str] =
        &["int", "serial", "numeric", "decimal", "real", "double", "float"];
    let lower = data_type.to_ascii_lowercase();
    NUMERIC_FRAGMENTS.iter().any(|frag| lower.contains(frag))
}

/// One target table column offered by the mapping UI (T7 supplies these
/// from `TableInfo.columns`, same source `detect_editable_pk` already
/// reads for sandbox editing) -- index-parallel to `ColumnMapping::targets`'
/// `Some` values. Kept as a small standalone struct (rather than pulling in
/// `dbc-core`'s catalog types) so this module stays free of any dependency
/// beyond `sandbox`/`dbc_core`'s existing quoting helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetColumn {
    pub name: String,
    /// Precomputed via `is_numeric_type_name(&column_info.data_type)` at
    /// mapping-build time (T7's job -- this module never sees a raw
    /// `data_type` string itself, only the already-classified flag).
    pub numeric: bool,
}

/// Header -> target-column mapping for one CSV import, index-parallel to
/// the CSV's header row. `targets[i] == Some(j)` maps CSV header `i` onto
/// `columns[j]`; `None` skips that CSV header entirely (its data is never
/// read into an INSERT). Target columns no header maps to are simply never
/// referenced -- they're omitted from the generated INSERT's column list,
/// letting the server's own default/NOT NULL handling apply (no
/// client-side NOT-NULL pre-validation, per the G12 design doc's §5
/// "errors are values, not a pre-flight schema audit" decision).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnMapping {
    pub targets: Vec<Option<usize>>,
}

impl ColumnMapping {
    /// `(csv_header_index, target_column_index)` pairs for every mapped
    /// (non-skipped) header, in CSV header order. This order fixes BOTH the
    /// generated INSERT's column list order and the order fields are read
    /// out of each CSV row -- the two are always kept in lockstep by
    /// deriving both from this single method.
    pub fn mapped_pairs(&self) -> Vec<(usize, usize)> {
        self.targets
            .iter()
            .enumerate()
            .filter_map(|(csv_ix, target)| target.map(|t| (csv_ix, t)))
            .collect()
    }
}

/// One CSV data row's per-field values, index-parallel to the CSV header
/// row (same indexing as `ColumnMapping::targets`). Already NULL-vs-empty
/// resolved by the caller (T7's job, via the `csv` crate's own
/// quote-awareness): `None` = an UNQUOTED empty CSV field -> SQL NULL;
/// `Some(s)` = a present (possibly quoted-empty) CSV field -> a SQL string
/// literal built from `s` (so `Some(String::new())` is a quoted empty
/// string, distinct from `None`'s NULL) -- the exact same outer-`Option`
/// convention `sandbox::EditState::cells` already uses for staged cell
/// edits (G12 design doc, §5).
pub type CsvRow = Vec<Option<String>>;

/// Generates the batched `INSERT INTO {table} ({cols}) VALUES (...), ...;`
/// statements for one CSV import, `CSV_IMPORT_BATCH_SIZE` rows per
/// statement (last batch may be smaller). Column list and per-row value
/// order both follow `ColumnMapping::mapped_pairs`'s CSV-header order.
/// Value emission is `sandbox::sql_value` UNCHANGED, `numeric` sourced from
/// each mapped `TargetColumn`. A CSV row shorter than the header row (a
/// ragged/malformed line) is treated fail-closed as NULL for any missing
/// trailing field rather than panicking -- consistent with every other
/// guard in this codebase preferring a safe fallback over a crash.
///
/// Returns an empty `Vec` (no statements) when `mapping` has no mapped
/// columns at all -- a CSV import with every header skipped has nothing to
/// insert into and is a UI-level misconfiguration, not something this pure
/// model should paper over with a `DEFAULT VALUES` guess.
pub fn generate_insert_batches(
    schema: Option<&str>,
    table: &str,
    columns: &[TargetColumn],
    mapping: &ColumnMapping,
    rows: &[CsvRow],
) -> Vec<String> {
    let pairs = mapping.mapped_pairs();
    if pairs.is_empty() {
        return Vec::new();
    }

    let table_sql = quote_qualified(schema, table);
    let cols_sql = pairs
        .iter()
        .map(|&(_, target_ix)| quote_ident(&columns[target_ix].name))
        .collect::<Vec<_>>()
        .join(", ");

    rows.chunks(CSV_IMPORT_BATCH_SIZE)
        .map(|batch| {
            let values_sql = batch
                .iter()
                .map(|row| {
                    let row_vals = pairs
                        .iter()
                        .map(|&(csv_ix, target_ix)| {
                            let v = row.get(csv_ix).and_then(|v| v.as_deref());
                            sql_value(v, columns[target_ix].numeric)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({row_vals})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {table_sql} ({cols_sql}) VALUES {values_sql};")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_numeric_type_name -------------------------------------------

    #[test]
    fn numeric_type_names_postgres() {
        for t in ["int4", "INTEGER", "bigint", "smallint", "serial", "bigserial", "numeric",
                  "numeric(10,2)", "decimal", "decimal(5,2)", "real", "double precision", "float4",
                  "float8"]
        {
            assert!(is_numeric_type_name(t), "expected numeric: {t}");
        }
    }

    #[test]
    fn numeric_type_names_sqlite_affinity() {
        for t in ["INTEGER", "integer", "REAL", "real", "NUMERIC", "numeric"] {
            assert!(is_numeric_type_name(t), "expected numeric: {t}");
        }
    }

    #[test]
    fn non_numeric_type_names_fail_closed() {
        for t in ["text", "varchar", "varchar(255)", "uuid", "boolean", "bytea", "json", "date",
                  "timestamp", "", "blob", "clob"]
        {
            assert!(!is_numeric_type_name(t), "expected non-numeric: {t}");
        }
    }

    // -- ColumnMapping -----------------------------------------------------

    #[test]
    fn mapping_with_skipped_headers() {
        // CSV headers: id, junk, name. "junk" is skipped.
        let mapping = ColumnMapping { targets: vec![Some(0), None, Some(1)] };
        assert_eq!(mapping.mapped_pairs(), vec![(0, 0), (2, 1)]);
    }

    #[test]
    fn unmapped_not_null_target_column_omitted_from_insert() {
        // Table has 3 columns (id, name, created_at NOT NULL no default) but
        // only 2 CSV headers map onto the first two -- created_at must not
        // appear in the generated column list at all (server decides).
        let columns = vec![
            TargetColumn { name: "id".into(), numeric: true },
            TargetColumn { name: "name".into(), numeric: false },
            TargetColumn { name: "created_at".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(1)] };
        let rows: Vec<CsvRow> = vec![vec![Some("1".into()), Some("Alice".into())]];

        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, 'Alice');");
    }

    #[test]
    fn no_mapped_columns_generates_nothing() {
        let columns = vec![TargetColumn { name: "id".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![None] };
        let rows: Vec<CsvRow> = vec![vec![Some("1".into())]];
        assert!(generate_insert_batches(None, "t", &columns, &mapping, &rows).is_empty());
    }

    // -- NULL vs. empty string ----------------------------------------------

    #[test]
    fn null_vs_empty_string_field() {
        let columns = vec![
            TargetColumn { name: "id".into(), numeric: true },
            TargetColumn { name: "note".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(1)] };

        // Unquoted empty CSV field -> SQL NULL.
        let rows_null: Vec<CsvRow> = vec![vec![Some("1".into()), None]];
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows_null);
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"note\") VALUES (1, NULL);");

        // Quoted-empty CSV field -> SQL empty string, distinct from NULL.
        let rows_empty: Vec<CsvRow> = vec![vec![Some("1".into()), Some(String::new())]];
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows_empty);
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"note\") VALUES (1, '');");
    }

    // -- numeric vs. text emission -------------------------------------------

    #[test]
    fn numeric_vs_text_emission_per_target_column() {
        let columns = vec![
            TargetColumn { name: "age".into(), numeric: true },
            TargetColumn { name: "name".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(1)] };
        let rows: Vec<CsvRow> = vec![
            vec![Some("42".into()), Some("Bob".into())],
            // Numeric column, non-numeric-looking text -> falls back to
            // quoted (server decides), matching `sql_value`'s own contract.
            vec![Some("abc".into()), Some("42".into())],
        ];

        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0],
            "INSERT INTO \"t\" (\"age\", \"name\") VALUES (42, 'Bob'), ('abc', '42');"
        );
    }

    // -- batch boundaries ----------------------------------------------------

    fn one_col_rows(n: usize) -> Vec<CsvRow> {
        (0..n).map(|i| vec![Some(i.to_string())]).collect()
    }

    #[test]
    fn batch_boundary_499_rows_one_statement() {
        let columns = vec![TargetColumn { name: "n".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows = one_col_rows(499);
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn batch_boundary_500_rows_one_statement() {
        let columns = vec![TargetColumn { name: "n".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows = one_col_rows(500);
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn batch_boundary_501_rows_two_statements() {
        let columns = vec![TargetColumn { name: "n".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows = one_col_rows(501);
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(stmts.len(), 2);
        // First batch holds 500 rows, second holds the 1 remainder.
        assert_eq!(stmts[0].matches("), (").count() + 1, 500);
        assert_eq!(stmts[1].matches("), (").count() + 1, 1);
    }

    // -- weird identifiers -----------------------------------------------

    #[test]
    fn weird_identifiers_quoted() {
        let columns = vec![TargetColumn { name: "na\"me".into(), numeric: false }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows: Vec<CsvRow> = vec![vec![Some("v".into())]];

        let stmts =
            generate_insert_batches(Some("pu\"blic"), "ta\"ble", &columns, &mapping, &rows);
        assert_eq!(
            stmts[0],
            "INSERT INTO \"pu\"\"blic\".\"ta\"\"ble\" (\"na\"\"me\") VALUES ('v');"
        );
    }

    // -- exact generated SQL for a small fixture ------------------------

    #[test]
    fn exact_sql_small_fixture() {
        // CSV headers: id, name, extra (skipped). Table: id (numeric), name.
        let columns = vec![
            TargetColumn { name: "id".into(), numeric: true },
            TargetColumn { name: "name".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(1), None] };
        let rows: Vec<CsvRow> = vec![
            vec![Some("1".into()), Some("Alice".into()), Some("ignored".into())],
            vec![Some("2".into()), Some("O'Reilly".into()), None],
            vec![Some("3".into()), None, Some("x".into())],
        ];

        let stmts = generate_insert_batches(Some("public"), "people", &columns, &mapping, &rows);
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0],
            "INSERT INTO \"public\".\"people\" (\"id\", \"name\") VALUES (1, 'Alice'), (2, 'O''Reilly'), (3, NULL);"
        );
    }

    #[test]
    fn ragged_row_missing_trailing_field_treated_as_null() {
        let columns = vec![
            TargetColumn { name: "id".into(), numeric: true },
            TargetColumn { name: "name".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(1)] };
        // Row is missing the second field entirely (ragged CSV line).
        let rows: Vec<CsvRow> = vec![vec![Some("1".into())]];

        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, NULL);");
    }
}
