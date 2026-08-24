use std::time::{Duration, Instant};

use dbc_core::arrow::array::RecordBatch;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{CancelToken, Connection, QueryError, SchemaSnapshot, CHANNEL_CAPACITY};
use dbc_state::ConnectionConfig;

use crate::backup;
use crate::connect;
use crate::monitor;

pub enum QueryEvent {
    Started { columns: SchemaRef },
    Batch(RecordBatch),
    Finished { elapsed: Duration },
    Failed(QueryError),
}

/// G12 T5: streaming progress events for `QueryRunner::connect_and_run_many`
/// (the editor's multi-statement unlock) — one connection, N statements,
/// STOP on first error (design §4: error-policy choice is a
/// script-runner-only concept, `run_script`'s `ErrorPolicy` doesn't apply
/// here).
pub enum MultiQueryEvent {
    /// `columns: Some` = a row-producing statement (`Batch`es follow before
    /// its `StatementFinished`); `None` = a non-row statement (a write) —
    /// no result tab opens for it.
    StatementStarted { index: usize, total: usize, columns: Option<SchemaRef> },
    Batch(RecordBatch),
    /// `affected: Some(n)` for a write; `None` for a read (its rows went to
    /// the caller's tab via `Batch`, not a count).
    StatementFinished { index: usize, affected: Option<u64>, elapsed: Duration },
    StatementFailed { index: usize, error: QueryError },
    RunFinished,
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

/// G12 T2: transaction discipline for `QueryRunner::run_script` (design §2
/// matrix). `None` = every statement autocommits individually (no
/// client-managed transaction at all); `PerFile` = one BEGIN…COMMIT per
/// file; `WholeRun` = one BEGIN…COMMIT spanning every file in the run.
/// Wired into `main.rs` by Task 3's script-runner UI
/// (`AppView::confirm_script_run`).
///
/// G15 T5: `PerFile`/`WholeRun` issue `dbc_core::tx_begin_sql`/`tx_commit_sql`/
/// `tx_rollback_sql` for the connection's dialect — on MSSQL that is the
/// fused `"SET XACT_ABORT ON; BEGIN TRANSACTION"` (fixes G12's bare-`BEGIN`
/// bug). T-SQL transactions legally span batches (unlike a client library
/// with per-batch autocommit assumptions), so a multi-file/multi-statement
/// script's `BEGIN … COMMIT` bracket is valid T-SQL as written. Some T-SQL
/// statements refuse to run inside an explicit user transaction at all
/// (`BACKUP DATABASE`, `ALTER DATABASE`, full-text catalog DDL, among
/// others) — this is NOT detected ahead of time; such a statement simply
/// errors verbatim from the server and is handled like any other
/// statement failure by `opts.error_policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxScope {
    None,
    PerFile,
    WholeRun,
}

/// G12 T2: what happens to the rest of the run after one statement fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPolicy {
    Stop,
    Continue,
}

/// G12 T2: `QueryRunner::run_script`'s options — plain data, GPUI-free,
/// so `drive_script` is unit-testable without a window.
pub struct ScriptRunOptions {
    pub tx_scope: TxScope,
    pub error_policy: ErrorPolicy,
    pub dialect: dbc_core::Dialect,
    /// From the connection's existing `cfg.timeout_secs` — bounds EACH
    /// statement individually (a whole-run timeout would be hostile for a
    /// long script), via a per-statement child `CancelToken` + tokio
    /// timeout, same shape `connect_and_run`'s watchdog uses.
    pub statement_timeout_secs: Option<u64>,
}

/// G12 T2: streaming progress events for `run_script`, same mpsc/
/// `CHANNEL_CAPACITY` convention as `QueryEvent`. `StatementStarted` carries
/// no `stmt_total_in_file` (deviation from the design's grounding text,
/// documented on `drive_script`) — the runner streams statements as the
/// splitter completes them and can't know a file's total mid-file; the UI's
/// own pre-scan has exact per-file totals and renders totals from there.
/// `sql_preview` (see `sql_preview` below) is the ONLY statement text ever
/// carried on these events — display-safe, capped, single-line (§3-novela:
/// no credentials/result data in `ScriptEvent`/logs/errors).
#[derive(Debug)]
pub enum ScriptEvent {
    FileStarted { path: std::path::PathBuf, index: usize, total_files: usize },
    StatementStarted { stmt_index: usize, sql_preview: String },
    StatementFinished { stmt_index: usize, affected: Option<u64>, elapsed: Duration },
    StatementFailed { stmt_index: usize, error: QueryError },
    FileFinished {
        path: std::path::PathBuf,
        statements_run: usize,
        statements_failed: usize,
        elapsed: Duration,
    },
    RunFinished {
        files_run: usize,
        statements_run: usize,
        statements_failed: usize,
        elapsed: Duration,
        aborted: bool,
    },
}

/// G12 T7: streaming progress events for `QueryRunner::run_csv_import`
/// (design §5) — ONE transaction for the whole import, so `BatchFinished`'s
/// `rows_committed_so_far` is "executed inside the still-open transaction",
/// not durable, until `Finished` actually lands (nothing is committed on a
/// `Failed`).
#[derive(Debug)]
pub enum CsvImportEvent {
    BatchStarted { batch_index: usize, rows_in_batch: usize },
    BatchFinished { batch_index: usize, rows_committed_so_far: u64 },
    Failed { error: QueryError },
    Finished { rows_imported: u64, elapsed: Duration },
}

/// G12 T7: one CSV import job — `main.rs`'s peek/pre-count pass builds this
/// from the file picker's chosen path, the schema snapshot's `TableInfo`
/// (`columns`), and the mapping modal's live `ColumnMapping`.
pub struct CsvImportJob {
    pub path: std::path::PathBuf,
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<crate::csv_import::TargetColumn>,
    pub mapping: crate::csv_import::ColumnMapping,
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

    /// G10 T3, design §5: one-shot fetch of the admin sub-views' labeled
    /// catalog SELECTs (`admin_sql::{roles_catalog, privileges_catalog,
    /// sizes_catalog}`'s output) — opens `spec` (same `open_spec` dispatch
    /// as every other one-shot here), runs each `(label, sql)` pair
    /// SEQUENTIALLY over the one connection, and drains each into
    /// materialized rows via `drain_all_rows` (the same drain path
    /// `fetch_lookup` uses). No read-only guard: this is a read, same
    /// posture as `fetch_lookup`/`fetch_schema`. The first query to error
    /// aborts the whole batch (CURATION item 5: no fallback query).
    ///
    /// Allow dead_code: T3 lands ahead of T4-T6's UI consumer
    /// (`admin_panel.rs`'s `AdminEvent::FetchCatalog` handler) — exercised
    /// directly by this file's own tests until then. Remove once T4 wires
    /// it into `main.rs`.
    #[allow(dead_code)]
    pub fn fetch_admin_catalog(
        &self,
        spec: ConnectSpec,
        queries: Vec<(&'static str, String)>,
    ) -> tokio::sync::oneshot::Receiver<Result<Vec<(&'static str, AdminCatalogRows)>, QueryError>>
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = fetch_admin_catalog_inner(spec, queries, handle).await;
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
    /// G10 T3: widened from `Vec<(String, Option<u64>)>` to
    /// `Vec<admin_sql::WriteStatement>` (design §0) — still the app's ONLY
    /// write path, now with TWO sanctioned callers (G5's sandbox Apply,
    /// whose statements arrive via `WriteStatement::from((String,
    /// Option<u64>))` so `exec_sql == display_sql`, and G10's admin Apply,
    /// whose password-bearing statements have a real `exec_sql` and a
    /// `'***'`-redacted `display_sql`), both through `main.rs`'s one
    /// confirm dialog, both behind the SAME `guard_not_read_only` choke
    /// point below — no fresh read-only logic added for admin.
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
        statements: Vec<crate::admin_sql::WriteStatement>,
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

    /// G12 T2: runs a multi-statement `.sql` script (design T2) — a
    /// SANCTIONED runner-owned write path (§3-novela): transactional
    /// discipline per `opts.tx_scope`, the SHARED `guard_not_read_only`
    /// read-only guard enforced per-statement BEFORE any write reaches the
    /// driver (`dispatch_statement`/`run_script_statement`), and
    /// `opts.error_policy` honored via `failure_action`. One dedicated
    /// connection for the WHOLE run (satisfies `Connection::execute`'s
    /// transaction-per-connection invariant across every tx scope), dropped
    /// when the spawned future completes. Read-only connections are NOT
    /// refused up front — a read-only script over a read-only connection is
    /// legitimate; write statements are rejected per-statement instead
    /// (CURATION item 1(c)/4(a)). Called by Task 3's script-runner UI
    /// (`AppView::confirm_script_run`, main.rs).
    pub fn run_script(
        &self,
        spec: ConnectSpec,
        files: Vec<std::path::PathBuf>,
        opts: ScriptRunOptions,
        cancel: CancelToken,
    ) -> tokio::sync::mpsc::Receiver<ScriptEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let handle = self.handle();
        self.runtime.spawn(async move {
            if cancel.is_cancelled() {
                let _ = tx
                    .send(ScriptEvent::RunFinished {
                        files_run: 0,
                        statements_run: 0,
                        statements_failed: 0,
                        elapsed: Duration::ZERO,
                        aborted: true,
                    })
                    .await;
                return;
            }
            // Captured BEFORE `spec` moves into `open_spec` below — same
            // "capture read_only up front" convention `open_monitor` uses.
            let read_only = spec_is_read_only(&spec);
            let mut opened = match open_spec(spec, handle).await {
                Ok(o) => o,
                Err(e) => {
                    let _ = tx.send(ScriptEvent::StatementFailed { stmt_index: 0, error: e }).await;
                    let _ = tx
                        .send(ScriptEvent::RunFinished {
                            files_run: 0,
                            statements_run: 0,
                            statements_failed: 1,
                            elapsed: Duration::ZERO,
                            aborted: true,
                        })
                        .await;
                    return;
                }
            };
            drive_script(&mut *opened.conn, read_only, &files, &opts, cancel, &tx).await;
            // `opened` (connection + tunnel) drops here unconditionally —
            // the ultimate rollback backstop, same note as
            // `run_write_transaction_inner`.
        });
        rx
    }

    /// G12 T5: the editor's multi-statement unlock — one connection, every
    /// statement in `statements` in order, STOP on the first error (no
    /// `ErrorPolicy` here, that's `run_script`-only). Per-statement
    /// read-only rejection via the SHARED `guard_not_read_only` guard
    /// (CURATION item 1(c)/4(c)) and a per-statement child-token timeout,
    /// same shape `run_script` uses. `statements` is caller-supplied
    /// ALREADY split + already auto-limited (see `main.rs`'s
    /// `dialect_for_engine`/`auto_limit_each`) — this method just dispatches
    /// them in order.
    pub fn connect_and_run_many(
        &self,
        spec: ConnectSpec,
        statements: Vec<String>,
        cancel: CancelToken,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::mpsc::Receiver<MultiQueryEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let handle = self.handle();
        self.runtime.spawn(async move {
            connect_and_run_many_inner(spec, statements, cancel, timeout_secs, handle, tx).await;
        });
        rx
    }

    /// G12 T7: runs a CSV import (design T7) — a SANCTIONED runner-owned
    /// write path (§3-novela): ONE transaction for the WHOLE import (not
    /// configurable, unlike `run_script`'s `tx_scope`), streaming batched
    /// `INSERT`s via `csv_import::generate_insert_batches`. FIRST action,
    /// before any file or DB touch: the SHARED `guard_not_read_only` guard
    /// (CURATION items 1(c)/4(b)'s runtime half). Cancellation is checked
    /// BETWEEN batches only (bounded by the 500-row cap); `timeout_secs`
    /// bounds each batch statement via the same per-statement child-token
    /// shape `run_script_statement` uses.
    pub fn run_csv_import(
        &self,
        spec: ConnectSpec,
        job: CsvImportJob,
        cancel: CancelToken,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::mpsc::Receiver<CsvImportEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let handle = self.handle();
        self.runtime.spawn(async move {
            run_csv_import_inner(spec, job, cancel, timeout_secs, handle, tx).await;
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

    /// G11 T4: one generic external-tool runner, used for `pg_dump`,
    /// `pg_restore`, AND `psql` alike — their spawn/stream/redact mechanics
    /// are identical (design §2/§3 both reduce to "spawn a program with
    /// PGPASSWORD in its env, stream stderr as log lines"), so one method
    /// serves all three rather than three near-duplicates. The caller
    /// (T6) builds `program`/`args` via `backup::resolve_tool_path` +
    /// `backup::build_pg_dump_args`/`build_pg_restore_args`/`build_psql_args`.
    ///
    /// DEVIATION from this plan's original sketch (grounded in T3's actual,
    /// already-committed `backup::run_and_stream`, which itself deviates
    /// from ITS OWN plan sample — see that function's doc comment): T3's
    /// `run_and_stream` spawns the child SYNCHRONOUSLY (a fast syscall, not
    /// a long block — the actual line-streaming + `wait()` work runs on a
    /// dedicated, internally-owned `std::thread`) and returns its
    /// `BackupHandle` immediately, before the first log line even arrives.
    /// That means this method needs neither the plan's `spawn_blocking`
    /// wrapper around the spawn step nor its `oneshot`-based handle
    /// handshake — `run_and_stream` is called directly (synchronous,
    /// millisecond-scale, matching the same "small enough to call inline
    /// from the UI-thread-callable method" posture this plan's own T6
    /// section already accepts for `resolve_tool_path`), and the
    /// `BackupHandle` it returns is handed straight back to the caller. Only
    /// the STREAMING side (`std_rx.recv()`, which genuinely blocks for the
    /// process's whole lifetime) needs to move onto a `spawn_blocking`
    /// thread — done once, looping internally and forwarding into the
    /// returned `tokio::sync::mpsc::Sender` via `blocking_send` (the
    /// sender-side counterpart to a blocking receiver, safe to call outside
    /// async context — same non-async-code accommodation `tokio::sync::mpsc`
    /// documents `blocking_send` for). This also means `BackupHandle`'s
    /// panic-recovery degenerate case (`from_already_gone` in the original
    /// plan sketch) is unneeded here: there is no `spawn_blocking` task that
    /// could panic before handing back a handle, so it is not added.
    pub fn run_external_tool(
        &self,
        program: String,
        args: Vec<String>,
        password: Option<String>,
    ) -> (tokio::sync::mpsc::Receiver<backup::BackupEvent>, backup::BackupHandle) {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let (std_tx, std_rx) = std::sync::mpsc::channel::<backup::BackupEvent>();

        let handle = backup::run_and_stream(&program, &args, password.as_deref(), &std_tx);

        // Forwarding loop: blocking std channel -> tokio channel, off any
        // runtime worker thread (a blocking `std_rx.recv()` must never run
        // there) — one `spawn_blocking` task, looping internally, rather
        // than one per message.
        self.handle().spawn_blocking(move || {
            while let Ok(ev) = std_rx.recv() {
                let terminal =
                    matches!(ev, backup::BackupEvent::Finished | backup::BackupEvent::Failed(_));
                if tx.blocking_send(ev).is_err() || terminal {
                    break;
                }
            }
        });

        (rx, handle)
    }

    /// G11 T4: MSSQL `BACKUP DATABASE` — allowed on read-only (design
    /// CURATION item 2, `backup::BackupOp::Backup` is exempt). Runs over ONE
    /// fresh connection (`open_spec`, dropped at the end), same one-shot
    /// shape `fetch_schema`/`test_connect` already use. `open_spec`'s
    /// `Engine::Mssql` arm is real since G15 T3 (`connect::open_config`
    /// dials out via `MssqlConnection::probe()`) — whatever `open_spec`
    /// returns (a real connect failure, or success) is what this sees; no
    /// MSSQL-specific handling is added around it here. `build_backup_sql`
    /// itself stays pg-bracket-quoted until G15 T4 dialectizes it.
    pub fn run_mssql_backup(
        &self,
        spec: ConnectSpec,
        database: String,
        server_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_mssql_backup_inner(spec, database, server_path, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    /// G11 T4: MSSQL restore — `SET SINGLE_USER` -> `RESTORE DATABASE` ->
    /// `SET MULTI_USER`, all three over the SAME dedicated connection
    /// (`Connection::execute`'s transaction-per-connection invariant), the
    /// closing `MULTI_USER` attempted even if `RESTORE` failed (best-effort,
    /// mirrors `drive_write_sequence`'s own "the ROLLBACK attempt's result
    /// is discarded" posture). Hard-blocked on read-only, no override
    /// (`backup::guard_backup_restore_read_only(BackupOp::Restore, ..)` is
    /// never exempt).
    pub fn run_mssql_restore(
        &self,
        spec: ConnectSpec,
        database: String,
        server_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_mssql_restore_inner(spec, database, server_path, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    /// G11 T4: SQLite `VACUUM INTO` via `Connection::execute` — allowed on
    /// read-only (design CURATION item 2).
    pub fn run_sqlite_backup(
        &self,
        spec: ConnectSpec,
        dest_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_sqlite_backup_inner(spec, dest_path, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    /// G11 T4: SQLite restore — magic-header check (`backup::sqlite_magic_header_ok`,
    /// design CURATION item 4) then `fs::copy` — no `Connection`/`ConnectSpec`
    /// involved at all (a plain file operation, no secret, no network
    /// round-trip).
    ///
    /// SECURITY (G11 T4 review MAJOR 2): unlike the other three backup/
    /// restore methods, this one has no `ConnectSpec` to read
    /// `spec_is_read_only` from — the caller (T6) already has `cfg.read_only`
    /// in hand before ever reaching this method, so it is threaded through
    /// explicitly as `read_only`. This method self-guards on it as its
    /// FIRST action (`backup::guard_backup_restore_read_only(BackupOp::Restore,
    /// read_only)`, same call every other restore/backup method here makes)
    /// — a write path whose only protection was a not-yet-written UI-layer
    /// caller would be unsafe by construction. T6's own pre-dispatch check
    /// is kept too (belt-and-braces, matching this codebase's established
    /// "each layer holds on its own" posture), but is no longer this
    /// method's SOLE protection.
    pub fn run_sqlite_restore(
        &self,
        db_path: String,
        backup_path: String,
        read_only: bool,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                run_sqlite_restore_inner(&db_path, &backup_path, read_only)
            })
            .await
            .unwrap_or_else(|_| Err(QueryError::msg("restore task panicked")));
            let _ = tx.send(result);
        });
        rx
    }
}

/// G7 T5: pure SQL composer + guard, extracted as a standalone function
/// specifically so the CURATION-REQUIRED test can prove the WHERE-box guard
/// fires BEFORE `open_spec` is ever called (design CURATION §0.2: "REQUIRED
/// test: `fetch_diff_side` with a WHERE-box payload failing
/// `is_read_statement` is refused client-side"). `dbc_core::quote_qualified_d`
/// is the SAME quoting function `sandbox.rs` already uses for its own
/// write-path SQL (Global Constraints' quoting note). G15 T5: gained
/// `dialect` — MSSQL bracket quoting via `quote_qualified_d` is no longer
/// out of scope now that `connect::open_config` wires MSSQL (T3); the
/// read-only guard below is bracket-aware too (`is_read_statement_d`).
fn compose_diff_select(
    dialect: dbc_core::Dialect,
    schema: Option<&str>,
    table: &str,
    where_clause: Option<&str>,
) -> Result<String, QueryError> {
    let base = format!("SELECT * FROM {}", dbc_core::quote_qualified_d(dialect, schema, table));
    let sql = match where_clause {
        Some(w) if !w.trim().is_empty() => format!("{base} WHERE {w}"),
        _ => base,
    };
    if !dbc_core::is_read_statement_d(&sql, dialect) {
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
    // Dialect captured BEFORE `spec` moves into `open_spec` (G15 T5).
    let dialect = spec_dialect(&spec);
    // Composed + guarded BEFORE `open_spec` — a failing WHERE box never
    // reaches a connection attempt (CURATION binding requirement).
    let sql = compose_diff_select(dialect, schema.as_deref(), &table, where_clause.as_deref())?;
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

/// G15 T5: `ConnectSpec` -> `dbc_core::Dialect` for every transaction-control
/// sequence in this file (`drive_write_sequence`, `drive_analyze_write`,
/// `run_csv_import_inner`, `connect_and_run_many_inner`) and for
/// `compose_diff_select`'s quoting/read-guard — the single spot that maps
/// `dbc_state::Engine` to `dbc_core::Dialect` for the tx-control call sites
/// (the SEPARATE `main.rs::dialect_for_engine` mapping — T8-gated, currently
/// `Mssql => None` — governs the splitter/auto-limit call sites and is out
/// of scope for this task's single-writer boundary). Captured BEFORE `spec`
/// moves into `open_spec`, mirroring `spec_is_read_only`'s own "capture up
/// front" convention.
fn spec_dialect(spec: &ConnectSpec) -> dbc_core::Dialect {
    match spec {
        ConnectSpec::Config { cfg, .. } => match cfg.engine {
            dbc_state::Engine::Postgres => dbc_core::Dialect::Postgres,
            dbc_state::Engine::Sqlite => dbc_core::Dialect::Sqlite,
            dbc_state::Engine::Mssql => dbc_core::Dialect::Mssql,
        },
        // CLI-arg URLs have no MSSQL form (main.rs::engine_from_url: a
        // postgres[ql]:// scheme or a sqlite file path only) — mirrors that
        // exact dispatch.
        ConnectSpec::Url(url) => {
            if url.starts_with("postgres://") || url.starts_with("postgresql://") {
                dbc_core::Dialect::Postgres
            } else {
                dbc_core::Dialect::Sqlite
            }
        }
    }
}

/// G12 T2: dispatch decision behind `run_script_statement`'s per-statement
/// read-only matrix — pure so the whole matrix is unit tested without any
/// connection. Fail-closed inputs (`is_read_statement_d` returning `false`
/// for an unterminated/unrecognized construct) are treated as writes, not
/// reads — same posture `is_read_statement_d`'s own doc comment mandates, so
/// an ambiguous statement on a read-only connection is rejected rather than
/// risked.
/// Consumed by both `run_script` (Task 1, wired into `main.rs` by Task 3)
/// and `connect_and_run_many` (Task 2, wired into `main.rs::run_many`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtDispatch {
    RunAsRead,
    RunAsWrite,
    RejectReadOnly,
}

/// G15 T5: gained `dialect` (was pg-only `is_read_statement`) — a
/// bracket-quoted reserved word (`[Delete]`, `[Order]`, `[Top]`) must never
/// be mistaken for the bare keyword on an MSSQL connection (`is_read_statement_d`'s
/// own doc comment). Postgres/Sqlite callers are unaffected.
pub fn dispatch_statement(sql: &str, read_only: bool, dialect: dbc_core::Dialect) -> StmtDispatch {
    if dbc_core::is_read_statement_d(sql, dialect) {
        StmtDispatch::RunAsRead
    } else if read_only {
        StmtDispatch::RejectReadOnly
    } else {
        StmtDispatch::RunAsWrite
    }
}

/// G12 T2: what to do with the REST of the run after one statement fails —
/// pure decision behind `drive_script`'s failure handling (design §2
/// matrix). `(Continue, WholeRun)` is a combination the UI never offers,
/// but the runner still fails SAFE if it arrives anyway: abort, never
/// continue inside one open transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    AbortRun,
    NextStatement,
    NextFile,
}

pub fn failure_action(policy: ErrorPolicy, scope: TxScope) -> FailureAction {
    match (policy, scope) {
        (ErrorPolicy::Stop, _) => FailureAction::AbortRun,
        (ErrorPolicy::Continue, TxScope::None) => FailureAction::NextStatement,
        (ErrorPolicy::Continue, TxScope::PerFile) => FailureAction::NextFile,
        (ErrorPolicy::Continue, TxScope::WholeRun) => FailureAction::AbortRun,
    }
}

/// G12 T2: single-line-collapsed, char-safe cap for `ScriptEvent`
/// `sql_preview` fields — display-safe, no credentials/result data
/// (§3-novela: statements shown must be display-safe). Same collapse idiom
/// as `tabs::collapse_title`/`history_panel::collapse_sql` — reused
/// directly with a different cap rather than re-implementing the collapse
/// (same precedent `history_panel::collapse_sql` itself documents against
/// `tabs::collapse_title`).
pub const SQL_PREVIEW_CAP: usize = 200;
pub fn sql_preview(sql: &str) -> String {
    crate::history_panel::collapse_sql(sql, SQL_PREVIEW_CAP)
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
///
/// G15 T5: `dialect` selects the tx-control TEXT via `dbc_core::tx_begin_sql`/
/// `tx_commit_sql`/`tx_rollback_sql` — pg/sqlite get the historic
/// byte-identical `"BEGIN"`/`"COMMIT"`/`"ROLLBACK"` literals (zero behavior
/// change), MSSQL gets the fused `"SET XACT_ABORT ON; BEGIN TRANSACTION"`
/// (fixes G12's bare-`BEGIN` bug — bare `BEGIN` is invalid T-SQL). §3b: on
/// MSSQL, once `XACT_ABORT` aborts the transaction after a runtime error,
/// the explicit ROLLBACK below fails with "no corresponding BEGIN
/// TRANSACTION" — exactly the case this function's existing `let _ =`
/// discard posture already tolerates (verified by the driver's §3c matrix,
/// case 3).
async fn drive_write_sequence(
    conn: &mut dyn Connection,
    statements: &[crate::admin_sql::WriteStatement],
    cancel: CancelToken,
    dialect: dbc_core::Dialect,
) -> Result<u64, QueryError> {
    if let Err(e) = conn.execute(dbc_core::tx_begin_sql(dialect), cancel.clone()).await {
        let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.clone()).await;
        return Err(e);
    }
    let mut total: u64 = 0;
    for st in statements {
        match conn.execute(&st.exec_sql, cancel.clone()).await {
            Ok(affected) => {
                if affected_mismatch(st.expected_affected, affected) {
                    let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.clone()).await;
                    return Err(QueryError::msg(AFFECTED_MISMATCH_MSG));
                }
                total += affected;
            }
            Err(e) => {
                let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.clone()).await;
                // G10 CURATION item 3 (redaction hardening): the surfaced
                // error is paired with `display_sql` ONLY — `exec_sql` is
                // used exactly once, in the `execute()` call above, and
                // must never appear in any error/status/log/history string.
                // For sandbox statements display_sql == exec_sql, so G5's
                // error surface just gains helpful statement context.
                return Err(QueryError::msg(format!(
                    "{} — příkaz: {}",
                    e.message, st.display_sql
                )));
            }
        }
    }
    if let Err(e) = conn.execute(dbc_core::tx_commit_sql(dialect), cancel.clone()).await {
        let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.clone()).await;
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
    statements: &[crate::admin_sql::WriteStatement],
    cancel: CancelToken,
    dialect: dbc_core::Dialect,
    timeout_secs: Option<u64>,
) -> Result<u64, QueryError> {
    match timeout_secs {
        Some(t) => {
            let sequence = drive_write_sequence(conn, statements, cancel.clone(), dialect);
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
                    let rollback = conn.execute(dbc_core::tx_rollback_sql(dialect), CancelToken::new());
                    let _ =
                        tokio::time::timeout(Duration::from_secs(ROLLBACK_GRACE_SECS), rollback)
                            .await;
                    Err(QueryError::msg(format!("[timeout] aplikace překročila {t}s")))
                }
            }
        }
        None => drive_write_sequence(conn, statements, cancel, dialect).await,
    }
}

/// G5 Task 4: `run_write_transaction`'s async body — guard, open, drive
/// (bounded). See `run_write_transaction`'s doc comment for the
/// connection-lifetime/decoupling rationale, and `drive_write_sequence_bounded`
/// for the timeout/cancel/rollback mechanics.
async fn run_write_transaction_inner(
    spec: ConnectSpec,
    statements: Vec<crate::admin_sql::WriteStatement>,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
) -> Result<u64, QueryError> {
    guard_not_read_only(spec_is_read_only(&spec))?;
    // Captured BEFORE `spec` moves into `open_spec` (G15 T5).
    let dialect = spec_dialect(&spec);
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();
    drive_write_sequence_bounded(&mut *opened.conn, &statements, cancel, dialect, timeout_secs).await
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
/// G15 T5: `dialect` selects the tx-control text — see `drive_write_sequence`'s
/// doc comment for the fused-MSSQL-begin/§3b rationale (identical here).
async fn drive_analyze_write(
    conn: &mut dyn Connection,
    explain_analyze_sql: &str,
    cancel: CancelToken,
    dialect: dbc_core::Dialect,
) -> Result<String, QueryError> {
    if let Err(e) = conn.execute(dbc_core::tx_begin_sql(dialect), cancel.clone()).await {
        let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.clone()).await;
        return Err(e);
    }
    let plan_result = drain_single_text_cell(conn, explain_analyze_sql, cancel.clone()).await;
    let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.clone()).await; // ALWAYS — see doc comment.
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
    dialect: dbc_core::Dialect,
    timeout_secs: Option<u64>,
) -> Result<String, QueryError> {
    match timeout_secs {
        Some(t) => {
            let sequence = drive_analyze_write(conn, explain_analyze_sql, cancel.clone(), dialect);
            match tokio::time::timeout(Duration::from_secs(t), sequence).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    cancel.cancel();
                    let rollback = conn.execute(dbc_core::tx_rollback_sql(dialect), CancelToken::new());
                    let _ =
                        tokio::time::timeout(Duration::from_secs(ROLLBACK_GRACE_SECS), rollback)
                            .await;
                    Err(QueryError::msg(format!("[timeout] analýza překročila {t}s")))
                }
            }
        }
        None => drive_analyze_write(conn, explain_analyze_sql, cancel, dialect).await,
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
    // Captured BEFORE `spec` moves into `open_spec` (G15 T5).
    let dialect = spec_dialect(&spec);
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();
    drive_analyze_write_bounded(&mut *opened.conn, &explain_analyze_sql, cancel, dialect, timeout_secs).await
    // `opened` drops here unconditionally — the ultimate backstop, same as run_write_transaction_inner.
}

/// G11 T4 review MAJOR 1: joins a possibly-relative `path` onto the current
/// working directory so `resolve_tool_path`'s configured-path branch always
/// hands back an absolute path — see that function's SECURITY doc comment.
/// Deliberately does NOT call `std::fs::canonicalize` (which would resolve
/// symlinks and, on Windows, prefix the result with `\\?\`, an
/// extended-length-path form some external tools handle poorly) — a plain
/// `current_dir().join(path)` is enough to defeat the CWD-relative-lookup
/// class of planting this guards against, without changing the path's
/// surface form for an already-absolute input.
fn absolutize(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p).to_string_lossy().to_string(),
            Err(_) => path.to_string(),
        }
    }
}

/// G11 T4: resolves an external tool's path per design §1's three-step
/// order: (1) `configured` if `Some` — validated as an existing FILE HERE,
/// at use time, not at save time (a stale saved path surfaces as an error,
/// never silently falls through); (2) PATH via `backup::find_on_path`; (3)
/// glob `C:\Program Files\PostgreSQL\*\bin\<name>.exe`, highest version wins
/// (`backup::pick_highest_version_dir` — pure, given the `(path, mtime)`
/// pairs this function reads from disk via `std::fs::read_dir`, the one
/// place in this module that touches that directory). Errors are
/// Czech-language, user-facing strings (never a panic on a missing/
/// malformed directory).
///
/// SECURITY (CWE-427, binary planting — G11 T4 review MAJOR 1): the string
/// returned here is handed straight to `Command::new` by every caller
/// (`run_external_tool`), so it MUST be an absolute path in every branch,
/// never a bare name — a bare name would let Windows' `CreateProcess`
/// search the application directory and the current working directory
/// BEFORE PATH, so a planted `pg_dump.exe` sitting in a writable CWD would
/// run instead of the real tool and receive the real `PGPASSWORD` set on
/// its (attacker-controlled) child environment. All three steps are
/// absolute: step 1 is `absolutize`d (defense in depth — a user-typed
/// configured path is expected to already be absolute, e.g. from a file
/// picker, but is not assumed to be); step 2 relies on `backup::find_on_path`
/// itself now returning the fully-resolved path rather than the bare
/// probed name; step 3's glob join is already absolute (`base` is a fixed
/// absolute root).
pub fn resolve_tool_path(configured: Option<&str>, name: &str) -> Result<String, QueryError> {
    if let Some(path) = configured {
        return if std::path::Path::new(path).is_file() {
            Ok(absolutize(path))
        } else {
            Err(QueryError::msg(format!(
                "nakonfigurovaná cesta k {name} neexistuje: {path} — nastavte ji znovu"
            )))
        };
    }
    if let Some(resolved) = backup::find_on_path(name) {
        return Ok(resolved);
    }
    let exe = format!("{name}.exe");
    let base = std::path::Path::new(r"C:\Program Files\PostgreSQL");
    let mut candidates: Vec<(String, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            if let Some(p) = entry.path().to_str() {
                candidates.push((p.to_string(), mtime));
            }
        }
    }
    match backup::pick_highest_version_dir(&candidates) {
        Some(dir) => {
            let full = std::path::Path::new(&dir).join("bin").join(&exe);
            if full.is_file() {
                Ok(full.to_string_lossy().to_string())
            } else {
                Err(QueryError::msg(format!("{name} nenalezen — nastavte cestu ručně")))
            }
        }
        None => Err(QueryError::msg(format!("{name} nenalezen — nastavte cestu ručně"))),
    }
}

/// G11 T4: `QueryRunner::run_mssql_backup`'s async body — belt-and-braces
/// read-only guard (Backup is exempt, design CURATION item 2), open ONE
/// dedicated connection (`open_spec`, dropped at the end), issue
/// `backup::build_backup_sql` via `Connection::execute` (sanctioned per this
/// commit's `connection.rs` doc-comment amendment).
async fn run_mssql_backup_inner(
    spec: ConnectSpec,
    database: String,
    server_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Backup, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    let sql = backup::build_backup_sql(&database, &server_path);
    opened.conn.execute(&sql, CancelToken::new()).await?;
    Ok(())
}

/// G11 T4: `QueryRunner::run_mssql_restore`'s async body — hard-blocked on
/// read-only, no override (`BackupOp::Restore` is never exempt). Opens ONE
/// dedicated connection and issues `SET SINGLE_USER` -> `RESTORE DATABASE`
/// -> `SET MULTI_USER` over that SAME connection, in order
/// (`Connection::execute`'s transaction-per-connection invariant). The
/// closing `SET MULTI_USER` is attempted even if `RESTORE` failed
/// (best-effort, mirrors `drive_write_sequence`'s own "the ROLLBACK
/// attempt's result is discarded" posture) — the FIRST failure among the
/// three statements (i.e. `RESTORE`'s, since `SINGLE_USER` failing returns
/// immediately via `?` before `RESTORE` is even attempted) is what's
/// returned to the caller; a subsequent `MULTI_USER` failure never
/// overrides it.
async fn run_mssql_restore_inner(
    spec: ConnectSpec,
    database: String,
    server_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Restore, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();

    opened
        .conn
        .execute(&backup::build_single_user_sql(&database, false), cancel.clone())
        .await?;
    let restore_result = opened
        .conn
        .execute(&backup::build_restore_sql(&database, &server_path), cancel.clone())
        .await;
    // Best-effort MULTI_USER regardless of RESTORE's outcome — its own
    // result never overrides `restore_result` (see doc comment above).
    let _ = opened
        .conn
        .execute(&backup::build_single_user_sql(&database, true), cancel)
        .await;
    restore_result.map(|_| ())
}

/// G11 T4: `QueryRunner::run_sqlite_backup`'s async body — belt-and-braces
/// read-only guard (Backup is exempt), open ONE dedicated connection, issue
/// `backup::build_vacuum_into_sql` via `Connection::execute` (sanctioned).
async fn run_sqlite_backup_inner(
    spec: ConnectSpec,
    dest_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Backup, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    opened.conn.execute(&backup::build_vacuum_into_sql(&dest_path), CancelToken::new()).await?;
    Ok(())
}

/// G11 T4: `QueryRunner::run_sqlite_restore`'s sync body (run inside
/// `spawn_blocking` by its caller — plain file I/O, no `Connection`
/// involved). SECURITY (T4 review MAJOR 2): self-guards on `read_only` as
/// its FIRST action, before even opening `backup_path` — no I/O is
/// attempted on a read-only refusal, matching every other backup/restore
/// method's own "guard before I/O" shape. T6's own pre-dispatch check stays
/// too (belt-and-braces), but this is no longer the sole protection. Design
/// CURATION item 4, hard requirement (checked second): reads the first 16
/// bytes of `backup_path` and refuses (no copy attempted) unless they are
/// exactly `backup::SQLITE_MAGIC_HEADER`.
fn run_sqlite_restore_inner(db_path: &str, backup_path: &str, read_only: bool) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Restore, read_only)
        .map_err(QueryError::msg)?;
    let mut header = [0u8; 16];
    let mut f = std::fs::File::open(backup_path).map_err(|e| QueryError::msg(e.to_string()))?;
    use std::io::Read;
    let n = f.read(&mut header).map_err(|e| QueryError::msg(e.to_string()))?;
    if !backup::sqlite_magic_header_ok(&header[..n]) {
        return Err(QueryError::msg("soubor není SQLite databáze"));
    }
    drop(f);
    std::fs::copy(backup_path, db_path).map_err(|e| QueryError::msg(e.to_string()))?;
    Ok(())
}

/// G12 T2: read-chunk size for streaming `.sql` files into the splitter
/// (design §2).
const SCRIPT_READ_CHUNK: usize = 64 * 1024;

/// G12 T2: one statement — dispatch per the read-only matrix, per-statement
/// child cancel + timeout. `Ok(Some(n))` — `n` is the drained row count for
/// a read, the affected-row count for a write. CURATION item 1(c): the
/// SHARED `guard_not_read_only` guard produces the read-only rejection, no
/// fresh read-only logic here.
async fn run_script_statement(
    conn: &mut dyn Connection,
    sql: &str,
    read_only: bool,
    dialect: dbc_core::Dialect,
    timeout_secs: Option<u64>,
    run_cancel: &CancelToken,
) -> Result<Option<u64>, QueryError> {
    let action = dispatch_statement(sql, read_only, dialect);
    if action == StmtDispatch::RejectReadOnly {
        return Err(guard_not_read_only(true).unwrap_err());
    }
    let stmt_cancel = run_cancel.child_token();
    let fut = async {
        match action {
            StmtDispatch::RunAsRead => {
                let mut stream = conn.query(sql, stmt_cancel.clone()).await?;
                let mut rows: u64 = 0;
                while let Some(item) = stream.batches.recv().await {
                    rows += item?.num_rows() as u64;
                }
                Ok(Some(rows))
            }
            StmtDispatch::RunAsWrite => conn.execute(sql, stmt_cancel.clone()).await.map(Some),
            StmtDispatch::RejectReadOnly => unreachable!("handled above"),
        }
    };
    match timeout_secs {
        Some(t) => match tokio::time::timeout(Duration::from_secs(t), fut).await {
            Ok(r) => r,
            Err(_elapsed) => {
                // Protocol-level cancel of the in-flight statement ONLY —
                // the run-level `run_cancel` is untouched, see
                // `ScriptRunOptions::statement_timeout_secs`'s doc comment.
                stmt_cancel.cancel();
                Err(QueryError::msg(format!("[timeout] statement exceeded {t}s")))
            }
        },
        None => fut.await,
    }
}

/// G12 T2: streams `path` in `SCRIPT_READ_CHUNK` pieces through a fresh
/// `StatementSplitter`, returning every completed statement in order
/// (including the splitter's own EOF flush of a final statement missing its
/// trailing `;`). Deviation from the plan's literal per-chunk-dispatch
/// grounding text (documented per this task's "reality/tests win, document
/// deviations" instruction): the plan describes dispatching each statement
/// the MOMENT the splitter completes it, interleaved with the file read
/// loop. That shape requires breaking two different labeled loops (the byte
/// read loop and the caller's per-file loop) from two different statement
/// sources (`push`'s Vec and `finish`'s tail) — doable, but only correctly
/// with a `macro_rules!` capturing the outer loop labels as `:lifetime`
/// metavariables (plain fn extraction can't `break` a caller's loop, and a
/// macro's own labels are hygienically distinct from the call site's).
/// Parsing the WHOLE file into a `Vec<String>` first (still streamed off
/// disk in bounded chunks — this function never holds more than one
/// `SCRIPT_READ_CHUNK` buffer plus the splitter's own pending-statement
/// buffer at a time) and dispatching afterward is behaviorally identical
/// for every `ScriptEvent` a caller observes (same events, same order) and
/// is far simpler to keep correct. `drive_script` stays fully iterative
/// either way (Global Constraints: no stack overflow on a pathological
/// script — this function loops, never recurses).
/// Deviation (found while fixing a plain-`cargo build` failure, documented
/// per this task's "reality/tests win" instruction): `tokio::fs` requires
/// tokio's `fs` feature, which is enabled only via other crates'
/// dev-dependencies in this workspace (`dbc-driver-sqlite`/`-postgres`'s
/// `features = ["full"]` dev-dep) — invisible to `cargo test` (dev-deps
/// feature-unify into the graph) but a hard compile error on a plain
/// `cargo build -p dbc-ui`. Rather than growing `dbc-ui`'s real `tokio`
/// dependency just for this, the whole read+split runs inside ONE
/// `spawn_blocking` over `std::fs`/`std::io::Read` — the same "blocking
/// work never runs on a runtime worker thread" dispatch `open_spec` already
/// uses for the driver connect step.
/// G15 T5 (design §2c iv): `SplitError::UnsupportedGoCount` (an MSSQL `GO
/// <n>` repeat count, refused fail-closed by the splitter) gets a dedicated
/// Czech message instead of the generic Debug-formatted text every other
/// variant still gets. Deliberately duplicated in T4's own SQL-composer
/// call sites (`split_error_message` there too) rather than shared — T4/T5
/// are parallel, disjoint-file tasks (Global Constraints' single-writer
/// rule), and this keeps both independently compilable/testable.
fn split_error_message(e: &dbc_core::SplitError) -> String {
    match e {
        dbc_core::SplitError::UnsupportedGoCount => {
            "GO s počtem opakování není podporováno".to_string()
        }
        other => format!("{other:?}"),
    }
}

async fn read_and_split_file(
    path: &std::path::Path,
    dialect: dbc_core::Dialect,
) -> Result<Vec<String>, QueryError> {
    let path = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<String>, QueryError> {
        use std::io::Read;
        let mut file = std::fs::File::open(&path)
            .map_err(|e| QueryError::msg(format!("[soubor] {}: {e}", path.display())))?;
        let mut splitter = dbc_core::StatementSplitter::new(dialect);
        let mut stmts = Vec::new();
        let mut chunk = vec![0u8; SCRIPT_READ_CHUNK];
        loop {
            let n = file
                .read(&mut chunk)
                .map_err(|e| QueryError::msg(format!("[soubor] {}: {e}", path.display())))?;
            if n == 0 {
                match splitter.finish() {
                    Ok(Some(tail)) => stmts.push(tail),
                    Ok(None) => {}
                    Err(e) => {
                        return Err(QueryError::msg(format!(
                            "[skript] neúplný SQL konstrukt na konci souboru: {}",
                            split_error_message(&e)
                        )));
                    }
                }
                return Ok(stmts);
            }
            match splitter.push(&chunk[..n]) {
                Ok(mut more) => stmts.append(&mut more),
                Err(e) => {
                    return Err(QueryError::msg(format!(
                        "[soubor] {}: {}",
                        path.display(),
                        split_error_message(&e)
                    )))
                }
            }
        }
    })
    .await;
    match result {
        Ok(inner) => inner,
        Err(_) => Err(QueryError::msg("[soubor] čtení souboru selhalo (panic)")),
    }
}

/// G12 T2: maps a FILE-LEVEL error (open/read/split/utf8, or a per-file
/// `BEGIN`/`COMMIT` control-statement failure) to a `FailureAction` — these
/// can't meaningfully "continue to the next statement" of a broken/unopened
/// file, so they collapse the full `failure_action` matrix down to just
/// `Stop -> AbortRun`, `Continue -> NextFile`, regardless of `tx_scope`
/// (design §2 deviation, documented on `drive_script`).
fn file_level_action(policy: ErrorPolicy) -> FailureAction {
    match policy {
        ErrorPolicy::Stop => FailureAction::AbortRun,
        ErrorPolicy::Continue => FailureAction::NextFile,
    }
}

/// G12 T2: the run driver — `TxScope`-appropriate `BEGIN`/`COMMIT`/
/// `ROLLBACK`, streaming `ScriptEvent`s over `tx` as it goes, honoring
/// `opts.error_policy` via `failure_action`/`file_level_action`. Fully
/// iterative (two nested `for`/`for` loops over files/statements, no
/// recursion) so a script with many statements can't stack-overflow
/// (Global Constraints). Kept generic over `&mut dyn Connection` (not
/// `ConnectSpec`/`open_spec`) so it's directly testable over a temp-file
/// sqlite connection, same posture as `drive_write_sequence`.
async fn drive_script(
    conn: &mut dyn Connection,
    read_only: bool,
    files: &[std::path::PathBuf],
    opts: &ScriptRunOptions,
    cancel: CancelToken,
    tx: &tokio::sync::mpsc::Sender<ScriptEvent>,
) {
    let run_started = Instant::now();
    let mut files_run = 0usize;
    let mut statements_run = 0usize;
    let mut statements_failed = 0usize;
    let mut aborted = false;
    let mut stmt_index = 0usize;

    // Two-tier cancellation discipline (per `connect_and_run`'s doc):
    // checked once here before anything (including a WholeRun `BEGIN`)
    // happens, then again before every file and every statement below.
    if cancel.is_cancelled() {
        let _ = tx
            .send(ScriptEvent::RunFinished {
                files_run,
                statements_run,
                statements_failed,
                elapsed: run_started.elapsed(),
                aborted: true,
            })
            .await;
        return;
    }

    if opts.tx_scope == TxScope::WholeRun {
        if conn.execute(dbc_core::tx_begin_sql(opts.dialect), cancel.child_token()).await.is_err() {
            // A BEGIN failure aborts the run immediately — no tx opened,
            // nothing to roll back.
            let _ = tx
                .send(ScriptEvent::RunFinished {
                    files_run,
                    statements_run,
                    statements_failed,
                    elapsed: run_started.elapsed(),
                    aborted: true,
                })
                .await;
            return;
        }
    }

    'files: for (index, path) in files.iter().enumerate() {
        if cancel.is_cancelled() {
            aborted = true;
            break 'files;
        }
        let _ = tx
            .send(ScriptEvent::FileStarted { path: path.clone(), index, total_files: files.len() })
            .await;
        let file_started = Instant::now();
        let mut file_stmts_run = 0usize;
        let mut file_stmts_failed = 0usize;
        // Set once ANY failure (file-level or per-statement) happens in
        // this file — routes to the shared rollback/abort handling below
        // instead of the normal end-of-file `COMMIT`.
        let mut stop_action: Option<FailureAction> = None;

        if opts.tx_scope == TxScope::PerFile {
            if let Err(e) = conn.execute(dbc_core::tx_begin_sql(opts.dialect), cancel.child_token()).await {
                let _ = tx.send(ScriptEvent::StatementFailed { stmt_index, error: e }).await;
                statements_failed += 1;
                file_stmts_failed += 1;
                stmt_index += 1;
                stop_action = Some(file_level_action(opts.error_policy));
            }
        }

        if stop_action.is_none() {
            match read_and_split_file(path, opts.dialect).await {
                Ok(stmts) => {
                    for stmt in &stmts {
                        if cancel.is_cancelled() {
                            stop_action = Some(FailureAction::AbortRun);
                            break;
                        }
                        let _ = tx
                            .send(ScriptEvent::StatementStarted {
                                stmt_index,
                                sql_preview: sql_preview(stmt),
                            })
                            .await;
                        let t0 = Instant::now();
                        match run_script_statement(
                            conn,
                            stmt,
                            read_only,
                            opts.dialect,
                            opts.statement_timeout_secs,
                            &cancel,
                        )
                        .await
                        {
                            Ok(affected) => {
                                let _ = tx
                                    .send(ScriptEvent::StatementFinished {
                                        stmt_index,
                                        affected,
                                        elapsed: t0.elapsed(),
                                    })
                                    .await;
                                statements_run += 1;
                                file_stmts_run += 1;
                                stmt_index += 1;
                            }
                            Err(e) => {
                                let _ =
                                    tx.send(ScriptEvent::StatementFailed { stmt_index, error: e }).await;
                                statements_failed += 1;
                                file_stmts_failed += 1;
                                stmt_index += 1;
                                let action = failure_action(opts.error_policy, opts.tx_scope);
                                if action != FailureAction::NextStatement {
                                    stop_action = Some(action);
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(file_err) => {
                    let _ =
                        tx.send(ScriptEvent::StatementFailed { stmt_index, error: file_err }).await;
                    statements_failed += 1;
                    file_stmts_failed += 1;
                    stmt_index += 1;
                    stop_action = Some(file_level_action(opts.error_policy));
                }
            }
        }

        // Clean end of file: commit a per-file tx if nothing failed.
        if stop_action.is_none() && opts.tx_scope == TxScope::PerFile {
            if let Err(e) = conn.execute(dbc_core::tx_commit_sql(opts.dialect), cancel.child_token()).await {
                let _ = conn.execute(dbc_core::tx_rollback_sql(opts.dialect), cancel.child_token()).await;
                let _ = tx.send(ScriptEvent::StatementFailed { stmt_index, error: e }).await;
                statements_failed += 1;
                file_stmts_failed += 1;
                stmt_index += 1;
                stop_action = Some(file_level_action(opts.error_policy));
            }
        }

        if let Some(action) = stop_action {
            match action {
                FailureAction::AbortRun => {
                    if opts.tx_scope != TxScope::None {
                        let _ = conn.execute(dbc_core::tx_rollback_sql(opts.dialect), cancel.child_token()).await;
                    }
                    aborted = true;
                }
                FailureAction::NextFile => {
                    if opts.tx_scope == TxScope::PerFile {
                        let _ = conn.execute(dbc_core::tx_rollback_sql(opts.dialect), cancel.child_token()).await;
                    }
                    // WholeRun: the run-level tx stays open — a broken file
                    // is skipped, not the whole accumulated run.
                }
                FailureAction::NextStatement => unreachable!("stop_action is never NextStatement"),
            }
        }

        let _ = tx
            .send(ScriptEvent::FileFinished {
                path: path.clone(),
                statements_run: file_stmts_run,
                statements_failed: file_stmts_failed,
                elapsed: file_started.elapsed(),
            })
            .await;
        files_run += 1;

        if aborted {
            break 'files;
        }
    }

    if !aborted && opts.tx_scope == TxScope::WholeRun {
        if conn.execute(dbc_core::tx_commit_sql(opts.dialect), cancel.child_token()).await.is_err() {
            let _ = conn.execute(dbc_core::tx_rollback_sql(opts.dialect), cancel.child_token()).await;
            aborted = true;
        }
    }

    let _ = tx
        .send(ScriptEvent::RunFinished {
            files_run,
            statements_run,
            statements_failed,
            elapsed: run_started.elapsed(),
            aborted,
        })
        .await;
}

/// G12 T5: runs ONE statement of a `connect_and_run_many` batch — dispatch
/// per the read-only matrix (CURATION item 1(c): the SHARED guard produces
/// the rejection, no fresh read-only logic here), sending
/// `MultiQueryEvent::StatementStarted`/`Batch`/`StatementFinished` as it
/// streams a read, or just `StatementStarted{columns: None}` +
/// `StatementFinished{affected: Some(n)}` for a write. `Err(())` means this
/// statement failed — its `StatementFailed` has already been sent, and the
/// caller stops (design §4: stop on first error, no continue policy here).
async fn run_one_multi_statement(
    conn: &mut dyn Connection,
    index: usize,
    total: usize,
    sql: &str,
    read_only: bool,
    dialect: dbc_core::Dialect,
    timeout_secs: Option<u64>,
    run_cancel: &CancelToken,
    tx: &tokio::sync::mpsc::Sender<MultiQueryEvent>,
) -> Result<(), ()> {
    let action = dispatch_statement(sql, read_only, dialect);
    if action == StmtDispatch::RejectReadOnly {
        let _ = tx
            .send(MultiQueryEvent::StatementFailed { index, error: guard_not_read_only(true).unwrap_err() })
            .await;
        return Err(());
    }
    let stmt_cancel = run_cancel.child_token();
    let started = Instant::now();
    let fut = async {
        match action {
            StmtDispatch::RunAsRead => {
                let mut stream = conn.query(sql, stmt_cancel.clone()).await?;
                let _ = tx
                    .send(MultiQueryEvent::StatementStarted {
                        index,
                        total,
                        columns: Some(stream.columns.clone()),
                    })
                    .await;
                while let Some(item) = stream.batches.recv().await {
                    let _ = tx.send(MultiQueryEvent::Batch(item?)).await;
                }
                Ok(None)
            }
            StmtDispatch::RunAsWrite => {
                let _ =
                    tx.send(MultiQueryEvent::StatementStarted { index, total, columns: None }).await;
                conn.execute(sql, stmt_cancel.clone()).await.map(Some)
            }
            StmtDispatch::RejectReadOnly => unreachable!("handled above"),
        }
    };
    let result = match timeout_secs {
        Some(t) => match tokio::time::timeout(Duration::from_secs(t), fut).await {
            Ok(r) => r,
            Err(_elapsed) => {
                stmt_cancel.cancel();
                Err(QueryError::msg(format!("[timeout] statement exceeded {t}s")))
            }
        },
        None => fut.await,
    };
    match result {
        Ok(affected) => {
            let _ = tx
                .send(MultiQueryEvent::StatementFinished { index, affected, elapsed: started.elapsed() })
                .await;
            Ok(())
        }
        Err(e) => {
            let _ = tx.send(MultiQueryEvent::StatementFailed { index, error: e }).await;
            Err(())
        }
    }
}

/// G12 T5: `QueryRunner::connect_and_run_many`'s async body — one dedicated
/// connection for the whole batch (same `Connection::execute`
/// session-sharing rationale every other one-shot in this file follows),
/// dispatching every statement via `run_one_multi_statement` in order,
/// stopping at the first failure. Extracted from the `spawn` closure so
/// it's directly testable — same "`_inner` function, driven under
/// `#[tokio::test]` with `Handle::current()`" precedent as
/// `run_write_transaction_inner`.
async fn connect_and_run_many_inner(
    spec: ConnectSpec,
    statements: Vec<String>,
    cancel: CancelToken,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
    tx: tokio::sync::mpsc::Sender<MultiQueryEvent>,
) {
    if cancel.is_cancelled() {
        return;
    }
    // Captured BEFORE `spec` moves into `open_spec` — same convention
    // `run_script`/`open_monitor` use.
    let read_only = spec_is_read_only(&spec);
    // G15 T5: dialect for `dispatch_statement`'s bracket-aware read
    // classification (`is_read_statement_d`) — also captured before `spec`
    // moves.
    let dialect = spec_dialect(&spec);
    let mut opened = match open_spec(spec, handle).await {
        Ok(o) => o,
        Err(e) => {
            let _ = tx.send(MultiQueryEvent::StatementFailed { index: 0, error: e }).await;
            return;
        }
    };
    if cancel.is_cancelled() {
        return;
    }
    let conn = &mut *opened.conn;
    let total = statements.len();
    for (index, sql) in statements.iter().enumerate() {
        if cancel.is_cancelled() {
            return;
        }
        if run_one_multi_statement(conn, index, total, sql, read_only, dialect, timeout_secs, &cancel, &tx)
            .await
            .is_err()
        {
            return; // stop on first error (design §4) — `opened` drops here.
        }
    }
    let _ = tx.send(MultiQueryEvent::RunFinished).await;
    // `opened` (connection + tunnel) drops here unconditionally.
}

/// G12 T7: read-chunk producer channel depth — small and bounded (design
/// §5: the producer never gets more than this many batches ahead of the
/// driver, so at most `CSV_IMPORT_PRODUCER_DEPTH * CSV_IMPORT_BATCH_SIZE`
/// rows are ever buffered in memory at once).
const CSV_IMPORT_PRODUCER_DEPTH: usize = 4;

/// G12 T7: the run driver — SHARED guard first, then open, then delegates to
/// `run_csv_import_drive` for everything from BEGIN onward. `dialect` is
/// captured BEFORE `spec` moves into `open_spec` (G15 T5).
async fn run_csv_import_inner(
    spec: ConnectSpec,
    job: CsvImportJob,
    cancel: CancelToken,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
    tx: tokio::sync::mpsc::Sender<CsvImportEvent>,
) {
    let started = Instant::now();
    // CURATION items 1(c)/4(b): the SHARED guard, checked before any file or
    // DB touch — a read-only spec is refused without ever opening the CSV
    // file or connecting.
    if let Err(e) = guard_not_read_only(spec_is_read_only(&spec)) {
        let _ = tx.send(CsvImportEvent::Failed { error: e }).await;
        return;
    }
    if cancel.is_cancelled() {
        let _ = tx.send(CsvImportEvent::Failed { error: QueryError::msg("cancelled") }).await;
        return;
    }
    let dialect = spec_dialect(&spec);
    let mut opened = match open_spec(spec, handle).await {
        Ok(o) => o,
        Err(e) => {
            let _ = tx.send(CsvImportEvent::Failed { error: e }).await;
            return;
        }
    };
    if cancel.is_cancelled() {
        let _ = tx.send(CsvImportEvent::Failed { error: QueryError::msg("cancelled") }).await;
        return;
    }
    let conn = &mut *opened.conn;
    run_csv_import_drive(conn, dialect, &job, &cancel, timeout_secs, &tx, started).await;
    // `opened` (connection + tunnel) drops here unconditionally.
}

/// G15 T5 (deviation, "reality/tests win" — a live MSSQL server isn't
/// available in a unit test, since `open_config`'s eager `probe()` requires
/// one, so the dialect-correct BEGIN/COMMIT/ROLLBACK text needs a seam that
/// doesn't require a live connection): the post-connect body of
/// `run_csv_import_inner`, extracted so it's kept generic over
/// `&mut dyn Connection` — same "testable via a mock `Connection`, no
/// `ConnectSpec`/`open_spec` needed" precedent `drive_write_sequence`/
/// `drive_script` already establish. BEGIN, then streams `job.path` through
/// a `spawn_blocking` producer (the `csv` crate's `Reader` is synchronous;
/// this is the same "blocking work never runs on a runtime worker thread"
/// dispatch `open_spec`/`read_and_split_file` already use) that chunks rows
/// into `CSV_IMPORT_BATCH_SIZE`-row pieces over a bounded channel, executing
/// one `INSERT` per chunk via `csv_import::generate_insert_batches`. ANY
/// failure (BEGIN, producer parse/IO error, a chunk's generated statement
/// erroring) ROLLBACKs and reports zero rows imported — never partial.
async fn run_csv_import_drive(
    conn: &mut dyn Connection,
    dialect: dbc_core::Dialect,
    job: &CsvImportJob,
    cancel: &CancelToken,
    timeout_secs: Option<u64>,
    tx: &tokio::sync::mpsc::Sender<CsvImportEvent>,
    started: Instant,
) {
    if let Err(e) = conn.execute(dbc_core::tx_begin_sql(dialect), cancel.child_token()).await {
        let _ = tx.send(CsvImportEvent::Failed { error: e }).await;
        return;
    }

    let (chunk_tx, mut chunk_rx) =
        tokio::sync::mpsc::channel::<Result<Vec<crate::csv_import::CsvRow>, String>>(
            CSV_IMPORT_PRODUCER_DEPTH,
        );
    let producer_path = job.path.clone();
    tokio::task::spawn_blocking(move || {
        let mut reader = match csv::Reader::from_path(&producer_path) {
            Ok(r) => r,
            Err(e) => {
                let _ = chunk_tx
                    .blocking_send(Err(format!("[CSV] {}: {e}", producer_path.display())));
                return;
            }
        };
        let mut chunk: Vec<crate::csv_import::CsvRow> = Vec::with_capacity(crate::csv_import::CSV_IMPORT_BATCH_SIZE);
        for record in reader.records() {
            let record = match record {
                Ok(r) => r,
                Err(e) => {
                    let _ = chunk_tx
                        .blocking_send(Err(format!("[CSV] {}: {e}", producer_path.display())));
                    return;
                }
            };
            let row: crate::csv_import::CsvRow =
                record.iter().map(crate::csv_field_to_value).collect();
            chunk.push(row);
            if chunk.len() >= crate::csv_import::CSV_IMPORT_BATCH_SIZE {
                if chunk_tx.blocking_send(Ok(std::mem::take(&mut chunk))).is_err() {
                    return; // driver side hung up (already failed/rolled back)
                }
            }
        }
        if !chunk.is_empty() {
            let _ = chunk_tx.blocking_send(Ok(chunk));
        }
        // `chunk_tx` drops here, closing the channel — the driver loop below
        // sees `None` and knows the whole file has been read.
    });

    let mut rows_committed: u64 = 0;
    let mut batch_index: usize = 0;
    while let Some(chunk_result) = chunk_rx.recv().await {
        if cancel.is_cancelled() {
            let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.child_token()).await;
            let _ = tx.send(CsvImportEvent::Failed { error: QueryError::msg("cancelled") }).await;
            return;
        }
        let rows = match chunk_result {
            Ok(rows) => rows,
            Err(msg) => {
                let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.child_token()).await;
                let _ = tx.send(CsvImportEvent::Failed { error: QueryError::msg(msg) }).await;
                return;
            }
        };
        let _ = tx
            .send(CsvImportEvent::BatchStarted { batch_index, rows_in_batch: rows.len() })
            .await;
        let stmts = crate::csv_import::generate_insert_batches(
            job.schema.as_deref(),
            &job.table,
            &job.columns,
            &job.mapping,
            &rows,
        );
        let stmt = match stmts {
            Ok(stmts) => {
                // NIT: `generate_insert_batches` returns one statement PER
                // `CSV_IMPORT_BATCH_SIZE`-row slice — `rows` here is always
                // ≤ `CSV_IMPORT_BATCH_SIZE` (the producer chunks to exactly
                // that size, see the `spawn_blocking` loop above), so this
                // ALWAYS has at most one statement; `.next()` below is safe
                // to take as "the whole batch's statement", never a partial
                // one silently dropped.
                debug_assert!(
                    stmts.len() <= 1,
                    "generate_insert_batches must return at most one statement for a chunk \
                     sized to CSV_IMPORT_BATCH_SIZE, got {}",
                    stmts.len()
                );
                stmts.into_iter().next()
            }
            Err(msg) => {
                let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.child_token()).await;
                let _ = tx.send(CsvImportEvent::Failed { error: QueryError::msg(msg) }).await;
                return;
            }
        };
        // Review fix (MINOR): only count `rows` as committed once a
        // statement for this chunk actually ran — `stmt == None` happens
        // when `generate_insert_batches` had nothing to insert (every
        // header skipped, an all-`None` mapping); currently unreachable via
        // the UI (the mapping modal disables "Spustit import" whenever
        // `sample_sql` would be `None` for this reason), but the counter
        // must not silently over-report rows that were never written.
        if let Some(stmt) = stmt {
            let stmt_cancel = cancel.child_token();
            let fut = conn.execute(&stmt, stmt_cancel.clone());
            let result = match timeout_secs {
                Some(t) => match tokio::time::timeout(Duration::from_secs(t), fut).await {
                    Ok(r) => r,
                    Err(_elapsed) => {
                        stmt_cancel.cancel();
                        Err(QueryError::msg(format!("[timeout] statement exceeded {t}s")))
                    }
                },
                None => fut.await,
            };
            if let Err(e) = result {
                let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.child_token()).await;
                let _ = tx.send(CsvImportEvent::Failed { error: e }).await;
                return;
            }
            rows_committed += rows.len() as u64;
        }
        batch_index += 1;
        let _ = tx
            .send(CsvImportEvent::BatchFinished { batch_index: batch_index - 1, rows_committed_so_far: rows_committed })
            .await;
    }

    if let Err(e) = conn.execute(dbc_core::tx_commit_sql(dialect), cancel.child_token()).await {
        let _ = conn.execute(dbc_core::tx_rollback_sql(dialect), cancel.child_token()).await;
        let _ = tx.send(CsvImportEvent::Failed { error: e }).await;
        return;
    }
    let _ = tx
        .send(CsvImportEvent::Finished { rows_imported: rows_committed, elapsed: started.elapsed() })
        .await;
    // caller (`run_csv_import_inner`) drops `opened` (connection + tunnel)
    // unconditionally once this function returns.
}

/// Defensive cap on materialized lookup rows — see `QueryRunner::fetch_lookup`.
const LOOKUP_ROW_CAP: usize = 100_000;

/// `(column names, rows)` — `rows[r][c]` is `None` for a real SQL NULL.
/// `rows[r][0]` is always the key column (see `fk_join::build_lookup_sql`,
/// which puts it first); `rows[r][1..]` line up with the caller's
/// `wanted_cols`, in order.
type LookupResult = AdminCatalogRows;

/// G10 T3: `(column names, rows)` — the same shape `fetch_lookup`'s
/// private `LookupResult` already had (now an alias of this), shared with
/// `fetch_admin_catalog`'s labeled multi-SELECT results.
pub type AdminCatalogRows = (Vec<String>, Vec<Vec<Option<String>>>);

/// G10 T3: runs `sql` on an ALREADY-OPEN connection and drains the full
/// result into materialized rows via a throwaway `dbc_buffer::ResultBuffer`,
/// capped at `cap` rows — extracted out of `fetch_lookup_inner`'s body
/// (moved verbatim) so `fetch_admin_catalog_inner` doesn't re-implement
/// arrow batch draining. Shared by both: `fetch_lookup_inner` keeps its own
/// `LOOKUP_ROW_CAP`; catalog results are small but capped defensively with
/// the same constant.
async fn drain_all_rows(
    conn: &mut dyn Connection,
    sql: &str,
    cap: usize,
) -> Result<AdminCatalogRows, QueryError> {
    let mut stream = conn.query(sql, CancelToken::new()).await?;
    let col_names: Vec<String> =
        stream.columns.fields().iter().map(|f| f.name().to_string()).collect();
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        match item {
            Ok(b) => {
                buf.push(b).map_err(|e| QueryError::msg(e.to_string()))?;
                if buf.row_count() >= cap {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    let n = buf.row_count().min(cap);
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

async fn fetch_lookup_inner(
    spec: ConnectSpec,
    sql: String,
    handle: tokio::runtime::Handle,
) -> Result<LookupResult, QueryError> {
    let mut opened = open_spec(spec, handle).await?;
    drain_all_rows(&mut *opened.conn, &sql, LOOKUP_ROW_CAP).await
}

/// G10 T3, design §5: one connection (`open_spec`, same dispatch as
/// `fetch_schema`/`fetch_lookup`), each labeled SELECT run SEQUENTIALLY
/// through the READ path (`Connection::query`, via `drain_all_rows`) —
/// never `execute`. First error aborts the whole fetch (CURATION item 5:
/// the privileges sub-view shows the error — there is no fallback query).
async fn fetch_admin_catalog_inner(
    spec: ConnectSpec,
    queries: Vec<(&'static str, String)>,
    handle: tokio::runtime::Handle,
) -> Result<Vec<(&'static str, AdminCatalogRows)>, QueryError> {
    let mut opened = open_spec(spec, handle).await?;
    let mut out = Vec::with_capacity(queries.len());
    for (label, sql) in queries {
        out.push((label, drain_all_rows(&mut *opened.conn, &sql, LOOKUP_ROW_CAP).await?));
    }
    Ok(out)
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
    use crate::admin_sql::{self, WriteStatement};

    /// G10 T3: builds a `WriteStatement` where `exec_sql == display_sql`,
    /// matching `WriteStatement::from((String, Option<u64>))` — the
    /// pre-G10 tests below only ever needed sandbox-shaped (non-redacted)
    /// statements.
    fn ws(sql: &str, expected: Option<u64>) -> WriteStatement {
        (sql.to_string(), expected).into()
    }

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
            mssql: None,
        };
        assert!(spec_is_read_only(&ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: None }));
        let mut cfg2 = cfg;
        cfg2.read_only = false;
        assert!(!spec_is_read_only(&ConnectSpec::Config { cfg: Box::new(cfg2), secret: None }));
        assert!(!spec_is_read_only(&ConnectSpec::Url("irrelevant".into())));
    }

    /// G15 T5: `spec_dialect` maps every `Engine` (Config path) and every
    /// CLI-arg URL scheme (`Url` path) exactly like `main.rs::engine_from_url`'s
    /// own postgres-vs-sqlite dispatch.
    #[test]
    fn spec_dialect_maps_engines_and_url_schemes() {
        fn cfg_with_engine(engine: dbc_state::Engine) -> dbc_state::ConnectionConfig {
            dbc_state::ConnectionConfig {
                id: "x".into(),
                name: "x".into(),
                folder: Vec::new(),
                engine,
                host: String::new(),
                port: None,
                database: String::new(),
                user: String::new(),
                read_only: false,
                timeout_secs: None,
                auto_limit: None,
                ssh: None,
                favourite: false,
                mssql: None,
            }
        }
        assert_eq!(
            spec_dialect(&ConnectSpec::Config { cfg: Box::new(cfg_with_engine(dbc_state::Engine::Postgres)), secret: None }),
            dbc_core::Dialect::Postgres
        );
        assert_eq!(
            spec_dialect(&ConnectSpec::Config { cfg: Box::new(cfg_with_engine(dbc_state::Engine::Sqlite)), secret: None }),
            dbc_core::Dialect::Sqlite
        );
        assert_eq!(
            spec_dialect(&ConnectSpec::Config { cfg: Box::new(cfg_with_engine(dbc_state::Engine::Mssql)), secret: None }),
            dbc_core::Dialect::Mssql
        );
        assert_eq!(
            spec_dialect(&ConnectSpec::Url("postgres://localhost/db".into())),
            dbc_core::Dialect::Postgres
        );
        assert_eq!(
            spec_dialect(&ConnectSpec::Url("postgresql://localhost/db".into())),
            dbc_core::Dialect::Postgres
        );
        assert_eq!(spec_dialect(&ConnectSpec::Url("C:/data/app.db".into())), dbc_core::Dialect::Sqlite);
        assert_eq!(spec_dialect(&ConnectSpec::Url(":memory:".into())), dbc_core::Dialect::Sqlite);
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
            ws("UPDATE t SET name = 'b' WHERE id = 1", Some(1)),
            ws("INSERT INTO t(id, name) VALUES (2, 'c')", None),
        ];
        let total = drive_write_sequence(&mut *conn, &stmts, CancelToken::new(), dbc_core::Dialect::Sqlite).await.unwrap();
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
            ws("UPDATE t SET name = 'b' WHERE id = 1", Some(1)),
            ws("UPDATE t SET name = 'z' WHERE id = 1", Some(2)),
        ];
        let err = drive_write_sequence(&mut *conn, &stmts, CancelToken::new(), dbc_core::Dialect::Sqlite).await.unwrap_err();
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
            ws("UPDATE t SET name = 'b' WHERE id = 1", Some(1)),
            ws("UPDATE no_such_table SET name = 'x'", None),
        ];
        let err = drive_write_sequence(&mut *conn, &stmts, CancelToken::new(), dbc_core::Dialect::Sqlite).await.unwrap_err();
        assert_ne!(err.message, AFFECTED_MISMATCH_MSG);

        assert_eq!(read_one(&mut *conn, "SELECT name FROM t WHERE id = 1").await, Some("a".to_string()));
    }

    #[tokio::test]
    async fn drive_write_sequence_empty_statements_still_begins_and_commits() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        let total = drive_write_sequence(&mut *conn, &[], CancelToken::new(), dbc_core::Dialect::Sqlite).await.unwrap();
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
            mssql: None,
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

    /// G15 T5 REQUIRED (Global Constraints §1a): the shared guard fires
    /// before `open_spec` for a read-only MSSQL connection too — no driver
    /// call, no `MssqlConfig`/`MssqlConnection` ever built. Clone of
    /// `run_write_transaction_refuses_read_only_connection_without_connecting`
    /// with `engine: Engine::Mssql`.
    #[tokio::test]
    async fn run_write_transaction_refuses_read_only_mssql_without_connecting() {
        let cfg = dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Mssql,
            database: "\0invalid".into(),
            host: String::new(),
            port: None,
            user: String::new(),
            read_only: true,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        };
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_write_transaction_inner(spec, Vec::new(), None, handle).await.unwrap_err();
        assert!(!err.message.is_empty());
    }

    /// G15 T5: mock `Connection` recording every `execute()`d statement, so
    /// the FIRST statement a transactional sequence sends is directly
    /// assertable — used by the pg/MSSQL tx-begin-text regression tests
    /// below.
    struct CapturingConnection {
        statements: Vec<String>,
    }

    #[async_trait::async_trait]
    impl Connection for CapturingConnection {
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
            self.statements.push(sql.to_string());
            Ok(0)
        }
    }

    /// G15 T5 REQUIRED: zero behavior change for pg — `drive_write_sequence`'s
    /// first statement over a Postgres dialect is byte-equal to the historic
    /// `"BEGIN"` literal.
    #[tokio::test]
    async fn pg_sequences_still_send_the_literal_begin() {
        let mut conn = CapturingConnection { statements: Vec::new() };
        let _ = drive_write_sequence(&mut conn, &[], CancelToken::new(), dbc_core::Dialect::Postgres).await;
        assert_eq!(conn.statements.first().map(String::as_str), Some("BEGIN"));
    }

    /// G15 T5 REQUIRED (regression for G12's bare-`BEGIN`-on-MSSQL bug):
    /// `drive_write_sequence`'s first statement over an Mssql dialect is the
    /// fused `SET XACT_ABORT ON; BEGIN TRANSACTION`.
    #[tokio::test]
    async fn mssql_write_sequence_opens_with_fused_xact_abort_begin() {
        let mut conn = CapturingConnection { statements: Vec::new() };
        let _ = drive_write_sequence(&mut conn, &[], CancelToken::new(), dbc_core::Dialect::Mssql).await;
        assert_eq!(
            conn.statements.first().map(String::as_str),
            Some(dbc_core::tx_begin_sql(dbc_core::Dialect::Mssql))
        );
    }

    /// G10 CURATION item 3's REQUIRED test: a mock `Connection` that fails
    /// exactly the password-bearing `ALTER ROLE` statement with a generic
    /// driver message — the runner's own error-pairing must attach
    /// `display_sql` (redacted), never `exec_sql` (the real password).
    struct FailsOnAlter;

    #[async_trait::async_trait]
    impl Connection for FailsOnAlter {
        async fn query(
            &mut self,
            _sql: &str,
            _cancel: CancelToken,
        ) -> Result<dbc_core::QueryStream, QueryError> {
            Err(QueryError::msg("not exercised"))
        }
        async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
            Err(QueryError::msg("not exercised"))
        }
        async fn execute(&mut self, sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
            if sql.starts_with("ALTER ROLE") {
                return Err(QueryError::msg("syntax error"));
            }
            Ok(0) // BEGIN / ROLLBACK
        }
    }

    /// BINDING carry-forward #1/#4 + CURATION item 3: the surfaced error
    /// for a failing password-bearing statement must carry the redacted
    /// `display_sql` and NEVER the real password from `exec_sql`.
    #[tokio::test]
    async fn statement_failure_pairs_display_sql_never_exec_sql() {
        let mut conn = FailsOnAlter;
        let stmts = admin_sql::alter_password(dbc_state::Engine::Postgres, "app_user", "s3cr'et");
        let err = drive_write_sequence(&mut conn, &stmts, CancelToken::new(), dbc_core::Dialect::Sqlite).await.unwrap_err();
        assert!(err.message.contains("'***'"), "error must carry the redacted display_sql: {}", err.message);
        assert!(err.message.contains("ALTER ROLE \"app_user\""));
        assert!(!err.message.contains("s3cr"), "real password leaked into surfaced error: {}", err.message);
    }

    /// CURATION item 6's REQUIRED guard-level test: admin statements over a
    /// read_only cfg are refused by the SHARED guard before any driver call
    /// — same choke point G5's own refusal test already exercises, now
    /// proven with admin-built statements (§3-novela: no fresh read-only
    /// logic for admin — the guard is shared).
    #[tokio::test]
    async fn admin_statements_refused_on_read_only_before_any_driver_call() {
        let cfg = dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Postgres,
            host: String::new(),
            port: None,
            database: "\0invalid".into(),
            user: String::new(),
            read_only: true,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        };
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let stmts = admin_sql::drop_role(dbc_state::Engine::Postgres, "bob");
        let handle = tokio::runtime::Handle::current();
        let err = run_write_transaction_inner(spec, stmts, None, handle).await.unwrap_err();
        assert_eq!(err.message, "připojení je jen pro čtení");
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
            // G15 T5: also recognizes the fused MSSQL begin so this mock
            // stays assertable for every dialect, not just pg/sqlite's
            // literal `"BEGIN"`.
            if sql == "BEGIN" || sql == dbc_core::tx_begin_sql(dbc_core::Dialect::Mssql) {
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
        let stmts = vec![ws("UPDATE t SET x = 1 WHERE id = 1", Some(1))];
        let cancel = CancelToken::new();

        let start = tokio::time::Instant::now();
        let result =
            drive_write_sequence_bounded(&mut conn, &stmts, cancel.clone(), dbc_core::Dialect::Sqlite, Some(1))
                .await;
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

/// G10 T3: `drain_all_rows`/`fetch_admin_catalog_inner`'s sqlite-backed
/// tests — generic SELECTs over a temp-file sqlite connection (same
/// `open_sqlite_test_conn` pattern as `write_transaction_tests`, duplicated
/// here for the same "private items don't cross sibling test-module
/// boundaries" reason that module's own doc comment gives), exercising the
/// drain shape and the sequential-labels/abort-on-first-error contract
/// without any docker/live-pg dependency.
#[cfg(test)]
mod admin_catalog_tests {
    use super::*;

    async fn open_sqlite_test_conn() -> (tempfile::NamedTempFile, Box<dyn Connection>) {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let conn =
            crate::connect::open(f.path().to_str().expect("utf8 temp path"), &handle).expect("open sqlite");
        (f, conn)
    }

    #[tokio::test]
    async fn drains_labeled_queries_in_order() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(a TEXT)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES ('x'), (NULL)", CancelToken::new()).await.unwrap();

        let (cols, rows) =
            drain_all_rows(&mut *conn, "SELECT a FROM t ORDER BY a IS NULL", 100).await.unwrap();
        assert_eq!(cols, vec!["a".to_string()]);
        assert_eq!(rows, vec![vec![Some("x".to_string())], vec![None]]);
    }

    #[tokio::test]
    async fn first_error_aborts_whole_catalog_fetch() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        // No fallback (CURATION item 5): an erroring catalog SELECT is a
        // hard Err for the whole labeled batch.
        let err = drain_all_rows(&mut *conn, "SELECT * FROM no_such_catalog", 100).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn fetch_admin_catalog_inner_runs_labels_sequentially_and_stops_at_first_error() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        {
            let mut conn =
                crate::connect::open(f.path().to_str().expect("utf8 temp path"), &handle).expect("open sqlite");
            conn.execute("CREATE TABLE t(a TEXT)", CancelToken::new()).await.unwrap();
            conn.execute("INSERT INTO t VALUES ('x')", CancelToken::new()).await.unwrap();
        }
        let url = f.path().to_str().expect("utf8 temp path").to_string();

        let ok = fetch_admin_catalog_inner(
            ConnectSpec::Url(url.clone()),
            vec![("first", "SELECT a FROM t".to_string()), ("second", "SELECT a FROM t".to_string())],
            tokio::runtime::Handle::current(),
        )
        .await
        .unwrap();
        assert_eq!(ok.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec!["first", "second"]);
        assert_eq!(ok[0].1, (vec!["a".to_string()], vec![vec![Some("x".to_string())]]));

        let err = fetch_admin_catalog_inner(
            ConnectSpec::Url(url),
            vec![("ok", "SELECT a FROM t".to_string()), ("bad", "SELECT * FROM no_such_catalog".to_string())],
            tokio::runtime::Handle::current(),
        )
        .await;
        assert!(err.is_err(), "the second label's error must abort the whole batch");
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
            // G15 T5: `tx_begin_sql` isn't a `const fn`, so the MSSQL-fused
            // literal can't be a match-arm pattern — an if/else-if chain
            // (still one dispatch, no functional change) recognizes it
            // alongside pg/sqlite's literal `"BEGIN"`, keeping this mock
            // assertable for every dialect.
            if sql == "BEGIN" || sql == dbc_core::tx_begin_sql(dbc_core::Dialect::Mssql) {
                self.in_txn = true;
                Ok(0)
            } else if sql == "ROLLBACK" {
                self.in_txn = false;
                self.pending_insert = false; // discarded — never committed
                Ok(0)
            } else if sql == "COMMIT" {
                if self.pending_insert {
                    self.committed.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                self.in_txn = false;
                Ok(0)
            } else {
                Err(QueryError::msg(format!("unexpected statement: {sql}")))
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
            mssql: None,
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

        let out = drive_analyze_write(&mut *conn, "SELECT 'plan-text'", CancelToken::new(), dbc_core::Dialect::Sqlite).await.unwrap();
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
            dbc_core::Dialect::Postgres,
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
        let err = drive_analyze_write(&mut *conn, "SELECT * FROM no_such_table", CancelToken::new(), dbc_core::Dialect::Sqlite)
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
        // Connection must still be usable — ROLLBACK ran despite the error.
        conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await.unwrap();
    }
}

/// G12 T2: `run_script`'s pure-decision tests plus `drive_script` tests
/// driven over a real sqlite driver via `crate::connect::open` (temp-file
/// database — no docker/network dependency), matching
/// `write_transaction_tests`'s fixtures. `open_sqlite_test_conn`/`read_one`
/// are duplicated here rather than imported — see
/// `analyze_write_tests`'s doc comment for why (private items in a sibling
/// test module aren't visible across module boundaries).
#[cfg(test)]
mod script_run_tests {
    use super::*;

    #[test]
    fn dispatch_statement_matrix() {
        use dbc_core::Dialect;
        assert_eq!(dispatch_statement("SELECT 1", false, Dialect::Postgres), StmtDispatch::RunAsRead);
        assert_eq!(dispatch_statement("SELECT 1", true, Dialect::Postgres), StmtDispatch::RunAsRead);
        assert_eq!(dispatch_statement("UPDATE t SET x = 1", false, Dialect::Postgres), StmtDispatch::RunAsWrite);
        assert_eq!(dispatch_statement("UPDATE t SET x = 1", true, Dialect::Postgres), StmtDispatch::RejectReadOnly);
        // fail-closed inputs are writes, not reads (guards.rs contract):
        assert_eq!(dispatch_statement("SELECT 1 /* unterminated", true, Dialect::Postgres), StmtDispatch::RejectReadOnly);
    }

    /// G15 T5: MSSQL dialect — a bracket-quoted reserved word must not
    /// false-reject a genuine read, and the guard is still bracket-aware for
    /// writes.
    #[test]
    fn dispatch_statement_matrix_mssql_bracket_aware() {
        use dbc_core::Dialect;
        assert_eq!(
            dispatch_statement("SELECT [Delete], [Update] FROM AuditLog", true, Dialect::Mssql),
            StmtDispatch::RunAsRead
        );
        assert_eq!(
            dispatch_statement("UPDATE t SET x = 1", true, Dialect::Mssql),
            StmtDispatch::RejectReadOnly
        );
        assert_eq!(
            dispatch_statement("UPDATE t SET x = 1", false, Dialect::Mssql),
            StmtDispatch::RunAsWrite
        );
    }

    #[test]
    fn failure_action_full_matrix() {
        use ErrorPolicy::*;
        use TxScope::*;
        assert_eq!(failure_action(Stop, None), FailureAction::AbortRun);
        assert_eq!(failure_action(Stop, PerFile), FailureAction::AbortRun);
        assert_eq!(failure_action(Stop, WholeRun), FailureAction::AbortRun);
        assert_eq!(failure_action(Continue, None), FailureAction::NextStatement);
        assert_eq!(failure_action(Continue, PerFile), FailureAction::NextFile);
        // UI forbids the combination; runner fails safe if it arrives anyway:
        assert_eq!(failure_action(Continue, WholeRun), FailureAction::AbortRun);
    }

    #[test]
    fn sql_preview_collapses_and_caps() {
        assert_eq!(sql_preview("SELECT\n  1"), "SELECT 1");
        let long = "x".repeat(300);
        let p = sql_preview(&long);
        assert_eq!(p.chars().count(), SQL_PREVIEW_CAP + 1);
        assert!(p.ends_with('…'));
    }

    /// See `write_transaction_tests::open_sqlite_test_conn`'s doc comment —
    /// identical shape, duplicated for sibling-module visibility.
    async fn open_sqlite_test_conn() -> (tempfile::NamedTempFile, Box<dyn Connection>) {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let conn =
            crate::connect::open(f.path().to_str().expect("utf8 temp path"), &handle).expect("open sqlite");
        (f, conn)
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

    /// ~15-line test helper (per the plan): drives `drive_script` and a
    /// concurrent receiver-drain via `tokio::join!`, returns the full
    /// `Vec<ScriptEvent>`. `tx` is moved into the `drive` future and
    /// explicitly `drop`ped once `drive_script` returns — WITHOUT that, the
    /// owning `Sender` would still be alive (on `drive_collect`'s own stack)
    /// after `drive_script` finishes sending, and `collect`'s `rx.recv()`
    /// loop would hang forever waiting for a channel close that never
    /// comes (found by running this test: several tests hung indefinitely
    /// on first run because of exactly this).
    async fn drive_collect(
        conn: &mut dyn Connection,
        read_only: bool,
        files: &[std::path::PathBuf],
        opts: &ScriptRunOptions,
    ) -> Vec<ScriptEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let cancel = CancelToken::new();
        let drive = async {
            drive_script(conn, read_only, files, opts, cancel, &tx).await;
            drop(tx);
        };
        let collect = async {
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            events
        };
        let (_, events) = tokio::join!(drive, collect);
        events
    }

    /// CURATION item 4(a): write statement over a read_only connection is
    /// rejected CLIENT-SIDE (before the driver — proven by the table staying
    /// unchanged even though the underlying test connection is writable), with
    /// the SHARED guard's exact message, and Continue policy keeps running the
    /// script's read statements.
    #[tokio::test]
    async fn script_write_statement_rejected_on_read_only_policy_matrix_honored() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "SELECT * FROM t;\nUPDATE t SET n = 'hacked' WHERE id = 1;\nSELECT 1;").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Continue,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, /* read_only */ true, &[f1], &opts).await;

        let guard_msg = guard_not_read_only(true).unwrap_err().message;
        assert!(events.iter().any(|e| matches!(e,
            ScriptEvent::StatementFailed { stmt_index: 1, error } if error.message == guard_msg)));
        // Continue: both SELECTs still ran.
        let finished: Vec<_> = events.iter().filter(|e| matches!(e, ScriptEvent::StatementFinished { .. })).collect();
        assert_eq!(finished.len(), 2);
        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { statements_run: 2, statements_failed: 1, aborted: false, .. })));
        // Client-side proof: the write never reached the (writable) driver.
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, Some("a".to_string()));
    }

    #[tokio::test]
    async fn per_file_scope_stop_policy_rolls_back_file_and_aborts_run() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "INSERT INTO t VALUES (1, 'a');\nTHIS IS NOT SQL;").unwrap();
        let f2 = dir.path().join("02.sql");
        std::fs::write(&f2, "INSERT INTO t VALUES (2, 'b');").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::PerFile,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1, f2], &opts).await;

        // file 2 never started.
        assert!(!events.iter().any(|e| matches!(e, ScriptEvent::FileStarted { index: 1, .. })));
        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { aborted: true, .. })));
        // file 1's INSERT rolled back.
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, None);
    }

    #[tokio::test]
    async fn per_file_scope_continue_policy_skips_failed_file_commits_next() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "INSERT INTO t VALUES (1, 'a');\nTHIS IS NOT SQL;").unwrap();
        let f2 = dir.path().join("02.sql");
        std::fs::write(&f2, "INSERT INTO t VALUES (2, 'b');").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::PerFile,
            error_policy: ErrorPolicy::Continue,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1, f2], &opts).await;

        let file_finished: Vec<_> =
            events.iter().filter(|e| matches!(e, ScriptEvent::FileFinished { .. })).collect();
        assert_eq!(file_finished.len(), 2);
        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { aborted: false, .. })));
        // file 1's INSERT rolled back; file 2's committed.
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, None);
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 2").await, Some("b".to_string()));
    }

    #[tokio::test]
    async fn whole_run_scope_rolls_back_everything_on_late_failure() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "INSERT INTO t VALUES (1, 'a');").unwrap();
        let f2 = dir.path().join("02.sql");
        std::fs::write(&f2, "INSERT INTO t VALUES (2, 'b');\nTHIS IS NOT SQL;").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::WholeRun,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1, f2], &opts).await;

        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { aborted: true, .. })));
        // NOTHING from file 1 is visible — the whole-run tx rolled back.
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, None);
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 2").await, None);
    }

    #[tokio::test]
    async fn no_tx_continue_skips_only_failing_statement() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "INSERT INTO t VALUES (1, 'a');\nTHIS IS NOT SQL;\nINSERT INTO t VALUES (3, 'c');").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Continue,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1], &opts).await;

        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { statements_failed: 1, aborted: false, .. })));
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, Some("a".to_string()));
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 3").await, Some("c".to_string()));
    }

    #[tokio::test]
    async fn final_statement_without_trailing_semicolon_runs() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "INSERT INTO t VALUES (2, 'b');\nSELECT * FROM t").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1], &opts).await;
        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { statements_run: 2, statements_failed: 0, aborted: false, .. })));
    }

    #[tokio::test]
    async fn unterminated_construct_surfaces_as_statement_failure() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "SELECT 'unterminated").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1], &opts).await;
        assert!(events.iter().any(|e| matches!(e,
            ScriptEvent::StatementFailed { error, .. } if error.message.starts_with("[skript]"))));
        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { aborted: true, .. })));
    }

    /// G15 T5: an MSSQL `GO <n>` repeat count is refused fail-closed by the
    /// splitter (`SplitError::UnsupportedGoCount`) and surfaces the
    /// dedicated Czech message via `split_error_message`, not a generic
    /// Debug dump.
    #[tokio::test]
    async fn mssql_go_repeat_count_surfaces_czech_message_in_script_run() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "SELECT 1\nGO 5\n").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Mssql,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1], &opts).await;
        assert!(events.iter().any(|e| matches!(e,
            ScriptEvent::StatementFailed { error, .. } if error.message.contains("GO s počtem opakování není podporováno"))));
        assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { aborted: true, .. })));
    }

    #[tokio::test]
    async fn precancelled_token_aborts_before_any_statement() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "INSERT INTO t VALUES (1);").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let cancel = CancelToken::new();
        cancel.cancel();
        let files = [f1];
        // See `drive_collect`'s doc comment: `tx` must be dropped once
        // `drive_script` returns, or `collect`'s `rx.recv()` hangs forever
        // waiting for a channel close that never comes.
        let drive = async {
            drive_script(&mut *conn, false, &files, &opts, cancel, &tx).await;
            drop(tx);
        };
        let collect = async {
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            events
        };
        let (_, events) = tokio::join!(drive, collect);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ScriptEvent::RunFinished { statements_run: 0, aborted: true, .. }));
    }

    #[tokio::test]
    async fn read_statements_report_drained_row_counts() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (2)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (3)", CancelToken::new()).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("01.sql");
        std::fs::write(&f1, "SELECT * FROM t;").unwrap();

        let opts = ScriptRunOptions {
            tx_scope: TxScope::None,
            error_policy: ErrorPolicy::Stop,
            dialect: dbc_core::Dialect::Sqlite,
            statement_timeout_secs: None,
        };
        let events = drive_collect(&mut *conn, false, &[f1], &opts).await;
        assert!(events.iter().any(|e| matches!(e,
            ScriptEvent::StatementFinished { affected: Some(3), .. })));
    }
}

/// G12 T5: `connect_and_run_many`'s integration tests, driven directly over
/// `connect_and_run_many_inner` with `Handle::current()` — same precedent
/// as `write_transaction_tests::run_write_transaction_refuses_read_only_connection_without_connecting`.
/// `tx` is moved into a `tokio::spawn`ed task (owning it, dropping it when
/// the task returns) rather than borrowed + manually dropped — see
/// `script_run_tests::drive_collect`'s doc comment for why a borrowed `tx`
/// that outlives its producing future deadlocks a concurrent drain.
#[cfg(test)]
mod run_many_tests {
    use super::*;

    fn sqlite_cfg(database: String, read_only: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            database,
            host: String::new(),
            port: None,
            user: String::new(),
            read_only,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    /// CURATION item 4(c): `SELECT 1; UPDATE …` on a READ-ONLY connection
    /// runs the SELECT (`Started` with columns + `Finished`), then stops at
    /// the UPDATE with the SHARED guard's message; nothing after it runs.
    #[tokio::test]
    async fn read_only_multi_run_runs_select_then_stops_at_update() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        {
            // Prepare the fixture via a WRITABLE open first.
            let mut conn = crate::connect::open(f.path().to_str().expect("utf8 path"), &handle)
                .expect("open sqlite");
            conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new())
                .await
                .unwrap();
            conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();
        }

        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), true);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let statements = vec![
            "SELECT 1".to_string(),
            "UPDATE t SET n = 'x'".to_string(),
            "SELECT 2".to_string(),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(connect_and_run_many_inner(
            spec,
            statements,
            CancelToken::new(),
            None,
            handle.clone(),
            tx,
        ));
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        task.await.unwrap();

        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementStarted { index: 0, columns: Some(_), .. })));
        assert!(events.iter().any(|e| matches!(e, MultiQueryEvent::StatementFinished { index: 0, .. })));
        let guard_msg = guard_not_read_only(true).unwrap_err().message;
        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementFailed { index: 1, error } if error.message == guard_msg)));
        // Nothing for statement index 2, no RunFinished.
        assert!(!events.iter().any(|e| matches!(e, MultiQueryEvent::StatementStarted { index: 2, .. })));
        assert!(!events.iter().any(|e| matches!(e, MultiQueryEvent::RunFinished)));

        // The write never reached the driver — table unchanged.
        let mut verify = crate::connect::open(f.path().to_str().unwrap(), &handle).expect("reopen");
        let mut stream =
            verify.query("SELECT n FROM t WHERE id = 1", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        assert_eq!(buf.cell_text(0, 0), "a");
    }

    #[tokio::test]
    async fn multi_run_mixed_reads_and_writes_over_writable_connection() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), false);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let statements = vec![
            "CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)".to_string(),
            "INSERT INTO t VALUES (1, 'a')".to_string(),
            "SELECT * FROM t".to_string(),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(connect_and_run_many_inner(
            spec,
            statements,
            CancelToken::new(),
            None,
            handle,
            tx,
        ));
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        task.await.unwrap();

        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementStarted { index: 0, columns: None, .. })));
        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementFinished { index: 0, affected: Some(0), .. })));
        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementStarted { index: 1, columns: None, .. })));
        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementFinished { index: 1, affected: Some(1), .. })));
        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementStarted { index: 2, columns: Some(_), .. })));
        assert!(events.iter().any(|e| matches!(e,
            MultiQueryEvent::StatementFinished { index: 2, affected: None, .. })));
        assert!(matches!(events.last(), Some(MultiQueryEvent::RunFinished)));
    }

    #[tokio::test]
    async fn multi_run_stops_on_first_error() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), false);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let statements = vec![
            "CREATE TABLE t(id INTEGER PRIMARY KEY)".to_string(),
            "UPDATE no_such_table SET x = 1".to_string(),
            "SELECT 1".to_string(),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(connect_and_run_many_inner(
            spec,
            statements,
            CancelToken::new(),
            None,
            handle,
            tx,
        ));
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        task.await.unwrap();

        assert!(events.iter().any(|e| matches!(e, MultiQueryEvent::StatementFailed { index: 1, .. })));
        // Statement 2 never dispatched — no third StatementStarted.
        assert!(!events.iter().any(|e| matches!(e, MultiQueryEvent::StatementStarted { index: 2, .. })));
        assert!(!events.iter().any(|e| matches!(e, MultiQueryEvent::RunFinished)));
    }
}

/// G12 T7: `run_csv_import_inner` integration tests over a temp-file sqlite
/// connection — same `sqlite_cfg`/`ConnectSpec::Config` fixture shape as
/// `run_many_tests`.
#[cfg(test)]
mod csv_import_tests {
    use super::*;
    use crate::csv_import::{ColumnMapping, TargetColumn};

    fn sqlite_cfg(database: String, read_only: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            database,
            host: String::new(),
            port: None,
            user: String::new(),
            read_only,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    async fn drive_csv_import(spec: ConnectSpec, job: CsvImportJob) -> Vec<CsvImportEvent> {
        let handle = tokio::runtime::Handle::current();
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let task =
            tokio::spawn(run_csv_import_inner(spec, job, CancelToken::new(), None, handle, tx));
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        task.await.unwrap();
        events
    }

    /// G15 T5: mock `Connection` recording every `execute()`d statement —
    /// used by the MSSQL-BEGIN regression test below, driven directly via
    /// `run_csv_import_drive` (no `ConnectSpec`/`open_spec`/live MSSQL
    /// server needed — `open_config`'s eager `probe()` would otherwise
    /// require one).
    struct CapturingConnection {
        statements: Vec<String>,
    }

    #[async_trait::async_trait]
    impl Connection for CapturingConnection {
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
            self.statements.push(sql.to_string());
            Ok(0)
        }
    }

    /// G15 T5 REQUIRED (regression for G12's bare-`BEGIN`-on-MSSQL bug —
    /// bare `BEGIN` is invalid T-SQL): `run_csv_import_drive`'s FIRST
    /// captured statement, on an Mssql dialect, is the fused
    /// `SET XACT_ABORT ON; BEGIN TRANSACTION`, byte-equal to
    /// `dbc_core::tx_begin_sql(Dialect::Mssql)`.
    #[tokio::test]
    async fn csv_import_mssql_begin_is_dialect_correct() {
        let mut conn = CapturingConnection { statements: Vec::new() };
        let job = CsvImportJob {
            path: std::path::PathBuf::from("Z:/does/not/exist.csv"),
            schema: None,
            table: "t".to_string(),
            columns: vec![TargetColumn { name: "id".into(), numeric: true }],
            mapping: ColumnMapping { targets: vec![Some(0)] },
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let cancel = CancelToken::new();
        let drive = async {
            run_csv_import_drive(&mut conn, dbc_core::Dialect::Mssql, &job, &cancel, None, &tx, Instant::now())
                .await;
            drop(tx);
        };
        let collect = async { while rx.recv().await.is_some() {} };
        tokio::join!(drive, collect);
        assert_eq!(
            conn.statements.first().map(String::as_str),
            Some(dbc_core::tx_begin_sql(dbc_core::Dialect::Mssql)),
        );
    }

    /// CURATION item 4(b), runtime half: a read-only spec is refused by the
    /// SHARED guard before any file or DB is touched (a nonsense path
    /// proves it — same pattern as
    /// `write_transaction_tests::run_write_transaction_refuses_read_only_connection_without_connecting`).
    #[tokio::test]
    async fn run_csv_import_refuses_read_only_spec_without_touching_anything() {
        let cfg = sqlite_cfg("\0invalid".to_string(), true);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let job = CsvImportJob {
            path: std::path::PathBuf::from("Z:/does/not/exist.csv"),
            schema: None,
            table: "t".to_string(),
            columns: vec![TargetColumn { name: "id".into(), numeric: true }],
            mapping: ColumnMapping { targets: vec![Some(0)] },
        };
        let events = drive_csv_import(spec, job).await;
        let guard_msg = guard_not_read_only(true).unwrap_err().message;
        assert!(events.iter().any(|e| matches!(e,
            CsvImportEvent::Failed { error } if error.message == guard_msg)));
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn csv_import_commits_all_rows_in_one_transaction() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        {
            let mut conn = crate::connect::open(f.path().to_str().expect("utf8 path"), &handle)
                .expect("open sqlite");
            conn.execute("CREATE TABLE t(id INTEGER, name TEXT, note TEXT)", CancelToken::new())
                .await
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("rows.csv");
        std::fs::write(&csv_path, "id,name,note\n1,alice,\n2,\"bob, jr\",''\n").unwrap();

        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), false);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let job = CsvImportJob {
            path: csv_path,
            schema: None,
            table: "t".to_string(),
            columns: vec![
                TargetColumn { name: "id".into(), numeric: true },
                TargetColumn { name: "name".into(), numeric: false },
                TargetColumn { name: "note".into(), numeric: false },
            ],
            mapping: ColumnMapping { targets: vec![Some(0), Some(1), Some(2)] },
        };
        let events = drive_csv_import(spec, job).await;
        assert!(matches!(events.last(), Some(CsvImportEvent::Finished { rows_imported: 2, .. })));

        let mut verify = crate::connect::open(f.path().to_str().unwrap(), &handle).expect("reopen");
        let mut stream = verify.query("SELECT id, name, note FROM t ORDER BY id", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        assert_eq!(buf.row_count(), 2);
        assert_eq!(buf.cell_text(0, 1), "alice");
        assert!(buf.cell_is_null(0, 2)); // empty field -> NULL
        assert_eq!(buf.cell_text(1, 1), "bob, jr");
        assert_eq!(buf.cell_text(1, 2), "''"); // literal two-quote text, not NULL
    }

    #[tokio::test]
    async fn csv_import_rolls_back_everything_on_batch_failure() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        {
            let mut conn = crate::connect::open(f.path().to_str().expect("utf8 path"), &handle)
                .expect("open sqlite");
            conn.execute("CREATE TABLE t(id INTEGER, name TEXT NOT NULL)", CancelToken::new())
                .await
                .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("rows.csv");
        // Last row's `name` field is empty -> NULL -> violates NOT NULL.
        std::fs::write(&csv_path, "id,name\n1,alice\n2,\n").unwrap();

        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), false);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let job = CsvImportJob {
            path: csv_path,
            schema: None,
            table: "t".to_string(),
            columns: vec![
                TargetColumn { name: "id".into(), numeric: true },
                TargetColumn { name: "name".into(), numeric: false },
            ],
            mapping: ColumnMapping { targets: vec![Some(0), Some(1)] },
        };
        let events = drive_csv_import(spec, job).await;
        assert!(matches!(events.last(), Some(CsvImportEvent::Failed { .. })));

        let mut verify = crate::connect::open(f.path().to_str().unwrap(), &handle).expect("reopen");
        let mut stream = verify.query("SELECT COUNT(*) FROM t", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        assert_eq!(buf.cell_text(0, 0), "0"); // nothing partial — zero rows.
    }

    #[tokio::test]
    async fn csv_import_batches_by_500() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        {
            let mut conn = crate::connect::open(f.path().to_str().expect("utf8 path"), &handle)
                .expect("open sqlite");
            conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("rows.csv");
        let mut content = String::from("id\n");
        for i in 0..1100 {
            content.push_str(&format!("{i}\n"));
        }
        std::fs::write(&csv_path, content).unwrap();

        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), false);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let job = CsvImportJob {
            path: csv_path,
            schema: None,
            table: "t".to_string(),
            columns: vec![TargetColumn { name: "id".into(), numeric: true }],
            mapping: ColumnMapping { targets: vec![Some(0)] },
        };
        let events = drive_csv_import(spec, job).await;
        let started =
            events.iter().filter(|e| matches!(e, CsvImportEvent::BatchStarted { .. })).count();
        assert_eq!(started, 3); // 500/500/100
        assert!(matches!(events.last(), Some(CsvImportEvent::Finished { rows_imported: 1100, .. })));
    }

    /// Review fix (MINOR): a mapping with zero mapped columns makes
    /// `generate_insert_batches` return `Ok(vec![])` for every chunk (no
    /// statement ever executes) — `rows_committed`/`rows_imported` must stay
    /// 0, not silently count the CSV's row total as if it had been written.
    /// (Currently unreachable via the UI — the mapping modal disables
    /// "Spustit import" whenever `sample_sql` would be `None` for this
    /// reason — this is the runner's own belt-and-braces correctness, same
    /// posture as `guard_not_read_only`'s "the UI already prevents this, but
    /// the write path must refuse for itself too".)
    #[tokio::test]
    async fn csv_import_zero_mapped_columns_does_not_inflate_rows_committed() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        {
            let mut conn = crate::connect::open(f.path().to_str().expect("utf8 path"), &handle)
                .expect("open sqlite");
            conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("rows.csv");
        std::fs::write(&csv_path, "id\n1\n2\n3\n").unwrap();

        let cfg = sqlite_cfg(f.path().to_str().unwrap().to_string(), false);
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let job = CsvImportJob {
            path: csv_path,
            schema: None,
            table: "t".to_string(),
            columns: vec![TargetColumn { name: "id".into(), numeric: true }],
            // Every header skipped — no mapped columns at all.
            mapping: ColumnMapping { targets: vec![None] },
        };
        let events = drive_csv_import(spec, job).await;
        assert!(matches!(events.last(), Some(CsvImportEvent::Finished { rows_imported: 0, .. })));
        // No BatchFinished ever reports a non-zero running total either.
        assert!(events.iter().all(|e| !matches!(
            e,
            CsvImportEvent::BatchFinished { rows_committed_so_far, .. } if *rows_committed_so_far != 0
        )));

        let mut verify = crate::connect::open(f.path().to_str().unwrap(), &handle).expect("reopen");
        let mut stream = verify.query("SELECT COUNT(*) FROM t", CancelToken::new()).await.unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        assert_eq!(buf.cell_text(0, 0), "0");
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
            compose_diff_select(dbc_core::Dialect::Postgres, Some("public"), "orders", None).unwrap(),
            "SELECT * FROM \"public\".\"orders\""
        );
        assert_eq!(
            compose_diff_select(dbc_core::Dialect::Postgres, None, "orders", Some("id > 10")).unwrap(),
            "SELECT * FROM \"orders\" WHERE id > 10"
        );
    }

    /// G15 T5: MSSQL gets bracket quoting from the same composer.
    #[test]
    fn compose_diff_select_mssql_uses_bracket_quoting() {
        assert_eq!(
            compose_diff_select(dbc_core::Dialect::Mssql, Some("dbo"), "orders", None).unwrap(),
            "SELECT * FROM [dbo].[orders]"
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
        let err = compose_diff_select(dbc_core::Dialect::Postgres, None, "orders", Some("1=1; DROP TABLE orders")).unwrap_err();
        assert!(err.message.contains("WHERE"));
    }

    #[test]
    fn compose_diff_select_allows_a_read_only_subquery_in_where() {
        assert!(compose_diff_select(dbc_core::Dialect::Postgres, None, "t", Some("id IN (SELECT id FROM other)")).is_ok());
    }

    #[test]
    fn compose_diff_select_empty_where_is_treated_as_absent() {
        assert_eq!(
            compose_diff_select(dbc_core::Dialect::Postgres, None, "t", Some("   ")).unwrap(),
            "SELECT * FROM \"t\""
        );
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

#[cfg(test)]
mod backup_runner_tests {
    use super::*;

    fn cfg(engine: dbc_state::Engine, read_only: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "x".into(),
            name: "x".into(),
            folder: Vec::new(),
            engine,
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            read_only,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    #[test]
    fn resolve_tool_path_configured_but_missing_is_a_value_error() {
        let err = resolve_tool_path(Some(r"D:\definitely\not\real\pg_dump.exe"), "pg_dump").unwrap_err();
        assert!(err.message.contains("pg_dump"));
    }

    // SECURITY (CWE-427, binary planting — G11 T4 review MAJOR 1): the
    // PATH-found branch must return an absolute path, never a bare name —
    // a bare name handed to `Command::new` lets Windows' `CreateProcess`
    // search the app dir + CWD (both potentially attacker-writable) before
    // PATH, and the child receives `PGPASSWORD` on its environment.
    #[test]
    fn resolve_tool_path_on_path_returns_an_absolute_path_not_a_bare_name() {
        // "cmd" is guaranteed on PATH on Windows (same assumption
        // `backup::process_tests::find_on_path_finds_a_universally_present_binary`
        // already makes).
        let resolved = resolve_tool_path(None, "cmd").expect("cmd must resolve via PATH");
        assert!(
            std::path::Path::new(&resolved).is_absolute(),
            "expected an absolute path, got: {resolved}"
        );
        assert_ne!(resolved, "cmd", "must not be the bare probed name");
    }

    // SECURITY (CWE-427, defense in depth): `resolve_tool_path`'s
    // configured-path branch absolutizes a relative-but-existing path too,
    // not just the PATH-found branch — tested directly against the
    // `absolutize` helper (rather than round-tripping through a real
    // relative file path, which would be fragile here: the repo's CWD and
    // `tempfile::tempdir()`'s default location are commonly on different
    // drive letters on Windows, and there's no portable relative path
    // between two different drives).
    #[test]
    fn absolutize_joins_a_relative_path_onto_the_current_directory() {
        let resolved = absolutize("sub\\dir\\fake_tool.exe");
        assert!(
            std::path::Path::new(&resolved).is_absolute(),
            "expected an absolute path, got: {resolved}"
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolved, cwd.join("sub\\dir\\fake_tool.exe").to_string_lossy());
    }

    #[test]
    fn absolutize_passes_an_already_absolute_path_through_unchanged() {
        assert_eq!(absolutize(r"D:\already\absolute\pg_dump.exe"), r"D:\already\absolute\pg_dump.exe");
    }

    #[test]
    fn resolve_tool_path_configured_relative_path_is_absolutized() {
        // Construct a relative path (relative to the real CWD) that points
        // at a real file, without changing the process's own CWD (which
        // would affect every other test running concurrently in this
        // binary) — write the fixture INTO a subdirectory of the actual
        // CWD instead.
        let cwd = std::env::current_dir().unwrap();
        let rel_dir = format!("g11_t4_review_tmp_{}", std::process::id());
        let dir = cwd.join(&rel_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fake_tool.exe");
        std::fs::write(&file, b"not a real binary, just needs to exist").unwrap();

        let relative = format!("{rel_dir}\\fake_tool.exe");
        let result = resolve_tool_path(Some(&relative), "fake_tool");

        std::fs::remove_dir_all(&dir).ok();

        let resolved = result.unwrap();
        assert!(
            std::path::Path::new(&resolved).is_absolute(),
            "expected an absolute path, got: {resolved}"
        );
    }

    #[test]
    fn resolve_tool_path_no_config_and_not_on_path_and_no_glob_hit_is_friendly_error() {
        // "definitely-not-a-real-tool-xyz" is neither configured, on PATH,
        // nor under C:\Program Files\PostgreSQL — exercises the full
        // fallthrough.
        let err = resolve_tool_path(None, "definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(err.message.contains("nenalezen"));
    }

    // --- MSSQL: fails fast at open_spec, no I/O — G15 T3 wired the
    // Engine::Mssql arm for real, so the `cfg()` test helper's empty `user`
    // field (never set for these engine-agnostic fixtures) now hits
    // `connect::mssql_connection_from_config`'s integrated-auth refusal
    // instead of the old permanent "driver zatím není k dispozici" stub —
    // REQUIRED, still zero I/O, still fails before any connection. ---
    #[tokio::test]
    async fn run_mssql_backup_against_mssql_engine_without_user_fails_before_connecting() {
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, false)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_backup_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(
            err.message.contains("ověření přes Windows účet zatím není podporováno"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn run_mssql_restore_against_mssql_engine_without_user_fails_before_connecting() {
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, false)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_restore_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(
            err.message.contains("ověření přes Windows účet zatím není podporováno"),
            "got: {}",
            err.message
        );
    }

    // --- read-only gates, REQUIRED, no I/O attempted in the refusing path ---
    #[tokio::test]
    async fn mssql_restore_refuses_read_only_without_connecting() {
        let spec = ConnectSpec::Config {
            cfg: Box::new(cfg(dbc_state::Engine::Sqlite, true)), // engine irrelevant — guard fires first
            secret: None,
        };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_restore_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert_eq!(err.message, "připojení je jen pro čtení");
    }

    #[tokio::test]
    async fn mssql_backup_allowed_even_when_read_only_reaches_open_spec_not_the_guard() {
        // read_only=true + Backup must NOT be refused by the guard — the
        // integrated-auth refusal (from inside open_spec's
        // mssql_connection_from_config, not the read-only guard) proves the
        // guard passed and open_spec was actually reached.
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, true)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_backup_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(
            err.message.contains("ověření přes Windows účet zatím není podporováno"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn sqlite_backup_allowed_on_read_only() {
        let spec = ConnectSpec::Config {
            cfg: Box::new({
                let mut c = cfg(dbc_state::Engine::Sqlite, true);
                c.database = "\0invalid".into();
                c
            }),
            secret: None,
        };
        let handle = tokio::runtime::Handle::current();
        let err = run_sqlite_backup_inner(spec, r"D:\x.sqlite".into(), handle).await;
        // Backup is exempt from read-only, so this must NOT be the
        // read-only message — it must instead fail later (bad path),
        // proving the guard passed through and open_spec was actually
        // attempted.
        assert_ne!(err.unwrap_err().message, "připojení je jen pro čtení");
    }

    #[tokio::test]
    async fn mssql_backup_refuses_read_only_when_actually_read_only_is_false_check_control() {
        // Control for the two "allowed even when read_only" tests above:
        // Restore on a NON-read-only connection must not be refused by the
        // guard either — it should reach the same integrated-auth refusal.
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, false)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_restore_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(
            err.message.contains("ověření přes Windows účet zatím není podporováno"),
            "got: {}",
            err.message
        );
    }

    // --- SQLite restore: magic header + real copy, temp files, no docker ---
    #[test]
    fn sqlite_restore_refuses_non_sqlite_source_without_copying() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("not_a_db.txt");
        std::fs::write(&src, b"hello world, not a database").unwrap();
        let dest = dir.path().join("target.sqlite");
        std::fs::write(&dest, b"ORIGINAL CONTENT").unwrap();

        let err =
            run_sqlite_restore_inner(dest.to_str().unwrap(), src.to_str().unwrap(), false).unwrap_err();
        assert_eq!(err.message, "soubor není SQLite databáze");
        // Original destination file must be untouched.
        assert_eq!(std::fs::read(&dest).unwrap(), b"ORIGINAL CONTENT");
    }

    // SECURITY (G11 T4 review MAJOR 2): `run_sqlite_restore_inner` must
    // self-guard on `read_only` — a write path whose sole protection was an
    // unwired future UI-layer caller (T6) is unsafe by construction. The
    // guard must fire BEFORE the source file is ever opened.
    #[test]
    fn sqlite_restore_refuses_read_only_without_touching_source_or_dest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("backup.sqlite");
        let mut content = backup::SQLITE_MAGIC_HEADER.to_vec();
        content.extend_from_slice(b"rest of a fake but header-valid sqlite file");
        std::fs::write(&src, &content).unwrap();
        let dest = dir.path().join("live.sqlite");
        std::fs::write(&dest, b"stale content").unwrap();

        let err =
            run_sqlite_restore_inner(dest.to_str().unwrap(), src.to_str().unwrap(), true).unwrap_err();
        assert_eq!(err.message, "připojení je jen pro čtení");
        // Neither file touched — the guard fires before the magic-header
        // check ever opens the source.
        assert_eq!(std::fs::read(&dest).unwrap(), b"stale content");
        assert_eq!(std::fs::read(&src).unwrap(), content);
    }

    #[test]
    fn sqlite_restore_copies_a_valid_sqlite_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("backup.sqlite");
        let mut content = backup::SQLITE_MAGIC_HEADER.to_vec();
        content.extend_from_slice(b"rest of a fake but header-valid sqlite file");
        std::fs::write(&src, &content).unwrap();
        let dest = dir.path().join("live.sqlite");
        std::fs::write(&dest, b"stale content").unwrap();

        run_sqlite_restore_inner(dest.to_str().unwrap(), src.to_str().unwrap(), false).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn sqlite_restore_missing_source_is_a_value_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("live.sqlite");
        std::fs::write(&dest, b"stale content").unwrap();
        let err = run_sqlite_restore_inner(
            dest.to_str().unwrap(),
            dir.path().join("does_not_exist.sqlite").to_str().unwrap(),
            false,
        )
        .unwrap_err();
        assert!(!err.message.is_empty());
        assert_eq!(std::fs::read(&dest).unwrap(), b"stale content");
    }

    // --- run_external_tool: end-to-end with a real (non-pg_dump) process,
    // proving spawn/stream/redact/handle wiring without any Postgres
    // dependency. ---
    //
    // NOTE: like `monitor_pg_tests`/`compare_pg_tests` above, these are
    // plain, NON-async `#[test]` fns that own a `QueryRunner` on an
    // ordinary OS thread and drive the body through
    // `runner.handle().block_on(...)`, NOT `#[tokio::test]` — wrapping in
    // `#[tokio::test]` runs the body ON a tokio runtime worker thread, and
    // `QueryRunner::new()` builds its OWN independent multi-thread
    // `tokio::Runtime`; dropping THAT runtime from inside an async context
    // (e.g. at the end of a `#[tokio::test]` fn) panics ("Cannot drop a
    // runtime in a context where blocking is not allowed"). A plain
    // `#[test]` fn is ordinary sync context, so `runner` drops safely.
    #[test]
    fn run_external_tool_streams_and_finishes_with_a_real_process() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let (mut rx, _handle) = runner.run_external_tool(
                "cmd".to_string(),
                vec!["/C".to_string(), "echo hello 1>&2".to_string()],
                None,
            );
            let mut saw_log = false;
            let mut saw_finished = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    backup::BackupEvent::Log(l) if l.contains("hello") => saw_log = true,
                    backup::BackupEvent::Finished => {
                        saw_finished = true;
                        break;
                    }
                    backup::BackupEvent::Failed(m) => panic!("unexpected failure: {m}"),
                    _ => {}
                }
            }
            assert!(saw_log && saw_finished);
        });
    }

    #[test]
    fn run_external_tool_missing_binary_is_a_failed_event() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let (mut rx, _handle) =
                runner.run_external_tool("definitely-not-a-real-binary-xyz".to_string(), vec![], None);
            let ev = rx.recv().await.unwrap();
            assert!(matches!(ev, backup::BackupEvent::Failed(_)));
        });
    }

    // --- SECURITY: PGPASSWORD reaches the child's env, never argv, and a
    // spawn/exit failure's surfaced message never contains it. ---
    #[test]
    fn run_external_tool_password_reaches_child_env_not_argv() {
        // `cmd /C echo %PGPASSWORD% 1>&2` — proves the env var is actually
        // set on the child (the whole point of the env-only mechanism)
        // while the password itself never appears in the `args` this test
        // passes in. Redirected to stderr (`1>&2`) because
        // `backup::run_and_stream` only pipes/streams the child's STDERR
        // (`stdout(Stdio::null())`) — a plain stdout `echo` would be
        // silently discarded, same as the sibling
        // `run_external_tool_streams_and_finishes_with_a_real_process` test
        // above already redirects its own `echo hello`.
        //
        // Uses a moderately special (space + `$`), but NOT cmd-quote-hostile,
        // password — a value containing an unbalanced `"`/`'` gets mangled by
        // cmd's OWN percent-expansion-then-reparse of the command line
        // before `run_and_stream` ever sees it, which would make this test
        // flaky for reasons unrelated to what it's actually proving. The
        // shell-hostile-character case (`'`, `"`, `--format=evil`, etc.) is
        // already covered by `backup.rs`'s own pure `build_pg_dump_args`
        // `NASTY_PASSWORD` tests, which assert the argv shape directly with
        // no shell in between.
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            const NASTY_PASSWORD: &str = "hunter2$secret pw";
            let args = vec!["/C".to_string(), "echo %PGPASSWORD% 1>&2".to_string()];
            assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
            let (mut rx, _handle) =
                runner.run_external_tool("cmd".to_string(), args, Some(NASTY_PASSWORD.to_string()));
            let mut saw_env_value = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    // The redacted echo of the env var comes back as `***`
                    // — proving both that the child actually saw the real
                    // password (echoed it) AND that the log line is
                    // redacted before ever leaving `run_and_stream`.
                    backup::BackupEvent::Log(l) if l.contains("***") => saw_env_value = true,
                    backup::BackupEvent::Finished => break,
                    backup::BackupEvent::Failed(m) => panic!("unexpected failure: {m}"),
                    _ => {}
                }
            }
            assert!(saw_env_value, "expected the redacted PGPASSWORD echo in the log");
        });
    }
}

/// G11 T5: docker + a real, locally-installed `pg_dump`/`pg_restore`
/// required — validates T4's `run_external_tool`/`resolve_tool_path` against
/// a live `postgres:16.13` container, end to end (tool resolution, real
/// `ConnectionConfig`-driven arg building, PGPASSWORD-via-env, spawn, log
/// streaming, redaction) rather than just the pure builders `backup.rs`'s
/// own unit tests already cover. Same hazard/pattern as
/// `monitor_pg_tests`/`compare_pg_tests` above (see those modules' doc
/// comments for the full nested-runtime rationale, not repeated here):
/// every test is a plain, NON-async `#[test]` driven through
/// `runner.handle().block_on(...)`, NOT `#[tokio::test]`.
///
/// Local `pg_dump`/`pg_restore` install is a real prerequisite, same class
/// of external requirement docker itself is. Unlike `resolve_tool_path`'s
/// own unit tests (which expect a hard failure), a MISSING install here is
/// NOT a test failure — these tests skip gracefully (an `eprintln!` note,
/// then an early `return`) rather than panicking, since "is PostgreSQL
/// client tools installed on this machine" is an environment fact, not a
/// regression this suite should ever fail CI over. Run with:
///   %USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored backup_docker_tests::
#[cfg(test)]
mod backup_docker_tests {
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

    /// open_spec (NOT connect::open): see `monitor_pg_tests`'s doc comment.
    async fn open_pg(url: &str) -> Box<dyn Connection> {
        let handle = tokio::runtime::Handle::current();
        open_spec(ConnectSpec::Url(url.to_string()), handle).await.expect("connect").conn
    }

    async fn create_database(default_db_url: &str, name: &str) {
        let mut conn = open_pg(default_db_url).await;
        conn.execute(&format!("CREATE DATABASE {name}"), CancelToken::new()).await.unwrap();
    }

    fn cfg_for(host: &str, port: u16, database: &str) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "docker-pg".into(),
            name: "docker-pg".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Postgres,
            host: host.into(),
            port: Some(port),
            database: database.into(),
            user: "postgres".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    /// Skip-gracefully helper (see module doc comment) — `None` means the
    /// caller should log and return early rather than fail the test.
    fn try_resolve(name: &str) -> Option<String> {
        match resolve_tool_path(None, name) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!(
                    "SKIP backup_docker_tests: {name} not resolvable ({e}) — install PostgreSQL client tools to run this test live"
                );
                None
            }
        }
    }

    /// Seed a live source database with known data -> real `pg_dump` (Custom
    /// format) -> assert the dump file exists, is non-empty, sniffs as
    /// Custom (PGDMP magic), and contains the seeded table's name in its
    /// TOC -> real `pg_restore` into a FRESH throwaway database -> assert
    /// the seeded rows actually roundtripped (queried back through the
    /// restored database, not just a `Finished` event).
    #[test]
    #[ignore]
    fn real_pg_dump_backup_then_pg_restore_roundtrip() {
        let Some(pg_dump) = try_resolve("pg_dump") else { return };
        let Some(pg_restore) = try_resolve("pg_restore") else { return };

        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let port = node.get_host_port_ipv4(5432).await.unwrap();
            let default_url = pg_url(&node, "postgres").await;

            {
                let mut setup = open_pg(&default_url).await;
                setup
                    .execute("CREATE TABLE roundtrip_t (id INT PRIMARY KEY, v TEXT)", CancelToken::new())
                    .await
                    .unwrap();
                setup
                    .execute(
                        "INSERT INTO roundtrip_t VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma')",
                        CancelToken::new(),
                    )
                    .await
                    .unwrap();
            }

            let cfg = cfg_for("127.0.0.1", port, "postgres");
            let out_dir = tempfile::tempdir().unwrap();
            let out_path = out_dir.path().join("roundtrip.backup");

            let opts = backup::PgBackupOptions { format: backup::PgDumpFormat::Custom, compress: 6 };
            let args = backup::build_pg_dump_args(
                &cfg,
                &cfg.host,
                cfg.port.unwrap(),
                &opts,
                out_path.to_str().unwrap(),
            )
            .expect("dbname passes validate_pg_dbname");

            let (mut rx, _handle) = runner.run_external_tool(pg_dump, args, Some("postgres".to_string()));
            let mut finished = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    backup::BackupEvent::Finished => {
                        finished = true;
                        break;
                    }
                    backup::BackupEvent::Failed(m) => panic!("pg_dump failed: {m}"),
                    backup::BackupEvent::Log(_) => {}
                }
            }
            assert!(finished, "pg_dump did not report Finished");
            assert!(out_path.is_file(), "dump file must exist");
            let dumped = std::fs::read(&out_path).unwrap();
            assert!(!dumped.is_empty(), "dump file must be non-empty");
            assert_eq!(
                backup::detect_dump_format(&dumped[..dumped.len().min(64)]),
                backup::DumpFormat::Custom,
                "a -Fc dump must sniff as Custom via the PGDMP magic"
            );
            // Custom-format archives embed their TOC entry tags (object
            // names, including table names) as plain readable text —
            // best-effort proof the real seeded DDL made it into the dump,
            // not just a magic-header check.
            let dumped_text = String::from_utf8_lossy(&dumped);
            assert!(
                dumped_text.contains("roundtrip_t"),
                "dump must contain the seeded table's name in its TOC"
            );

            create_database(&default_url, "roundtrip_target").await;
            let restore_cfg = cfg_for("127.0.0.1", port, "roundtrip_target");
            let restore_opts = backup::PgRestoreOptions::default();
            let restore_args = backup::build_pg_restore_args(
                &restore_cfg,
                &restore_cfg.host,
                restore_cfg.port.unwrap(),
                &restore_opts,
                out_path.to_str().unwrap(),
            )
            .expect("dbname passes validate_pg_dbname");

            let (mut rx2, _h2) =
                runner.run_external_tool(pg_restore, restore_args, Some("postgres".to_string()));
            let mut restored = false;
            while let Some(ev) = rx2.recv().await {
                match ev {
                    backup::BackupEvent::Finished => {
                        restored = true;
                        break;
                    }
                    backup::BackupEvent::Failed(m) => panic!("pg_restore failed: {m}"),
                    backup::BackupEvent::Log(_) => {}
                }
            }
            assert!(restored, "pg_restore did not report Finished");

            // Prove the DATA genuinely roundtripped — not just a Finished
            // event — by querying the restored database back.
            let target_url = pg_url(&node, "roundtrip_target").await;
            let mut verify = open_pg(&target_url).await;
            let mut stream = verify
                .query("SELECT id, v FROM roundtrip_t ORDER BY id", CancelToken::new())
                .await
                .unwrap();
            let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
            while let Some(item) = stream.batches.recv().await {
                buf.push(item.unwrap()).unwrap();
            }
            assert_eq!(buf.row_count(), 3, "all 3 seeded rows must roundtrip");
            assert_eq!(buf.cell_text(0, 1), "alpha");
            assert_eq!(buf.cell_text(1, 1), "beta");
            assert_eq!(buf.cell_text(2, 1), "gamma");
        });
    }

    /// SECURITY REQUIRED (Global Constraints item 3): a deliberately WRONG
    /// password against a real Postgres container makes `pg_dump` fail via
    /// its own auth rejection — the resulting `BackupEvent::Failed`/`Log`
    /// text must never contain the real (wrong-but-still-a-real-string)
    /// password anywhere. A second, defense-in-depth assertion ties this
    /// SAME live error text to the redaction mechanism directly: run
    /// through `backup::redact_secret` (the exact function every
    /// `run_and_stream` log line/failure message already passes through)
    /// with the real password appended, proving that had the password
    /// leaked into pg_dump's own stderr, it would have come back as `***`
    /// rather than as plaintext.
    #[test]
    #[ignore]
    fn wrong_password_error_never_contains_the_real_password() {
        let Some(pg_dump) = try_resolve("pg_dump") else { return };

        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let port = node.get_host_port_ipv4(5432).await.unwrap();
            let cfg = cfg_for("127.0.0.1", port, "postgres");

            let out_dir = tempfile::tempdir().unwrap();
            let out_path = out_dir.path().join("should_not_exist.backup");
            let opts = backup::PgBackupOptions { format: backup::PgDumpFormat::Custom, compress: 0 };
            let args = backup::build_pg_dump_args(
                &cfg,
                &cfg.host,
                cfg.port.unwrap(),
                &opts,
                out_path.to_str().unwrap(),
            )
            .expect("dbname passes validate_pg_dbname");

            const WRONG_PASSWORD: &str = "definitely-the-wrong-password-42";
            let (mut rx, _handle) =
                runner.run_external_tool(pg_dump, args, Some(WRONG_PASSWORD.to_string()));
            let mut failure_text = String::new();
            let mut saw_finished = false;
            while let Some(ev) = rx.recv().await {
                match ev {
                    backup::BackupEvent::Failed(m) => {
                        failure_text = m;
                        break;
                    }
                    backup::BackupEvent::Finished => {
                        saw_finished = true;
                        break;
                    }
                    backup::BackupEvent::Log(l) => failure_text.push_str(&l),
                }
            }
            assert!(!saw_finished, "expected an auth failure with the wrong password, got Finished");
            assert!(!failure_text.contains(WRONG_PASSWORD), "leaked password in: {failure_text}");
            assert!(!out_path.exists(), "no dump file should be produced on an auth failure");

            let synthetic_leak = format!("{failure_text} PGPASSWORD={WRONG_PASSWORD}");
            let redacted = backup::redact_secret(&synthetic_leak, Some(WRONG_PASSWORD));
            assert!(redacted.contains("***"), "redaction must substitute the secret with ***");
            assert!(!redacted.contains(WRONG_PASSWORD), "redacted text must never contain the real secret");
        });
    }
}

/// G10 T1+T2 review carry-forward (BLOCKER 2): `admin_sql::privileges_catalog`'s
/// Postgres SQL (`aclexplode`/`acldefault`) had never run against a live
/// PostgreSQL server — string-unit-tested only. This module closes that gap,
/// plus proves the full admin write path (create role -> grant -> revoke ->
/// drop) end-to-end through the SAME sanctioned `run_write_transaction_inner`
/// every admin Apply click will use.
///
/// Same "plain #[test] + `runner.handle().block_on(...)`, NEVER
/// `#[tokio::test]`" discipline as `monitor_pg_tests`/`compare_pg_tests`
/// above (see either module's doc comment for the full nested-runtime-panic
/// rationale) — `open_spec` is used for every connection (never
/// `connect::open` directly).
#[cfg(test)]
mod admin_pg_tests {
    use super::*;
    use crate::admin_sql;
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
    async fn open_pg(url: &str) -> Box<dyn Connection> {
        let handle = tokio::runtime::Handle::current();
        open_spec(ConnectSpec::Url(url.to_string()), handle).await.expect("connect").conn
    }

    /// BLOCKER 2: runs `admin_sql::privileges_catalog(Postgres, "public")`
    /// (via `fetch_admin_catalog_inner`, the exact function the runner's
    /// public `fetch_admin_catalog` spawns) against a live PG 16.13 with a
    /// table, an extra role, and an explicit GRANT seeded — asserts the
    /// labeled result shape AND that the seeded grant is actually visible in
    /// `object_acl`'s rows, proving `aclexplode`/`acldefault` parse and run
    /// for real (never exercised end-to-end before this test).
    #[test]
    #[ignore]
    fn admin_pg_privileges_catalog_sees_seeded_grant_on_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;

            {
                let mut setup = open_pg(&url).await;
                setup
                    .execute("CREATE TABLE priv_t (id integer PRIMARY KEY)", CancelToken::new())
                    .await
                    .unwrap();
                setup
                    .execute("CREATE ROLE priv_grantee LOGIN", CancelToken::new())
                    .await
                    .unwrap();
                setup
                    .execute("GRANT SELECT ON priv_t TO priv_grantee", CancelToken::new())
                    .await
                    .unwrap();
            }

            let queries = admin_sql::privileges_catalog(dbc_state::Engine::Postgres, "public");
            let result = fetch_admin_catalog_inner(
                ConnectSpec::Url(url),
                queries,
                tokio::runtime::Handle::current(),
            )
            .await
            .expect("privileges_catalog must run cleanly against live PG (aclexplode/acldefault)");

            assert_eq!(
                result.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
                vec!["object_acl", "schema_acl", "db_acl"]
            );

            let (_, (cols, rows)) = &result[0]; // object_acl
            assert_eq!(cols, &vec![
                "schema".to_string(), "object".to_string(), "kind".to_string(),
                "grantee".to_string(), "privilege_type".to_string(), "is_grantable".to_string(),
            ]);
            let grantee_ix = cols.iter().position(|c| c == "grantee").unwrap();
            let object_ix = cols.iter().position(|c| c == "object").unwrap();
            let priv_ix = cols.iter().position(|c| c == "privilege_type").unwrap();
            assert!(
                rows.iter().any(|r| {
                    r[object_ix].as_deref() == Some("priv_t")
                        && r[grantee_ix].as_deref() == Some("priv_grantee")
                        && r[priv_ix].as_deref() == Some("SELECT")
                }),
                "seeded GRANT SELECT ON priv_t TO priv_grantee must appear in object_acl rows: {rows:?}"
            );

            // schema_acl/db_acl must at least run without error and return
            // their declared columns — sanity, not exhaustive (no schema-
            // or database-level grant was seeded).
            let (_, (schema_cols, _)) = &result[1];
            assert!(schema_cols.contains(&"grantee".to_string()));
            let (_, (db_cols, _)) = &result[2];
            assert!(db_cols.contains(&"grantee".to_string()));
        });
    }

    /// Plan T3 step 4 (adapted to this file's testcontainers discipline
    /// rather than the plan's `DBC_PG_ADMIN_URL` env-var variant — Docker is
    /// available in this environment, so the live test can actually run
    /// here rather than merely being documented): end-to-end create role ->
    /// grant -> revoke -> drop through the REAL write path
    /// (`run_write_transaction_inner`, the same body
    /// `QueryRunner::run_write_transaction` spawns), asserting the
    /// history-bound display join is redacted and the whole sequence
    /// commits successfully against live PG.
    #[test]
    #[ignore]
    fn admin_pg_roundtrip_create_grant_revoke_drop() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;
            let engine = dbc_state::Engine::Postgres;
            let password = "tajne'heslo";
            let role = "g10_admin_test_role";

            let mut stmts = admin_sql::create_role(
                engine,
                role,
                password,
                &admin_sql::RoleFlags { login: true, ..Default::default() },
            );
            stmts.extend(admin_sql::database_privilege_pg(
                "postgres",
                "CONNECT",
                role,
                admin_sql::CellState::Granted,
            ));
            stmts.extend(admin_sql::database_privilege_pg(
                "postgres",
                "CONNECT",
                role,
                admin_sql::CellState::NotSet,
            ));
            stmts.extend(admin_sql::drop_role(engine, role));

            // What record_history/the confirm modal would show — display only.
            let shown = stmts.iter().map(|s| s.display_sql.as_str()).collect::<Vec<_>>().join("\n");
            assert!(shown.contains("'***'"));
            assert!(!shown.contains("tajne"));

            let total =
                run_write_transaction_inner(ConnectSpec::Url(url), stmts, None, tokio::runtime::Handle::current())
                    .await
                    .expect("create -> grant -> revoke -> drop must commit against live PG");
            let _ = total; // DDL affected counts are driver-defined; success is the assertion
        });
    }

    /// G10 T5: the Privileges sub-view's full loop against LIVE PostgreSQL
    /// — fetch -> `admin_panel::MatrixState::from_catalog` (parsing REAL
    /// rows, not the hand-built fixtures `matrix_tests` uses) -> click_cell
    /// (stage a REVOKE) -> `to_statements` -> the SAME sanctioned write path
    /// (`run_write_transaction_inner`) -> refetch -> `from_catalog` again,
    /// asserting the revoke actually committed and the untouched privilege
    /// survived. Closes the gap between T3's separate "catalog SQL runs
    /// live" and "write path runs live" tests: this is the one place the
    /// CLIENT-SIDE parsing/diffing logic (`MatrixState`) is proven against
    /// a real server's column/row shapes rather than fixtures.
    #[test]
    #[ignore]
    fn admin_pg_privileges_matrix_click_revoke_round_trips_on_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;
            let engine = dbc_state::Engine::Postgres;
            let grantee = "g10_matrix_test_role";

            {
                let mut setup = open_pg(&url).await;
                setup
                    .execute("CREATE TABLE priv_matrix_t (id integer PRIMARY KEY)", CancelToken::new())
                    .await
                    .unwrap();
                setup.execute(&format!("CREATE ROLE {grantee} LOGIN"), CancelToken::new()).await.unwrap();
                setup
                    .execute(
                        &format!("GRANT SELECT, INSERT ON priv_matrix_t TO {grantee}"),
                        CancelToken::new(),
                    )
                    .await
                    .unwrap();
            }

            let fetch_matrix = |url: String| {
                let grantee = grantee.to_string();
                async move {
                    let rows = fetch_admin_catalog_inner(
                        ConnectSpec::Url(url),
                        admin_sql::privileges_catalog(engine, "public"),
                        tokio::runtime::Handle::current(),
                    )
                    .await
                    .expect("privileges_catalog must run cleanly against live PG");
                    crate::admin_panel::MatrixState::from_catalog(engine, &grantee, &rows)
                }
            };

            let before = fetch_matrix(url.clone()).await;
            assert_eq!(before.effective("priv_matrix_t", "SELECT"), admin_sql::CellState::Granted);
            assert_eq!(before.effective("priv_matrix_t", "INSERT"), admin_sql::CellState::Granted);

            let mut m = before;
            m.click_cell(engine, "priv_matrix_t", "SELECT"); // Granted -> NotSet (revoke)
            let stmts = m
                .to_statements(engine, "public", grantee, "postgres")
                .expect("no staged Denied/empty-privs cell on pg — must not refuse");
            assert_eq!(stmts.len(), 1, "only SELECT was staged: {stmts:?}");
            assert_eq!(stmts[0].exec_sql, format!("REVOKE SELECT ON \"public\".\"priv_matrix_t\" FROM \"{grantee}\""));

            run_write_transaction_inner(ConnectSpec::Url(url.clone()), stmts, None, tokio::runtime::Handle::current())
                .await
                .expect("the staged REVOKE must commit against live PG");

            let after = fetch_matrix(url).await;
            assert_eq!(
                after.effective("priv_matrix_t", "SELECT"),
                admin_sql::CellState::NotSet,
                "SELECT must show revoked after refetch"
            );
            assert_eq!(
                after.effective("priv_matrix_t", "INSERT"),
                admin_sql::CellState::Granted,
                "INSERT was never touched — must survive untouched"
            );
        });
    }

    /// G10 T6: `admin_sql::sizes_catalog`/`create_schema`/`drop_schema` had
    /// never run against live PostgreSQL before this task (T3's docker
    /// tests only ever exercised `privileges_catalog` and the role write
    /// path) — closes that gap. Fetches `sizes_catalog`, parses it through
    /// the SAME `admin_panel` pure functions the Databases sub-view uses
    /// (`current_db_size_label`/`parse_db_sizes`/`parse_schema_sizes`) and
    /// asserts sane results; then exercises the full schema-DDL path live:
    /// `create_schema` -> appears in a refetch's `schema_sizes`; a plain
    /// `DROP SCHEMA` on that now-non-empty schema fails (design §2/§6 "let
    /// the server say no" — the engine's own error, not app-level
    /// pre-flight checking); `drop_schema(..., cascade: true)` succeeds and
    /// the schema disappears from a final refetch.
    #[test]
    #[ignore]
    fn admin_pg_sizes_catalog_and_schema_ddl_round_trip_on_live_postgres() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;
            let engine = dbc_state::Engine::Postgres;
            let schema = "g10_sizes_test_schema";

            {
                let mut setup = open_pg(&url).await;
                setup
                    .execute("CREATE TABLE sizes_t (id integer PRIMARY KEY, v text)", CancelToken::new())
                    .await
                    .unwrap();
                setup
                    .execute("INSERT INTO sizes_t SELECT g, 'x' FROM generate_series(1, 100) g", CancelToken::new())
                    .await
                    .unwrap();
            }

            let fetch_sizes = |url: String| async move {
                fetch_admin_catalog_inner(
                    ConnectSpec::Url(url),
                    admin_sql::sizes_catalog(engine),
                    tokio::runtime::Handle::current(),
                )
                .await
                .expect("sizes_catalog must run cleanly against live PG")
            };

            // --- sanity: sizes_catalog's three labels parse to sane values.
            let rows = fetch_sizes(url.clone()).await;
            let headline = rows
                .iter()
                .find(|(l, _)| *l == "current_db_size")
                .and_then(|(_, data)| crate::admin_panel::current_db_size_label(engine, data));
            assert!(headline.is_some(), "current_db_size must parse to a non-empty label: {rows:?}");

            let databases = rows
                .iter()
                .find(|(l, _)| *l == "databases")
                .map(|(_, data)| crate::admin_panel::parse_db_sizes(engine, data))
                .unwrap();
            assert!(
                databases.iter().any(|(name, bytes)| name == "postgres" && bytes.unwrap_or(0) > 0),
                "the postgres database itself must appear with a nonzero size: {databases:?}"
            );

            let schema_sizes_before = rows
                .iter()
                .find(|(l, _)| *l == "schema_sizes")
                .map(|(_, data)| crate::admin_panel::parse_schema_sizes(engine, data))
                .unwrap();
            assert!(
                schema_sizes_before.iter().any(|(s, bytes)| s == "public" && *bytes > 0),
                "public must show a nonzero size after seeding sizes_t: {schema_sizes_before:?}"
            );
            assert!(!schema_sizes_before.iter().any(|(s, _)| s == schema), "test schema must not exist yet");
            // Review finding M3 (live-verified): pg_catalog/information_schema
            // and the toast/temp implementation-detail namespaces must never
            // appear as "selectable schemas" in the Databases sub-view.
            assert!(
                !schema_sizes_before.iter().any(|(s, _)| s == "pg_catalog" || s == "information_schema"),
                "system schemas must be filtered out: {schema_sizes_before:?}"
            );
            assert!(
                !schema_sizes_before.iter().any(|(s, _)| s.starts_with("pg_toast") || s.starts_with("pg_temp_")),
                "toast/temp namespaces must be filtered out: {schema_sizes_before:?}"
            );

            // --- create_schema, live.
            let create_stmts = admin_sql::create_schema(engine, schema);
            run_write_transaction_inner(
                ConnectSpec::Url(url.clone()),
                create_stmts,
                None,
                tokio::runtime::Handle::current(),
            )
            .await
            .expect("CREATE SCHEMA must commit against live PG");

            let after_create = fetch_sizes(url.clone()).await;
            let schema_sizes_after_create = after_create
                .iter()
                .find(|(l, _)| *l == "schema_sizes")
                .map(|(_, data)| crate::admin_panel::parse_schema_sizes(engine, data))
                .unwrap();
            assert!(
                schema_sizes_after_create.iter().any(|(s, _)| s == schema),
                "the new schema must appear in a refetch: {schema_sizes_after_create:?}"
            );

            // Populate the new schema so it's non-empty for the next step.
            {
                let mut conn = open_pg(&url).await;
                conn.execute(&format!("CREATE TABLE {schema}.t (id integer)"), CancelToken::new())
                    .await
                    .unwrap();
            }

            // --- plain DROP SCHEMA on a non-empty schema must fail — the
            // engine's own error, "let the server say no" (design §2/§6).
            let drop_no_cascade = admin_sql::drop_schema(engine, schema, false);
            let err = run_write_transaction_inner(
                ConnectSpec::Url(url.clone()),
                drop_no_cascade,
                None,
                tokio::runtime::Handle::current(),
            )
            .await
            .expect_err("DROP SCHEMA without CASCADE must fail on a non-empty schema");
            assert!(!err.message.is_empty());

            // --- DROP SCHEMA ... CASCADE succeeds and removes it.
            let drop_cascade = admin_sql::drop_schema(engine, schema, true);
            assert!(drop_cascade[0].exec_sql.ends_with(" CASCADE"));
            run_write_transaction_inner(
                ConnectSpec::Url(url.clone()),
                drop_cascade,
                None,
                tokio::runtime::Handle::current(),
            )
            .await
            .expect("DROP SCHEMA ... CASCADE must commit against live PG");

            let after_drop = fetch_sizes(url).await;
            let schema_sizes_after_drop = after_drop
                .iter()
                .find(|(l, _)| *l == "schema_sizes")
                .map(|(_, data)| crate::admin_panel::parse_schema_sizes(engine, data))
                .unwrap();
            assert!(
                !schema_sizes_after_drop.iter().any(|(s, _)| s == schema),
                "the schema must be gone after CASCADE drop: {schema_sizes_after_drop:?}"
            );
        });
    }
}
