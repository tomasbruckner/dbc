//! G15 §3c — the XACT_ABORT empirical verification matrix (design
//! `g15-mssql-wiring-design.md` §3c, plan T2). This is the phase KEYSTONE:
//! every sanctioned write sequence (T5) switches to `dbc_core::tx_begin_sql`
//! = `"SET XACT_ABORT ON; BEGIN TRANSACTION"`, and these cases are the
//! empirical proof that doing so gives MSSQL a pg-like "stop at first error,
//! roll back everything" contract before any feature-ON flip merges (T8).
//!
//! Docker required, `#[ignore]`d — mirrors `mssql_integration.rs`'s
//! convention (no testcontainers wiring here yet; T8 rewires both files
//! onto the shared testcontainers `mssql_server` helper). Point
//! `DBC_MSSQL_TEST_CONN` at a full ODBC connection string, e.g.:
//!
//! ```text
//! DBC_MSSQL_TEST_CONN="Driver={ODBC Driver 18 for SQL Server};Server=tcp:localhost,1433;\
//!   Database=tempdb;Uid=sa;Pwd=yourStrong(!)Password;Encrypt=yes;TrustServerCertificate=yes;"
//! cargo test -p dbc-driver-mssql -- --ignored
//! ```
//!
//! Each case drives `execute()` exactly as the app does: a persistent
//! `exec_conn` (one `MssqlConnection` per case), statements as separate
//! calls — never a single multi-statement batch, since that's not how the
//! runner issues transaction control.

use dbc_core::{CancelToken, Connection};
use dbc_driver_mssql::MssqlConnection;

fn conn_str() -> Option<String> {
    std::env::var("DBC_MSSQL_TEST_CONN").ok()
}

fn connect() -> MssqlConnection {
    MssqlConnection::from_connection_string(conn_str().expect("DBC_MSSQL_TEST_CONN not set"))
}

async fn exec(conn: &mut MssqlConnection, sql: &str) -> Result<u64, dbc_core::QueryError> {
    conn.execute(sql, CancelToken::new()).await
}

/// Errors exactly when the assertion fails; produces no result set, so it
/// is legal on the persistent `exec_conn` the app actually uses (§3c).
fn trancount_probe(n: u32) -> String {
    format!("IF @@TRANCOUNT <> {n} THROW 50000, 'trancount mismatch', 1")
}

/// The exact text every sanctioned sequence sends from T5 on
/// (`dbc_core::tx_begin_sql(Dialect::Mssql)`).
const TX_BEGIN: &str = "SET XACT_ABORT ON; BEGIN TRANSACTION";

/// `SELECT COUNT(*)` via a fresh `query()` connection — driver rows come
/// back as UTF-8 text (see `lib.rs` module doc), so the count is parsed
/// back out of the single returned cell. Used for every data-visibility
/// assertion (design §3c: "Data-visibility assertions use a SECOND
/// connection's `query()`").
async fn count_rows(conn: &mut MssqlConnection, table: &str) -> Result<i64, dbc_core::QueryError> {
    let mut s = conn.query(&format!("SELECT COUNT(*) AS n FROM {table}"), CancelToken::new()).await?;
    let mut n: i64 = 0;
    while let Some(b) = s.batches.recv().await {
        let b = b?;
        if b.num_rows() > 0 {
            let col = b
                .column(0)
                .as_any()
                .downcast_ref::<dbc_core::arrow::array::StringArray>()
                .expect("COUNT(*) column decodes as Utf8 per this driver's convention");
            n = col.value(0).parse().unwrap_or(0);
        }
    }
    Ok(n)
}

/// T2 Step 1 "red state": a compile-visible use of both new public items
/// (`probe`, `query_with_session`) so `cargo test -p dbc-driver-mssql`
/// fails to compile until they exist. Kept as a genuine `#[ignore]`d test
/// (not just a dead-code smoke fn) so the crate's usual test-listing shows
/// it, matching this file's other cases.
#[tokio::test]
#[ignore]
async fn probe_and_query_with_session_are_callable() {
    let mut c = connect();
    c.probe().expect("probe should succeed against a reachable, correctly-configured server");
    let mut s = c
        .query_with_session(&[], "SELECT 1 AS a", &[], CancelToken::new())
        .await
        .expect("query_with_session should run a trivial single-batch query");
    let mut rows = 0usize;
    while let Some(b) = s.batches.recv().await {
        rows += b.unwrap().num_rows();
    }
    assert_eq!(rows, 1);
}

/// Case 0 (row-count characterization — added by this plan, gates
/// Appendix F2): tx-control batches (`SET XACT_ABORT ON; BEGIN
/// TRANSACTION`, `COMMIT`) must report a row count, not error. Before the
/// F2 fix in `types.rs::map_row_count`, the driver-reported
/// `SQL_NO_ROW_COUNT` (`row_count() == None`) for these non-DML batches
/// mapped to an ERROR — which would break every T5 sequence's very first
/// statement on MSSQL. This crate ships the F2 fix proactively (see
/// `types.rs`), so this case is expected green even before a live run.
#[tokio::test]
#[ignore]
async fn tx_control_batches_report_a_row_count() {
    let mut c = connect();
    exec(&mut c, TX_BEGIN).await.expect("BEGIN batch must report a row count, not error");
    exec(&mut c, "COMMIT").await.expect("COMMIT must report a row count, not error");
}

/// Case 1: a PK violation inside an XACT_ABORT-ON transaction aborts and
/// rolls back the WHOLE transaction, not just the failed statement.
#[tokio::test]
#[ignore]
async fn xact_abort_pk_violation_aborts_and_rolls_back_whole_tx() {
    let mut c = connect();
    let table = format!("mssql_tx_case1_{}", std::process::id());
    exec(&mut c, &format!("CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY)")).await.unwrap();

    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await.unwrap();
    let dup = exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await;
    assert!(dup.is_err(), "duplicate PK insert must fail");

    // Same session: the transaction must be gone (trancount 0).
    exec(&mut c, &trancount_probe(0)).await.expect("XACT_ABORT ON must have doomed the transaction to trancount 0");

    // Second connection, fresh by construction: the first INSERT's row
    // must be GONE — the whole transaction rolled back, not just the
    // failed statement.
    let mut c2 = connect();
    let n = count_rows(&mut c2, &table).await.unwrap();
    assert_eq!(n, 0, "row inserted before the PK violation must be rolled back with the whole tx");

    let _ = exec(&mut c, &format!("DROP TABLE {table}")).await;
}

/// Case 2: conversion and arithmetic errors — the failure classes that
/// diverge under `XACT_ABORT OFF` (some abort only the statement, some the
/// whole batch) — must all behave identically to a constraint violation
/// under `XACT_ABORT ON`: whole transaction gone, trancount 0, no rows.
#[tokio::test]
#[ignore]
async fn conversion_and_arithmetic_errors_behave_like_constraint_errors() {
    let mut c = connect();
    let table = format!("mssql_tx_case2_{}", std::process::id());
    exec(&mut c, &format!("CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY)")).await.unwrap();

    // Conversion error.
    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await.unwrap();
    let conv_err = exec(&mut c, &format!("INSERT INTO {table} (id) SELECT CAST('x' AS int)")).await;
    assert!(conv_err.is_err(), "conversion error must fail the statement");
    exec(&mut c, &trancount_probe(0)).await.expect("conversion error must doom the tx to trancount 0, same as a constraint violation");
    let mut c2 = connect();
    assert_eq!(count_rows(&mut c2, &table).await.unwrap(), 0, "conversion error must roll back the whole tx");

    // Arithmetic error (divide by zero).
    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (2)")).await.unwrap();
    let arith_err = exec(&mut c, &format!("INSERT INTO {table} (id) SELECT 1 / 0")).await;
    assert!(arith_err.is_err(), "divide-by-zero must fail the statement");
    exec(&mut c, &trancount_probe(0)).await.expect("arithmetic error must doom the tx to trancount 0, same as a constraint violation");
    assert_eq!(count_rows(&mut c2, &table).await.unwrap(), 0, "arithmetic error must roll back the whole tx");

    let _ = exec(&mut c, &format!("DROP TABLE {table}")).await;
}

/// Case 3: after case 1's abort, the app's best-effort `ROLLBACK` (the
/// `let _ =` discard posture every sanctioned sequence uses) errors with
/// "no corresponding BEGIN TRANSACTION" — but the session itself is NOT
/// poisoned: a following plain statement on the SAME connection succeeds.
#[tokio::test]
#[ignore]
async fn best_effort_rollback_after_abort_errors_but_session_stays_usable() {
    let mut c = connect();
    let table = format!("mssql_tx_case3_{}", std::process::id());
    exec(&mut c, &format!("CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY)")).await.unwrap();

    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await.unwrap();
    let dup = exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await;
    assert!(dup.is_err());

    let rollback = exec(&mut c, "ROLLBACK").await;
    assert!(
        rollback.is_err(),
        "ROLLBACK with no open tx (XACT_ABORT already closed it) must error — the `let _ =` \
         discard posture in every sanctioned sequence relies on this being safe to ignore"
    );

    // Session must still be usable: a plain INSERT on the same connection
    // succeeds (the discard is safe, not masking a poisoned session).
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (2)")).await.expect(
        "the session must remain usable after the best-effort ROLLBACK's harmless error",
    );

    let _ = exec(&mut c, &format!("DROP TABLE {table}")).await;
}

/// Case 4 (KEYSTONE — gates Appendix F1): proves ODBC's
/// `SQL_ATTR_AUTOCOMMIT = ON` does NOT commit between `execute()` calls
/// once a literal `BEGIN TRANSACTION` is open — i.e. the app's persistent
/// `exec_conn` genuinely holds an open transaction across calls, and a
/// second connection cannot see the uncommitted row.
#[tokio::test]
#[ignore]
async fn autocommit_does_not_commit_between_execute_calls_inside_open_tx() {
    let mut c = connect();
    let table = format!("mssql_tx_case4_{}", std::process::id());
    exec(&mut c, &format!("CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY)")).await.unwrap();

    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &trancount_probe(1)).await.expect("BEGIN TRANSACTION must open a real tx (trancount 1)");
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await.unwrap();

    // Fresh second connection. `query()` always opens a brand-new
    // connection per call (see the crate's module doc), so a bare
    // `execute("SET LOCK_TIMEOUT ...")` followed by a separate `query()`
    // call would land on two DIFFERENT sessions and the timeout would
    // never take effect (the default is to wait indefinitely — exactly
    // the hang this would otherwise cause). `query_with_session`'s
    // prelude is what puts the SET and the SELECT on the same session.
    // NEVER read with READUNCOMMITTED here — that would see the
    // uncommitted row regardless of autocommit interference and prove
    // nothing. A lock timeout under the default isolation level is an
    // equally valid proof of non-visibility as a bare 0 count.
    let mut c2 = connect();
    let visibility = c2
        .query_with_session(
            &["SET LOCK_TIMEOUT 1000".to_string()],
            &format!("SELECT COUNT(*) AS n FROM {table}"),
            &[],
            CancelToken::new(),
        )
        .await;
    match visibility {
        Ok(mut s) => {
            let mut n: i64 = 0;
            while let Some(b) = s.batches.recv().await {
                let b = b.unwrap();
                if b.num_rows() > 0 {
                    let col = b
                        .column(0)
                        .as_any()
                        .downcast_ref::<dbc_core::arrow::array::StringArray>()
                        .unwrap();
                    n = col.value(0).parse().unwrap_or(0);
                }
            }
            assert_eq!(n, 0, "uncommitted row must not be visible to a second connection");
        }
        Err(e) => {
            // Lock-timeout error: the second connection blocked trying to
            // read a row locked by the still-open transaction — also
            // proves the row is not committed/visible yet. This is the
            // KEYSTONE case, so accepting ANY error here (as an earlier
            // version of this test did) would make it spuriously green
            // under a `query_with_session` regression that broke the
            // second connection for unrelated reasons — a real proof
            // requires the SPECIFIC lock-timeout error, not just "some
            // error". Live-characterized shape (odbc-api 29 / ODBC Driver
            // 18 / SQL Server 2022): SQLSTATE `42000`, native error 1222,
            // message "...Lock request time out period exceeded." SQLSTATE
            // alone isn't a reliable discriminator here (`42000` is a
            // generic access-violation class shared with unrelated
            // errors), so match on the message text instead.
            let msg_lower = e.message.to_lowercase();
            assert!(
                msg_lower.contains("time out") || msg_lower.contains("timeout"),
                "case 4 KEYSTONE: expected a lock-timeout error proving the row is not yet \
                 visible, got a DIFFERENT error instead — this would make the keystone \
                 spuriously green on a real regression. Got: {e:?}"
            );
        }
    }

    exec(&mut c, "COMMIT").await.unwrap();

    let mut c3 = connect();
    let n = count_rows(&mut c3, &table).await.unwrap();
    assert_eq!(n, 1, "row must be visible to a fresh connection once COMMIT has run");

    let _ = exec(&mut c, &format!("DROP TABLE {table}")).await;
}

/// Case 5: session persistence — `SET XACT_ABORT ON` issued in the
/// `TX_BEGIN` batch still governs a LATER `TX_BEGIN` on the same
/// `exec_conn` (harmless redundancy either way, but characterizes the
/// session so a future optimization that skips re-stating it on later
/// BEGINs would be safe).
#[tokio::test]
#[ignore]
async fn xact_abort_persists_across_tx_begins_on_same_exec_conn() {
    let mut c = connect();
    let table = format!("mssql_tx_case5_{}", std::process::id());
    exec(&mut c, &format!("CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY)")).await.unwrap();

    // First cycle: ordinary commit.
    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (1)")).await.unwrap();
    exec(&mut c, "COMMIT").await.unwrap();

    // Second TX_BEGIN on the SAME connection, followed by a PK violation:
    // still aborts to trancount 0.
    exec(&mut c, TX_BEGIN).await.unwrap();
    exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (2)")).await.unwrap();
    let dup = exec(&mut c, &format!("INSERT INTO {table} (id) VALUES (2)")).await;
    assert!(dup.is_err());
    exec(&mut c, &trancount_probe(0))
        .await
        .expect("XACT_ABORT must still govern the second TX_BEGIN on the same connection");

    let n = count_rows(&mut c, &table).await.unwrap();
    assert_eq!(n, 1, "only the first cycle's committed row (id=1) survives; id=2 was rolled back");

    let _ = exec(&mut c, &format!("DROP TABLE {table}")).await;
}

// Case 6 (CSV all-or-nothing) is app-level and lives in T8's
// `dbc-ui::mssql_docker_tests` (backlog item 6) — noted here only so the
// numbering matches the design; nothing to author in this crate.
