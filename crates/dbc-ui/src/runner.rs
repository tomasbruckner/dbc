use std::time::{Duration, Instant};

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{CancelToken, Connection, QueryError, SchemaSnapshot, CHANNEL_CAPACITY};
use dbc_state::ConnectionConfig;

use crate::connect;
use crate::monitor;

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

/// G9 T3: commands the view (T4) sends into a held `open_monitor` loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorCmd {
    Refresh { generation: u64 },
    Kill { generation: u64, pid: i64 },
}

/// G9 T3: events the `monitor_loop` background task sends back.
#[derive(Debug)]
pub enum MonitorEvent {
    Data { generation: u64, snapshot: monitor::MonitorSnapshot },
    Error { generation: u64, message: String },
    KillResult {
        /// G9 T4 review finding: carried for symmetry with Data/Error and
        /// asserted by T3's own tests, but deliberately NOT read by
        /// `MonitorView::on_event` — a kill outcome is never superseded by
        /// refresh generations (design §4 gates Data/Error only, not
        /// KillResult). Plain `cargo build` (no test cfg) has no reader at
        /// all, hence the explicit allow.
        #[allow(dead_code)]
        generation: u64,
        pid: i64,
        result: Result<u64, QueryError>,
    },
}

/// Design §0/§9.1: the app-level read_only flag is the ONLY kill
/// enforcement — this exact message is what the background task returns
/// when it independently refuses a Kill.
pub const MONITOR_READ_ONLY_KILL_MSG: &str =
    "spojení je pouze pro čtení — zabití procesu odmítnuto";

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

    /// G9 T3: opens ONE dedicated connection held for the monitor tab's
    /// lifetime — no reconnect per tick (design §4). `read_only` and
    /// `engine` are captured once at open time; the background task refuses
    /// Kill on its OWN captured `read_only`, independent of whatever the UI
    /// renders (belt-and-braces, design §6). Dropping the returned `Sender`
    /// ends the loop and drops the connection ("drop tears everything
    /// down", same as OpenConnection/Tunnel).
    pub fn open_monitor(
        &self,
        spec: ConnectSpec,
        read_only: bool,
        engine: dbc_state::Engine,
    ) -> (tokio::sync::mpsc::Sender<MonitorCmd>, tokio::sync::mpsc::Receiver<MonitorEvent>) {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let handle = self.handle();
        self.runtime.spawn(async move {
            let opened = match open_spec(spec, handle).await {
                Ok(o) => o,
                Err(e) => {
                    // Report against the FIRST dispatched command's
                    // generation (MonitorView sends Refresh{1} immediately
                    // on open, T4) so the view's generation match doesn't
                    // silently drop the connect error. If the tab already
                    // closed, just exit.
                    if let Some(cmd) = cmd_rx.recv().await {
                        let generation = match cmd {
                            MonitorCmd::Refresh { generation } | MonitorCmd::Kill { generation, .. } => generation,
                        };
                        let _ = event_tx.send(MonitorEvent::Error { generation, message: e.message }).await;
                    }
                    return;
                }
            };
            // Keep the tunnel (if any) alive for the whole loop lifetime.
            let _tunnel = opened._tunnel;
            monitor_loop(opened.conn, engine, read_only, cmd_rx, event_tx).await;
            // conn + _tunnel drop here — DB session closed (design §4's
            // "no explicit Close command needed").
        });
        (cmd_tx, event_rx)
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
/// `cancel` is threaded through to EVERY `execute()` call in the sequence
/// (T4 review round 1, MAJOR 2) — `drive_write_sequence_bounded` cancels
/// this SAME token when its outer timeout fires, and — since T4 review round
/// 1 also gave `PostgresConnection::execute` the same protocol-level cancel
/// watcher `query()` already has — that reaches the backend for real on
/// Postgres, instead of merely being checked once before dispatch (sqlite's
/// pre-existing "no mid-statement interrupt — statements are tiny" design).
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
    cancel: CancelToken,
) -> Result<u64, QueryError> {
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

/// T4 review round 1, MAJOR 2: bound on the post-timeout ROLLBACK attempt
/// below — independent of, and much shorter than, `timeout_secs` itself.
/// Without this, a ROLLBACK that itself never resolves (e.g. queued behind a
/// statement still "in flight" server-side — see `drive_write_sequence_bounded`'s
/// doc comment) would make `run_write_transaction` hang FOREVER: the
/// `oneshot::Receiver` it returns never resolves, the Apply dialog's
/// `running` flag is stuck `true`, and even Esc is disabled (per
/// `AppView::on_cancel_query`'s "no cancellation support while running"
/// guard) — a fully wedged UI. This constant guarantees the function ALWAYS
/// returns within `timeout_secs + ROLLBACK_GRACE_SECS`, no exceptions.
const ROLLBACK_GRACE_SECS: u64 = 5;

/// G5 Task 4: runs `drive_write_sequence` bounded by `timeout_secs` — races
/// it with `tokio::time::timeout`, and on expiry cancels `cancel` (reaching
/// the backend for real on Postgres, see `drive_write_sequence`'s doc
/// comment) and attempts a best-effort ROLLBACK.
///
/// T4 review round 1, MAJOR 2 (both parts, this function is where they
/// meet): (a) that ROLLBACK attempt is ITSELF wrapped in a short
/// `ROLLBACK_GRACE_SECS` timeout — on ITS expiry this function still returns
/// the timeout error immediately rather than continuing to wait, tolerating
/// the ROLLBACK's own failure exactly like `drive_write_sequence` already
/// tolerates one; the caller-owned `conn` is dropped once this function
/// returns either way, which aborts the transaction server-side on both
/// engines regardless of whether the explicit ROLLBACK ever completed
/// (`Connection::execute`'s doc comment) — so THIS function's return is
/// unconditionally bounded even in the worst case where nothing else
/// worked. (b) `cancel.cancel()` fires before the ROLLBACK attempt, over the
/// SAME token every statement's `execute()` call received, so a Postgres
/// backend genuinely gets asked to abort whatever statement was still
/// running when the outer timeout fired — see `drive_write_sequence`'s doc
/// comment and `dbc-driver-postgres`'s `execute()`.
///
/// Extracted as its own function — separate from `run_write_transaction_inner`
/// — so the "always returns, even when the connection's ROLLBACK hangs"
/// property is directly testable against a mock `Connection` (no live
/// Postgres, no `ConnectSpec`/`open_spec` needed).
async fn drive_write_sequence_bounded(
    conn: &mut dyn Connection,
    statements: &[(String, Option<u64>)],
    cancel: CancelToken,
    timeout_secs: Option<u64>,
) -> Result<u64, QueryError> {
    match timeout_secs {
        Some(t) => {
            let sequence = drive_write_sequence(conn, statements, cancel.clone());
            match tokio::time::timeout(Duration::from_secs(t), sequence).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    // (b): ask the backend itself to abort whatever
                    // statement was still in flight — see this function's
                    // doc comment.
                    cancel.cancel();
                    // (a): bounded best-effort rollback — see this
                    // function's doc comment for why this must NOT be
                    // allowed to hang the whole function.
                    let rollback = conn.execute("ROLLBACK", CancelToken::new());
                    let _ =
                        tokio::time::timeout(Duration::from_secs(ROLLBACK_GRACE_SECS), rollback)
                            .await;
                    Err(QueryError::msg(format!("[timeout] aplikace překročila {t}s")))
                }
            }
        }
        None => drive_write_sequence(conn, statements, cancel).await,
    }
}

/// G5 Task 4: `run_write_transaction`'s async body — guard, open, drive
/// (bounded). See `run_write_transaction`'s doc comment for the
/// connection-lifetime/decoupling rationale, and `drive_write_sequence_bounded`
/// for the timeout/cancel/rollback mechanics.
async fn run_write_transaction_inner(
    spec: ConnectSpec,
    statements: Vec<(String, Option<u64>)>,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
) -> Result<u64, QueryError> {
    guard_not_read_only(spec_is_read_only(&spec))?;
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();
    drive_write_sequence_bounded(&mut *opened.conn, &statements, cancel, timeout_secs).await
    // `opened` (connection + tunnel) drops here unconditionally, tearing the
    // connection down — the ultimate backstop regardless of how the write
    // sequence above resolved.
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

/// Bounds a runaway monitor sub-query result (RUNNING/TABLES are already
/// LIMIT/TOP-bounded server-side; this is the defensive client-side cap,
/// same posture as LOOKUP_ROW_CAP above).
const MONITOR_ROW_CAP: usize = 10_000;

/// Free function (not a method) so tests drive it over a mock Connection
/// directly — the REQUIRED read-only kill refusal test (design §9.1
/// CURATION) depends on this seam existing.
async fn monitor_loop(
    mut conn: Box<dyn Connection>,
    engine: dbc_state::Engine,
    read_only: bool,
    mut cmd_rx: tokio::sync::mpsc::Receiver<MonitorCmd>,
    event_tx: tokio::sync::mpsc::Sender<MonitorEvent>,
) {
    let mut pending: Option<MonitorCmd> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => match cmd_rx.recv().await {
                Some(c) => c,
                None => return, // Sender dropped = tab closed (design §4)
            },
        };
        match cmd {
            MonitorCmd::Refresh { generation } => {
                let cancel = CancelToken::new();
                let refresh = run_monitor_refresh(&mut *conn, engine, cancel.clone());
                // Race the refresh against channel activity — same
                // tokio::select! shape connect_and_run's watchdog uses
                // (design §4 "cancellation of an in-flight refresh").
                tokio::select! {
                    results = refresh => {
                        let event = match monitor::assemble_snapshot(results, Instant::now()) {
                            Ok(snapshot) => MonitorEvent::Data { generation, snapshot },
                            Err(message) => MonitorEvent::Error { generation, message },
                        };
                        if event_tx.send(event).await.is_err() {
                            return; // receiver (MonitorView) gone
                        }
                    }
                    next = cmd_rx.recv() => {
                        // A command arrived mid-refresh (only Kill or a
                        // close can — the UI's `awaiting` flag blocks new
                        // Refresh dispatches, design §4 overlap prevention).
                        cancel.cancel(); // protocol-level cancel on pg
                        match next {
                            Some(c) => pending = Some(c),
                            None => return, // tab closed mid-refresh: no stale Data/Error sent
                        }
                    }
                }
            }
            MonitorCmd::Kill { generation, pid } => {
                // BELT-AND-BRACES GATE (design §0/§6/§9.1): this check is
                // one of the TWO independent code paths implementing the
                // ONLY real enforcement — neither engine blocks kill
                // server-side (pg's default_transaction_read_only does NOT
                // stop pg_terminate_backend). It must run BEFORE any
                // conn.execute call. Deliberately a direct check rather
                // than guard_not_read_only() so the design's mandated
                // message text is exact.
                let result = if read_only {
                    Err(QueryError::msg(MONITOR_READ_ONLY_KILL_MSG))
                } else {
                    match crate::monitor_sql::kill_sql(engine, pid) {
                        Some(sql) => conn.execute(&sql, CancelToken::new()).await,
                        None => Err(QueryError::msg("kill není pro tento engine k dispozici")),
                    }
                };
                if event_tx.send(MonitorEvent::KillResult { generation, pid, result }).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// One full refresh: the 8 pg statements, strictly sequential over the ONE
/// dedicated connection (session-sharing caveat), each failure captured
/// per-statement so assemble_snapshot can degrade per-tile (risk #4).
async fn run_monitor_refresh(
    conn: &mut dyn Connection,
    engine: dbc_state::Engine,
    cancel: CancelToken,
) -> monitor::RefreshResults {
    use crate::monitor_sql::pg;
    match engine {
        dbc_state::Engine::Postgres => monitor::RefreshResults {
            connections: drain_rows(conn, pg::CONNECTIONS, &cancel).await,
            locks: drain_rows(conn, pg::LOCKS, &cancel).await,
            data_size: drain_rows(conn, pg::DATA_SIZE, &cancel).await,
            wal_size: drain_rows(conn, pg::WAL_SIZE, &cancel).await,
            perf: drain_rows(conn, pg::PERF, &cancel).await,
            running: drain_rows(conn, pg::RUNNING, &cancel).await,
            blocking: drain_rows(conn, pg::BLOCKING, &cancel).await,
            tables: drain_rows(conn, pg::TABLES, &cancel).await,
        },
        // Unreachable today: monitor_available gates open_monitor to
        // Postgres. When dbc-driver-mssql lands, this arm switches to the
        // monitor_sql::mssql statement set (design §7).
        _ => {
            let err = || Err("monitor není pro tento engine k dispozici".to_string());
            monitor::RefreshResults {
                connections: err(), locks: err(), data_size: err(), wal_size: err(),
                perf: err(), running: err(), blocking: err(), tables: err(),
            }
        }
    }
}

/// One statement -> materialized text rows. Mirrors fetch_lookup_inner's
/// throwaway-ResultBuffer drain (the tested batch-push/cell-read path, not
/// a second arrow-reading code path — design §1 parse strategy), but over
/// an EXISTING connection and returning the error as a plain message
/// String (RefreshResults' per-statement Err type).
async fn drain_rows(
    conn: &mut dyn Connection,
    sql: &str,
    cancel: &CancelToken,
) -> Result<Vec<monitor::Row>, String> {
    let mut stream = conn.query(sql, cancel.clone()).await.map_err(|e| e.message)?;
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        match item {
            Ok(b) => {
                buf.push(b).map_err(|e| e.to_string())?;
                if buf.row_count() >= MONITOR_ROW_CAP {
                    break;
                }
            }
            Err(e) => return Err(e.message),
        }
    }
    let n = buf.row_count().min(MONITOR_ROW_CAP);
    let ncols = buf.column_count();
    let mut rows = Vec::with_capacity(n);
    for r in 0..n {
        let mut row = Vec::with_capacity(ncols);
        for c in 0..ncols {
            row.push(if buf.cell_is_null(r, c) { None } else { Some(buf.cell_text(r, c)) });
        }
        rows.push(row);
    }
    Ok(rows)
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
        let total = drive_write_sequence(&mut *conn, &stmts, CancelToken::new()).await.unwrap();
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
        let err = drive_write_sequence(&mut *conn, &stmts, CancelToken::new()).await.unwrap_err();
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
        let err = drive_write_sequence(&mut *conn, &stmts, CancelToken::new()).await.unwrap_err();
        assert_ne!(err.message, AFFECTED_MISMATCH_MSG);

        assert_eq!(read_one(&mut *conn, "SELECT name FROM t WHERE id = 1").await, Some("a".to_string()));
    }

    #[tokio::test]
    async fn drive_write_sequence_empty_statements_still_begins_and_commits() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        let total = drive_write_sequence(&mut *conn, &[], CancelToken::new()).await.unwrap();
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

    /// T4 review round 1, MAJOR 2: a mock `Connection` whose `execute()`
    /// calls never resolve on their own (`ROLLBACK` included) — simulates a
    /// Postgres backend still busy with a statement that was "in flight"
    /// when the outer timeout fired, the exact scenario that could
    /// previously make `run_write_transaction` hang forever. `query()`/
    /// `schema()` aren't exercised by this test; they return an error rather
    /// than panicking if something unexpectedly calls them.
    struct HangingConnection {
        rollback_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Connection for HangingConnection {
        async fn query(
            &mut self,
            _sql: &str,
            _cancel: CancelToken,
        ) -> Result<dbc_core::QueryStream, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn execute(&mut self, sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
            if sql == "BEGIN" {
                return Ok(0);
            }
            if sql == "ROLLBACK" {
                self.rollback_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            // Never resolves on its own — only a surrounding timeout (in
            // `drive_write_sequence_bounded`) can end this call.
            tokio::time::sleep(Duration::from_secs(9_999)).await;
            unreachable!("must be bounded by drive_write_sequence_bounded's own timeouts");
        }
    }

    /// T4 review round 1, MAJOR 2 (both parts): proves
    /// `drive_write_sequence_bounded` ALWAYS returns — within
    /// `timeout_secs + ROLLBACK_GRACE_SECS` — even when the statement AND
    /// the post-timeout ROLLBACK attempt both hang on the underlying
    /// connection, and that the `cancel` token threaded through every
    /// `execute()` call is actually cancelled once the outer timeout fires
    /// (part (b) — what makes `dbc-driver-postgres`'s new cancel watcher
    /// reachable). Runs under a paused/virtual clock
    /// (`#[tokio::test(start_paused = true)]`, `tokio` `test-util` dev-dep
    /// feature) so the test completes near-instantly in real wall-clock
    /// time while still exercising the genuine timeout/grace-period
    /// durations.
    #[tokio::test(start_paused = true)]
    async fn drive_write_sequence_bounded_always_returns_even_when_rollback_hangs() {
        let rollback_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut conn = HangingConnection { rollback_calls: rollback_calls.clone() };
        let stmts = vec![("UPDATE t SET x = 1 WHERE id = 1".to_string(), Some(1))];
        let cancel = CancelToken::new();

        let start = tokio::time::Instant::now();
        let result =
            drive_write_sequence_bounded(&mut conn, &stmts, cancel.clone(), Some(1)).await;
        let elapsed = start.elapsed();

        let err = result.unwrap_err();
        assert!(err.message.contains("timeout"), "unexpected error: {}", err.message);
        // Bounded by timeout_secs (1s) + ROLLBACK_GRACE_SECS (5s) — NOT by
        // the connection's simulated 9999s hang on either call.
        assert!(
            elapsed <= Duration::from_secs(1 + ROLLBACK_GRACE_SECS + 1),
            "took {elapsed:?}, should have been bounded"
        );
        assert_eq!(
            rollback_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "ROLLBACK must still be attempted exactly once after the outer timeout fires"
        );
        assert!(
            cancel.is_cancelled(),
            "the SAME cancel token threaded through every execute() call must be cancelled on \
             timeout — this is what reaches the backend for real on Postgres (part (b) of the fix)"
        );
    }
}

/// G9 T3: the kill gate + loop-lifecycle tests. The read-only refusal test
/// is the design's §9.1 CURATION-REQUIRED guard-level test — the app-level
/// flag is the ONLY enforcement (no server-side backstop on either
/// engine), so it gets the same test discipline as guards.rs.
#[cfg(test)]
mod monitor_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Mock driver: records every execute() call; query() errors (a test
    /// that never sends Refresh never reaches it).
    struct RecordingConnection {
        execute_calls: Arc<AtomicUsize>,
        executed_sql: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingConnection {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let sqls = Arc::new(Mutex::new(Vec::new()));
            (
                Self { execute_calls: calls.clone(), executed_sql: sqls.clone() },
                calls,
                sqls,
            )
        }
    }

    #[async_trait::async_trait]
    impl Connection for RecordingConnection {
        async fn query(
            &mut self,
            _sql: &str,
            _cancel: CancelToken,
        ) -> Result<dbc_core::QueryStream, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn execute(&mut self, sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            self.executed_sql.lock().unwrap().push(sql.to_string());
            Ok(1)
        }
    }

    /// REQUIRED (design §9.1 CURATION): a Kill over a read_only connection
    /// is refused BEFORE reaching the driver — conn.execute is never
    /// called, and the exact Czech refusal message comes back, independent
    /// of whatever the UI renders.
    #[tokio::test]
    async fn monitor_kill_refused_on_read_only_before_reaching_driver() {
        let (conn, calls, _sqls) = RecordingConnection::new();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            Box::new(conn),
            dbc_state::Engine::Postgres,
            /* read_only */ true,
            cmd_rx,
            event_tx,
        ));

        cmd_tx.send(MonitorCmd::Kill { generation: 7, pid: 42 }).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event within 5s")
            .expect("channel open");
        match ev {
            MonitorEvent::KillResult { generation, pid, result } => {
                assert_eq!(generation, 7);
                assert_eq!(pid, 42);
                assert_eq!(result.unwrap_err().message, MONITOR_READ_ONLY_KILL_MSG);
            }
            other => panic!("expected KillResult, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "read-only kill must never reach Connection::execute"
        );

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("loop exits when the Sender drops")
            .unwrap();
    }

    /// Companion positive case: writable connection -> exactly one
    /// execute() with the exact pid-formatted kill SQL, Ok result echoed.
    #[tokio::test]
    async fn monitor_kill_executes_exact_sql_on_writable_connection() {
        let (conn, calls, sqls) = RecordingConnection::new();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            Box::new(conn),
            dbc_state::Engine::Postgres,
            /* read_only */ false,
            cmd_rx,
            event_tx,
        ));

        cmd_tx.send(MonitorCmd::Kill { generation: 1, pid: 42 }).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            MonitorEvent::KillResult { pid: 42, result: Ok(1), .. } => {}
            other => panic!("expected Ok KillResult, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sqls.lock().unwrap().as_slice(),
            &["SELECT pg_terminate_backend(42)".to_string()]
        );

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.unwrap().unwrap();
    }

    /// End-to-end all-failed path without docker: a real sqlite connection
    /// can't run any pg catalog query, so every drain fails and the loop
    /// must send Error (with the dispatched generation), not Data and not
    /// a panic — proving assemble_snapshot's all-failed contract through
    /// the real drain path.
    #[tokio::test]
    async fn monitor_refresh_all_queries_failing_sends_error_with_generation() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let conn =
            crate::connect::open(f.path().to_str().expect("utf8 path"), &handle).expect("open sqlite");
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            conn,
            dbc_state::Engine::Postgres,
            false,
            cmd_rx,
            event_tx,
        ));

        cmd_tx.send(MonitorCmd::Refresh { generation: 3 }).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(10), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            MonitorEvent::Error { generation: 3, message } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected Error{{generation: 3}}, got {other:?}"),
        }

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.unwrap().unwrap();
        drop(f);
    }
}
