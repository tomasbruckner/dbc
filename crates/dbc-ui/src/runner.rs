use std::time::{Duration, Instant};

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{CancelToken, Connection, QueryError, SchemaSnapshot, CHANNEL_CAPACITY};
use dbc_state::ConnectionConfig;

use crate::connect;

pub enum QueryEvent {
    Started { columns: SchemaRef },
    Batch(RecordBatch),
    Finished { elapsed: Duration },
    Failed(QueryError),
}

/// Where to connect from for a `connect_and_run` dispatch: either a saved
/// [`ConnectionConfig`] (Task 7's connection manager — may carry a secret
/// and/or an SSH tunnel), or the back-compat CLI-arg connection string.
pub enum ConnectSpec {
    Config { cfg: Box<ConnectionConfig>, secret: Option<String> },
    Url(String),
}

/// Owns the tokio runtime. All DB I/O lives here; the UI thread only ever
/// awaits the event channel from inside `cx.spawn`.
pub struct QueryRunner {
    runtime: tokio::runtime::Runtime,
}

impl QueryRunner {
    pub fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime"),
        }
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Connects (off the UI thread) and runs `sql`, reporting both the
    /// connect outcome and the query outcome over the same `QueryEvent`
    /// channel `on_run_query` already knows how to drain — a connect
    /// failure surfaces as `QueryEvent::Failed`, exactly like a query
    /// failure did before Task 8.
    ///
    /// - **Off the UI thread (I4 fix):** the whole tunnel-open + connect +
    ///   query sequence runs inside this runtime; the actual blocking work
    ///   (`Tunnel::open`'s child-process poll loop, `Handle::block_on` for
    ///   the Postgres handshake) happens inside `spawn_blocking`, which is
    ///   legal to block on (unlike a runtime worker task). The UI thread
    ///   only ever awaits this channel.
    /// - **Cancel-scoped checks between steps:** `cancel` is checked once
    ///   before connecting starts and once after the connect step returns
    ///   (before the query is issued) — the two points reachable without
    ///   reaching into the middle of the blocking connect call itself, which
    ///   `open_config`'s brief-mandated signature (`cfg, secret, runtime`,
    ///   no cancel token) doesn't thread a cancel check into. An Esc fired
    ///   while a connect is in flight (e.g. an unreachable host's TCP
    ///   timeout) is picked up at the next checkpoint and the eventual
    ///   connect result is discarded rather than surfaced to the UI.
    /// - **Timeout watchdog:** when `timeout_secs` is set, a
    ///   `tokio::time::sleep` races the *entire* query-and-drain sequence
    ///   (not just connecting). On firing it cancels `cancel` (the same
    ///   token passed to `Connection::query`, so drivers issue their normal
    ///   protocol-level cancel) and reports
    ///   `QueryError::msg("[timeout] query exceeded {t}s")`.
    pub fn connect_and_run(
        &self,
        spec: ConnectSpec,
        sql: String,
        cancel: CancelToken,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::mpsc::Receiver<QueryEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let handle = self.handle();
        self.runtime.spawn(async move {
            if cancel.is_cancelled() {
                let _ = tx.send(QueryEvent::Failed(QueryError::msg("cancelled"))).await;
                return;
            }

            let opened = match open_spec(spec, handle.clone()).await {
                Ok(opened) => opened,
                Err(e) => {
                    let _ = tx.send(QueryEvent::Failed(e)).await;
                    return;
                }
            };

            if cancel.is_cancelled() {
                // `opened` (and its tunnel, if any) drops here, tearing the
                // connection/tunnel down without ever running the query.
                let _ = tx.send(QueryEvent::Failed(QueryError::msg("cancelled"))).await;
                return;
            }

            let mut conn = opened.conn;
            let _tunnel = opened._tunnel;
            let started = Instant::now();
            let query_cancel = cancel.clone();

            let run = stream_query(&mut conn, &sql, query_cancel, &tx, started);

            match timeout_secs {
                Some(t) => {
                    tokio::select! {
                        _ = run => {}
                        _ = tokio::time::sleep(Duration::from_secs(t)) => {
                            cancel.cancel();
                            let _ = tx
                                .send(QueryEvent::Failed(QueryError::msg(format!(
                                    "[timeout] query exceeded {t}s"
                                ))))
                                .await;
                        }
                    }
                }
                None => run.await,
            }
        });
        rx
    }

    /// Connects (off the UI thread) using `spec` and immediately drops the
    /// resulting connection/tunnel — used by the connection-manager's Test
    /// button and dropdown connection-switch to validate a connection
    /// without blocking the UI thread (Task 8 review issue #1/#2). Reuses
    /// the exact `spawn_blocking(open_config(...))` dispatch
    /// `connect_and_run` uses for its connect step, via the shared
    /// `open_spec` helper, so both paths get the same `connect_timeout`
    /// bound and the same "blocking work never runs on a runtime worker
    /// thread" guarantee.
    ///
    /// No `CancelToken` is threaded through here: unlike a query, there is
    /// no in-flight query step to cancel, and `open_config`'s signature
    /// (brief-mandated: `cfg, secret, runtime`) doesn't accept one either —
    /// same limitation `connect_and_run`'s own connect step has (cancel is
    /// only checked before/after the blocking call, never during it). The
    /// `connect_timeout` bound is what actually caps how long this can run.
    pub fn test_connect(
        &self,
        spec: ConnectSpec,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = open_spec(spec, handle).await.map(|_opened| ());
            let _ = tx.send(result);
        });
        rx
    }

    /// Fetches a `SchemaSnapshot` for the tree panel (G2 Task 6): opens
    /// `spec` off the UI thread (same `open_spec` dispatch as
    /// `test_connect`/`connect_and_run`'s connect step), calls
    /// `Connection::schema()`, then drops the connection/tunnel — this is a
    /// one-shot fetch, not a held connection.
    pub fn fetch_schema(
        &self,
        spec: ConnectSpec,
    ) -> tokio::sync::oneshot::Receiver<Result<SchemaSnapshot, QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = match open_spec(spec, handle).await {
                Ok(mut opened) => opened.conn.schema().await,
                Err(e) => Err(e),
            };
            let _ = tx.send(result);
        });
        rx
    }

    /// G4 Task 5: one-shot batched lookup for the ad-hoc-tab FK join path
    /// (`fk_join::build_lookup_sql`'s output) — opens `spec` (same
    /// `open_spec` dispatch as every other one-shot here), runs `sql`, and
    /// drains the resulting `QueryStream` fully into materialized rows
    /// (`Vec<Vec<Option<String>>>`, `None` = SQL NULL) rather than streaming
    /// batches back — the caller (`AppView::start_lookup`) just needs a
    /// small `HashMap<value, row>` built from the WHOLE result, not
    /// incremental rendering. Draining goes through a throwaway
    /// `dbc_buffer::ResultBuffer` (reusing its tested batch-push/cell-read
    /// logic rather than re-implementing arrow value extraction here), and
    /// is capped at `LOOKUP_ROW_CAP` rows defensively — a lookup query is
    /// always `WHERE key IN (<= 1000 values)`, so a normal schema returns at
    /// most one row per value; the cap only guards against a pathological
    /// one-to-many FK "reference" or a misbehaving driver, not the expected
    /// case.
    pub fn fetch_lookup(
        &self,
        spec: ConnectSpec,
        sql: String,
    ) -> tokio::sync::oneshot::Receiver<Result<LookupResult, QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = fetch_lookup_inner(spec, sql, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    /// G5 Task 4: the sandbox Apply flow's execution — the app's ONLY write
    /// path. Opens ONE dedicated connection (same `open_spec` dispatch every
    /// other one-shot here uses) used EXCLUSIVELY for this BEGIN…COMMIT
    /// sequence and dropped the moment this future completes (`opened` goes
    /// out of scope at the end of `run_write_transaction_inner`) — this is
    /// what satisfies `Connection::execute`'s "session-sharing caveat" doc
    /// comment on `dbc-core`: no other `query()`/`execute()` call ever runs
    /// over this same connection while the transaction is open.
    ///
    /// `statements` is `sandbox::generate_statements`' output verbatim
    /// (`main.rs` builds it, this module stays decoupled from `sandbox`'s
    /// types — a plain `Vec<(String, Option<u64>)>` is all this needs).
    /// `timeout_secs` bounds the WHOLE sequence (not just the connect step),
    /// same "race the whole thing with `tokio::time::timeout`" shape
    /// `connect_and_run`'s watchdog uses.
    ///
    /// Returns the total affected-row count summed across every real
    /// statement (BEGIN/COMMIT/ROLLBACK don't contribute) on success — the
    /// brief's history-entry `row_count`.
    pub fn run_write_transaction(
        &self,
        spec: ConnectSpec,
        statements: Vec<(String, Option<u64>)>,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::oneshot::Receiver<Result<u64, QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_write_transaction_inner(spec, statements, timeout_secs, handle).await;
            let _ = tx.send(result);
        });
        rx
    }
}

/// Exact rollback message the brief mandates for an affected-rows mismatch
/// (PK-based optimistic check — a row changed/vanished under us since the
/// Apply dialog's statements were generated).
const AFFECTED_MISMATCH_MSG: &str = "řádek se mezitím změnil — aplikace zrušena";

/// G5 Task 4: pure guard behind `run_write_transaction`'s belt-and-braces
/// read-only refusal (brief contract #5). The UI never even offers edit
/// affordances for a read-only connection (`main.rs::detect_editable_pk`
/// excludes it), so this should be unreachable in practice — but the write
/// path is the app's ONLY write path and must refuse for itself too, not
/// rely solely on the UI gate upstream. Pure/GPUI-free so it's directly unit
/// tested without spinning up a connection.
pub fn guard_not_read_only(read_only: bool) -> Result<(), QueryError> {
    if read_only {
        Err(QueryError::msg("připojení je jen pro čtení"))
    } else {
        Ok(())
    }
}

/// `ConnectSpec::Config`'s `cfg.read_only`, or `false` for the CLI-arg URL
/// path (no read-only concept there — same convention
/// `main.rs::run_query_with` already applies when building its own spec).
fn spec_is_read_only(spec: &ConnectSpec) -> bool {
    match spec {
        ConnectSpec::Config { cfg, .. } => cfg.read_only,
        ConnectSpec::Url(_) => false,
    }
}

/// G5 Task 4: pure decision behind the per-statement affected-rows check in
/// `drive_write_sequence` — `expected` is `sandbox::generate_statements`'
/// per-statement `Option<u64>` (`Some(1)` for UPDATE/DELETE, `None` for
/// INSERT — the driver reports 1 but server triggers may differ, so INSERT
/// is never checked); `reported` is what `Connection::execute` actually
/// returned. `true` means "abort the whole transaction" (brief: PK-based
/// optimistic check). Pure/GPUI-free, unit tested directly.
pub fn affected_mismatch(expected: Option<u64>, reported: u64) -> bool {
    matches!(expected, Some(n) if n != reported)
}

/// G5 Task 4: the transaction driver core — BEGIN, each statement in order
/// (checking `affected_mismatch` when the statement carries an expectation),
/// COMMIT, all over the SAME `conn` (per `Connection::execute`'s
/// transaction-per-connection invariant). Stops at the FIRST error (a
/// mismatch synthesizes one via `AFFECTED_MISMATCH_MSG`) — per
/// `Connection::execute`'s documented engine divergence (SQLite leaves an
/// aborted transaction open and usable; PostgreSQL aborts every further
/// statement until ROLLBACK), so continuing past the first failure would
/// either silently succeed on the wrong data (SQLite) or just accumulate
/// "current transaction is aborted" noise (Postgres) — neither is useful,
/// and the trait docs mandate stopping regardless. The following ROLLBACK
/// attempt's own result is discarded (`let _ =`) — tolerated per the same
/// doc comment ("dropping the connection aborts the transaction server-side
/// on both engines" is the real backstop, this is best-effort).
///
/// Kept generic over `&mut dyn Connection` (not `ConnectSpec`/`open_spec`)
/// so it's testable by driving it directly over a `dbc-driver-sqlite`
/// connection opened via `crate::connect::open` against a temp file — no
/// live network/docker dependency, and no `dbc-driver-sqlite` import
/// outside `connect.rs` (the whole point of routing through `connect::open`
/// rather than constructing `SqliteConnection` here).
async fn drive_write_sequence(
    conn: &mut dyn Connection,
    statements: &[(String, Option<u64>)],
) -> Result<u64, QueryError> {
    let cancel = CancelToken::new();
    if let Err(e) = conn.execute("BEGIN", cancel.clone()).await {
        let _ = conn.execute("ROLLBACK", cancel.clone()).await;
        return Err(e);
    }
    let mut total: u64 = 0;
    for (sql, expected) in statements {
        match conn.execute(sql, cancel.clone()).await {
            Ok(affected) => {
                if affected_mismatch(*expected, affected) {
                    let _ = conn.execute("ROLLBACK", cancel.clone()).await;
                    return Err(QueryError::msg(AFFECTED_MISMATCH_MSG));
                }
                total += affected;
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", cancel.clone()).await;
                return Err(e);
            }
        }
    }
    if let Err(e) = conn.execute("COMMIT", cancel.clone()).await {
        let _ = conn.execute("ROLLBACK", cancel.clone()).await;
        return Err(e);
    }
    Ok(total)
}

/// G5 Task 4: `run_write_transaction`'s async body — guard, open, drive,
/// (optionally) time-bound. See `run_write_transaction`'s doc comment for
/// the connection-lifetime/decoupling rationale.
async fn run_write_transaction_inner(
    spec: ConnectSpec,
    statements: Vec<(String, Option<u64>)>,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
) -> Result<u64, QueryError> {
    guard_not_read_only(spec_is_read_only(&spec))?;
    let mut opened = open_spec(spec, handle).await?;
    match timeout_secs {
        Some(t) => {
            let sequence = drive_write_sequence(&mut *opened.conn, &statements);
            match tokio::time::timeout(Duration::from_secs(t), sequence).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    // Best-effort rollback (brief: "on timeout attempt
                    // ROLLBACK"), tolerated if it fails.
                    //
                    // Caveat, honestly flagged rather than hidden: the T1
                    // sqlite driver's `execute` design note says "no
                    // mid-statement interrupt needed for v1 — statements are
                    // tiny" — it takes its persistent connection out of
                    // `self.exec_conn` for the duration of one call and puts
                    // it back only after that call's future resolves. If
                    // THIS timeout fires while a statement's `execute()` is
                    // still in flight, dropping `sequence` above (which
                    // `tokio::time::timeout` does internally on the timeout
                    // branch) can leave `exec_conn` reset to `None`, so this
                    // ROLLBACK may run over a FRESH underlying handle with
                    // nothing to roll back (a harmless no-op/error, still
                    // tolerated) rather than the one with the open
                    // transaction. That abandoned handle's own transaction
                    // is still aborted once its connection object is
                    // dropped at the end of the now-orphaned blocking
                    // closure — the same "dropping aborts server-side"
                    // backstop `Connection::execute`'s doc comment relies on
                    // — so correctness doesn't depend on this ROLLBACK
                    // landing on the right handle, only on `opened` being
                    // dropped (which happens unconditionally when this
                    // function returns).
                    let _ = opened.conn.execute("ROLLBACK", CancelToken::new()).await;
                    Err(QueryError::msg(format!("[timeout] aplikace překročila {t}s")))
                }
            }
        }
        None => drive_write_sequence(&mut *opened.conn, &statements).await,
    }
}

/// Defensive cap on materialized lookup rows — see `QueryRunner::fetch_lookup`.
const LOOKUP_ROW_CAP: usize = 100_000;

/// `(column names, rows)` — `rows[r][c]` is `None` for a real SQL NULL.
/// `rows[r][0]` is always the key column (see `fk_join::build_lookup_sql`,
/// which puts it first); `rows[r][1..]` line up with the caller's
/// `wanted_cols`, in order.
type LookupResult = (Vec<String>, Vec<Vec<Option<String>>>);

async fn fetch_lookup_inner(
    spec: ConnectSpec,
    sql: String,
    handle: tokio::runtime::Handle,
) -> Result<LookupResult, QueryError> {
    let mut opened = open_spec(spec, handle).await?;
    let mut stream = opened.conn.query(&sql, CancelToken::new()).await?;
    let col_names: Vec<String> =
        stream.columns.fields().iter().map(|f| f.name().to_string()).collect();
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        match item {
            Ok(b) => {
                buf.push(b).map_err(|e| QueryError::msg(e.to_string()))?;
                if buf.row_count() >= LOOKUP_ROW_CAP {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    let n = buf.row_count().min(LOOKUP_ROW_CAP);
    let ncols = buf.column_count();
    let mut rows = Vec::with_capacity(n);
    for r in 0..n {
        let mut row = Vec::with_capacity(ncols);
        for c in 0..ncols {
            row.push(if buf.cell_is_null(r, c) { None } else { Some(buf.cell_text(r, c)) });
        }
        rows.push(row);
    }
    Ok((col_names, rows))
}

/// Dispatches a `ConnectSpec` to the right driver inside `spawn_blocking`
/// (legal to block there; not on a runtime worker thread) — shared by
/// `connect_and_run`'s connect step and `test_connect`, so both get the
/// same `connect_timeout` bound and panic handling.
async fn open_spec(
    spec: ConnectSpec,
    handle: tokio::runtime::Handle,
) -> Result<connect::OpenConnection, QueryError> {
    let blocking_handle = handle.clone();
    let opened = tokio::task::spawn_blocking(move || match spec {
        ConnectSpec::Config { cfg, secret } => connect::open_config(&cfg, secret, &blocking_handle),
        ConnectSpec::Url(url) => connect::open(&url, &blocking_handle)
            .map(|conn| connect::OpenConnection { conn, _tunnel: None }),
    })
    .await;

    match opened {
        Ok(Ok(opened)) => Ok(opened),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(QueryError::msg("connect task panicked")),
    }
}

/// Runs `sql` on an already-open connection and streams the result over
/// `tx` as `QueryEvent`s: `Started` once columns are known, then `Batch` per
/// arrow batch, then `Finished` — or `Failed` at whichever step errors.
/// Factored out of `connect_and_run` so the timeout watchdog above can race
/// the whole thing with `tokio::select!`.
async fn stream_query(
    conn: &mut Box<dyn Connection>,
    sql: &str,
    cancel: CancelToken,
    tx: &tokio::sync::mpsc::Sender<QueryEvent>,
    started: Instant,
) {
    match conn.query(sql, cancel).await {
        Err(e) => {
            let _ = tx.send(QueryEvent::Failed(e)).await;
        }
        Ok(mut stream) => {
            let _ = tx.send(QueryEvent::Started { columns: stream.columns.clone() }).await;
            let mut failed = false;
            while let Some(item) = stream.batches.recv().await {
                match item {
                    Ok(b) => {
                        let _ = tx.send(QueryEvent::Batch(b)).await;
                    }
                    Err(e) => {
                        let _ = tx.send(QueryEvent::Failed(e)).await;
                        failed = true;
                        // Contract (see Connection::query doc-comment): after
                        // sending an Err batch, the driver stops sending and
                        // drops its Sender. Don't keep draining past the
                        // first error — break rather than rely solely on the
                        // driver closing the channel.
                        break;
                    }
                }
            }
            if !failed {
                let _ = tx.send(QueryEvent::Finished { elapsed: started.elapsed() }).await;
            }
        }
    }
}

/// G5 Task 4: pure guard/decision tests (no I/O) plus `drive_write_sequence`
/// tests driven over the real sqlite driver via `crate::connect::open` (a
/// temp-file database — no docker/network dependency), matching the plan's
/// "drive it over the sqlite driver in a temp file" guidance. Every test
/// here reads back through the SAME `Box<dyn Connection>` the sequence just
/// ran over, so a rolled-back statement's absence is verified against the
/// live handle, not a fresh reconnect (which would trivially see a
/// committed-by-sqlite-autocommit row even if `BEGIN`/`ROLLBACK` hadn't
/// actually shared a connection).
#[cfg(test)]
mod write_transaction_tests {
    use super::*;

    #[test]
    fn affected_mismatch_pure() {
        // INSERT (no expectation) never mismatches, whatever the driver
        // reports.
        assert!(!affected_mismatch(None, 0));
        assert!(!affected_mismatch(None, 1));
        assert!(!affected_mismatch(None, 5));
        // UPDATE/DELETE (Some(1)) matches only an exact 1.
        assert!(!affected_mismatch(Some(1), 1));
        assert!(affected_mismatch(Some(1), 0));
        assert!(affected_mismatch(Some(1), 2));
    }

    #[test]
    fn guard_not_read_only_pure() {
        assert!(guard_not_read_only(false).is_ok());
        let err = guard_not_read_only(true).unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[test]
    fn spec_is_read_only_reads_cfg_and_defaults_url_to_writable() {
        let cfg = dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            read_only: true,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
        };
        assert!(spec_is_read_only(&ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: None }));
        let mut cfg2 = cfg;
        cfg2.read_only = false;
        assert!(!spec_is_read_only(&ConnectSpec::Config { cfg: Box::new(cfg2), secret: None }));
        assert!(!spec_is_read_only(&ConnectSpec::Url("irrelevant".into())));
    }

    /// Opens a fresh temp-file sqlite connection via `crate::connect::open`
    /// (the ONLY sanctioned driver-crate entry point outside `connect.rs` —
    /// see that module's own doc comment) — the `NamedTempFile` must be
    /// returned alongside so it isn't deleted (and the path invalidated)
    /// while the connection is still in use.
    async fn open_sqlite_test_conn() -> (tempfile::NamedTempFile, Box<dyn Connection>) {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let conn =
            crate::connect::open(f.path().to_str().expect("utf8 temp path"), &handle).expect("open sqlite");
        (f, conn)
    }

    /// Reads back a single text cell via `dbc_buffer::ResultBuffer` (same
    /// drain pattern `fetch_lookup_inner` already uses) — `None` when the
    /// query returned no rows.
    async fn read_one(conn: &mut dyn Connection, sql: &str) -> Option<String> {
        let mut stream = conn.query(sql, CancelToken::new()).await.expect("query");
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.expect("batch")).expect("push");
        }
        if buf.row_count() == 0 {
            None
        } else if buf.cell_is_null(0, 0) {
            Some("<NULL>".to_string())
        } else {
            Some(buf.cell_text(0, 0))
        }
    }

    #[tokio::test]
    async fn drive_write_sequence_commits_on_success_over_one_connection() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        conn.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        let stmts = vec![
            ("UPDATE t SET name = 'b' WHERE id = 1".to_string(), Some(1)),
            ("INSERT INTO t(id, name) VALUES (2, 'c')".to_string(), None),
        ];
        let total = drive_write_sequence(&mut *conn, &stmts).await.unwrap();
        // 1 (the UPDATE's reported affected rows) + 1 (the INSERT's, even
        // though INSERT carries no expectation — the driver still reports
        // it, and it still counts toward the total).
        assert_eq!(total, 2);

        assert_eq!(read_one(&mut *conn, "SELECT name FROM t WHERE id = 1").await, Some("b".to_string()));
        assert_eq!(read_one(&mut *conn, "SELECT name FROM t WHERE id = 2").await, Some("c".to_string()));
    }

    #[tokio::test]
    async fn drive_write_sequence_rolls_back_whole_transaction_on_affected_mismatch() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        conn.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        // First statement succeeds and matches; second expects 2 affected
        // but only 1 row matches -> mismatch -> the WHOLE transaction
        // (including the first, already-successful statement) rolls back.
        let stmts = vec![
            ("UPDATE t SET name = 'b' WHERE id = 1".to_string(), Some(1)),
            ("UPDATE t SET name = 'z' WHERE id = 1".to_string(), Some(2)),
        ];
        let err = drive_write_sequence(&mut *conn, &stmts).await.unwrap_err();
        assert_eq!(err.message, AFFECTED_MISMATCH_MSG);

        assert_eq!(read_one(&mut *conn, "SELECT name FROM t WHERE id = 1").await, Some("a".to_string()));
    }

    #[tokio::test]
    async fn drive_write_sequence_rolls_back_whole_transaction_on_statement_error() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        conn.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        // First statement succeeds; second is invalid SQL (unknown table) —
        // stops at the first error, rolls back the first statement too.
        let stmts = vec![
            ("UPDATE t SET name = 'b' WHERE id = 1".to_string(), Some(1)),
            ("UPDATE no_such_table SET name = 'x'".to_string(), None),
        ];
        let err = drive_write_sequence(&mut *conn, &stmts).await.unwrap_err();
        assert_ne!(err.message, AFFECTED_MISMATCH_MSG);

        assert_eq!(read_one(&mut *conn, "SELECT name FROM t WHERE id = 1").await, Some("a".to_string()));
    }

    #[tokio::test]
    async fn drive_write_sequence_empty_statements_still_begins_and_commits() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        let total = drive_write_sequence(&mut *conn, &[]).await.unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn run_write_transaction_refuses_read_only_connection_without_connecting() {
        let cfg = dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            // A path that doesn't exist and isn't creatable under a real
            // sqlite driver would fail differently (a connect/open error) —
            // using one here would make this test ambiguous about WHICH
            // error fired. `guard_not_read_only` runs before `open_spec` is
            // ever called, so this never actually touches the filesystem;
            // a nonsense path is fine and proves that.
            database: "\0invalid".into(),
            host: String::new(),
            port: None,
            user: String::new(),
            read_only: true,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
        };
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        // Exercises `run_write_transaction_inner` (the same body
        // `QueryRunner::run_write_transaction` spawns) directly over the
        // CURRENT test runtime's handle — constructing a whole second
        // `QueryRunner` here would build (and, at end of scope, drop) its
        // own nested multi-thread `tokio::runtime::Runtime`, which tokio
        // forbids doing synchronously from inside an async context.
        let handle = tokio::runtime::Handle::current();
        let err = run_write_transaction_inner(spec, Vec::new(), None, handle).await.unwrap_err();
        assert!(!err.message.is_empty());
    }
}
