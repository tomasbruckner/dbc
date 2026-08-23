//! Microsoft SQL Server driver via `odbc-api` (ODBC), implementing
//! `dbc_core::Connection`.
//!
//! # Architecture
//!
//! This driver mirrors `dbc-driver-sqlite`, not the async
//! `dbc-driver-postgres`: `odbc_api::Connection<'_>` is a blocking,
//! non-`Sync` (but `Send`) handle, so every DB operation runs inside
//! `tokio::task::spawn_blocking` and the handle is moved in and back out
//! across that boundary rather than being driven by an async I/O driver
//! task the way `tokio_postgres`'s `Client` is.
//!
//! * `query()` opens a **fresh** connection per call (like sqlite), runs it
//!   to completion inside one `spawn_blocking`, and streams `RecordBatch`es
//!   out over a channel as they're produced.
//! * `execute()` keeps a **persistent** connection in `exec_conn`, taken out
//!   of the `Option` and moved into `spawn_blocking` then put back — the
//!   same take-out/put-back invariant `SqliteConnection::exec_conn` uses, so
//!   a `BEGIN … COMMIT`/`ROLLBACK` sequence issued via successive `execute`
//!   calls runs over one underlying ODBC connection handle instead of being
//!   silently split across separate sessions. See `dbc_core::Connection`'s
//!   doc comment for the transaction-per-connection contract this
//!   maintains and the engine-divergence behavior callers must respect.
//! * `schema()` opens a fresh connection and runs the `sys.*` catalog
//!   queries in `schema.rs`.
//!
//! `Environment` is process-global ODBC state — the ODBC spec allows only
//! one per process — so it lives in a single `OnceLock<Result<Environment,
//! String>>` ([`environment`]), giving every connection a `'static`
//! borrow without unsafe.
//!
//! # Integration notes (things this crate does NOT fix)
//!
//! 1. **Identifier quoting.** `dbc_core::ddl::quote_ident` emits
//!    double-quoted identifiers (`"name"`), which is the ANSI/Postgres/
//!    SQLite convention. SQL Server's default (non-`SET QUOTED_IDENTIFIER`)
//!    convention is bracket quoting (`[name]`); double-quoted identifiers
//!    only work under `SET QUOTED_IDENTIFIER ON`, which is a session
//!    setting the DDL generator can't assume. Until `ddl.rs` is
//!    dialectized per-engine, this driver's `execute()` MUST NOT be wired
//!    into the sandbox Apply flow — it is otherwise ready (transaction
//!    invariant, affected-row reporting), but any DDL/DML text the sandbox
//!    generates via `quote_ident`/`quote_qualified` would be silently wrong
//!    against a real SQL Server unless that session happens to have
//!    `QUOTED_IDENTIFIER ON` (the ODBC driver's default, incidentally — but
//!    relying on a driver default rather than an explicit dialect is the
//!    gap being flagged, not a fix).
//! 2. **Encrypt / TrustServerCertificate as connect-dialog options.**
//!    [`config::MssqlConfig`] exposes both as first-class fields with a
//!    secure-by-default posture (`encrypt: true`,
//!    `trust_server_certificate: false`, matching ODBC Driver 18's own
//!    default), but nothing upstream (dbc-ui's connect dialog / saved
//!    connection config) surfaces them yet. Wiring that up is out of scope
//!    here (dbc-ui is owned by a different track).
//! 3. **Mid-transaction error divergence — needs empirical verification.**
//!    `dbc_core::Connection::execute`'s doc comment documents two known
//!    engine behaviors after a failed statement inside an open transaction:
//!    SQLite leaves the transaction open and usable; PostgreSQL aborts it
//!    (every further statement fails until `ROLLBACK`). SQL Server's
//!    default session setting `XACT_ABORT OFF` is documented (informally,
//!    from T-SQL semantics — NOT verified against a live server by this
//!    crate) to imply a **third** behavior: most runtime errors do *not*
//!    abort the transaction, and do not even necessarily roll back the
//!    failed statement's partial effects, unless the error is a
//!    constraint violation (which does roll back just that statement) or a
//!    severity high enough to abort the batch. A transaction driver built
//!    on this crate must not assume either the sqlite or the postgres
//!    behavior; it needs a real server to characterize this before the
//!    Apply flow's "stop at first error and roll back" logic can be trusted
//!    to observe consistent state on MSSQL. Related: this driver never
//!    calls `set_autocommit(false)`/`Connection::commit`/`Connection::
//!    rollback` — like the sqlite/postgres drivers, `BEGIN`/`COMMIT`/
//!    `ROLLBACK` are forwarded as plain T-SQL text over the persistent
//!    `exec_conn`, at ODBC's default `SQL_ATTR_AUTOCOMMIT = ON`. Relying on
//!    a driver-level autocommit setting instead would need the driver
//!    itself (not the Apply flow's SQL text) to decide transaction
//!    boundaries, which doesn't fit this trait's "caller drives the SQL
//!    text" contract — but whether ODBC Driver 18's autocommit-on mode
//!    ever interferes with an app-issued literal `BEGIN TRANSACTION` (e.g.
//!    by auto-committing between statements the app believes are inside an
//!    open transaction) is itself unverified and should be checked
//!    alongside the `XACT_ABORT` behavior above.
//! 4. **Runtime prerequisite.** This driver requires "ODBC Driver 17" or
//!    "18 for SQL Server" to be installed on the machine running it (not
//!    bundled — `odbc-api` links the platform's ODBC driver manager, which
//!    loads the named driver from the system driver registry/registry
//!    keys). [`config::MssqlConfig::driver`] can target 17 instead of the
//!    18 default if that's what's installed.
//!
//! # Cancellation
//!
//! Only cooperative, batch-granularity cancellation is implemented: the
//! `CancelToken` is checked before opening a connection, before running the
//! statement, and between each `BlockCursor::fetch()` batch in `query()`.
//! There is no protocol-level cancel (no `SQLCancelHandle` call): odbc-api's
//! safe surface doesn't expose one directly on `CursorImpl`/`Preallocated`
//! as of the version pinned here, and reaching for the raw `odbc-sys`
//! `SQLCancelHandle` FFI would mean unsafely sharing/duplicating a raw
//! statement handle across the async watcher task and the blocking query
//! task — exactly the kind of sprawling unsafe this crate was asked to
//! avoid. The practical consequence: cancelling while `Connection::execute`
//! itself is blocked server-side (i.e. before the first batch of rows is
//! available to fetch) does not interrupt that wait; cancellation only
//! takes effect once row fetching begins, or before it starts.

mod config;
mod schema;
mod types;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, Connection as DbcConnection, QueryError, QueryStream, SchemaSnapshot,
    BATCH_ROWS, CHANNEL_CAPACITY,
};
use odbc_api::buffers::TextRowSet;
use odbc_api::{ConnectionOptions, Cursor, Environment, ResultSetMetadata};

pub use config::{escape_odbc_value, MssqlConfig};

use types::{cancelled_err, map_row_count, odbc_err};

/// Column buffer size cap for `query()`'s result-set streaming (distinct
/// from the smaller/larger caps `schema.rs` uses for its own catalog
/// queries): generous enough for ordinary text/numeric columns; a
/// `varchar(max)`/`nvarchar(max)`/blob-ish column longer than this is
/// truncated by the ODBC driver at fetch time — a known limitation, not
/// currently surfaced as a distinct error (it reads like an ordinary
/// shorter value). Mirrors the sqlite driver's blob placeholder in spirit
/// (both drivers choose a bounded, lossy text representation over
/// unbounded memory use per row) but does not synthesize an explicit
/// placeholder marker the way sqlite's `<blob N B>` does, since odbc-api
/// does not report the true untruncated length back to us here.
const QUERY_MAX_STR_LEN: usize = 65536;

/// Process-wide ODBC environment. Only one may exist per process (ODBC
/// constraint) — see the module doc's [`Environment`] discussion. Stored as
/// `Result<Environment, String>` (not `Environment` directly) so a creation
/// failure can be reported as a `QueryError` on first use instead of
/// panicking on `OnceLock::get_or_init`.
static ENVIRONMENT: OnceLock<Result<Environment, String>> = OnceLock::new();

fn environment() -> Result<&'static Environment, QueryError> {
    let cell = ENVIRONMENT.get_or_init(|| Environment::new().map_err(|e| e.to_string()));
    match cell {
        Ok(env) => Ok(env),
        Err(msg) => {
            Err(QueryError::msg(format!("failed to initialize ODBC environment: {msg}")))
        }
    }
}

fn connect(conn_str: &str) -> Result<odbc_api::Connection<'static>, QueryError> {
    let env = environment()?;
    env.connect_with_connection_string(conn_str, ConnectionOptions::default()).map_err(odbc_err)
}

pub struct MssqlConnection {
    conn_str: String,
    /// Persistent connection reused across [`DbcConnection::execute`]
    /// calls — see the module doc's `execute()` bullet. `query`/`schema`
    /// always open a fresh connection and never touch this field.
    exec_conn: Option<odbc_api::Connection<'static>>,
}

impl MssqlConnection {
    pub fn new(config: &MssqlConfig) -> Self {
        Self { conn_str: config.to_connection_string(), exec_conn: None }
    }

    /// Escape hatch for a caller-constructed connection string (e.g. one
    /// read from a config file) instead of [`MssqlConfig`].
    pub fn from_connection_string(conn_str: impl Into<String>) -> Self {
        Self { conn_str: conn_str.into(), exec_conn: None }
    }
}

#[async_trait]
impl DbcConnection for MssqlConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (schema_tx, schema_rx) =
            tokio::sync::oneshot::channel::<Result<SchemaRef, QueryError>>();
        let conn_str = self.conn_str.clone();
        let sql = sql.to_owned();

        tokio::task::spawn_blocking(move || {
            if cancel.is_cancelled() {
                let _ = schema_tx.send(Err(cancelled_err()));
                return;
            }
            let conn = match connect(&conn_str) {
                Ok(c) => c,
                Err(e) => {
                    let _ = schema_tx.send(Err(e));
                    return;
                }
            };
            if cancel.is_cancelled() {
                let _ = schema_tx.send(Err(cancelled_err()));
                return;
            }
            let mut cursor = match conn.execute(&sql, (), None) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    let _ = schema_tx
                        .send(Err(QueryError::msg("statement produced no result set")));
                    return;
                }
                Err(e) => {
                    let _ = schema_tx.send(Err(odbc_err(e)));
                    return;
                }
            };

            let col_names: Vec<String> = match cursor.column_names() {
                Ok(it) => match it.collect::<Result<Vec<_>, _>>() {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = schema_tx.send(Err(odbc_err(e)));
                        return;
                    }
                },
                Err(e) => {
                    let _ = schema_tx.send(Err(odbc_err(e)));
                    return;
                }
            };
            let ncols = col_names.len();
            let schema: SchemaRef = Arc::new(Schema::new(
                col_names.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect::<Vec<_>>(),
            ));
            let _ = schema_tx.send(Ok(schema.clone()));

            let mut buffers = match TextRowSet::for_cursor(
                BATCH_ROWS,
                &mut cursor,
                Some(QUERY_MAX_STR_LEN),
            ) {
                Ok(b) => b,
                Err(e) => {
                    let _ = tx.blocking_send(Err(odbc_err(e)));
                    return;
                }
            };
            let mut row_set_cursor = match cursor.bind_buffer(&mut buffers) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.blocking_send(Err(odbc_err(e)));
                    return;
                }
            };

            loop {
                // Cooperative check: see the module doc's Cancellation
                // section — this is batch-granularity, not protocol-level.
                if cancel.is_cancelled() {
                    let _ = tx.blocking_send(Err(cancelled_err()));
                    break;
                }
                match row_set_cursor.fetch() {
                    Ok(Some(batch)) => {
                        let mut builders: Vec<StringBuilder> =
                            (0..ncols).map(|_| StringBuilder::new()).collect();
                        for row_index in 0..batch.num_rows() {
                            for (col_index, b) in builders.iter_mut().enumerate() {
                                match batch.at_as_str(col_index, row_index) {
                                    Ok(Some(s)) => b.append_value(s),
                                    Ok(None) => b.append_null(),
                                    Err(_) => b.append_value("<decode error: invalid utf8>"),
                                }
                            }
                        }
                        let arrays: Vec<ArrayRef> =
                            builders.into_iter().map(|mut b| Arc::new(b.finish()) as ArrayRef).collect();
                        match RecordBatch::try_new(schema.clone(), arrays) {
                            Ok(rb) => {
                                if tx.blocking_send(Ok(rb)).is_err() {
                                    break; // consumer gone
                                }
                            }
                            Err(e) => {
                                let _ = tx.blocking_send(Err(QueryError::msg(e.to_string())));
                                break;
                            }
                        }
                    }
                    Ok(None) => break, // exhausted
                    Err(e) => {
                        let _ = tx.blocking_send(Err(odbc_err(e)));
                        break;
                    }
                }
            }
        });

        let columns = schema_rx.await.map_err(|_| QueryError::msg("driver task died"))??;
        Ok(QueryStream { columns, batches: rx })
    }

    /// Executes a non-returning statement over the persistent connection in
    /// `exec_conn` — see the module doc's `execute()` bullet and, for the
    /// engine-divergence contract this must respect, `dbc_core::
    /// Connection::execute`'s doc comment.
    ///
    /// A statement that DOES produce a result set (e.g. a caller mistakenly
    /// routing a `SELECT` through `execute()`) is treated as an error, same
    /// as the sqlite driver's `rusqlite::Connection::execute` behavior —
    /// this path is for writes only.
    async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError> {
        if cancel.is_cancelled() {
            return Err(cancelled_err());
        }
        let conn_str = self.conn_str.clone();
        let taken = self.exec_conn.take();
        let sql = sql.to_owned();

        let (result, conn) = tokio::task::spawn_blocking(move || {
            let conn = match taken {
                Some(c) => c,
                None => match connect(&conn_str) {
                    Ok(c) => c,
                    Err(e) => return (Err(e), None),
                },
            };
            let result = run_execute(&conn, &sql);
            (result, Some(conn))
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?;

        if let Some(c) = conn {
            self.exec_conn = Some(c);
        }
        result
    }

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let conn_str = self.conn_str.clone();
        tokio::task::spawn_blocking(move || {
            let conn = connect(&conn_str)?;
            schema::fetch_schema_snapshot(&conn)
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?
    }
}

fn run_execute(conn: &odbc_api::Connection<'_>, sql: &str) -> Result<u64, QueryError> {
    let mut prealloc = conn.preallocate().map_err(odbc_err)?;
    match prealloc.execute(sql, ()) {
        Ok(Some(cursor)) => {
            drop(cursor);
            return Err(QueryError::msg(
                "statement returned a result set; execute() is for non-returning statements",
            ));
        }
        Ok(None) => {}
        Err(e) => return Err(odbc_err(e)),
    }
    let row_count = prealloc.row_count().map_err(odbc_err)?;
    map_row_count(row_count)
}
