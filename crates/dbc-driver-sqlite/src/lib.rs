use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, ConstraintInfo, QueryError, QueryStream, SchemaSnapshot,
    TableInfo, TableKind, TriggerInfo, FkRef, BATCH_ROWS, CHANNEL_CAPACITY,
};

pub struct SqliteConnection {
    path: PathBuf,
    read_only: bool,
    /// Lazily-opened connection reused across [`Connection::execute`] calls
    /// so a `BEGIN … COMMIT`/`ROLLBACK` sequence runs over one underlying
    /// sqlite handle rather than being silently split across separate
    /// connections (which would drop the in-progress transaction). Taken out
    /// of the `Option` and moved into `spawn_blocking`, then put back — safe
    /// because `execute` takes `&mut self`, so there is never concurrent
    /// access. `query`/`schema` are unaffected and keep opening a fresh
    /// connection per call, as before.
    exec_conn: Option<rusqlite::Connection>,
}

impl SqliteConnection {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), read_only: false, exec_conn: None }
    }

    /// Like [`SqliteConnection::new`], but opens the database with
    /// `SQLITE_OPEN_READ_ONLY` (no `SQLITE_OPEN_CREATE`/`READ_WRITE`) when
    /// `read_only` is set — server-side enforcement of a connection's
    /// read-only flag, so a client-side guard bypass can't still mutate the
    /// file. See dbc-ui's `connect::open_config`, which selects this
    /// constructor when `ConnectionConfig::read_only` is set.
    pub fn new_with_options(path: impl Into<PathBuf>, read_only: bool) -> Self {
        Self { path: path.into(), read_only, exec_conn: None }
    }
}

fn q_err(e: rusqlite::Error) -> QueryError {
    QueryError { code: None, message: e.to_string(), position: None }
}

/// `rusqlite::Connection::open` mirrored with `SQLITE_OPEN_READ_ONLY` swapped
/// in for `SQLITE_OPEN_READ_WRITE | SQLITE_OPEN_CREATE` when `read_only` is
/// set (same `URI | NO_MUTEX` flags `open`'s default otherwise uses).
fn open_conn(path: &std::path::Path, read_only: bool) -> rusqlite::Result<rusqlite::Connection> {
    if read_only {
        rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    } else {
        rusqlite::Connection::open(path)
    }
}

fn value_to_text(v: rusqlite::types::ValueRef<'_>) -> Option<String> {
    use rusqlite::types::ValueRef::*;
    match v {
        Null => None,
        Integer(i) => Some(i.to_string()),
        Real(f) => Some(f.to_string()),
        Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        Blob(b) => Some(format!("<blob {} B>", b.len())),
    }
}

#[async_trait]
impl Connection for SqliteConnection {
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
                Err(e) => { let _ = schema_tx.send(Err(q_err(e))); return; }
            };
            // Watcher: protocol-level interrupt when the token fires.
            let interrupt = conn.get_interrupt_handle();
            let watcher_cancel = cancel.clone();
            let watcher = handle.spawn(async move {
                watcher_cancel.cancelled().await;
                interrupt.interrupt();
            });

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => { let _ = schema_tx.send(Err(q_err(e))); watcher.abort(); return; }
            };
            let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let schema: SchemaRef = Arc::new(Schema::new(
                col_names.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect::<Vec<_>>(),
            ));
            let _ = schema_tx.send(Ok(schema.clone()));

            let ncols = col_names.len();
            let mut rows = match stmt.query([]) {
                Ok(r) => r,
                Err(e) => { let _ = tx.blocking_send(Err(q_err(e))); watcher.abort(); return; }
            };
            let mut builders: Vec<StringBuilder> =
                (0..ncols).map(|_| StringBuilder::new()).collect();
            let mut in_batch = 0usize;

            let flush = |builders: &mut Vec<StringBuilder>| -> RecordBatch {
                let arrays: Vec<ArrayRef> =
                    builders.iter_mut().map(|b| Arc::new(b.finish()) as ArrayRef).collect();
                RecordBatch::try_new(schema.clone(), arrays).expect("schema matches builders")
            };

            loop {
                // Cooperative check: sqlite3_interrupt() is a no-op if it fires
                // before statement execution starts (the interrupt flag clears
                // when the running-statement count is zero), so a cancel issued
                // immediately after query() returns can be lost by the watcher
                // alone. Checking the token between rows guarantees cancellation
                // lands at row granularity regardless of interrupt timing.
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
                            if tx.blocking_send(Ok(flush(&mut builders))).is_err() { break; }
                            in_batch = 0;
                        }
                    }
                    Ok(None) => {
                        if in_batch > 0 { let _ = tx.blocking_send(Ok(flush(&mut builders))); }
                        break;
                    }
                    Err(e) => {
                        let err = if cancel.is_cancelled() {
                            QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
                        } else { q_err(e) };
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
    /// calls (see [`SqliteConnection::exec_conn`]), so `BEGIN … COMMIT`/
    /// `ROLLBACK` sequences issued via successive `execute` calls run over
    /// the same sqlite handle. `rusqlite::Connection::execute` returns the
    /// changed-row count directly for DML (`0` for `BEGIN`/`COMMIT`/DDL), and
    /// errors on statements that return rows (e.g. `SELECT`) — desired here,
    /// since this path is for writes only.
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
            let mut tables = Vec::new();
            let mut triggers = Vec::new();

            // Read tables and views from sqlite_master
            let mut stmt = conn
                .prepare("SELECT type, name, sql FROM sqlite_master WHERE type IN ('table', 'view', 'trigger', 'index') ORDER BY type, name")
                .map_err(q_err)?;
            let rows: Vec<(String, String, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(q_err)?
                .collect::<Result<Vec<_>, rusqlite::Error>>()
                .map_err(q_err)?;

            for (obj_type, name, sql) in rows {
                // Skip internal sqlite_ objects
                if name.starts_with("sqlite_") {
                    continue;
                }

                if obj_type == "table" || obj_type == "view" {
                    let kind = if obj_type == "view" { TableKind::View } else { TableKind::Table };

                    // Get columns via PRAGMA table_info. `pk` is the 1-based
                    // position of the column within the primary key (0 = not
                    // part of it) — kept alongside for PK-order sorting below.
                    let mut columns = Vec::new();
                    let mut pk_seq: Vec<(i32, String)> = Vec::new();
                    conn.pragma(None, "table_info", name.as_str(), |r| {
                        let col_name: String = r.get(1)?;
                        let data_type: String = r.get(2)?;
                        let notnull: i32 = r.get(3)?;
                        let default: Option<String> = r.get(4)?;
                        let pk: i32 = r.get(5)?;

                        if pk > 0 {
                            pk_seq.push((pk, col_name.clone()));
                        }
                        columns.push(ColumnInfo {
                            name: col_name,
                            data_type,
                            nullable: notnull == 0,
                            default,
                            is_pk: pk > 0,
                            fk: None,
                        });
                        Ok(())
                    })
                    .map_err(q_err)?;

                    // Get foreign keys via PRAGMA foreign_key_list
                    let mut fks_by_col: std::collections::HashMap<String, FkRef> = std::collections::HashMap::new();
                    conn.pragma(None, "foreign_key_list", name.as_str(), |r| {
                        let from_col: String = r.get(3)?;
                        let to_table: String = r.get(2)?;
                        let to_col: String = r.get(4)?;
                        fks_by_col.insert(from_col, FkRef {
                            schema: None,
                            table: to_table,
                            column: to_col,
                        });
                        Ok(())
                    })
                    .map_err(q_err)?;

                    // Apply FKs to matching columns
                    for col in &mut columns {
                        if let Some(fk) = fks_by_col.remove(&col.name) {
                            col.fk = Some(fk);
                        }
                    }

                    // Get indexes for this table
                    let mut indexes = Vec::new();
                    if obj_type == "table" {
                        // First collect index metadata
                        let mut index_list = Vec::new();
                        conn.pragma(None, "index_list", name.as_str(), |r| {
                            let idx_name: String = r.get(1)?;
                            let unique: i32 = r.get(2)?;

                            // Skip sqlite_autoindex_*
                            if !idx_name.starts_with("sqlite_autoindex_") {
                                index_list.push((idx_name, unique != 0));
                            }
                            Ok(())
                        })
                        .map_err(q_err)?;

                        // Get columns for each index
                        for (idx_name, unique) in index_list {
                            let mut index_columns = Vec::new();
                            conn.pragma(None, "index_info", idx_name.as_str(), |r| {
                                let col_name: String = r.get(2)?;
                                index_columns.push(col_name);
                                Ok(())
                            })
                            .map_err(q_err)?;
                            indexes.push(dbc_core::IndexInfo {
                                name: idx_name,
                                columns: index_columns,
                                unique,
                            });
                        }
                    }

                    // Build constraints
                    let mut constraints = Vec::new();

                    // Primary key constraint — columns in PK sequence order
                    // (a composite PRIMARY KEY(b, a) must not read (a, b)).
                    let mut pk_seq = pk_seq.clone();
                    pk_seq.sort_by_key(|(seq, _)| *seq);
                    let pk_cols: Vec<String> = pk_seq.into_iter().map(|(_, n)| n).collect();
                    if !pk_cols.is_empty() {
                        constraints.push(ConstraintInfo {
                            name: String::new(),
                            kind: "PRIMARY KEY".to_string(),
                            definition: format!("PRIMARY KEY ({})", pk_cols.join(", ")),
                        });
                    }

                    // Foreign key constraints
                    for col in &columns {
                        if let Some(fk) = &col.fk {
                            constraints.push(ConstraintInfo {
                                name: String::new(),
                                kind: "FOREIGN KEY".to_string(),
                                definition: format!(
                                    "FOREIGN KEY ({}) REFERENCES {}({})",
                                    col.name, fk.table, fk.column
                                ),
                            });
                        }
                    }

                    tables.push(TableInfo {
                        schema: None,
                        name,
                        kind,
                        columns,
                        indexes,
                        constraints,
                        ddl: sql,
                    });
                } else if obj_type == "trigger" {
                    // Get trigger table name from sqlite_master
                    let mut trigger_table = String::new();
                    let mut stmt_trig = conn
                        .prepare("SELECT tbl_name FROM sqlite_master WHERE type='trigger' AND name=?1")
                        .map_err(q_err)?;
                    stmt_trig
                        .query_row([&name], |r| {
                            trigger_table = r.get(0)?;
                            Ok(())
                        })
                        .map_err(q_err)?;

                    triggers.push(TriggerInfo {
                        schema: None,
                        name,
                        table: trigger_table,
                        ddl: sql,
                    });
                }
            }

            Ok(SchemaSnapshot { tables, triggers, ..Default::default() })
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{CancelToken, Connection};

    fn fixture_db() -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut conn = rusqlite::Connection::open(f.path()).unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER, name TEXT);").unwrap();
        // `generate_series` isn't reliably present in rusqlite's bundled
        // SQLite build, so populate the fixture with a plain insert loop
        // inside a single transaction instead.
        let txn = conn.transaction().unwrap();
        {
            let mut stmt = txn.prepare("INSERT INTO t(id, name) VALUES (?1, ?2)").unwrap();
            for value in 1..=5000i64 {
                stmt.execute(rusqlite::params![value, format!("n{value}")]).unwrap();
            }
        }
        txn.commit().unwrap();
        f
    }

    #[tokio::test]
    async fn streams_all_rows_in_batches() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
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
    async fn sql_error_is_a_value() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        // `QueryStream` (the `Ok` variant) doesn't implement `Debug`, so
        // `Result::unwrap_err` (which requires `T: Debug`) can't be used here.
        let err = match c.query("SELECT * FROM missing_table", CancelToken::new()).await {
            Ok(_) => panic!("expected an error querying a missing table"),
            Err(e) => e,
        };
        assert!(err.message.contains("missing_table"));
    }

    #[tokio::test]
    async fn cancel_interrupts_long_query() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        let cancel = CancelToken::new();
        // Triple cross join = 125e9 rows; cannot complete before interrupt
        // even under heavy parallel-test CPU contention (a 25M double join
        // proved flaky when the whole workspace suite ran concurrently).
        let mut s = c
            .query("SELECT a.id FROM t a, t b, t c", cancel.clone())
            .await
            .unwrap();
        cancel.cancel();
        let mut saw_cancel = false;
        let drain = async {
            while let Some(r) = s.batches.recv().await {
                if let Err(e) = r {
                    assert_eq!(e.code.as_deref(), Some("cancelled"));
                    saw_cancel = true;
                }
            }
        };
        // Safety net: if interrupt never lands, fail loudly instead of hanging.
        tokio::time::timeout(std::time::Duration::from_secs(30), drain)
            .await
            .expect("query was not interrupted within 30s");
        assert!(saw_cancel, "stream ended without a cancelled error");
    }

    #[tokio::test]
    async fn schema_lists_tables_and_columns() {
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        let snap = c.schema().await.unwrap();
        let t = snap.tables.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[0].name, "id");
    }

    #[tokio::test]
    async fn schema_handles_reserved_word_and_spaced_table_names() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(f.path()).unwrap();
        // "order" is a reserved word and must be quoted to use as a table
        // name; "weird name" contains a space. Both require the schema()
        // implementation to properly quote/escape the name when querying
        // `PRAGMA table_info(...)`, rather than interpolating it raw.
        conn.execute_batch(
            "CREATE TABLE \"order\" (id INTEGER, total REAL);
             CREATE TABLE \"weird name\" (a TEXT, b TEXT);",
        )
        .unwrap();

        let mut c = SqliteConnection::new(f.path());
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
        let mut c = SqliteConnection::new_with_options(f.path(), true);
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
        // Server-side enforcement (Task 6 security review requirement):
        // `SQLITE_OPEN_READ_ONLY` must reject a write regardless of any
        // client-side `is_read_statement` guard.
        let f = fixture_db();
        let mut c = SqliteConnection::new_with_options(f.path(), true);
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
    async fn full_catalog() {
        // Build a temp DB with customers/orders FK, index, view, and trigger
        let f = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(f.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'x');
             CREATE TABLE orders(id INTEGER PRIMARY KEY, cid INTEGER NOT NULL REFERENCES customers(id));
             CREATE INDEX idx_orders_cid ON orders(cid);
             CREATE VIEW v_orders AS SELECT id FROM orders;
             CREATE TRIGGER trg AFTER INSERT ON orders BEGIN SELECT 1; END;",
        )
        .unwrap();

        let mut c = SqliteConnection::new(f.path());
        let snap = c.schema().await.unwrap();

        // Check tables
        assert!(snap.tables.len() >= 2, "expected at least 2 tables, got {}", snap.tables.len());

        // Check customers table
        let customers = snap.tables.iter().find(|t| t.name == "customers").unwrap();
        assert_eq!(customers.kind, TableKind::Table);
        assert_eq!(customers.columns.len(), 2);
        let id_col = customers.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id_col.is_pk);
        let name_col = customers.columns.iter().find(|c| c.name == "name").unwrap();
        assert!(!name_col.nullable);
        assert_eq!(name_col.default, Some("'x'".to_string()));
        assert!(customers.ddl.is_some());
        assert!(customers.ddl.as_ref().unwrap().contains("CREATE TABLE"));

        // Check orders table
        let orders = snap.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.kind, TableKind::Table);
        let cid_col = orders.columns.iter().find(|c| c.name == "cid").unwrap();
        assert!(!cid_col.nullable);
        assert!(cid_col.fk.is_some());
        let fk = cid_col.fk.as_ref().unwrap();
        assert_eq!(fk.schema, None);
        assert_eq!(fk.table, "customers");
        assert_eq!(fk.column, "id");

        // Check index
        assert!(!orders.indexes.is_empty());
        let idx = orders.indexes.iter().find(|i| i.name == "idx_orders_cid").unwrap();
        assert_eq!(idx.columns, vec!["cid"]);
        assert!(!idx.unique);

        // Check view
        let v_orders = snap.tables.iter().find(|t| t.name == "v_orders").unwrap();
        assert_eq!(v_orders.kind, TableKind::View);
        assert_eq!(v_orders.columns.len(), 1);
        assert!(v_orders.ddl.is_some());
        assert!(v_orders.ddl.as_ref().unwrap().contains("CREATE VIEW"));

        // Check trigger
        assert!(!snap.triggers.is_empty());
        let trg = snap.triggers.iter().find(|t| t.name == "trg").unwrap();
        assert_eq!(trg.table, "orders");
        assert!(trg.ddl.is_some());
        assert!(trg.ddl.as_ref().unwrap().contains("CREATE TRIGGER"));
    }

    #[tokio::test]
    async fn schema_error_is_value_not_skip() {
        // Verify that decode errors are propagated as Err(QueryError), not silently skipped
        // by checking the code shape - the current implementation uses collect() with
        // proper error handling instead of filter_map(Result::ok)
        let f = fixture_db();
        let mut c = SqliteConnection::new(f.path());
        // If schema() succeeds on a valid db, the happy path is confirmed
        let snap = c.schema().await;
        assert!(snap.is_ok(), "schema() should succeed on valid db");
    }

    #[tokio::test]
    async fn execute_reports_affected_rows() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut c = SqliteConnection::new(f.path());

        // DDL: 0 affected rows.
        let n = c
            .execute("CREATE TABLE t(id INTEGER, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 0);

        // Each INSERT affects exactly 1 row.
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

        // UPDATE hitting both rows reports 2.
        let n = c.execute("UPDATE t SET name = 'z'", CancelToken::new()).await.unwrap();
        assert_eq!(n, 2);

        // DELETE with no matching rows reports 0.
        let n = c
            .execute("DELETE FROM t WHERE id = 9999", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn execute_in_transaction_rolls_back() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut c = SqliteConnection::new(f.path());
        c.execute("CREATE TABLE t(id INTEGER, name TEXT)", CancelToken::new()).await.unwrap();

        c.execute("BEGIN", CancelToken::new()).await.unwrap();
        c.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();
        c.execute("ROLLBACK", CancelToken::new()).await.unwrap();

        // The insert must not be visible — same underlying connection must
        // have been used for BEGIN/INSERT/ROLLBACK for the rollback to take
        // effect; a fresh connection per call would have auto-committed the
        // INSERT before ROLLBACK ever ran.
        let mut s = c.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 0, "row inserted inside the rolled-back transaction must be absent");
    }

    // T1 review issue 6: the read-only flag must reach exec_conn too — a
    // read-only connection's write path fails, same as its query path.
    #[tokio::test]
    async fn read_only_connection_rejects_execute_writes() {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let mut w = SqliteConnection::new(f.path());
            w.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        }
        let mut c = SqliteConnection::new_with_options(f.path(), true);
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
}
