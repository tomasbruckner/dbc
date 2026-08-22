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
