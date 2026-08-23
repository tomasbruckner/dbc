//! `schema()` catalog introspection: raw T-SQL against `sys.*` catalog
//! views, NOT the generic ODBC catalog functions (`SQLTables`/`SQLColumns`/
//! ...) — those are driver-normalized and lose SQL Server-specific detail
//! (default constraint text, check constraint bodies, view/routine source)
//! that the `sys.*` views expose directly and that `SchemaSnapshot` wants.
//!
//! Every query here filters out engine-internal objects the same two ways:
//! `is_ms_shipped = 0` on the object row, and excluding the `sys` and
//! `INFORMATION_SCHEMA` schemas. This mirrors the postgres driver's
//! `SCHEMA_EXCLUDE` constant and its `fetch_*`/`attach_*` decomposition
//! (`dbc-driver-postgres/src/lib.rs`): one pass builds the table/column
//! skeleton and an object_id/column lookup table, then successive `attach_*`
//! passes fill in PKs, FKs, constraints, indexes, and view DDL by joining
//! back through that lookup.
//!
//! Every query is run through [`run_query_text`], which executes via
//! `Connection::execute` and drains the result with a bulk columnar
//! [`TextRowSet`] block cursor (not row-by-row `next_row`) — catalog result
//! sets are small, so full materialization into `Vec<Vec<Option<String>>>`
//! keeps every `attach_*` function a plain, easily-reviewed loop, at the
//! cost of holding the whole (small) catalog result in memory at once.
//!
//! CAVEAT: these queries are written against documented `sys.*` view shapes
//! but have NOT been run against a live SQL Server instance (no server or
//! ODBC Driver 18 install was available in the environment this crate was
//! authored in — see the ignored integration test `schema_snapshot_smoke`
//! in `tests/mssql_integration.rs`, which is the first thing to run against
//! a real server).

use std::collections::HashMap;

use dbc_core::{
    ColumnInfo, ConstraintInfo, FkRef, IndexInfo, QueryError, RoutineInfo,
    RoutineKind, SchemaSnapshot, TableInfo, TableKind, TriggerInfo, SequenceInfo,
};
use odbc_api::buffers::TextRowSet;
use odbc_api::{Connection, Cursor, ResultSetMetadata};

use crate::types::odbc_err;

/// Excludes engine-internal schemas. Interpolated directly (not
/// parameterized) since it's a fixed literal, same style as the postgres
/// driver's `SCHEMA_EXCLUDE`.
const SCHEMA_EXCLUDE: &str = "s.name NOT IN ('sys', 'INFORMATION_SCHEMA')";

/// Row/column batch size for narrow metadata queries (names, flags, short
/// identifiers) — short values so a generous row count keeps buffers small.
const META_BATCH: usize = 1024;
const META_MAX_STR: usize = 4096;

/// Row/column batch size for definition-heavy queries (view/routine/trigger
/// source text), which can be long — smaller row count so buffer sizing
/// (`batch_size * max_str_limit` per column) stays bounded.
const DEF_BATCH: usize = 64;
const DEF_MAX_STR: usize = 131_072;

/// Runs `sql` (no bind parameters — every query here is a fixed catalog
/// query) and materializes the full result as rows of `Option<String>`
/// cells. A `None` `Connection::execute` result (no result set) yields an
/// empty `Vec`, which no query in this module should hit but which errors
/// downstream cleanly (as "column not found") rather than panicking if it
/// ever did.
fn run_query_text(
    conn: &Connection<'_>,
    sql: &str,
    batch_size: usize,
    max_str_limit: usize,
) -> Result<Vec<Vec<Option<String>>>, QueryError> {
    let mut cursor = match conn.execute(sql, (), None).map_err(odbc_err)? {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let ncols = cursor.num_result_cols().map_err(odbc_err)? as usize;
    let mut buffers =
        TextRowSet::for_cursor(batch_size, &mut cursor, Some(max_str_limit)).map_err(odbc_err)?;
    let mut row_set_cursor = cursor.bind_buffer(&mut buffers).map_err(odbc_err)?;

    let mut out = Vec::new();
    while let Some(batch) = row_set_cursor.fetch().map_err(odbc_err)? {
        for row_index in 0..batch.num_rows() {
            let mut row = Vec::with_capacity(ncols);
            for col_index in 0..ncols {
                let val = match batch.at_as_str(col_index, row_index) {
                    Ok(v) => v.map(|s| s.to_string()),
                    Err(_) => Some("<decode error: invalid utf8>".to_string()),
                };
                row.push(val);
            }
            out.push(row);
        }
    }
    Ok(out)
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).and_then(|v| v.clone()).unwrap_or_default()
}

fn cell_opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).and_then(|v| v.clone())
}

fn cell_bool(row: &[Option<String>], i: usize) -> bool {
    cell(row, i) == "1"
}

fn cell_i64(row: &[Option<String>], i: usize) -> Result<i64, QueryError> {
    cell(row, i)
        .parse::<i64>()
        .map_err(|_| QueryError::msg(format!("expected integer catalog value at column {i}")))
}

/// Lookup from (object_id, column_name) to (table index, column index)
/// inside the `Vec<TableInfo>` being built — mirrors the postgres driver's
/// `ColLookup`.
type ColLookup = HashMap<(i64, String), (usize, usize)>;

pub fn fetch_schema_snapshot(conn: &Connection<'_>) -> Result<SchemaSnapshot, QueryError> {
    let (mut tables, oid_idx, col_idx) = fetch_tables(conn)?;
    attach_pks(conn, &mut tables, &col_idx)?;
    attach_fks(conn, &mut tables, &col_idx)?;
    attach_constraints(conn, &mut tables, &oid_idx)?;
    attach_indexes(conn, &mut tables, &oid_idx)?;
    attach_view_ddl(conn, &mut tables, &oid_idx)?;
    let routines = fetch_routines(conn)?;
    let triggers = fetch_triggers(conn)?;
    let sequences = fetch_sequences(conn)?;
    Ok(SchemaSnapshot { tables, routines, triggers, sequences })
}

/// Tables and views with their columns, in one pass. `sys.tables`/
/// `sys.views` (unioned, tagged `is_view`) joined to `sys.schemas`,
/// `sys.columns`, `sys.types` (for the base type name) and
/// `sys.default_constraints` (column default text). Column `data_type` is
/// rendered with length/precision/scale suffixes for the types where that
/// matters (`varchar(n)`, `decimal(p,s)`, ...); other types render as the
/// bare type name.
fn fetch_tables(
    conn: &Connection<'_>,
) -> Result<(Vec<TableInfo>, HashMap<i64, usize>, ColLookup), QueryError> {
    let sql = format!(
        "SELECT s.name, t.object_id, t.name, t.is_view, c.column_id, c.name,
                CASE
                  WHEN ty.name IN ('varchar','char','varbinary','binary')
                    THEN ty.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')'
                  WHEN ty.name IN ('nvarchar','nchar')
                    THEN ty.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')'
                  WHEN ty.name IN ('decimal','numeric')
                    THEN ty.name + '(' + CAST(c.precision AS varchar(10)) + ',' + CAST(c.scale AS varchar(10)) + ')'
                  WHEN ty.name IN ('datetime2','time','datetimeoffset')
                    THEN ty.name + '(' + CAST(c.scale AS varchar(10)) + ')'
                  ELSE ty.name
                END AS data_type,
                c.is_nullable, dc.definition
         FROM sys.schemas s
         JOIN (
             SELECT object_id, name, schema_id, CAST(0 AS bit) AS is_view FROM sys.tables WHERE is_ms_shipped = 0
             UNION ALL
             SELECT object_id, name, schema_id, CAST(1 AS bit) AS is_view FROM sys.views WHERE is_ms_shipped = 0
         ) t ON t.schema_id = s.schema_id
         JOIN sys.columns c ON c.object_id = t.object_id
         JOIN sys.types ty ON ty.user_type_id = c.user_type_id
         LEFT JOIN sys.default_constraints dc
                ON dc.parent_object_id = t.object_id AND dc.parent_column_id = c.column_id
         WHERE {SCHEMA_EXCLUDE}
         ORDER BY s.name, t.name, c.column_id"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;

    let mut tables: Vec<TableInfo> = Vec::new();
    let mut oid_idx: HashMap<i64, usize> = HashMap::new();
    let mut col_idx: ColLookup = HashMap::new();

    for row in rows {
        let schema = cell(&row, 0);
        let oid = cell_i64(&row, 1)?;
        let name = cell(&row, 2);
        let is_view = cell_bool(&row, 3);
        let col_name = cell(&row, 5);
        let data_type = cell(&row, 6);
        let nullable = cell_bool(&row, 7);
        let default = cell_opt(&row, 8);

        let table_idx = *oid_idx.entry(oid).or_insert_with(|| {
            tables.push(TableInfo {
                schema: Some(schema.clone()),
                name: name.clone(),
                kind: if is_view { TableKind::View } else { TableKind::Table },
                columns: Vec::new(),
                indexes: Vec::new(),
                constraints: Vec::new(),
                ddl: None,
            });
            tables.len() - 1
        });

        let table = &mut tables[table_idx];
        let col_pos = table.columns.len();
        table.columns.push(ColumnInfo {
            name: col_name.clone(),
            data_type,
            nullable,
            default,
            is_pk: false,
            fk: None,
        });
        col_idx.insert((oid, col_name), (table_idx, col_pos));
    }

    Ok((tables, oid_idx, col_idx))
}

/// PKs: `sys.key_constraints` (`type = 'PK'`) joined to `sys.index_columns`
/// via the constraint's backing unique index, ordered by `key_ordinal` so a
/// composite PK's column order is preserved.
fn attach_pks(
    conn: &Connection<'_>,
    tables: &mut [TableInfo],
    col_idx: &ColLookup,
) -> Result<(), QueryError> {
    let sql = format!(
        "SELECT kc.parent_object_id, c.name
         FROM sys.key_constraints kc
         JOIN sys.tables t ON t.object_id = kc.parent_object_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         JOIN sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id
         JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
         WHERE kc.type = 'PK' AND t.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}
         ORDER BY kc.parent_object_id, ic.key_ordinal"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let col_name = cell(&row, 1);
        if let Some(&(t_idx, c_idx)) = col_idx.get(&(oid, col_name)) {
            tables[t_idx].columns[c_idx].is_pk = true;
        }
    }
    Ok(())
}

/// FKs: `sys.foreign_keys` joined to `sys.foreign_key_columns` for the
/// per-column local/referenced pairs, resolved to names via `sys.columns`.
fn attach_fks(
    conn: &Connection<'_>,
    tables: &mut [TableInfo],
    col_idx: &ColLookup,
) -> Result<(), QueryError> {
    let sql = format!(
        "SELECT fk.parent_object_id, pc.name, rs.name, rt.name, rc.name
         FROM sys.foreign_keys fk
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
         JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
         JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
         JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
         JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
         JOIN sys.tables t ON t.object_id = fk.parent_object_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         WHERE {SCHEMA_EXCLUDE}
         ORDER BY fk.parent_object_id, fkc.constraint_column_id"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let col_name = cell(&row, 1);
        let ref_schema = cell(&row, 2);
        let ref_table = cell(&row, 3);
        let ref_col = cell(&row, 4);
        if let Some(&(t_idx, c_idx)) = col_idx.get(&(oid, col_name)) {
            tables[t_idx].columns[c_idx].fk =
                Some(FkRef { schema: Some(ref_schema), table: ref_table, column: ref_col });
        }
    }
    Ok(())
}

/// All constraints per table: PK/UNIQUE from `sys.key_constraints`
/// (`STRING_AGG` over `sys.index_columns` for the ordered column list), FK
/// from `sys.foreign_keys`/`sys.foreign_key_columns`, and CHECK from
/// `sys.check_constraints.definition` (already a parenthesized boolean
/// expression, used as-is).
fn attach_constraints(
    conn: &Connection<'_>,
    tables: &mut [TableInfo],
    oid_idx: &HashMap<i64, usize>,
) -> Result<(), QueryError> {
    // PK / UNIQUE
    let sql = format!(
        "SELECT kc.parent_object_id, kc.name, kc.type,
                (SELECT STRING_AGG(c.name, ', ') WITHIN GROUP (ORDER BY ic.key_ordinal)
                 FROM sys.index_columns ic
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                 WHERE ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id) AS cols
         FROM sys.key_constraints kc
         JOIN sys.tables t ON t.object_id = kc.parent_object_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         WHERE t.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}
         ORDER BY s.name, t.name, kc.name"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let name = cell(&row, 1);
        let ctype = cell(&row, 2);
        let cols = cell(&row, 3);
        let kind = if ctype == "PK" { "PRIMARY KEY" } else { "UNIQUE" };
        if let Some(&t_idx) = oid_idx.get(&oid) {
            tables[t_idx].constraints.push(ConstraintInfo {
                name,
                kind: kind.to_string(),
                definition: format!("{kind} ({cols})"),
            });
        }
    }

    // FOREIGN KEY
    let sql = format!(
        "SELECT fk.parent_object_id, fk.name,
                (SELECT STRING_AGG(pc.name, ', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id)
                 FROM sys.foreign_key_columns fkc
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
                 WHERE fkc.constraint_object_id = fk.object_id) AS local_cols,
                rs.name, rt.name,
                (SELECT STRING_AGG(rc.name, ', ') WITHIN GROUP (ORDER BY fkc2.constraint_column_id)
                 FROM sys.foreign_key_columns fkc2
                 JOIN sys.columns rc ON rc.object_id = fkc2.referenced_object_id AND rc.column_id = fkc2.referenced_column_id
                 WHERE fkc2.constraint_object_id = fk.object_id) AS ref_cols
         FROM sys.foreign_keys fk
         JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
         JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
         JOIN sys.tables t ON t.object_id = fk.parent_object_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         WHERE {SCHEMA_EXCLUDE}
         ORDER BY s.name, t.name, fk.name"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let name = cell(&row, 1);
        let local_cols = cell(&row, 2);
        let ref_schema = cell(&row, 3);
        let ref_table = cell(&row, 4);
        let ref_cols = cell(&row, 5);
        if let Some(&t_idx) = oid_idx.get(&oid) {
            tables[t_idx].constraints.push(ConstraintInfo {
                name,
                kind: "FOREIGN KEY".to_string(),
                definition: format!(
                    "FOREIGN KEY ({local_cols}) REFERENCES {ref_schema}.{ref_table} ({ref_cols})"
                ),
            });
        }
    }

    // CHECK
    let sql = format!(
        "SELECT cc.parent_object_id, cc.name, cc.definition
         FROM sys.check_constraints cc
         JOIN sys.tables t ON t.object_id = cc.parent_object_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         WHERE t.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}
         ORDER BY s.name, t.name, cc.name"
    );
    let rows = run_query_text(conn, &sql, DEF_BATCH, DEF_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let name = cell(&row, 1);
        let definition = cell(&row, 2);
        if let Some(&t_idx) = oid_idx.get(&oid) {
            tables[t_idx].constraints.push(ConstraintInfo {
                name,
                kind: "CHECK".to_string(),
                definition,
            });
        }
    }

    Ok(())
}

/// Non-PK indexes: `sys.indexes` (`is_primary_key = 0`, `index_id > 0` to
/// skip heaps, named indexes only), ordered key columns via `STRING_AGG`
/// over `sys.index_columns` (excluding `INCLUDE`d columns). Unique-
/// constraint-backed indexes are intentionally NOT excluded here (parity
/// with the postgres driver: only PK-backing indexes are skipped, since
/// those are already represented via `ColumnInfo::is_pk`/`ConstraintInfo`).
fn attach_indexes(
    conn: &Connection<'_>,
    tables: &mut [TableInfo],
    oid_idx: &HashMap<i64, usize>,
) -> Result<(), QueryError> {
    let sql = format!(
        "SELECT i.object_id, i.name, i.is_unique,
                (SELECT STRING_AGG(c.name, ', ') WITHIN GROUP (ORDER BY ic.key_ordinal)
                 FROM sys.index_columns ic
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                 WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id
                   AND ic.is_included_column = 0) AS cols
         FROM sys.indexes i
         JOIN sys.tables t ON t.object_id = i.object_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         WHERE i.is_primary_key = 0 AND i.index_id > 0 AND i.name IS NOT NULL
           AND t.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}
         ORDER BY s.name, t.name, i.name"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let name = cell(&row, 1);
        let unique = cell_bool(&row, 2);
        let cols_csv = cell(&row, 3);
        let columns: Vec<String> = cols_csv.split(", ").filter(|s| !s.is_empty()).map(String::from).collect();
        if let Some(&t_idx) = oid_idx.get(&oid) {
            tables[t_idx].indexes.push(IndexInfo { name, columns, unique });
        }
    }
    Ok(())
}

/// View DDL: `sys.sql_modules.definition` (the original `CREATE VIEW ...`
/// text as submitted, including the `CREATE VIEW` header — unlike Postgres
/// there is no separate "reconstruct from catalog" step needed). `NULL`
/// when the view is encrypted (`WITH ENCRYPTION`) or the module row is
/// otherwise absent; `ddl` stays `None` in that case, same as a plain table.
fn attach_view_ddl(
    conn: &Connection<'_>,
    tables: &mut [TableInfo],
    oid_idx: &HashMap<i64, usize>,
) -> Result<(), QueryError> {
    let sql = format!(
        "SELECT v.object_id, sm.definition
         FROM sys.views v
         JOIN sys.schemas s ON s.schema_id = v.schema_id
         LEFT JOIN sys.sql_modules sm ON sm.object_id = v.object_id
         WHERE v.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}"
    );
    let rows = run_query_text(conn, &sql, DEF_BATCH, DEF_MAX_STR)?;
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let def = cell_opt(&row, 1);
        if let (Some(&t_idx), Some(def)) = (oid_idx.get(&oid), def) {
            tables[t_idx].ddl = Some(def);
        }
    }
    Ok(())
}

/// Routines: `sys.objects` filtered to `type IN ('P','FN','TF','IF')`
/// (procedures, scalar functions, table-valued functions, inline
/// table-valued functions). Signature lists declared parameters (name +
/// base type) from `sys.parameters`, skipping the `parameter_id = 0` return
/// slot; DDL comes from `OBJECT_DEFINITION`, which returns `NULL` for
/// `WITH ENCRYPTION` routines — tolerated as `None` rather than an error,
/// same as the view-DDL path.
fn fetch_routines(conn: &Connection<'_>) -> Result<Vec<RoutineInfo>, QueryError> {
    let sql = format!(
        "SELECT o.object_id, s.name, o.name, o.type
         FROM sys.objects o
         JOIN sys.schemas s ON s.schema_id = o.schema_id
         WHERE o.type IN ('P','FN','TF','IF') AND o.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}
         ORDER BY s.name, o.name"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;

    let param_sql = format!(
        "SELECT p.object_id, p.name, ty.name
         FROM sys.parameters p
         JOIN sys.types ty ON ty.user_type_id = p.user_type_id
         JOIN sys.objects o ON o.object_id = p.object_id
         JOIN sys.schemas s ON s.schema_id = o.schema_id
         WHERE p.parameter_id > 0 AND o.type IN ('P','FN','TF','IF') AND o.is_ms_shipped = 0
           AND {SCHEMA_EXCLUDE}
         ORDER BY p.object_id, p.parameter_id"
    );
    let param_rows = run_query_text(conn, &param_sql, META_BATCH, META_MAX_STR)?;
    let mut params_by_oid: HashMap<i64, Vec<String>> = HashMap::new();
    for row in param_rows {
        let oid = cell_i64(&row, 0)?;
        let name = cell(&row, 1);
        let ty = cell(&row, 2);
        params_by_oid.entry(oid).or_default().push(format!("{name} {ty}"));
    }

    let def_sql = format!(
        "SELECT o.object_id, OBJECT_DEFINITION(o.object_id)
         FROM sys.objects o
         JOIN sys.schemas s ON s.schema_id = o.schema_id
         WHERE o.type IN ('P','FN','TF','IF') AND o.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}"
    );
    let def_rows = run_query_text(conn, &def_sql, DEF_BATCH, DEF_MAX_STR)?;
    let mut ddl_by_oid: HashMap<i64, Option<String>> = HashMap::new();
    for row in def_rows {
        let oid = cell_i64(&row, 0)?;
        ddl_by_oid.insert(oid, cell_opt(&row, 1));
    }

    let mut routines = Vec::with_capacity(rows.len());
    for row in rows {
        let oid = cell_i64(&row, 0)?;
        let schema = cell(&row, 1);
        let name = cell(&row, 2);
        let obj_type = cell(&row, 3);
        let kind = if obj_type.trim() == "P" { RoutineKind::Procedure } else { RoutineKind::Function };
        let params = params_by_oid.get(&oid).cloned().unwrap_or_default();
        let signature = format!("({})", params.join(", "));
        let ddl = ddl_by_oid.get(&oid).cloned().flatten();
        routines.push(RoutineInfo { schema: Some(schema), name, kind, signature, ddl });
    }
    Ok(routines)
}

/// Triggers: `sys.triggers` (table/view DML triggers; excludes `is_ms_shipped`
/// ones) joined back to the parent table and `sys.sql_modules` for source.
fn fetch_triggers(conn: &Connection<'_>) -> Result<Vec<TriggerInfo>, QueryError> {
    let sql = format!(
        "SELECT s.name, tr.name, t.name, sm.definition
         FROM sys.triggers tr
         JOIN sys.tables t ON t.object_id = tr.parent_id
         JOIN sys.schemas s ON s.schema_id = t.schema_id
         LEFT JOIN sys.sql_modules sm ON sm.object_id = tr.object_id
         WHERE tr.is_ms_shipped = 0 AND {SCHEMA_EXCLUDE}
         ORDER BY s.name, t.name, tr.name"
    );
    let rows = run_query_text(conn, &sql, DEF_BATCH, DEF_MAX_STR)?;
    let mut triggers = Vec::with_capacity(rows.len());
    for row in rows {
        let schema = cell(&row, 0);
        let name = cell(&row, 1);
        let table = cell(&row, 2);
        let ddl = cell_opt(&row, 3);
        triggers.push(TriggerInfo { schema: Some(schema), name, table, ddl });
    }
    Ok(triggers)
}

/// Sequences: `sys.sequences`.
fn fetch_sequences(conn: &Connection<'_>) -> Result<Vec<SequenceInfo>, QueryError> {
    let sql = format!(
        "SELECT s.name, sq.name
         FROM sys.sequences sq
         JOIN sys.schemas s ON s.schema_id = sq.schema_id
         WHERE {SCHEMA_EXCLUDE}
         ORDER BY s.name, sq.name"
    );
    let rows = run_query_text(conn, &sql, META_BATCH, META_MAX_STR)?;
    let mut sequences = Vec::with_capacity(rows.len());
    for row in rows {
        let schema = cell(&row, 0);
        let name = cell(&row, 1);
        sequences.push(SequenceInfo { schema: Some(schema), name });
    }
    Ok(sequences)
}
