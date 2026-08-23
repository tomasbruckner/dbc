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

    /// G13 CURATION item 2: the app's THIRD sanctioned write path (after
    /// G5's Apply flow and G9's kill flow) — a dedicated one-shot
    /// connection, BEGIN -> the EXPLAIN ANALYZE query -> ROLLBACK, ALWAYS
    /// (never COMMIT — the whole point is to measure real execution
    /// without keeping the effects). Belt-and-braces: refuses on
    /// `spec_is_read_only(&spec)` itself, independent of whatever gate the
    /// caller already applied (`plan::analyze_gate`).
    pub fn run_analyze_write(
        &self,
        spec: ConnectSpec,
        explain_analyze_sql: String,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::oneshot::Receiver<Result<String, QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_analyze_write_inner(spec, explain_analyze_sql, timeout_secs, handle).await;
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

    /// G7 T5: two independent one-shot schema fetches, run CONCURRENTLY
    /// (`tokio::join!`), reusing `open_spec` unchanged — the same
    /// "ephemeral one-shot connection, opened and dropped" pattern
    /// `fetch_schema`/`fetch_lookup`/`test_connect` already use, just issued
    /// twice. Neither leg touches `active_connection_id`. Each `Result` is
    /// independent — a failure on one side does not cancel or block the
    /// other.
    ///
    /// G7 T6 wired the first real call site
    /// (`connections_ui::AppView::confirm_compare_dialog`) — the
    /// `#[allow(dead_code)]` this carried through T5 is removed.
    pub fn fetch_schema_pair(
        &self,
        spec_a: ConnectSpec,
        spec_b: ConnectSpec,
    ) -> tokio::sync::oneshot::Receiver<(Result<SchemaSnapshot, QueryError>, Result<SchemaSnapshot, QueryError>)>
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle_a = self.handle();
        let handle_b = self.handle();
        self.runtime.spawn(async move {
            let fetch_a = async {
                match open_spec(spec_a, handle_a).await {
                    Ok(mut opened) => opened.conn.schema().await,
                    Err(e) => Err(e),
                }
            };
            let fetch_b = async {
                match open_spec(spec_b, handle_b).await {
                    Ok(mut opened) => opened.conn.schema().await,
                    Err(e) => Err(e),
                }
            };
            let (result_a, result_b) = tokio::join!(fetch_a, fetch_b);
            let _ = tx.send((result_a, result_b));
        });
        rx
    }

    /// G7 T5: full `SELECT * FROM {quoted table}` [+ `WHERE {where_clause}`],
    /// drained into a `dbc_buffer::ResultBuffer` — NOT `LIMIT`-bounded (a
    /// diff must see the whole table or explicitly say it didn't). The
    /// WHERE box is refused CLIENT-SIDE (before any connection is
    /// attempted) unless the COMPOSED statement passes
    /// `dbc_core::is_read_statement` — CURATION binding requirement (design
    /// CURATION §0.1(b)/§0.2). Returns the composed SQL alongside the
    /// result so the caller can show it verbatim in the compare tab header.
    ///
    /// G7 T8 wired the first real call site (`CompareView::start_data_diff`,
    /// dispatched from `AppView::on_compare_view_event`) — the
    /// `#[allow(dead_code)]` this carried through T5/T6/T7 is removed.
    pub fn fetch_diff_side(
        &self,
        spec: ConnectSpec,
        schema: Option<String>,
        table: String,
        where_clause: Option<String>,
    ) -> tokio::sync::oneshot::Receiver<Result<(String, SchemaRef, dbc_buffer::ResultBuffer), QueryError>>
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = fetch_diff_side_inner(spec, schema, table, where_clause, handle).await;
            let _ = tx.send(result);
        });
        rx
    }
}

/// G7 T5: pure SQL composer + guard, extracted as a standalone function
/// specifically so the CURATION-REQUIRED test can prove the WHERE-box guard
/// fires BEFORE `open_spec` is ever called (design CURATION §0.2: "REQUIRED
/// test: `fetch_diff_side` with a WHERE-box payload failing
/// `is_read_statement` is refused client-side"). `dbc_core::quote_qualified`
/// is the SAME quoting function `sandbox.rs` already uses for its own
/// write-path SQL (Global Constraints' quoting note — MSSQL bracket
/// quoting via `admin_sql::quote_ident_for` is out of scope here since
/// MSSQL is unwired in `connect::open_config` today).
fn compose_diff_select(
    schema: Option<&str>,
    table: &str,
    where_clause: Option<&str>,
) -> Result<String, QueryError> {
    let base = format!("SELECT * FROM {}", dbc_core::quote_qualified(schema, table));
    let sql = match where_clause {
        Some(w) if !w.trim().is_empty() => format!("{base} WHERE {w}"),
        _ => base,
    };
    if !dbc_core::is_read_statement(&sql) {
        return Err(QueryError::msg(
            "WHERE výraz nelze spustit — musí jít o čistě čtecí SQL (žádné oddělené příkazy)"
                .to_string(),
        ));
    }
    Ok(sql)
}

/// G7 T5: `QueryRunner::fetch_diff_side`'s async body — composes + guards
/// the SELECT (see `compose_diff_select`'s doc comment) BEFORE `open_spec`
/// is called at all, then drains the result into a `ResultBuffer`, bounded
/// by `dbc_diff::data_diff::DIFF_ROW_CAP` as an EXPLICIT error (never a
/// silent truncation — design §4), same "row-cap check inside the drain
/// loop" shape `fetch_lookup_inner`'s `LOOKUP_ROW_CAP` break uses, except
/// hard-`Err` here rather than a silent `break`.
async fn fetch_diff_side_inner(
    spec: ConnectSpec,
    schema: Option<String>,
    table: String,
    where_clause: Option<String>,
    handle: tokio::runtime::Handle,
) -> Result<(String, SchemaRef, dbc_buffer::ResultBuffer), QueryError> {
    // Composed + guarded BEFORE `open_spec` — a failing WHERE box never
    // reaches a connection attempt (CURATION binding requirement).
    let sql = compose_diff_select(schema.as_deref(), &table, where_clause.as_deref())?;
    let mut opened = open_spec(spec, handle).await?;
    let mut stream = opened.conn.query(&sql, CancelToken::new()).await?;
    let columns = stream.columns.clone();
    let mut buf = dbc_buffer::ResultBuffer::new(columns.clone());
    while let Some(item) = stream.batches.recv().await {
        match item {
            Ok(b) => {
                buf.push(b).map_err(|e| QueryError::msg(e.to_string()))?;
                if buf.row_count() > dbc_diff::data_diff::DIFF_ROW_CAP {
                    return Err(QueryError::msg(format!(
                        "tabulka má víc než {} řádků — porovnání dat na tak velké tabulce zatím není podporováno; zúžete výběr přes WHERE",
                        dbc_diff::data_diff::DIFF_ROW_CAP
                    )));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok((sql, columns, buf))
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

/// G13 T6: drains a single-row, single-column TEXT result (pg's `EXPLAIN
/// (ANALYZE, BUFFERS, FORMAT JSON)` output shape, and MSSQL's
/// `STATISTICS XML` result set once T7 wires it) via the same
/// `dbc_buffer::ResultBuffer` drain `fetch_lookup_inner` already uses.
async fn drain_single_text_cell(
    conn: &mut dyn Connection,
    sql: &str,
    cancel: CancelToken,
) -> Result<String, QueryError> {
    let mut stream = conn.query(sql, cancel).await?;
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        buf.push(item?).map_err(|e| QueryError::msg(e.to_string()))?;
    }
    if buf.row_count() == 0 || buf.cell_is_null(0, 0) {
        return Err(QueryError::msg("EXPLAIN ANALYZE nevrátil žádný řádek"));
    }
    Ok(buf.cell_text(0, 0))
}

/// G13 T6: BEGIN -> query -> ROLLBACK, ALWAYS (never COMMIT — see
/// `QueryRunner::run_analyze_write`'s doc comment). Stops nothing early on
/// the query step's own error; the ROLLBACK still runs either way, same
/// "tolerate ROLLBACK itself failing" posture `drive_write_sequence`
/// already documents.
async fn drive_analyze_write(
    conn: &mut dyn Connection,
    explain_analyze_sql: &str,
    cancel: CancelToken,
) -> Result<String, QueryError> {
    if let Err(e) = conn.execute("BEGIN", cancel.clone()).await {
        let _ = conn.execute("ROLLBACK", cancel.clone()).await;
        return Err(e);
    }
    let plan_result = drain_single_text_cell(conn, explain_analyze_sql, cancel.clone()).await;
    let _ = conn.execute("ROLLBACK", cancel.clone()).await; // ALWAYS — see doc comment.
    plan_result
}

/// G13 T6: same timeout/cancel/bounded-rollback-grace shape as
/// `drive_write_sequence_bounded` — reuses the SAME `ROLLBACK_GRACE_SECS`
/// constant so a hung ROLLBACK can never wedge this path any differently
/// than it can already wedge the Apply flow.
async fn drive_analyze_write_bounded(
    conn: &mut dyn Connection,
    explain_analyze_sql: &str,
    cancel: CancelToken,
    timeout_secs: Option<u64>,
) -> Result<String, QueryError> {
    match timeout_secs {
        Some(t) => {
            let sequence = drive_analyze_write(conn, explain_analyze_sql, cancel.clone());
            match tokio::time::timeout(Duration::from_secs(t), sequence).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    cancel.cancel();
                    let rollback = conn.execute("ROLLBACK", CancelToken::new());
                    let _ =
                        tokio::time::timeout(Duration::from_secs(ROLLBACK_GRACE_SECS), rollback)
                            .await;
                    Err(QueryError::msg(format!("[timeout] analýza překročila {t}s")))
                }
            }
        }
        None => drive_analyze_write(conn, explain_analyze_sql, cancel).await,
    }
}

/// G13 T6: `QueryRunner::run_analyze_write`'s async body — belt-and-braces
/// read-only guard (independent of `plan::analyze_gate`'s own UI-side
/// refusal), open, drive (bounded). See `run_analyze_write`'s doc comment
/// for the connection-lifetime rationale and `drive_analyze_write_bounded`
/// for the timeout/cancel/rollback mechanics.
async fn run_analyze_write_inner(
    spec: ConnectSpec,
    explain_analyze_sql: String,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
) -> Result<String, QueryError> {
    guard_not_read_only(spec_is_read_only(&spec))?; // belt-and-braces — see doc comment.
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();
    drive_analyze_write_bounded(&mut *opened.conn, &explain_analyze_sql, cancel, timeout_secs).await
    // `opened` drops here unconditionally — the ultimate backstop, same as run_write_transaction_inner.
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

/// G13 T6: the analyze-write sequence's REQUIRED read-only-discipline tests
/// (Global Constraints: `run_analyze_write`'s belt-and-braces refusal is
/// unit tested directly against a read-only `ConnectSpec` with NO live
/// connection ever attempted) plus the always-rolls-back tests, mirroring
/// `write_transaction_tests`'s exact fixtures. `open_sqlite_test_conn`/
/// `read_one` are duplicated here rather than imported (private items in a
/// sibling test module aren't visible across module boundaries) — per that
/// module's own doc-comment precedent for this exact situation.
#[cfg(test)]
mod analyze_write_tests {
    use super::*;

    /// See `write_transaction_tests::open_sqlite_test_conn`'s doc comment —
    /// identical shape, duplicated for sibling-module visibility.
    async fn open_sqlite_test_conn() -> (tempfile::NamedTempFile, Box<dyn Connection>) {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let conn =
            crate::connect::open(f.path().to_str().expect("utf8 temp path"), &handle).expect("open sqlite");
        (f, conn)
    }

    /// Single-row, single-text-column `QueryStream` — simulates the one row
    /// `drain_single_text_cell` expects (pg/MSSQL's real `EXPLAIN ANALYZE`
    /// shape), for `TxnMockConnection::query` below.
    fn single_text_row_stream(col_name: &str, value: &str) -> dbc_core::QueryStream {
        use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
        use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
        let schema: SchemaRef = std::sync::Arc::new(Schema::new(vec![Field::new(col_name, DataType::Utf8, true)]));
        let mut builder = StringBuilder::new();
        builder.append_value(value);
        let array: ArrayRef = std::sync::Arc::new(builder.finish());
        let batch = RecordBatch::try_new(schema.clone(), vec![array]).expect("schema matches builder");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx.try_send(Ok(batch));
        dbc_core::QueryStream { columns: schema, batches: rx }
    }

    /// Minimal mock of a driver where `query()`/`execute()` share ONE
    /// session (see `Connection::execute`'s doc comment — true of pg/MSSQL,
    /// NOT of `dbc-driver-sqlite`, see the doc comment on the test below
    /// that needs this) — tracks whether an in-transaction `INSERT` (routed
    /// through `query()`, matching `drive_analyze_write`'s own shape) ever
    /// became durable. `committed` only ever flips to `true` on an actual
    /// `COMMIT` — `drive_analyze_write` must never issue one.
    struct TxnMockConnection {
        in_txn: bool,
        pending_insert: bool,
        committed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Connection for TxnMockConnection {
        async fn query(&mut self, sql: &str, _cancel: CancelToken) -> Result<dbc_core::QueryStream, QueryError> {
            assert!(self.in_txn, "the write must run inside the BEGIN…ROLLBACK bracket");
            assert!(sql.starts_with("INSERT"), "unexpected query in this mock: {sql}");
            self.pending_insert = true;
            Ok(single_text_row_stream("n", "ghost"))
        }
        async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn execute(&mut self, sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
            match sql {
                "BEGIN" => {
                    self.in_txn = true;
                    Ok(0)
                }
                "ROLLBACK" => {
                    self.in_txn = false;
                    self.pending_insert = false; // discarded — never committed
                    Ok(0)
                }
                "COMMIT" => {
                    if self.pending_insert {
                        self.committed.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    self.in_txn = false;
                    Ok(0)
                }
                other => Err(QueryError::msg(format!("unexpected statement: {other}"))),
            }
        }
    }

    /// See `write_transaction_tests::read_one`'s doc comment — identical
    /// shape, duplicated for sibling-module visibility.
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

    /// REQUIRED (Global Constraints): refuses BEFORE `open_spec` is ever
    /// called — no connection attempted, no driver reached.
    #[tokio::test]
    async fn run_analyze_write_refuses_read_only_connection_without_connecting() {
        let cfg = dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            database: "\0invalid".into(), // never actually opened — guard fires first
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
        let handle = tokio::runtime::Handle::current();
        let err = run_analyze_write_inner(
            spec,
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT 1".to_string(),
            None,
            handle,
        )
        .await
        .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn drive_analyze_write_always_rolls_back_on_success() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        let out = drive_analyze_write(&mut *conn, "SELECT 'plan-text'", CancelToken::new()).await.unwrap();
        assert_eq!(out, "plan-text");
        // Sanity: this connection is still usable afterward (ROLLBACK, not
        // a leaked open transaction) — a fresh statement succeeds.
        conn.execute("INSERT INTO t VALUES (2, 'b')", CancelToken::new()).await.unwrap();
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 2").await, Some("b".to_string()));
    }

    /// Deviation from the plan's own grounding code (documented per this
    /// task's instructions — "reality/tests win"): the plan drove a plain
    /// `INSERT` (no `RETURNING`) through `drive_analyze_write` over
    /// `crate::connect::open`'s real sqlite driver, then re-read via a
    /// SECOND `query()` call on the SAME `Box<dyn Connection>`, expecting
    /// the earlier `BEGIN`/`ROLLBACK` (issued via `execute()`) to have
    /// undone it. Two problems, both found by actually running this:
    /// (1) a bare `INSERT` run through `Connection::query` returns zero
    /// rows on sqlite, so `drive_analyze_write` (which always goes through
    /// `drain_single_text_cell`, requiring >=1 row — true of pg/MSSQL's
    /// real `EXPLAIN ANALYZE`) fails at the "nevrátil žádný řádek" guard
    /// before reaching the rollback assertion at all. (2)
    /// `dbc-driver-sqlite`'s `SqliteConnection::query` opens a **brand-new**
    /// `rusqlite::Connection` on every call (see its doc comment) —
    /// entirely separate from `execute()`'s persistent `exec_conn` — so a
    /// `BEGIN` issued via `execute()` is invisible to any `query()` call
    /// (each of which runs in its own autocommit session): the `INSERT`
    /// commits immediately, outside any transaction, and the subsequent
    /// `ROLLBACK` on `exec_conn` has nothing of this test's to undo. This is
    /// a structural property of the sqlite driver specifically —
    /// `Connection::execute`'s own doc comment only promises session
    /// sharing between `query()`/`execute()` for PostgreSQL — so no sqlite
    /// fixture can validate "the write really gets rolled back" for the
    /// `BEGIN -> query() -> ROLLBACK` sequence `drive_analyze_write` uses.
    ///
    /// Fix: test `drive_analyze_write`'s OWN contract directly (COMMIT is
    /// NEVER issued, ROLLBACK always is) against a minimal mock `Connection`
    /// that models a single shared session — same "hand-rolled mock
    /// `Connection`" precedent as `HangingConnection` above, just modeling
    /// transactional visibility instead of a hang.
    #[tokio::test]
    async fn drive_analyze_write_rolls_back_writes_even_though_it_never_commits() {
        let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut conn = TxnMockConnection { in_txn: false, pending_insert: false, committed: committed.clone() };
        let out = drive_analyze_write(
            &mut conn,
            "INSERT INTO t VALUES (99, 'ghost') RETURNING n",
            CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out, "ghost");
        assert!(
            !committed.load(std::sync::atomic::Ordering::SeqCst),
            "drive_analyze_write must never COMMIT — the write must never durably land"
        );
    }

    #[tokio::test]
    async fn drive_analyze_write_still_rolls_back_when_the_query_step_errors() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        let err = drive_analyze_write(&mut *conn, "SELECT * FROM no_such_table", CancelToken::new())
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
        // Connection must still be usable — ROLLBACK ran despite the error.
        conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await.unwrap();
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

/// G9 T7: docker-gated proof of the pg monitor SQL against a live server —
/// real refresh over all 8 tiles, a genuine lock-wait blocking chain, a
/// real kill through `monitor_loop`, and the review-mandated kill-
/// promptness-during-slow-refresh characterization. Docker required. Run
/// with:
///   %USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored monitor_pg_tests::
///
/// NOTE (deviation from the plan's grounding — the same hazard G13 T2 hit
/// first and fixed, see `plan.rs`'s `pg_docker_tests` module for the fuller
/// writeup): every test here is a plain, NON-async `#[test]` that owns a
/// `QueryRunner` on an ordinary OS thread and drives the whole body through
/// `runner.handle().block_on(...)`, NOT `#[tokio::test]`. Wrapping a test in
/// `#[tokio::test]` runs its body ON a tokio runtime worker thread; calling
/// `connect::open`'s Postgres arm (which itself calls `runtime.block_on`)
/// from there panics ("Cannot start a runtime from within a runtime").
/// `open_spec` (used everywhere below, never `connect::open` directly)
/// avoids that by wrapping the same call in `spawn_blocking`, which is the
/// only place a nested `block_on` is legal to call from again — but that
/// alone doesn't fix a second, independent hazard: `QueryRunner::new()`
/// builds its OWN fully independent multi-thread `tokio::Runtime`, and
/// dropping a `Runtime` from inside an async context (e.g. at the end of a
/// `#[tokio::test]` fn, when `runner` would go out of scope inside that
/// fn's own ambient runtime) panics too. A plain `#[test]` fn is ordinary
/// sync context, so `runner` drops safely there instead.
#[cfg(test)]
mod monitor_pg_tests {
    use super::*;
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{runners::AsyncRunner, ImageExt},
    };

    async fn pg_url(
        node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    ) -> String {
        format!(
            "postgres://postgres:postgres@127.0.0.1:{}/postgres",
            node.get_host_port_ipv4(5432).await.unwrap()
        )
    }

    /// open_spec (NOT connect::open): see the module doc comment above.
    /// Also keeps this file free of driver-crate names.
    async fn open_pg(url: &str) -> Box<dyn Connection> {
        let handle = tokio::runtime::Handle::current();
        open_spec(ConnectSpec::Url(url.to_string()), handle).await.expect("connect").conn
    }

    #[test]
    #[ignore]
    fn monitor_refresh_produces_populated_snapshot_on_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;

            // Setup session: a table with data so the tables section is non-empty.
            let mut setup = open_pg(&url).await;
            setup
                .execute("CREATE TABLE mon_t(id INT PRIMARY KEY, v TEXT)", CancelToken::new())
                .await
                .unwrap();
            setup
                .execute(
                    "INSERT INTO mon_t SELECT g, 'v' || g FROM generate_series(1, 1000) g",
                    CancelToken::new(),
                )
                .await
                .unwrap();

            let mut conn = open_pg(&url).await;
            let results =
                run_monitor_refresh(&mut *conn, dbc_state::Engine::Postgres, CancelToken::new())
                    .await;
            let snap = monitor::assemble_snapshot(results, Instant::now()).expect("snapshot");

            let connections = snap.connections.expect("connections tile");
            assert!(connections.max.unwrap_or(0) > 0, "max_connections should parse");
            assert!(snap.locks.is_some());
            assert!(snap.size.data_bytes.unwrap_or(0) > 0, "database has a size");
            // Container's postgres superuser may read pg_ls_waldir:
            assert!(snap.size.wal_or_log_bytes.is_some(), "WAL size readable as superuser");
            let perf = snap.perf.expect("perf tile");
            assert!(perf.uptime_secs >= 0);
            assert!(perf.xact_total.is_some());
            assert_eq!(perf.tps, None, "tps is a client-side delta, never parsed");
            assert!(snap.running.is_some());
            let tables = snap.tables.expect("tables section");
            assert!(
                tables.iter().any(|t| t.table == "mon_t"),
                "created table must appear in per-table sizes"
            );
        });
    }

    /// Design §8 T7: a deliberate lock wait — session A holds a row lock,
    /// session B blocks on it; the blocking-chain query must return the
    /// waiter/blocker pair and build_blocking_tree must nest them.
    #[test]
    #[ignore]
    fn blocking_chain_query_sees_a_real_lock_wait() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;

            let mut setup = open_pg(&url).await;
            setup
                .execute("CREATE TABLE lock_t(id INT PRIMARY KEY, v INT)", CancelToken::new())
                .await
                .unwrap();
            setup.execute("INSERT INTO lock_t VALUES (1, 0)", CancelToken::new()).await.unwrap();

            // Session A: open transaction holding the row lock.
            let mut a = open_pg(&url).await;
            a.execute("BEGIN", CancelToken::new()).await.unwrap();
            a.execute("UPDATE lock_t SET v = 1 WHERE id = 1", CancelToken::new()).await.unwrap();

            // Session B: blocks on the same row, in a background task.
            let mut b = open_pg(&url).await;
            let b_task = tokio::spawn(async move {
                let _ = b.execute("UPDATE lock_t SET v = 2 WHERE id = 1", CancelToken::new()).await;
                b
            });
            tokio::time::sleep(Duration::from_secs(2)).await; // let B reach the lock queue

            let mut mon = open_pg(&url).await;
            let results =
                run_monitor_refresh(&mut *mon, dbc_state::Engine::Postgres, CancelToken::new())
                    .await;
            let snap = monitor::assemble_snapshot(results, Instant::now()).expect("snapshot");

            let tree = snap.blocking.expect("blocking section");
            assert_eq!(tree.len(), 1, "exactly one blocking chain expected, got {tree:?}");
            assert_eq!(tree[0].children.len(), 1, "one waiter under the blocker");
            assert!(!tree[0].cycle && !tree[0].children[0].cycle);
            assert!(
                tree[0].children[0].query.as_deref().unwrap_or("").contains("UPDATE lock_t"),
                "waiter query text should surface"
            );
            let waiting = snap.locks.expect("locks tile").waiting;
            assert!(waiting >= 1, "waiting-locks counter must see the queued lock");

            // Release: A rolls back, B completes.
            a.execute("ROLLBACK", CancelToken::new()).await.unwrap();
            let _b = tokio::time::timeout(Duration::from_secs(10), b_task)
                .await
                .expect("B unblocks once A rolls back")
                .unwrap();
        });
    }

    /// End-to-end kill through the REAL loop: find the pg_sleep session's
    /// pid via a refresh, Kill it, assert KillResult Ok — the full
    /// execute()-path counterpart of T3's mock-level gate tests.
    #[test]
    #[ignore]
    fn kill_terminates_a_live_session_via_monitor_loop() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;

            // Victim session: a long sleep, driven in a background task.
            let mut victim = open_pg(&url).await;
            let victim_task = tokio::spawn(async move {
                let cancel = CancelToken::new();
                match victim.query("SELECT pg_sleep(600)", cancel).await {
                    Ok(mut s) => {
                        while let Some(item) = s.batches.recv().await {
                            if item.is_err() {
                                return true; // stream errored = terminated
                            }
                        }
                        false
                    }
                    Err(_) => true,
                }
            });
            tokio::time::sleep(Duration::from_secs(2)).await;

            let mon = open_pg(&url).await;
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let loop_task = tokio::spawn(monitor_loop(
                mon,
                dbc_state::Engine::Postgres,
                /* read_only */ false,
                cmd_rx,
                event_tx,
            ));

            // Refresh to learn the victim's pid.
            cmd_tx.send(MonitorCmd::Refresh { generation: 1 }).await.unwrap();
            let pid = loop {
                match tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
                    .await
                    .expect("event")
                    .expect("channel open")
                {
                    MonitorEvent::Data { snapshot, .. } => {
                        let running = snapshot.running.expect("running section");
                        let found = running
                            .iter()
                            .find(|r| r.query.as_deref().unwrap_or("").contains("pg_sleep(600)"))
                            .map(|r| r.pid);
                        break found.expect("victim session visible in running queries");
                    }
                    MonitorEvent::Error { message, .. } => panic!("refresh failed: {message}"),
                    MonitorEvent::KillResult { .. } => unreachable!("no kill dispatched yet"),
                }
            };

            cmd_tx.send(MonitorCmd::Kill { generation: 1, pid }).await.unwrap();
            match tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
                .await
                .expect("event")
                .expect("channel open")
            {
                MonitorEvent::KillResult { pid: killed, result, .. } => {
                    assert_eq!(killed, pid);
                    assert!(result.is_ok(), "kill failed: {result:?}");
                }
                other => panic!("expected KillResult, got {other:?}"),
            }

            let terminated = tokio::time::timeout(Duration::from_secs(30), victim_task)
                .await
                .expect("victim query ends after termination")
                .unwrap();
            assert!(terminated, "victim's stream must surface the termination error");

            drop(cmd_tx);
            tokio::time::timeout(Duration::from_secs(5), loop_task).await.unwrap().unwrap();
        });
    }

    /// REVIEW-MANDATED (T3's accepted MINOR, recorded in the phase ledger):
    /// characterizes kill promptness when Kill is dispatched WHILE a
    /// refresh is genuinely stuck — not touching production SQL to
    /// manufacture the stall. TABLES's `pg_relation_size`/`pg_indexes_size`
    /// calls need an AccessShareLock on each relation they measure; holding
    /// an ACCESS EXCLUSIVE lock on the one user table in this database from
    /// a second session makes that statement block for real, mid-refresh,
    /// the same way an operator's ALTER TABLE/VACUUM FULL would in
    /// production — no pg_sleep injected into monitor SQL.
    ///
    /// Bound rationale: Kill is NOT instant here by design (`monitor_loop`'s
    /// doc comment: "a kill never interleaves with an in-flight refresh:
    /// the tokio::select! ... cancels the refresh first"). Promptness has
    /// two real steps, not zero: (a) the `select!` noticing `cmd_rx` over
    /// the still-pending refresh future — effectively immediate, tokio
    /// polls the ready branch on the next wake — then (b) the ONE dedicated
    /// connection actually absorbing the cancel-triggered error from the
    /// stuck TABLES statement before the KILL statement can even be SENT
    /// (single-connection wire pipelining, same session-sharing caveat
    /// `Connection::execute`'s doc comment describes). Both are protocol-
    /// level round trips against a local docker container, NOT proportional
    /// to how long the lock would otherwise be held (nothing here ever
    /// releases `slow_t`'s lock before the kill completes) — 8s
    /// comfortably covers that round trip on a local container while still
    /// failing a regression that made Kill wait out the stuck statement
    /// instead of pre-empting it.
    #[test]
    #[ignore]
    fn kill_is_prompt_when_dispatched_mid_slow_refresh() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;

            let mut setup = open_pg(&url).await;
            setup
                .execute("CREATE TABLE slow_t(id INT PRIMARY KEY)", CancelToken::new())
                .await
                .unwrap();

            // Locker session: holds an ACCESS EXCLUSIVE lock so the
            // refresh's TABLES statement genuinely blocks instead of
            // racing to finish before the Kill is dispatched.
            let mut locker = open_pg(&url).await;
            locker.execute("BEGIN", CancelToken::new()).await.unwrap();
            let pid_rows = drain_rows(&mut *locker, "SELECT pg_backend_pid()", &CancelToken::new())
                .await
                .expect("pid query");
            let locker_pid: i64 = pid_rows[0][0]
                .as_deref()
                .expect("pg_backend_pid() is never NULL")
                .parse()
                .expect("pid is numeric");
            locker
                .execute("LOCK TABLE slow_t IN ACCESS EXCLUSIVE MODE", CancelToken::new())
                .await
                .unwrap();

            let mon = open_pg(&url).await;
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let loop_task = tokio::spawn(monitor_loop(
                mon,
                dbc_state::Engine::Postgres,
                /* read_only */ false,
                cmd_rx,
                event_tx,
            ));

            cmd_tx.send(MonitorCmd::Refresh { generation: 1 }).await.unwrap();
            // Give the refresh time to run the first 7 (fast, metadata-only)
            // statements and reach TABLES, where it blocks for real on
            // slow_t's lock.
            tokio::time::sleep(Duration::from_secs(2)).await;

            let kill_dispatched_at = Instant::now();
            cmd_tx.send(MonitorCmd::Kill { generation: 1, pid: locker_pid }).await.unwrap();

            let event = tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
                .await
                .expect("KillResult must arrive well before a hard 30s ceiling")
                .expect("channel open");
            let elapsed = kill_dispatched_at.elapsed();

            match &event {
                MonitorEvent::KillResult { pid, result, .. } => {
                    assert_eq!(*pid, locker_pid);
                    assert!(result.is_ok(), "kill failed: {result:?}");
                }
                other => panic!(
                    "expected the select!'s cmd_rx branch to pre-empt the stuck refresh and \
                     yield a KillResult first, got {other:?}"
                ),
            }
            assert!(
                elapsed < Duration::from_secs(8),
                "kill took {elapsed:?} while a refresh was stuck on a lock wait — expected the \
                 select!'s cancel-on-arrival path to pre-empt it promptly, not wait out the lock \
                 (see this test's doc comment for the bound rationale)"
            );

            // Sanity: the kill genuinely terminated the locker's backend
            // (not merely accepted well-formed SQL) — a follow-up
            // statement on that same connection must now fail.
            let after = locker.execute("SELECT 1", CancelToken::new()).await;
            assert!(after.is_err(), "locker's connection must be dead after being terminated");

            drop(cmd_tx);
            let _ = tokio::time::timeout(Duration::from_secs(5), loop_task).await;
        });
    }
}

/// G7 T5: `fetch_schema_pair`/`fetch_diff_side` + the client-side WHERE-box
/// guard (design CURATION §0.1(b)/§0.2 — REQUIRED). `compose_diff_select_*`
/// tests exercise the pure composer/guard directly (no `ConnectSpec`/
/// `open_spec` anywhere in their call path — the strongest possible proof
/// that a rejected WHERE box never reaches a connection attempt); the
/// end-to-end test drives `fetch_diff_side` over a real (writable) sqlite
/// connection to prove the guard fires before the driver is ever touched,
/// even though sqlite would happily run a multi-statement batch if asked.
#[cfg(test)]
mod diff_fetch_tests {
    use super::*;

    #[test]
    fn compose_diff_select_quotes_table_and_appends_where() {
        assert_eq!(
            compose_diff_select(Some("public"), "orders", None).unwrap(),
            "SELECT * FROM \"public\".\"orders\""
        );
        assert_eq!(
            compose_diff_select(None, "orders", Some("id > 10")).unwrap(),
            "SELECT * FROM \"orders\" WHERE id > 10"
        );
    }

    /// CURATION §0.1(b)/§0.2 REQUIRED test: a WHERE-box payload that would
    /// smuggle a second statement is refused BEFORE any connection is
    /// attempted — proven by calling the pure composer directly, with no
    /// `ConnectSpec`/`open_spec` anywhere in this test's call path at all
    /// (the strongest possible proof of "client-side, never reaches the
    /// driver": there is no driver reachable from this test in the first
    /// place).
    #[test]
    fn compose_diff_select_refuses_multi_statement_injection_client_side() {
        let err = compose_diff_select(None, "orders", Some("1=1; DROP TABLE orders")).unwrap_err();
        assert!(err.message.contains("WHERE"));
    }

    #[test]
    fn compose_diff_select_allows_a_read_only_subquery_in_where() {
        assert!(compose_diff_select(None, "t", Some("id IN (SELECT id FROM other)")).is_ok());
    }

    #[test]
    fn compose_diff_select_empty_where_is_treated_as_absent() {
        assert_eq!(compose_diff_select(None, "t", Some("   ")).unwrap(), "SELECT * FROM \"t\"");
    }

    /// End-to-end proof over a REAL (writable) sqlite connection: the guard
    /// fires even though the underlying driver would happily run a
    /// multi-statement batch if asked — the table is untouched afterward.
    #[tokio::test]
    async fn fetch_diff_side_end_to_end_refuses_before_touching_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        {
            let handle = tokio::runtime::Handle::current();
            let mut conn = crate::connect::open(db_path.to_str().unwrap(), &handle).unwrap();
            conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new())
                .await
                .unwrap();
            conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();
        }
        // Exercises `fetch_diff_side_inner` directly (the same body
        // `QueryRunner::fetch_diff_side` spawns) over the CURRENT test
        // runtime's handle — constructing a whole `QueryRunner` here would
        // build (and, at end of scope, drop) its own nested multi-thread
        // `tokio::runtime::Runtime`, which tokio forbids doing synchronously
        // from inside an async context (see
        // `write_transaction_tests::run_write_transaction_refuses_read_only_connection_without_connecting`'s
        // doc comment for the same rationale).
        let spec = ConnectSpec::Url(db_path.to_str().unwrap().to_string());
        let handle = tokio::runtime::Handle::current();
        let result = fetch_diff_side_inner(
            spec,
            None,
            "t".to_string(),
            Some("1=1; DELETE FROM t".to_string()),
            handle,
        )
        .await;
        assert!(result.is_err());

        // Table untouched — the malicious WHERE never reached the driver.
        let handle = tokio::runtime::Handle::current();
        let mut verify = crate::connect::open(db_path.to_str().unwrap(), &handle).unwrap();
        let mut stream = verify.query("SELECT COUNT(*) FROM t", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        assert_eq!(buf.cell_text(0, 0), "1", "row count must be unchanged — the WHERE-box injection never executed");
    }

    /// `fetch_schema_pair` runs both legs concurrently and reports each
    /// side's outcome independently — a bad spec on one side doesn't cancel
    /// or fail the other. Driven directly over `open_spec` +
    /// `Connection::schema()` (the exact two steps `fetch_schema_pair`'s
    /// spawned body performs per leg) using the CURRENT test runtime's
    /// handle — same "no nested `QueryRunner`" rationale as
    /// `fetch_diff_side_end_to_end_refuses_before_touching_the_connection`
    /// above — over one real sqlite temp file plus an unreachable postgres
    /// port (fails fast, no docker/network dependency).
    #[tokio::test]
    async fn fetch_schema_pair_reports_each_side_independently() {
        let dir = tempfile::tempdir().unwrap();
        let ok_path = dir.path().join("ok.db");
        {
            let handle = tokio::runtime::Handle::current();
            let mut conn = crate::connect::open(ok_path.to_str().unwrap(), &handle).unwrap();
            conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        }
        let spec_ok = ConnectSpec::Url(ok_path.to_str().unwrap().to_string());
        // Postgres URL to an unreachable port — fails fast without a real
        // server, giving the "other leg failed" case with no docker
        // dependency.
        let spec_bad = ConnectSpec::Url("postgres://user:pass@127.0.0.1:1/nosuchdb".to_string());

        let handle_a = tokio::runtime::Handle::current();
        let handle_b = tokio::runtime::Handle::current();
        let fetch_a = async {
            match open_spec(spec_ok, handle_a).await {
                Ok(mut opened) => opened.conn.schema().await,
                Err(e) => Err(e),
            }
        };
        let fetch_b = async {
            match open_spec(spec_bad, handle_b).await {
                Ok(mut opened) => opened.conn.schema().await,
                Err(e) => Err(e),
            }
        };
        let (result_a, result_b) = tokio::join!(fetch_a, fetch_b);
        assert!(result_a.is_ok(), "the good side must succeed independently of the bad side: {result_a:?}");
        assert!(result_b.is_err(), "the bad side must fail without being masked by the good side");
    }
}

/// G7 T9: docker-based empirical validation of the whole T5 pipeline
/// (`fetch_schema_pair`, `fetch_diff_side`, `compose_diff_select`'s guard)
/// against a REAL Postgres 16.13 server. `diff_schema_tests` (dbc-diff, T2)
/// and `diff_fetch_tests` above already prove the pure logic and the guard
/// over hand-built fixtures / a writable sqlite connection; this module
/// proves the same pipeline survives genuine live catalog output — two
/// actually-different databases, `format_type()` text, real PK/index
/// metadata — and that the WHERE-box guard holds end-to-end against a real
/// server, not just a mock or a driver that happens not to support batched
/// statements. Docker required. Run with:
///   %USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored compare_pg_tests::
///
/// Same hazard/pattern as `monitor_pg_tests` above (see that module's doc
/// comment for the full rationale, not repeated here): every test is a
/// plain, NON-async `#[test]` driven through `runner.handle().block_on(...)`,
/// NOT `#[tokio::test]` — `#[tokio::test]` runs the body on a tokio worker
/// thread, where `open_spec`'s nested `spawn_blocking` -> `block_on` for the
/// Postgres handshake panics ("Cannot start a runtime from within a
/// runtime"), and dropping `QueryRunner`'s own `Runtime` at end of scope
/// panics too if that scope is itself async. `open_spec` is used for every
/// setup connection (never `connect::open` directly, same reason). The ONE
/// `QueryRunner` each test constructs is used for BOTH the setup
/// (`open_pg`, which reuses `open_spec` under `Handle::current()` — the
/// runtime `block_on` itself is driving) and the actual
/// `fetch_schema_pair`/`fetch_diff_side` calls under test — a second nested
/// `QueryRunner::new()` is deliberately never constructed inside the
/// `block_on` body, since dropping ITS `Runtime` before the outer
/// `block_on` returns would hit the exact same nested-runtime panic.
#[cfg(test)]
mod compare_pg_tests {
    use super::*;
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{runners::AsyncRunner, ImageExt},
    };

    async fn pg_url(
        node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
        db: &str,
    ) -> String {
        format!(
            "postgres://postgres:postgres@127.0.0.1:{}/{db}",
            node.get_host_port_ipv4(5432).await.unwrap()
        )
    }

    /// open_spec (NOT connect::open): see the module doc comment above.
    async fn open_pg(url: &str) -> Box<dyn Connection> {
        let handle = tokio::runtime::Handle::current();
        open_spec(ConnectSpec::Url(url.to_string()), handle).await.expect("connect").conn
    }

    /// `CREATE DATABASE` can't run inside sqlx-style batching anyway — one
    /// statement per `execute()` call, autocommit, exactly like every other
    /// setup statement in this module and in `monitor_pg_tests`.
    async fn create_database(default_db_url: &str, name: &str) {
        let mut conn = open_pg(default_db_url).await;
        conn.execute(&format!("CREATE DATABASE {name}"), CancelToken::new()).await.unwrap();
    }

    /// Seeds TWO genuinely different live databases inside the same
    /// container with KNOWN schema deltas: `only_a` exists only on the left
    /// (must diff as Removed), `only_b` only on the right (Added), and
    /// `keep` exists on both sides but with a changed column (`note`'s type,
    /// integer -> bigint) AND an added column (`extra`) — so the shared
    /// table is Changed for two independent, individually-asserted reasons.
    /// Runs the REAL `fetch_schema_pair` -> `diff_schema` pipeline over live
    /// catalog output (not hand-built `SchemaSnapshot` fixtures, unlike
    /// every T2 test) and asserts the diff matches the seeded deltas
    /// exactly. Container is torn down automatically when `node` drops at
    /// the end of this fn (testcontainers' own `ContainerAsync` Drop impl —
    /// no manual cleanup step needed, same as every other docker test in
    /// this file).
    #[test]
    #[ignore]
    fn fetch_schema_pair_matches_seeded_deltas_on_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let default_url = pg_url(&node, "postgres").await;
            create_database(&default_url, "dba").await;
            create_database(&default_url, "dbb").await;
            let url_a = pg_url(&node, "dba").await;
            let url_b = pg_url(&node, "dbb").await;

            {
                let mut a = open_pg(&url_a).await;
                a.execute(
                    "CREATE TABLE keep (id integer PRIMARY KEY, name text NOT NULL, note integer)",
                    CancelToken::new(),
                )
                .await
                .unwrap();
                a.execute("CREATE TABLE only_a (id integer PRIMARY KEY)", CancelToken::new())
                    .await
                    .unwrap();
            }
            {
                let mut b = open_pg(&url_b).await;
                b.execute(
                    "CREATE TABLE keep (id integer PRIMARY KEY, name text NOT NULL, note bigint, extra text)",
                    CancelToken::new(),
                )
                .await
                .unwrap();
                b.execute("CREATE TABLE only_b (id integer PRIMARY KEY)", CancelToken::new())
                    .await
                    .unwrap();
            }

            let rx = runner.fetch_schema_pair(ConnectSpec::Url(url_a), ConnectSpec::Url(url_b));
            let (result_a, result_b) = rx.await.unwrap();
            let (snap_a, snap_b) = (result_a.unwrap(), result_b.unwrap());

            let diff = dbc_diff::schema_diff::diff_schema(
                &snap_a,
                &snap_b,
                dbc_diff::schema_diff::CompareMode::SameEngine,
            );

            let only_a = diff.tables.iter().find(|t| t.name == "only_a").expect("only_a present in diff");
            assert_eq!(only_a.status, dbc_diff::schema_diff::TableStatus::Removed);

            let only_b = diff.tables.iter().find(|t| t.name == "only_b").expect("only_b present in diff");
            assert_eq!(only_b.status, dbc_diff::schema_diff::TableStatus::Added);

            let keep = diff.tables.iter().find(|t| t.name == "keep").expect("keep present in diff");
            assert_eq!(keep.status, dbc_diff::schema_diff::TableStatus::Changed);
            assert!(
                keep.columns.iter().any(|c| matches!(c,
                    dbc_diff::schema_diff::ObjectDiff::Changed { left, fields, .. }
                        if left.name == "note" && fields.iter().any(|f| f.field == "data_type"))),
                "note's type change (integer -> bigint) must be detected as Changed: {:?}",
                keep.columns
            );
            assert!(
                keep.columns.iter().any(|c| matches!(c,
                    dbc_diff::schema_diff::ObjectDiff::Added(col) if col.name == "extra")),
                "extra column must be detected as Added: {:?}",
                keep.columns
            );
        });
    }

    /// Seeds a `rows_t` table with KNOWN row deltas on two live databases —
    /// id=1 only on the left (Removed), id=4 only on the right (Added),
    /// id=3's `val` differs on each side (Changed), id=2 is identical
    /// (Unchanged) — and runs the REAL `fetch_diff_side` (both sides) ->
    /// `dbc_diff::data_diff::diff_data` pipeline over live query output,
    /// asserting every row lands in the expected bucket by its actual `id`
    /// value read back out of the `ResultBuffer`s (not by row-index
    /// position, which live sequential-scan order doesn't formally
    /// guarantee).
    #[test]
    #[ignore]
    fn fetch_diff_side_and_diff_data_detect_seeded_row_deltas_on_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let default_url = pg_url(&node, "postgres").await;
            create_database(&default_url, "dra").await;
            create_database(&default_url, "drb").await;
            let url_a = pg_url(&node, "dra").await;
            let url_b = pg_url(&node, "drb").await;

            {
                let mut a = open_pg(&url_a).await;
                a.execute("CREATE TABLE rows_t (id integer PRIMARY KEY, val text)", CancelToken::new())
                    .await
                    .unwrap();
                a.execute(
                    "INSERT INTO rows_t VALUES (1, 'only-left'), (2, 'same'), (3, 'left-version')",
                    CancelToken::new(),
                )
                .await
                .unwrap();
            }
            {
                let mut b = open_pg(&url_b).await;
                b.execute("CREATE TABLE rows_t (id integer PRIMARY KEY, val text)", CancelToken::new())
                    .await
                    .unwrap();
                b.execute(
                    "INSERT INTO rows_t VALUES (2, 'same'), (3, 'right-version'), (4, 'only-right')",
                    CancelToken::new(),
                )
                .await
                .unwrap();
            }

            let rx_a = runner.fetch_diff_side(ConnectSpec::Url(url_a), None, "rows_t".to_string(), None);
            let (_, schema_a, mut buf_a) = rx_a.await.unwrap().unwrap();
            let rx_b = runner.fetch_diff_side(ConnectSpec::Url(url_b), None, "rows_t".to_string(), None);
            let (_, schema_b, mut buf_b) = rx_b.await.unwrap().unwrap();

            let names_a: Vec<String> = schema_a.fields().iter().map(|f| f.name().to_string()).collect();
            let names_b: Vec<String> = schema_b.fields().iter().map(|f| f.name().to_string()).collect();
            let pk_a = vec![names_a.iter().position(|n| n == "id").expect("id column on left")];
            let pk_b = vec![names_b.iter().position(|n| n == "id").expect("id column on right")];

            let outcome =
                dbc_diff::data_diff::diff_data(&mut buf_a, &names_a, &pk_a, &mut buf_b, &names_b, &pk_b)
                    .expect("diff_data must succeed under DIFF_ROW_CAP");

            let removed_ids: Vec<String> = outcome
                .rows
                .iter()
                .filter_map(|r| match r {
                    dbc_diff::data_diff::RowDiff::Removed { left_row } => {
                        Some(buf_a.cell_text(*left_row, pk_a[0]))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(removed_ids, vec!["1".to_string()], "id=1 only exists on the left — must be Removed");

            let added_ids: Vec<String> = outcome
                .rows
                .iter()
                .filter_map(|r| match r {
                    dbc_diff::data_diff::RowDiff::Added { right_row } => {
                        Some(buf_b.cell_text(*right_row, pk_b[0]))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(added_ids, vec!["4".to_string()], "id=4 only exists on the right — must be Added");

            let changed_ids: Vec<String> = outcome
                .rows
                .iter()
                .filter_map(|r| match r {
                    dbc_diff::data_diff::RowDiff::Changed { left_row, .. } => {
                        Some(buf_a.cell_text(*left_row, pk_a[0]))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(changed_ids, vec!["3".to_string()], "id=3's val differs on each side — must be Changed");

            let unchanged_ids: Vec<String> = outcome
                .rows
                .iter()
                .filter_map(|r| match r {
                    dbc_diff::data_diff::RowDiff::Unchanged { left_row, .. } => {
                        Some(buf_a.cell_text(*left_row, pk_a[0]))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(unchanged_ids, vec!["2".to_string()], "id=2 is identical on both sides — must be Unchanged");

            let val_idx = outcome
                .intersection_columns
                .iter()
                .position(|c| c == "val")
                .expect("val is a shared column");
            let changed_cols_ok = outcome.rows.iter().any(|r| matches!(r,
                dbc_diff::data_diff::RowDiff::Changed { changed_cols, .. } if changed_cols.contains(&val_idx)));
            assert!(changed_cols_ok, "the Changed row's changed_cols must point at the val column");
        });
    }

    /// End-to-end proof of the CURATION-required WHERE-box guard against a
    /// REAL server (see `diff_fetch_tests::compose_diff_select_refuses_multi_statement_injection_client_side`
    /// for the pure-function proof and
    /// `diff_fetch_tests::fetch_diff_side_end_to_end_refuses_before_touching_the_connection`
    /// for the sqlite companion): a malicious multi-statement WHERE-box
    /// payload is refused by `compose_diff_select` before `fetch_diff_side`
    /// ever opens a connection to the live container, and a follow-up CLEAN
    /// fetch proves the row count is untouched — the strongest possible
    /// proof against a real server that "refused client-side" really means
    /// "never even reached the database", not merely "the database also
    /// happened to reject it".
    #[test]
    #[ignore]
    fn fetch_diff_side_where_box_guard_holds_against_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node, "postgres").await;
            {
                let mut conn = open_pg(&url).await;
                conn.execute("CREATE TABLE t (id integer PRIMARY KEY, n text)", CancelToken::new())
                    .await
                    .unwrap();
                conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();
            }

            let rx = runner.fetch_diff_side(
                ConnectSpec::Url(url.clone()),
                None,
                "t".to_string(),
                Some("1=1; DELETE FROM t".to_string()),
            );
            let result = rx.await.unwrap();
            assert!(
                result.is_err(),
                "a multi-statement WHERE-box payload must be refused before touching the container"
            );

            // Follow-up: a CLEAN fetch (no WHERE box) proves the row is
            // still there — the malicious statement never reached Postgres.
            let rx2 = runner.fetch_diff_side(ConnectSpec::Url(url), None, "t".to_string(), None);
            let (_, _, mut buf) = rx2.await.unwrap().expect("clean fetch must succeed");
            assert_eq!(buf.row_count(), 1, "row must be untouched — the injected DELETE never executed");
            assert_eq!(buf.cell_text(0, 0), "1", "the surviving row must still be id=1");
        });
    }
}
