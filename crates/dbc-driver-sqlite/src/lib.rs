use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, QueryError, QueryStream, SchemaSnapshot, TableInfo,
    BATCH_ROWS, CHANNEL_CAPACITY,
};

pub struct SqliteConnection {
    path: PathBuf,
}

impl SqliteConnection {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

fn q_err(e: rusqlite::Error) -> QueryError {
    QueryError { code: None, message: e.to_string(), position: None }
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
        let sql = sql.to_owned();
        let handle = tokio::runtime::Handle::current();

        tokio::task::spawn_blocking(move || {
            let conn = match rusqlite::Connection::open(&path) {
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

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&path).map_err(q_err)?;
            let mut tables = Vec::new();
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .map_err(q_err)?;
            let names: Vec<String> = stmt
                .query_map([], |r| r.get(0)).map_err(q_err)?
                .filter_map(Result::ok).collect();
            for name in names {
                let mut columns = Vec::new();
                // Use rusqlite's `pragma` helper rather than formatting the
                // table name into the SQL text directly: it passes `name` as
                // a properly quoted/escaped SQL string literal, so table
                // names that are reserved words (e.g. "order", "group") or
                // contain spaces/hyphens/leading digits don't break the
                // query and abort the whole schema snapshot.
                conn.pragma(None, "table_info", name.as_str(), |r| {
                    columns.push(ColumnInfo { name: r.get(1)?, data_type: r.get(2)? });
                    Ok(())
                })
                .map_err(q_err)?;
                tables.push(TableInfo { schema: None, name, columns });
            }
            Ok(SchemaSnapshot { tables })
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
}
