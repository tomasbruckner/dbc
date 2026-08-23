use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, ConstraintInfo, FkRef, IndexInfo, QueryError, QueryStream,
    SchemaSnapshot, SequenceInfo, TableInfo, TableKind, BATCH_ROWS, CHANNEL_CAPACITY,
};
use duckdb::types::{Value, ValueRef};

pub struct DuckdbConnection {
    path: PathBuf,
    read_only: bool,
    /// Lazily-opened connection reused across [`Connection::execute`] calls
    /// so a `BEGIN … COMMIT`/`ROLLBACK` sequence runs over one underlying
    /// DuckDB handle rather than being silently split across separate
    /// connections (which would drop the in-progress transaction). Taken out
    /// of the `Option` and moved into `spawn_blocking`, then put back — safe
    /// because `execute` takes `&mut self`, so there is never concurrent
    /// access. `query`/`schema` are unaffected and keep opening a fresh
    /// connection per call, as before. Mirrors
    /// `dbc-driver-sqlite::SqliteConnection::exec_conn`.
    exec_conn: Option<duckdb::Connection>,
}

impl DuckdbConnection {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), read_only: false, exec_conn: None }
    }

    /// Like [`DuckdbConnection::new`], but opens the database with DuckDB's
    /// `access_mode = READ_ONLY` config (instead of the default `AUTOMATIC`)
    /// when `read_only` is set — server-side enforcement of a connection's
    /// read-only flag, so a client-side guard bypass can't still mutate the
    /// file. Mirrors sqlite's `SQLITE_OPEN_READ_ONLY` swap in
    /// `SqliteConnection::new_with_options`. See dbc-ui's `connect::open_config`,
    /// which selects this constructor when `ConnectionConfig::read_only` is
    /// set.
    pub fn new_with_options(path: impl Into<PathBuf>, read_only: bool) -> Self {
        Self { path: path.into(), read_only, exec_conn: None }
    }
}

fn q_err(e: duckdb::Error) -> QueryError {
    QueryError { code: None, message: e.to_string(), position: None }
}

/// `duckdb::Connection::open` mirrored with `AccessMode::ReadOnly` swapped in
/// for the default `Automatic` access mode when `read_only` is set.
fn open_conn(path: &std::path::Path, read_only: bool) -> duckdb::Result<duckdb::Connection> {
    if read_only {
        let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
        duckdb::Connection::open_with_flags(path, config)
    } else {
        duckdb::Connection::open(path)
    }
}

/// Formats a DuckDB `DATE32` (days since 1970-01-01) as `YYYY-MM-DD`, via
/// Howard Hinnant's `civil_from_days` — self-contained so this crate doesn't
/// need a chrono dependency (see the ground rules in the driver's task doc:
/// dbc-core, dbc-buffer, tokio, duckdb only).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn format_date32(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a count of microseconds-since-midnight as `HH:MM:SS[.ffffff]`.
fn format_time_of_day(micros: i64) -> String {
    let micros = micros.rem_euclid(86_400_000_000);
    let us = micros % 1_000_000;
    let total_secs = micros / 1_000_000;
    let s = total_secs % 60;
    let m = (total_secs / 60) % 60;
    let h = total_secs / 3600;
    if us == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{us:06}")
    }
}

fn format_timestamp(micros_since_epoch: i64) -> String {
    let days = micros_since_epoch.div_euclid(86_400_000_000);
    let time_micros = micros_since_epoch.rem_euclid(86_400_000_000);
    format!("{} {}", format_date32(days as i32), format_time_of_day(time_micros))
}

/// Converts a single cell to its text representation for the UI grid, or
/// `None` for SQL NULL. Mirrors `dbc-driver-sqlite::value_to_text`: scalar
/// types get a natural rendering (numbers via `to_string()`; dates/times/
/// timestamps reconstructed from DuckDB's epoch-based integer encodings
/// above); blobs get a size placeholder like sqlite's `<blob N B>`. Compound
/// types (LIST/STRUCT/MAP/ARRAY/UNION/INTERVAL) fall back to `ValueRef`'s
/// `Debug` rendering as a decode-error-style placeholder — accurate but not
/// pretty-printed as SQL, which is an acceptable tradeoff for a preview grid
/// cell that most query results will never hit.
fn value_to_text(v: ValueRef<'_>) -> Option<String> {
    use ValueRef::*;
    match v {
        Null => None,
        // `as_str` handles both Text and Enum (dictionary-decoded) values.
        Text(_) | Enum(..) => v.as_str().ok().map(|s| s.to_string()),
        Boolean(b) => Some(b.to_string()),
        TinyInt(i) => Some(i.to_string()),
        SmallInt(i) => Some(i.to_string()),
        Int(i) => Some(i.to_string()),
        BigInt(i) => Some(i.to_string()),
        HugeInt(i) => Some(i.to_string()),
        UTinyInt(i) => Some(i.to_string()),
        USmallInt(i) => Some(i.to_string()),
        UInt(i) => Some(i.to_string()),
        UBigInt(i) => Some(i.to_string()),
        Float(f) => Some(f.to_string()),
        Double(f) => Some(f.to_string()),
        Decimal(d) => Some(d.to_string()),
        Blob(b) => Some(format!("<blob {} B>", b.len())),
        Date32(days) => Some(format_date32(days)),
        Time64(unit, t) => Some(format_time_of_day(unit.to_micros(t))),
        Timestamp(unit, t) => Some(format_timestamp(unit.to_micros(t))),
        other => Some(format!("{other:?}")),
    }
}

/// Decodes a `duckdb_constraints()`/`duckdb_indexes()`-style LIST(VARCHAR)
/// value (e.g. `constraint_column_names`) into a plain `Vec<String>`.
/// `duckdb-rs` has no generic `FromSql` for `Vec<String>` (only `Vec<u8>` for
/// blobs), so this goes through the owned `Value::List` representation by
/// hand.
fn list_of_strings(v: ValueRef<'_>) -> Vec<String> {
    match v.to_owned() {
        Value::List(items) => items
            .into_iter()
            .map(|it| match it {
                Value::Text(s) => s,
                other => format!("{other:?}"),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Parses DuckDB's `duckdb_indexes().expressions` column: a bracketed,
/// comma-separated textual rendering of the index's key expressions (e.g.
/// `[a, b]` for plain columns, `['"weird col"', a]` when an identifier needs
/// quoting) — empirically confirmed against DuckDB 1.10504.0, since
/// `duckdb_indexes()` has no structured column-list field (unlike sqlite's
/// `PRAGMA index_info` or postgres's `pg_index.indkey`). Simple top-level
/// split on `", "`; an expression containing a literal `", "` (e.g. a
/// function call with multiple arguments) would misparse — accepted
/// limitation, same spirit as the postgres driver's expression-index
/// handling.
fn parse_index_expressions(expr: &str) -> Vec<String> {
    let trimmed = expr.trim();
    let inner = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(trimmed);
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(", ")
        .map(|tok| {
            let tok = tok.trim();
            let tok = tok.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(tok);
            let tok = tok.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(tok);
            tok.replace("\"\"", "\"")
        })
        .collect()
}

#[async_trait]
impl Connection for DuckdbConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (schema_tx, schema_rx) = tokio::sync::oneshot::channel::<Result<SchemaRef, QueryError>>();
        let path = self.path.clone();
        let read_only = self.read_only;
        let sql = sql.to_owned();
        let handle = tokio::runtime::Handle::current();

        tokio::task::spawn_blocking(move || {
            let conn = match open_conn(&path, read_only) {
                Ok(c) => c,
                Err(e) => {
                    let _ = schema_tx.send(Err(q_err(e)));
                    return;
                }
            };
            // Watcher: protocol-level interrupt when the token fires.
            // Unlike sqlite's per-statement interrupt handle, DuckDB's
            // `interrupt_handle()` is tied to the `Connection` and applies
            // to whatever statement is currently executing on it — obtained
            // up front, before `prepare`/`query`, same effect either way
            // since this connection is used for exactly one query.
            let interrupt = conn.interrupt_handle();
            let watcher_cancel = cancel.clone();
            let watcher = handle.spawn(async move {
                watcher_cancel.cancelled().await;
                interrupt.interrupt();
            });

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => {
                    let _ = schema_tx.send(Err(q_err(e)));
                    watcher.abort();
                    return;
                }
            };

            // Unlike rusqlite, DuckDB's column metadata (`Statement::
            // column_names`) panics unless the statement has already been
            // executed at least once — so `query()` doubles as both "run
            // the statement" and "learn its shape" here. It also runs the
            // statement to completion (DuckDB materializes the Arrow result
            // before returning), so a cancel fired while this call is
            // blocking surfaces right here as an `Err`, not later in the
            // row loop below.
            let mut rows = match stmt.query([]) {
                Ok(r) => r,
                Err(e) => {
                    let err = if cancel.is_cancelled() {
                        QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
                    } else {
                        q_err(e)
                    };
                    let _ = schema_tx.send(Err(err));
                    watcher.abort();
                    return;
                }
            };

            let col_names = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
            let schema: SchemaRef = Arc::new(Schema::new(
                col_names.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect::<Vec<_>>(),
            ));
            let _ = schema_tx.send(Ok(schema.clone()));

            let ncols = col_names.len();
            let mut builders: Vec<StringBuilder> = (0..ncols).map(|_| StringBuilder::new()).collect();
            let mut in_batch = 0usize;

            let flush = |builders: &mut Vec<StringBuilder>| -> RecordBatch {
                let arrays: Vec<ArrayRef> =
                    builders.iter_mut().map(|b| Arc::new(b.finish()) as ArrayRef).collect();
                RecordBatch::try_new(schema.clone(), arrays).expect("schema matches builders")
            };

            loop {
                // Cooperative check, same rationale as sqlite's: even though
                // the query already fully ran by the time we get here (see
                // above), checking between rows keeps this loop consistent
                // with the other drivers and costs nothing.
                if cancel.is_cancelled() {
                    let _ = tx.blocking_send(Err(QueryError {
                        code: Some("cancelled".into()),
                        message: "query cancelled".into(),
                        position: None,
                    }));
                    break;
                }
                match rows.next() {
                    Ok(Some(row)) => {
                        for (i, b) in builders.iter_mut().enumerate() {
                            match row.get_ref(i).ok().and_then(value_to_text) {
                                Some(s) => b.append_value(s),
                                None => b.append_null(),
                            }
                        }
                        in_batch += 1;
                        if in_batch >= BATCH_ROWS {
                            if tx.blocking_send(Ok(flush(&mut builders))).is_err() {
                                break;
                            }
                            in_batch = 0;
                        }
                    }
                    Ok(None) => {
                        if in_batch > 0 {
                            let _ = tx.blocking_send(Ok(flush(&mut builders)));
                        }
                        break;
                    }
                    Err(e) => {
                        let err = if cancel.is_cancelled() {
                            QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
                        } else {
                            q_err(e)
                        };
                        let _ = tx.blocking_send(Err(err));
                        break;
                    }
                }
            }
            watcher.abort();
        });

        let columns = schema_rx.await.map_err(|_| QueryError::msg("driver task died"))??;
        Ok(QueryStream { columns, batches: rx })
    }

    /// Executes a non-returning statement over a connection kept open across
    /// calls (see [`DuckdbConnection::exec_conn`]), so `BEGIN … COMMIT`/
    /// `ROLLBACK` sequences issued via successive `execute` calls run over
    /// the same DuckDB handle. `duckdb::Connection::execute` returns the
    /// changed-row count directly for DML (`0` for `BEGIN`/`COMMIT`/DDL).
    ///
    /// Engine divergence (empirically verified against DuckDB 1.10504.0, see
    /// the `mid_transaction_error_aborts_like_postgres` test below): after a
    /// failed statement inside an open transaction, DuckDB — like
    /// PostgreSQL, unlike SQLite — invalidates the transaction. Every
    /// further statement fails (DuckDB reports "current transaction is
    /// aborted" verbatim, same message text as Postgres) until `ROLLBACK`.
    /// Callers must follow the same stop-at-first-error-and-roll-back
    /// discipline the trait doc mandates for Postgres.
    ///
    /// Locking note (also empirically verified): unlike sqlite, DuckDB does
    /// not let two independently-`open()`ed `duckdb::Connection`s share the
    /// same database file within one process — each `open()` takes its own
    /// exclusive file lock, so a second `open()` of the same path while this
    /// `exec_conn` is alive fails outright on Windows ("file is already open
    /// in this process"). This is consistent with the trait doc's existing
    /// requirement that `execute`'s BEGIN…COMMIT sequence run over a
    /// DEDICATED connection not interleaved with `query()`/`schema()` calls
    /// on another instance pointed at the same file — for DuckDB that
    /// requirement is load-bearing, not just a best practice.
    async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError> {
        if cancel.is_cancelled() {
            return Err(QueryError {
                code: Some("cancelled".into()),
                message: "query cancelled".into(),
                position: None,
            });
        }
        let path = self.path.clone();
        let read_only = self.read_only;
        let conn = match self.exec_conn.take() {
            Some(c) => c,
            None => open_conn(&path, read_only).map_err(q_err)?,
        };
        let sql = sql.to_owned();
        let (result, conn) = tokio::task::spawn_blocking(move || {
            let result = conn.execute(&sql, []).map(|n| n as u64).map_err(q_err);
            (result, conn)
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?;
        self.exec_conn = Some(conn);
        result
    }

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let path = self.path.clone();
        let read_only = self.read_only;
        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path, read_only).map_err(q_err)?;
            let mut tables: Vec<TableInfo> = Vec::new();
            // DuckDB catalog object ids (table_oid/view_oid) are unique
            // across object kinds, so tables and views share one lookup.
            let mut oid_idx: HashMap<i64, usize> = HashMap::new();

            // Base tables. `internal`/`temporary` exclude DuckDB's own
            // catalog/system objects and session-local TEMP tables — the
            // DuckDB analogue of the sqlite driver's `sqlite_` prefix skip
            // and the postgres driver's `SCHEMA_EXCLUDE`.
            let mut stmt = conn
                .prepare(
                    "SELECT schema_name, table_name, table_oid, sql FROM duckdb_tables() \
                     WHERE NOT internal AND NOT temporary ORDER BY schema_name, table_name",
                )
                .map_err(q_err)?;
            let rows: Vec<(String, String, i64, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (schema, name, oid, sql) in rows {
                oid_idx.insert(oid, tables.len());
                tables.push(TableInfo {
                    schema: Some(schema),
                    name,
                    kind: TableKind::Table,
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    ddl: sql,
                });
            }

            // Views: DDL comes straight from `duckdb_views().sql` (DuckDB
            // echoes the original CREATE VIEW text), unlike Postgres, which
            // has no server-side "get view DDL" and needs
            // `pg_get_viewdef` reconstruction.
            let mut stmt = conn
                .prepare(
                    "SELECT schema_name, view_name, view_oid, sql FROM duckdb_views() \
                     WHERE NOT internal AND NOT temporary ORDER BY schema_name, view_name",
                )
                .map_err(q_err)?;
            let rows: Vec<(String, String, i64, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (schema, name, oid, sql) in rows {
                oid_idx.insert(oid, tables.len());
                tables.push(TableInfo {
                    schema: Some(schema),
                    name,
                    kind: TableKind::View,
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    ddl: sql,
                });
            }

            // Columns, in `column_index` order (the `ORDER BY` below makes
            // push order == declaration order, same trick sqlite's PRAGMA
            // loop relies on via `pk` sequence numbers).
            let mut stmt = conn
                .prepare(
                    "SELECT table_oid, column_name, data_type, is_nullable, column_default \
                     FROM duckdb_columns() WHERE NOT internal ORDER BY table_oid, column_index",
                )
                .map_err(q_err)?;
            let rows: Vec<(i64, String, String, bool, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (oid, name, data_type, nullable, default) in rows {
                if let Some(&t_idx) = oid_idx.get(&oid) {
                    tables[t_idx].columns.push(ColumnInfo {
                        name,
                        data_type,
                        nullable,
                        default,
                        is_pk: false,
                        fk: None,
                    });
                }
            }

            // Constraints: PRIMARY KEY / FOREIGN KEY / UNIQUE / CHECK.
            // `duckdb_constraints()` also emits one "NOT NULL" row per
            // NOT-NULL column; skipped here since `ColumnInfo::nullable`
            // above already captures it and it isn't a named/listable
            // constraint the UI would want to show.
            //
            // Target schema for FKs is left `None`: `duckdb_constraints()`
            // exposes `referenced_table` but no `referenced_schema`, and
            // guessing "same schema as the referencing table" would be
            // faking data DuckDB doesn't actually give us (unlike sqlite,
            // where there IS no cross-schema concept, so `None` there is
            // simply correct rather than a guess).
            let mut stmt = conn
                .prepare(
                    "SELECT table_oid, constraint_type, constraint_text, constraint_column_names, \
                            constraint_name, referenced_table, referenced_column_names \
                     FROM duckdb_constraints()",
                )
                .map_err(q_err)?;
            let mut rows = stmt.query([]).map_err(q_err)?;
            while let Some(row) = rows.next().map_err(q_err)? {
                let oid: i64 = row.get(0).map_err(q_err)?;
                let Some(&t_idx) = oid_idx.get(&oid) else { continue };
                let ctype: String = row.get(1).map_err(q_err)?;
                let ctext: String = row.get(2).map_err(q_err)?;
                let cols = list_of_strings(row.get_ref(3).map_err(q_err)?);
                let cname: Option<String> = row.get(4).map_err(q_err)?;
                let rtable: Option<String> = row.get(5).map_err(q_err)?;
                let rcols = list_of_strings(row.get_ref(6).map_err(q_err)?);

                match ctype.as_str() {
                    "NOT NULL" => continue,
                    "PRIMARY KEY" => {
                        for col_name in &cols {
                            if let Some(c) =
                                tables[t_idx].columns.iter_mut().find(|c| &c.name == col_name)
                            {
                                c.is_pk = true;
                            }
                        }
                        tables[t_idx].constraints.push(ConstraintInfo {
                            name: cname.unwrap_or_default(),
                            kind: "PRIMARY KEY".to_string(),
                            definition: ctext,
                        });
                    }
                    "FOREIGN KEY" => {
                        if let Some(rtable) = &rtable {
                            for (col_name, r_col) in cols.iter().zip(rcols.iter()) {
                                if let Some(c) =
                                    tables[t_idx].columns.iter_mut().find(|c| &c.name == col_name)
                                {
                                    c.fk = Some(FkRef {
                                        schema: None,
                                        table: rtable.clone(),
                                        column: r_col.clone(),
                                    });
                                }
                            }
                        }
                        tables[t_idx].constraints.push(ConstraintInfo {
                            name: cname.unwrap_or_default(),
                            kind: "FOREIGN KEY".to_string(),
                            definition: ctext,
                        });
                    }
                    other => {
                        tables[t_idx].constraints.push(ConstraintInfo {
                            name: cname.unwrap_or_default(),
                            kind: other.to_string(),
                            definition: ctext,
                        });
                    }
                }
            }

            // Secondary indexes. `duckdb_indexes()` has no structured
            // column-list field (only `sql` and a textual `expressions`
            // rendering) and never lists the hidden index backing a PRIMARY
            // KEY (see `parse_index_expressions` doc comment and
            // `is_primary` in DuckDB's own docs, "always false") — so no
            // PK-backing-index filter is needed here, unlike postgres's
            // `NOT i.indisprimary`.
            let mut stmt = conn
                .prepare("SELECT table_oid, index_name, is_unique, expressions FROM duckdb_indexes()")
                .map_err(q_err)?;
            let rows: Vec<(i64, String, bool, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (oid, name, unique, expr) in rows {
                if let Some(&t_idx) = oid_idx.get(&oid) {
                    let columns = expr.as_deref().map(parse_index_expressions).unwrap_or_default();
                    tables[t_idx].indexes.push(IndexInfo { name, columns, unique });
                }
            }

            // Sequences: schema-level objects, not attached to any table.
            let mut stmt = conn
                .prepare(
                    "SELECT schema_name, sequence_name FROM duckdb_sequences() WHERE NOT temporary \
                     ORDER BY schema_name, sequence_name",
                )
                .map_err(q_err)?;
            let sequences: Vec<SequenceInfo> = stmt
                .query_map([], |r| Ok(SequenceInfo { schema: Some(r.get(0)?), name: r.get(1)? }))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;

            // DuckDB has no `CREATE TRIGGER` support at all, and routine/
            // macro introspection (`duckdb_functions()`, `CREATE MACRO`) is
            // not implemented — left empty rather than faking data, per the
            // task's "leave genuinely-unavailable collections empty" rule.
            Ok(SchemaSnapshot { tables, routines: Vec::new(), triggers: Vec::new(), sequences })
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{CancelToken, Connection};

    fn fixture_db() -> tempfile::TempPath {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        // DuckDB refuses to open a path that already exists as an empty
        // file with unexpected content in some versions; remove it first so
        // `Connection::open` creates a fresh database at that path.
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER, name TEXT);").unwrap();
        let mut stmt = conn.prepare("INSERT INTO t(id, name) VALUES (?, ?)").unwrap();
        for value in 1..=5000i64 {
            stmt.execute(duckdb::params![value, format!("n{value}")]).unwrap();
        }
        path
    }

    #[tokio::test]
    async fn streams_all_rows_in_batches() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let mut s = c.query("SELECT id, name FROM t ORDER BY id", CancelToken::new()).await.unwrap();
        assert_eq!(s.columns.fields().len(), 2);
        assert_eq!(s.columns.field(0).name(), "id");
        let mut rows = 0usize;
        let mut batches = 0usize;
        while let Some(b) = s.batches.recv().await {
            let b = b.unwrap();
            rows += b.num_rows();
            batches += 1;
        }
        assert_eq!(rows, 5000);
        assert!(batches >= 4, "expected multiple 1024-row batches, got {batches}");
    }

    #[tokio::test]
    async fn null_values_round_trip_as_none() {
        let f = fixture_db();
        let conn = duckdb::Connection::open(&*f).unwrap();
        conn.execute_batch("INSERT INTO t(id, name) VALUES (9999, NULL)").unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&*f);
        let mut s = c.query("SELECT name FROM t WHERE id = 9999", CancelToken::new()).await.unwrap();
        use dbc_core::arrow::array::Array;
        let mut saw_null = false;
        while let Some(b) = s.batches.recv().await {
            let b = b.unwrap();
            let col = b.column(0).as_any().downcast_ref::<dbc_core::arrow::array::StringArray>().unwrap();
            for i in 0..col.len() {
                if col.is_null(i) {
                    saw_null = true;
                }
            }
        }
        assert!(saw_null, "expected a NULL name cell");
    }

    #[tokio::test]
    async fn sql_error_is_a_value() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let err = match c.query("SELECT * FROM missing_table", CancelToken::new()).await {
            Ok(_) => panic!("expected an error querying a missing table"),
            Err(e) => e,
        };
        assert!(err.message.to_lowercase().contains("missing_table"));
    }

    #[tokio::test]
    async fn cancel_interrupts_long_query() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let cancel = CancelToken::new();
        let watcher_cancel = cancel.clone();
        // DuckDB runs `query()` to completion before returning control (see
        // the `Connection::query` doc comment), so cancellation has to land
        // WHILE that call is still in flight rather than after — spawn the
        // cancel from a concurrent task instead of calling it inline like
        // sqlite's equivalent test does.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            watcher_cancel.cancel();
        });
        // Triple cross join of a 5000-row table = 125e9 rows; must not
        // complete within the 200ms head start above.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            c.query("SELECT a.id FROM t a, t b, t c", cancel),
        )
        .await
        .expect("query was not interrupted within 30s");

        let err = match result {
            Ok(_) => panic!("expected the interrupted query to fail"),
            Err(e) => e,
        };
        assert_eq!(err.code.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn schema_lists_tables_and_columns() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let snap = c.schema().await.unwrap();
        let t = snap.tables.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[0].name, "id");
    }

    #[tokio::test]
    async fn schema_handles_reserved_word_and_spaced_table_names() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"order\" (id INTEGER, total DOUBLE);
             CREATE TABLE \"weird name\" (a TEXT, b TEXT);",
        )
        .unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&path);
        let snap = c.schema().await.unwrap();

        let order = snap.tables.iter().find(|t| t.name == "order").unwrap();
        assert_eq!(order.columns.len(), 2);
        assert_eq!(order.columns[0].name, "id");
        assert_eq!(order.columns[1].name, "total");

        let weird = snap.tables.iter().find(|t| t.name == "weird name").unwrap();
        assert_eq!(weird.columns.len(), 2);
        assert_eq!(weird.columns[0].name, "a");
        assert_eq!(weird.columns[1].name, "b");
    }

    #[tokio::test]
    async fn read_only_connection_allows_select() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new_with_options(&*f, true);
        let mut s = c
            .query("SELECT id FROM t ORDER BY id LIMIT 1", CancelToken::new())
            .await
            .unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn read_only_connection_rejects_writes() {
        // Server-side enforcement, mirroring sqlite's
        // `read_only_connection_rejects_writes`: `AccessMode::ReadOnly` must
        // reject a write regardless of any client-side `is_read_statement`
        // guard.
        let f = fixture_db();
        let mut c = DuckdbConnection::new_with_options(&*f, true);
        let mut s = match c.query("INSERT INTO t(id, name) VALUES (9999, 'x')", CancelToken::new()).await {
            Ok(s) => s,
            Err(e) => {
                assert!(e.message.to_lowercase().contains("read"), "expected a read-only error, got: {}", e.message);
                return;
            }
        };
        let mut saw_error = false;
        while let Some(item) = s.batches.recv().await {
            if let Err(e) = item {
                assert!(e.message.to_lowercase().contains("read"), "expected a read-only error, got: {}", e.message);
                saw_error = true;
            }
        }
        assert!(saw_error, "expected the write to be rejected by the read-only connection");
    }

    #[tokio::test]
    async fn read_only_connection_rejects_execute_writes() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut w = DuckdbConnection::new(&path);
            w.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        }
        let mut c = DuckdbConnection::new_with_options(&path, true);
        let err = c
            .execute("INSERT INTO t(id) VALUES (1)", CancelToken::new())
            .await
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("readonly") || msg.contains("read-only") || msg.contains("read only"),
            "expected a read-only rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn full_catalog() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE customers(id INTEGER PRIMARY KEY, name VARCHAR NOT NULL DEFAULT 'x');
             CREATE TABLE orders(id INTEGER PRIMARY KEY, cid INTEGER NOT NULL REFERENCES customers(id));
             CREATE INDEX idx_orders_cid ON orders(cid);
             CREATE VIEW v_orders AS SELECT id FROM orders;
             CREATE SEQUENCE seq_test;",
        )
        .unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&path);
        let snap = c.schema().await.unwrap();

        assert!(snap.tables.len() >= 2, "expected at least 2 tables, got {}", snap.tables.len());

        let customers = snap.tables.iter().find(|t| t.name == "customers").unwrap();
        assert_eq!(customers.kind, TableKind::Table);
        assert_eq!(customers.columns.len(), 2);
        let id_col = customers.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id_col.is_pk);
        let name_col = customers.columns.iter().find(|c| c.name == "name").unwrap();
        assert!(!name_col.nullable);
        assert!(customers.ddl.is_some());
        assert!(customers.ddl.as_ref().unwrap().to_uppercase().contains("CREATE TABLE"));

        let orders = snap.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.kind, TableKind::Table);
        let cid_col = orders.columns.iter().find(|c| c.name == "cid").unwrap();
        assert!(!cid_col.nullable);
        assert!(cid_col.fk.is_some());
        let fk = cid_col.fk.as_ref().unwrap();
        assert_eq!(fk.table, "customers");
        assert_eq!(fk.column, "id");

        assert!(!orders.indexes.is_empty());
        let idx = orders.indexes.iter().find(|i| i.name == "idx_orders_cid").unwrap();
        assert_eq!(idx.columns, vec!["cid"]);
        assert!(!idx.unique);

        let v_orders = snap.tables.iter().find(|t| t.name == "v_orders").unwrap();
        assert_eq!(v_orders.kind, TableKind::View);
        assert_eq!(v_orders.columns.len(), 1);
        assert!(v_orders.ddl.is_some());
        assert!(v_orders.ddl.as_ref().unwrap().to_uppercase().contains("CREATE VIEW"));

        assert!(snap.sequences.iter().any(|s| s.name == "seq_test"));
    }

    #[tokio::test]
    async fn execute_reports_affected_rows() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let mut c = DuckdbConnection::new(&path);

        let n = c
            .execute("CREATE TABLE t(id INTEGER, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 0);

        let n = c
            .execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 1);
        let n = c
            .execute("INSERT INTO t(id, name) VALUES (2, 'b')", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 1);

        let n = c.execute("UPDATE t SET name = 'z'", CancelToken::new()).await.unwrap();
        assert_eq!(n, 2);

        let n = c
            .execute("DELETE FROM t WHERE id = 9999", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn execute_in_transaction_rolls_back() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut c = DuckdbConnection::new(&path);
            c.execute("CREATE TABLE t(id INTEGER, name TEXT)", CancelToken::new()).await.unwrap();

            c.execute("BEGIN", CancelToken::new()).await.unwrap();
            c.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();
            c.execute("ROLLBACK", CancelToken::new()).await.unwrap();
            // `c` (and its persistent `exec_conn`) is dropped at the end of
            // this block. DuckDB holds an exclusive file lock for as long as
            // any `Connection` to the file is open (see the locking note on
            // `Connection::execute` above), so the verification query below
            // needs a connection of its own, opened after this one closes.
        }

        let mut verify = DuckdbConnection::new(&path);
        // The insert must not be visible — same underlying connection must
        // have been used for BEGIN/INSERT/ROLLBACK for the rollback to take
        // effect; a fresh connection per call would have auto-committed the
        // INSERT before ROLLBACK ever ran.
        let mut s = verify.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 0, "row inserted inside the rolled-back transaction must be absent");
    }

    #[tokio::test]
    async fn execute_in_transaction_commits() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut c = DuckdbConnection::new(&path);
            c.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();

            c.execute("BEGIN", CancelToken::new()).await.unwrap();
            c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();
            c.execute("COMMIT", CancelToken::new()).await.unwrap();
        }

        let mut verify = DuckdbConnection::new(&path);
        let mut s = verify.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 1);
    }

    /// T1-review-style engine divergence proof (see the `execute` doc
    /// comment on the trait impl above): unlike SQLite, DuckDB aborts an
    /// open transaction on the first error — every subsequent statement in
    /// that transaction fails until `ROLLBACK`, matching Postgres's
    /// behavior rather than SQLite's "transaction stays open and usable"
    /// one. Empirically confirmed against DuckDB 1.10504.0.
    #[tokio::test]
    async fn mid_transaction_error_aborts_like_postgres() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let mut c = DuckdbConnection::new(&path);
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();

        c.execute("BEGIN", CancelToken::new()).await.unwrap();
        c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();

        // Duplicate primary key: the first error inside the transaction.
        let first_error = c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await;
        assert!(first_error.is_err(), "expected the duplicate-key insert to fail");

        // A perfectly valid statement, issued on the SAME connection right
        // after the error: DuckDB rejects it too, because the transaction
        // itself is now invalidated (Postgres-style), unlike sqlite where
        // this would succeed.
        let after_error = c.execute("INSERT INTO t(id) VALUES (2)", CancelToken::new()).await;
        let err = after_error.expect_err("DuckDB must abort the transaction after the first error");
        assert!(
            err.message.to_lowercase().contains("transaction"),
            "expected a transaction-aborted style error, got: {}",
            err.message
        );

        // Rollback still works and leaves the connection usable afterwards.
        c.execute("ROLLBACK", CancelToken::new()).await.unwrap();
        let n = c.execute("INSERT INTO t(id) VALUES (3)", CancelToken::new()).await.unwrap();
        assert_eq!(n, 1);
    }
}
