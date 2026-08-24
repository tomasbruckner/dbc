//! G12 Task 6: CSV import pure model -- header/target-column mapping and
//! batched INSERT SQL generation. Mirrors `sandbox.rs`'s "pure model,
//! GPUI-free, exhaustively tested" split (see the G12 design doc, §5): no
//! dependency on `dbc-core::split`, no filesystem/DB access, no `csv` crate
//! dependency (kept dependency-free per §6's T6 scope -- the UI task that
//! wires this in owns the actual CSV parsing and its quote-awareness).
//!
//! Value emission reuses `sandbox::sql_value_d` and identifiers reuse
//! `dbc_core::{quote_ident_d, quote_qualified_d}` -- both already `pub`, so
//! no visibility changes were needed here.
//!
//! T7 (CSV import UI) is what actually calls into this module (file picker,
//! header peek, mapping modal, row pre-count, the runner method that drives
//! `generate_insert_batches_d` against a real connection) -- wired in by
//! `runner::run_csv_import`/`main.rs`'s CSV import UI.
//!
//! G15 T4: `generate_insert_batches_d` is the dialect-aware sibling
//! (bracket-quoted identifiers, `N''` literals for MSSQL). G15 batch C
//! review (BLOCKER 1): `main.rs`'s two preview/sample-SQL call sites
//! (`start_csv_import`, `recompute_csv_sample`) and
//! `runner.rs::run_csv_import_drive`'s execution call now ALL thread a real
//! resolved dialect through `generate_insert_batches_d` together, in the
//! same change -- display/exec parity holds because both resolve dialect
//! from the same connection (`main.rs::sql_dialect`/`runner.rs::spec_dialect`
//! agree by construction, both mapping `dbc_state::Engine` 1:1 onto
//! `dbc_core::Dialect`). `generate_insert_batches` (no `_d`) stays the
//! pg-convention wrapper, now used only by this module's own pg-shaped
//! tests.

use std::collections::HashSet;

use crate::sandbox::sql_value_d;
use dbc_core::{quote_ident_d, quote_qualified_d, Dialect};

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
///
/// Known false POSITIVES exist (e.g. `point`, `int4range`, `integer[]` all
/// contain "int") and are intentionally left uncaught -- they're harmless
/// by construction, not a gap: `numeric=true` only ever *offers* bare
/// emission, it doesn't force it. `sandbox::sql_value`'s own parse gate
/// (`i128`/finite-`f64` parse of the trimmed value) still runs downstream,
/// so a non-numeric value in a false-positive-classified column simply
/// falls through to the quoted branch same as any other unparseable
/// string -- the safety net is `sql_value`'s parse gate, not the accuracy
/// of this classifier. There are no false NEGATIVES for the type names
/// actually listed above, which is the direction that would matter (an
/// actually-numeric value getting needlessly quoted is always safe; the
/// server coerces it back).
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
/// statement (last batch may be smaller). Each returned `String` is one
/// complete, independently-dispatchable statement -- the trailing `;` is
/// intentional (unlike `sandbox::generate_statements`'s bare statements,
/// which are fed to a driver call that supplies its own terminator): a CSV
/// import batch is meant to be handed straight to `Connection::execute()`
/// as a standalone string, one call per batch, so it carries its own
/// terminator rather than relying on a caller to add one.
///
/// Column list and per-row value order both follow
/// `ColumnMapping::mapped_pairs`'s CSV-header order. Value emission is
/// `sandbox::sql_value` UNCHANGED, `numeric` sourced from each mapped
/// `TargetColumn`. A CSV row shorter than the header row (a
/// ragged/malformed line) is treated fail-closed as NULL for any missing
/// trailing field rather than panicking -- consistent with every other
/// guard in this codebase preferring a safe fallback over a crash.
///
/// Returns `Ok(vec![])` (no statements) when `mapping` has no mapped
/// columns at all -- a CSV import with every header skipped has nothing to
/// insert into and is a UI-level misconfiguration, not something this pure
/// model should paper over with a `DEFAULT VALUES` guess.
///
/// Returns `Err` when two (or more) CSV headers map onto the SAME target
/// column -- defense in depth against a mapping-UI bug (T7 hasn't landed
/// yet; this must not rely on a future UI never allowing it): an
/// unguarded duplicate would generate a syntactically valid but
/// always-failing `INSERT INTO t (id, id) VALUES (1, 2)` (the driver
/// rejects "column specified more than once" on every batch, rolling back
/// the whole import) -- caught here, before any SQL is built, with a
/// message identifying the offending column.
///
/// Thin pg-convention wrapper over [`generate_insert_batches_d`] --
/// byte-identical pre-G15 behavior. **G15 batch C review (BLOCKER 1) closed
/// the T4 deviation this doc comment used to flag:** every real call site
/// (`main.rs`'s two preview/sample-SQL sites, `runner.rs::run_csv_import_drive`'s
/// execution call) now goes through `generate_insert_batches_d` with a real
/// resolved dialect, switched together in one change to preserve
/// display/exec parity -- none of them call this wrapper anymore, so it's
/// `#[cfg(test)]`-only now (this module's own pg-shaped tests + the
/// pg-byte-identity golden test) rather than dead production code.
#[cfg(test)]
pub fn generate_insert_batches(
    schema: Option<&str>,
    table: &str,
    columns: &[TargetColumn],
    mapping: &ColumnMapping,
    rows: &[CsvRow],
) -> Result<Vec<String>, String> {
    generate_insert_batches_d(Dialect::Postgres, schema, table, columns, mapping, rows)
}

/// Dialect-aware sibling of [`generate_insert_batches`] (G15 §2b/§2c —
/// bracket-quoted identifiers, `N''` string literals for MSSQL). Wired into
/// every real call site as of the batch C review fix -- see
/// `generate_insert_batches`'s doc comment.
pub fn generate_insert_batches_d(
    dialect: Dialect,
    schema: Option<&str>,
    table: &str,
    columns: &[TargetColumn],
    mapping: &ColumnMapping,
    rows: &[CsvRow],
) -> Result<Vec<String>, String> {
    let pairs = mapping.mapped_pairs();
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    let mut seen_targets = HashSet::new();
    for &(_, target_ix) in &pairs {
        if !seen_targets.insert(target_ix) {
            let name = columns.get(target_ix).map(|c| c.name.as_str()).unwrap_or("?");
            return Err(format!("sloupec {name} je namapován vícekrát"));
        }
    }

    let table_sql = quote_qualified_d(dialect, schema, table);
    let cols_sql = pairs
        .iter()
        .map(|&(_, target_ix)| quote_ident_d(dialect, &columns[target_ix].name))
        .collect::<Vec<_>>()
        .join(", ");

    let statements = rows
        .chunks(CSV_IMPORT_BATCH_SIZE)
        .map(|batch| {
            let values_sql = batch
                .iter()
                .map(|row| {
                    let row_vals = pairs
                        .iter()
                        .map(|&(csv_ix, target_ix)| {
                            let v = row.get(csv_ix).and_then(|v| v.as_deref());
                            sql_value_d(v, columns[target_ix].numeric, dialect)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({row_vals})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("INSERT INTO {table_sql} ({cols_sql}) VALUES {values_sql};")
        })
        .collect();
    Ok(statements)
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

    /// G16 (design §4): DuckDB's own type-name spellings classify without
    /// any code change — the fragment matcher already covers them.
    #[test]
    fn duckdb_type_names_classify_numeric_vs_quoted() {
        for numeric in ["HUGEINT", "UTINYINT", "USMALLINT", "UINTEGER", "UBIGINT",
                        "BIGINT", "DOUBLE", "FLOAT", "REAL", "DECIMAL(18,3)"] {
            assert!(is_numeric_type_name(numeric), "{numeric} must classify numeric");
        }
        for quoted in ["VARCHAR", "BLOB", "DATE", "TIMESTAMP",
                       "TIMESTAMP WITH TIME ZONE", "BOOLEAN", "UUID"] {
            assert!(!is_numeric_type_name(quoted), "{quoted} must fall to the quoted path");
        }
        // "INTERVAL" contains the "int" fragment — a known, SAFE false
        // positive: sql_value's parse gate quotes any value that isn't a
        // bare numeral anyway (this classifier's own doc comment).
        assert!(is_numeric_type_name("INTERVAL"));
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

        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, 'Alice');");
    }

    #[test]
    fn no_mapped_columns_generates_nothing() {
        let columns = vec![TargetColumn { name: "id".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![None] };
        let rows: Vec<CsvRow> = vec![vec![Some("1".into())]];
        assert!(generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap().is_empty());
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
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows_null).unwrap();
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"note\") VALUES (1, NULL);");

        // Quoted-empty CSV field -> SQL empty string, distinct from NULL.
        let rows_empty: Vec<CsvRow> = vec![vec![Some("1".into()), Some(String::new())]];
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows_empty).unwrap();
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

        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
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
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn batch_boundary_500_rows_one_statement() {
        let columns = vec![TargetColumn { name: "n".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows = one_col_rows(500);
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn batch_boundary_501_rows_two_statements() {
        let columns = vec![TargetColumn { name: "n".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows = one_col_rows(501);
        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
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
            generate_insert_batches(Some("pu\"blic"), "ta\"ble", &columns, &mapping, &rows).unwrap();
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

        let stmts = generate_insert_batches(Some("public"), "people", &columns, &mapping, &rows).unwrap();
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

        let stmts = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
        assert_eq!(stmts[0], "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, NULL);");
    }

    // -- duplicate target-column mapping (defense in depth) -----------------

    #[test]
    fn duplicate_target_mapping_is_an_error_no_sql_generated() {
        // Two CSV headers both mapped onto target column 0 ("id") -- a
        // mapping-UI bug this model must reject on its own, not rely on T7
        // to prevent, since an unguarded duplicate would otherwise emit a
        // guaranteed-to-fail `INSERT INTO t (id, id) VALUES (...)`.
        let columns = vec![
            TargetColumn { name: "id".into(), numeric: true },
            TargetColumn { name: "name".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(0)] };
        let rows: Vec<CsvRow> = vec![vec![Some("1".into()), Some("2".into())]];

        let result = generate_insert_batches(None, "t", &columns, &mapping, &rows);
        assert_eq!(result, Err("sloupec id je namapován vícekrát".to_string()));
    }

    // -- G15 T4: dialect-aware generation -----------------------------------

    #[test]
    fn generate_insert_batches_mssql_brackets_and_nchar() {
        let columns = vec![
            TargetColumn { name: "id".into(), numeric: true },
            TargetColumn { name: "we]ird".into(), numeric: false },
        ];
        let mapping = ColumnMapping { targets: vec![Some(0), Some(1)] };
        let rows: Vec<CsvRow> = vec![vec![Some("1".into()), Some("Příliš".into())]];

        let stmts =
            generate_insert_batches_d(Dialect::Mssql, Some("s"), "t", &columns, &mapping, &rows)
                .unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0],
            "INSERT INTO [s].[t] ([id], [we]]ird]) VALUES (1, N'Příliš');"
        );
    }

    #[test]
    fn generate_insert_batches_d_pg_output_is_byte_identical_to_wrapper() {
        let columns = vec![TargetColumn { name: "id".into(), numeric: true }];
        let mapping = ColumnMapping { targets: vec![Some(0)] };
        let rows: Vec<CsvRow> = vec![vec![Some("1".into())]];
        let via_wrapper = generate_insert_batches(None, "t", &columns, &mapping, &rows).unwrap();
        let via_d =
            generate_insert_batches_d(Dialect::Postgres, None, "t", &columns, &mapping, &rows)
                .unwrap();
        assert_eq!(via_wrapper, via_d);
    }

    #[test]
    fn csv_import_batch_size_is_under_the_tsql_values_row_cap() {
        // T-SQL: a VALUES clause may contain at most 1000 row
        // constructors -- a future bump past that would silently break
        // MSSQL imports at runtime.
        assert!(CSV_IMPORT_BATCH_SIZE <= 1000);
    }
}
