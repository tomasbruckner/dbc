//! Server-dependent integration tests, all `#[ignore]`d — mirrors
//! `dbc-driver-postgres`'s docker-gated test style, except there is no
//! docker/testcontainers setup here (no SQL Server test image is pulled
//! automatically): the environment this crate was authored in had neither a
//! reachable SQL Server nor a confirmed ODBC Driver 17/18 install, so these
//! are written against the documented `odbc-api`/T-SQL surface but have NOT
//! been run for real. They are the first thing to run against a live server
//! before trusting this driver in the sandbox Apply flow.
//!
//! Point `DBC_MSSQL_TEST_CONN` at a full ODBC connection string (see
//! `dbc_driver_mssql::MssqlConfig::to_connection_string` for the shape) to
//! run them, e.g.:
//!
//! ```text
//! DBC_MSSQL_TEST_CONN="Driver={ODBC Driver 18 for SQL Server};Server=tcp:localhost,1433;\
//!   Database=tempdb;Uid=sa;Pwd=yourStrong(!)Password;Encrypt=yes;TrustServerCertificate=yes;"
//! cargo test -p dbc-driver-mssql -- --ignored
//! ```

use dbc_core::{CancelToken, Connection};
use dbc_driver_mssql::MssqlConnection;

fn conn_str() -> Option<String> {
    std::env::var("DBC_MSSQL_TEST_CONN").ok()
}

fn connect() -> MssqlConnection {
    MssqlConnection::from_connection_string(conn_str().expect("DBC_MSSQL_TEST_CONN not set"))
}

/// Basic round trip: connect, run a trivial SELECT, drain the stream.
#[tokio::test]
#[ignore]
async fn query_stream_smoke() {
    let mut c = connect();
    let mut s = c.query("SELECT 1 AS a, 'x' AS b", CancelToken::new()).await.unwrap();
    assert_eq!(s.columns.fields().len(), 2);
    let mut rows = 0usize;
    while let Some(b) = s.batches.recv().await {
        rows += b.unwrap().num_rows();
    }
    assert_eq!(rows, 1);
}

/// NULLs must come through as null cells, not the literal string "NULL" or
/// an empty string that's indistinguishable from an empty-but-non-null
/// value.
#[tokio::test]
#[ignore]
async fn query_handles_nulls() {
    let mut c = connect();
    let mut s = c
        .query("SELECT CAST(NULL AS int) AS a, CAST(NULL AS nvarchar(10)) AS b", CancelToken::new())
        .await
        .unwrap();
    let mut saw_row = false;
    while let Some(b) = s.batches.recv().await {
        let b = b.unwrap();
        for col in 0..b.num_columns() {
            assert!(b.column(col).is_null(0), "expected column {col} to be null");
        }
        saw_row = true;
    }
    assert!(saw_row);
}

/// Czech diacritics must round-trip exactly through `nvarchar` — the
/// motivating case for wide (`SQL_C_WCHAR`) binding in `wide.rs`. Narrow
/// (`SQL_C_CHAR`) binding would transcode this through the process ANSI
/// codepage and silently corrupt it while still "successfully" decoding as
/// UTF-8, so this specifically needs a live server + driver to catch a
/// regression back to narrow binding — a unit test can't observe the
/// codepage transcoding odbc-api/the driver performs internally.
#[tokio::test]
#[ignore]
async fn query_roundtrips_czech_diacritics() {
    let mut c = connect();
    let text = "Příliš žluťoučký kůň úpěl ďábelské ódy";
    let mut s = c
        .query(&format!("SELECT N'{text}' AS greeting"), CancelToken::new())
        .await
        .unwrap();
    let mut seen = None;
    while let Some(b) = s.batches.recv().await {
        let b = b.unwrap();
        let col = b.column(0).as_any().downcast_ref::<dbc_core::arrow::array::StringArray>().unwrap();
        if b.num_rows() > 0 {
            seen = Some(col.value(0).to_string());
        }
    }
    assert_eq!(seen.as_deref(), Some(text));
}

/// A value longer than `QUERY_MAX_STR_LEN` (64Ki UTF-16 code units) must
/// come back as `wide::cell_text`'s explicit truncation marker
/// (`"<zkráceno: >= N znaků>"`), never a silently shortened string that
/// reads like ordinary, complete data. `REPLICATE` builds an `nvarchar(max)`
/// well past the cap.
#[tokio::test]
#[ignore]
async fn query_reports_truncation_marker_for_oversized_nvarchar_max() {
    let mut c = connect();
    let mut s = c
        .query(
            "SELECT REPLICATE(CAST(N'x' AS nvarchar(max)), 100000) AS big",
            CancelToken::new(),
        )
        .await
        .unwrap();
    let mut seen = None;
    while let Some(b) = s.batches.recv().await {
        let b = b.unwrap();
        let col = b.column(0).as_any().downcast_ref::<dbc_core::arrow::array::StringArray>().unwrap();
        if b.num_rows() > 0 {
            seen = Some(col.value(0).to_string());
        }
    }
    let seen = seen.expect("expected one row");
    assert!(
        seen.starts_with("<zkráceno:"),
        "expected an explicit truncation marker for a 100000-char value against a 65536 cap, got a string of length {}",
        seen.len()
    );
}

/// A syntactically valid query against a nonexistent table must surface as
/// a stream `Err`, not a panic or a silently-empty stream.
#[tokio::test]
#[ignore]
async fn query_error_is_a_value() {
    let mut c = connect();
    let err = match c.query("SELECT * FROM no_such_table_xyz", CancelToken::new()).await {
        Ok(_) => panic!("expected an error querying a missing table"),
        Err(e) => e,
    };
    assert!(!err.message.is_empty());
}

/// `schema()` against a scratch table with a PK, an FK, an index, a view,
/// and a default constraint — exercises the full `fetch_*`/`attach_*`
/// decomposition in `schema.rs` end to end.
#[tokio::test]
#[ignore]
async fn schema_snapshot_smoke() {
    let mut c = connect();
    let suffix = std::process::id();
    let customers = format!("mssql_it_customers_{suffix}");
    let orders = format!("mssql_it_orders_{suffix}");
    let view = format!("mssql_it_v_orders_{suffix}");

    c.execute(
        &format!(
            "CREATE TABLE {customers} (id INT NOT NULL PRIMARY KEY, name NVARCHAR(50) NOT NULL DEFAULT 'x')"
        ),
        CancelToken::new(),
    )
    .await
    .unwrap();
    c.execute(
        &format!(
            "CREATE TABLE {orders} (id INT NOT NULL PRIMARY KEY, cid INT NOT NULL REFERENCES {customers}(id))"
        ),
        CancelToken::new(),
    )
    .await
    .unwrap();
    c.execute(&format!("CREATE INDEX idx_{orders}_cid ON {orders}(cid)"), CancelToken::new())
        .await
        .unwrap();
    c.execute(&format!("CREATE VIEW {view} AS SELECT id FROM {orders}"), CancelToken::new())
        .await
        .unwrap();

    let snap = c.schema().await.unwrap();

    let customers_t = snap.tables.iter().find(|t| t.name == customers).unwrap();
    let id_col = customers_t.columns.iter().find(|c| c.name == "id").unwrap();
    assert!(id_col.is_pk);
    let name_col = customers_t.columns.iter().find(|c| c.name == "name").unwrap();
    assert!(!name_col.nullable);
    assert!(name_col.default.is_some());

    let orders_t = snap.tables.iter().find(|t| t.name == orders).unwrap();
    let cid_col = orders_t.columns.iter().find(|c| c.name == "cid").unwrap();
    assert!(cid_col.fk.is_some());
    assert_eq!(cid_col.fk.as_ref().unwrap().table, customers);
    assert!(orders_t.indexes.iter().any(|i| i.columns == vec!["cid".to_string()]));

    let view_t = snap.tables.iter().find(|t| t.name == view).unwrap();
    assert_eq!(view_t.kind, dbc_core::TableKind::View);
    assert!(view_t.ddl.is_some());

    // Cleanup best-effort; not asserted.
    let _ = c.execute(&format!("DROP VIEW {view}"), CancelToken::new()).await;
    let _ = c.execute(&format!("DROP TABLE {orders}"), CancelToken::new()).await;
    let _ = c.execute(&format!("DROP TABLE {customers}"), CancelToken::new()).await;
}

/// `execute()` affected-row reporting for INSERT/UPDATE/DELETE, and the
/// `-1`/unknown row-count case surfacing as an error rather than a silent
/// zero (`map_row_count` unit-tests the pure mapping; this exercises the
/// live path end to end).
#[tokio::test]
#[ignore]
async fn execute_reports_affected_rows() {
    let mut c = connect();
    let table = format!("mssql_it_rows_{}", std::process::id());
    c.execute(&format!("CREATE TABLE {table} (id INT, name NVARCHAR(50))"), CancelToken::new())
        .await
        .unwrap();

    let n = c.execute(&format!("INSERT INTO {table} (id, name) VALUES (1, 'a')"), CancelToken::new())
        .await
        .unwrap();
    assert_eq!(n, 1);

    let n = c.execute(&format!("INSERT INTO {table} (id, name) VALUES (2, 'b')"), CancelToken::new())
        .await
        .unwrap();
    assert_eq!(n, 1);

    let n = c.execute(&format!("UPDATE {table} SET name = 'z'"), CancelToken::new()).await.unwrap();
    assert_eq!(n, 2);

    let n = c.execute(&format!("DELETE FROM {table} WHERE id = 9999"), CancelToken::new())
        .await
        .unwrap();
    assert_eq!(n, 0);

    let _ = c.execute(&format!("DROP TABLE {table}"), CancelToken::new()).await;
}

/// `BEGIN TRANSACTION` … `ROLLBACK` over successive `execute()` calls must
/// run on the same underlying connection — a fresh connection per call
/// would auto-commit the INSERT before the ROLLBACK ever ran.
#[tokio::test]
#[ignore]
async fn execute_transaction_commit_and_rollback() {
    let mut c = connect();
    let table = format!("mssql_it_tx_{}", std::process::id());
    c.execute(&format!("CREATE TABLE {table} (id INT)"), CancelToken::new()).await.unwrap();

    c.execute("BEGIN TRANSACTION", CancelToken::new()).await.unwrap();
    c.execute(&format!("INSERT INTO {table} (id) VALUES (1)"), CancelToken::new()).await.unwrap();
    c.execute("ROLLBACK", CancelToken::new()).await.unwrap();

    let mut s = c.query(&format!("SELECT id FROM {table}"), CancelToken::new()).await.unwrap();
    let mut rows = 0usize;
    while let Some(b) = s.batches.recv().await {
        rows += b.unwrap().num_rows();
    }
    assert_eq!(rows, 0, "row inserted inside the rolled-back transaction must be absent");

    c.execute("BEGIN TRANSACTION", CancelToken::new()).await.unwrap();
    c.execute(&format!("INSERT INTO {table} (id) VALUES (2)"), CancelToken::new()).await.unwrap();
    c.execute("COMMIT", CancelToken::new()).await.unwrap();

    let mut s = c.query(&format!("SELECT id FROM {table}"), CancelToken::new()).await.unwrap();
    let mut rows = 0usize;
    while let Some(b) = s.batches.recv().await {
        rows += b.unwrap().num_rows();
    }
    assert_eq!(rows, 1, "committed row must be visible");

    let _ = c.execute(&format!("DROP TABLE {table}"), CancelToken::new()).await;
}

/// Probes the mid-transaction error-divergence behavior flagged as
/// needs-empirical-verification in `lib.rs`'s module doc: with the session
/// default `XACT_ABORT OFF`, does a failed statement inside an open
/// `BEGIN TRANSACTION` leave the transaction open and usable (sqlite-like),
/// abort it (postgres-like), or something else? This test intentionally
/// does not assert a specific outcome yet — it just exercises the sequence
/// and prints what actually happened, pending a real server to run it
/// against.
#[tokio::test]
#[ignore]
async fn mid_tx_error_behavior_probe_xact_abort_off() {
    let mut c = connect();
    let table = format!("mssql_it_probe_{}", std::process::id());
    c.execute(&format!("CREATE TABLE {table} (id INT PRIMARY KEY)"), CancelToken::new())
        .await
        .unwrap();

    c.execute("BEGIN TRANSACTION", CancelToken::new()).await.unwrap();
    c.execute(&format!("INSERT INTO {table} (id) VALUES (1)"), CancelToken::new()).await.unwrap();
    // Duplicate PK — expected to fail this one statement.
    let dup_err = c.execute(&format!("INSERT INTO {table} (id) VALUES (1)"), CancelToken::new()).await;
    assert!(dup_err.is_err(), "duplicate PK insert should fail");

    // Does the transaction still accept statements, or is it aborted?
    let follow_up = c.execute(&format!("INSERT INTO {table} (id) VALUES (2)"), CancelToken::new()).await;
    println!("follow-up statement after failed insert: {follow_up:?}");

    let _ = c.execute("ROLLBACK", CancelToken::new()).await;
    let _ = c.execute(&format!("DROP TABLE {table}"), CancelToken::new()).await;
}
