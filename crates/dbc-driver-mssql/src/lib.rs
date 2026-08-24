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
//! Result-set text (both `query()`'s rows and `schema()`'s catalog data) is
//! bound and decoded as UTF-16 (`SQL_C_WCHAR`), not the narrower
//! `SQL_C_CHAR`/`TextRowSet` convenience type odbc-api ships — see the
//! `wide` module doc for why narrow binding is actively wrong for non-ASCII
//! text (it transcodes through the process ANSI codepage) rather than just
//! a missed optimization.
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
//!    alongside the `XACT_ABORT` behavior above — see
//!    `tests/mssql_tx_matrix.rs` (G15 §3c), which characterizes both
//!    empirically before any MSSQL write path ships.
//! 4. **Runtime prerequisite.** This driver requires "ODBC Driver 17" or
//!    "18 for SQL Server" to be installed on the machine running it (not
//!    bundled — `odbc-api` links the platform's ODBC driver manager, which
//!    loads the named driver from the system driver registry/registry
//!    keys). [`config::MssqlConfig::driver`] can target 17 instead of the
//!    18 default if that's what's installed.
//! 5. **No read-only connection mode.** [`config::MssqlConfig`] has no
//!    equivalent of the sqlite driver's `new_with_options(path, read_only:
//!    true)`, which enforces read-only *server-side*
//!    (`SQLITE_OPEN_READ_ONLY` — a client-side guard bypass still can't
//!    mutate the file). SQL Server's ODBC connection string does have a
//!    comparable knob, `ApplicationIntent=ReadOnly`, but it only routes the
//!    connection to a readable secondary in an Always On availability
//!    group — on a standalone instance (the common case, and the only kind
//!    these ignored integration tests target) it is accepted but does not
//!    reject writes. So there is no server-enforced read-only mode this
//!    driver can wire up universally; enforcement for MSSQL will have to be
//!    app-level only (the existing `is_read_statement` guard in
//!    `dbc_core::guards`), and that gap must be called out explicitly
//!    wherever the sqlite driver's read-only connection guarantee is
//!    currently assumed to generalize across drivers — in particular when
//!    the dbc-ui read-only gate for this driver is lifted.
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
//! [`MssqlConnection::query_with_session`] checks the token at the same
//! granularity as `query()`: before connect, before the main batch (after
//! preludes), and per fetch — never inside a prelude/postlude statement
//! itself. Unlike `query()`, though, `query_with_session` is NOT
//! streaming: it fully materializes every result set in memory before
//! selecting one to hand back (see its doc comment's "BOUNDED STATEMENTS
//! ONLY" warning) — it exists for the G13 T7 Showplan-via-session-prelude
//! path, not as a general substitute for `query()`.

mod config;
mod schema;
mod types;
mod wide;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, Connection as DbcConnection, QueryError, QueryStream, SchemaSnapshot,
    BATCH_ROWS, CHANNEL_CAPACITY,
};
use odbc_api::{ConnectionOptions, Cursor, Environment, ResultSetMetadata};

pub use config::{escape_odbc_value, MssqlConfig};

use types::{cancelled_err, map_row_count, odbc_err};

/// Column buffer size cap for `query()`'s result-set streaming (distinct
/// from the smaller/larger caps `schema.rs` uses for its own catalog
/// queries), in UTF-16 code units: generous enough for ordinary
/// text/numeric columns; a `varchar(max)`/`nvarchar(max)`/blob-ish column
/// longer than this is reported via `wide::cell_text`'s explicit
/// truncation marker rather than silently shortened — see `wide.rs`'s
/// module doc for how that detection works and its known false-positive-
/// safe imprecision.
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

    /// G15 §1a eager handshake: opens ONE ODBC connection with the stored
    /// connection string and drops it. [`MssqlConnection::new`] is lazy
    /// (connects per operation); `open_config`'s contract — relied on by
    /// `test_connect` and the status bar's connect-vs-query error split —
    /// is "bad host/credentials fail HERE". Blocking (no async, no
    /// `block_on`): callers must be on a blocking-legal thread
    /// (`open_config` only ever runs inside `spawn_blocking`).
    pub fn probe(&self) -> Result<(), QueryError> {
        connect(&self.conn_str).map(|_| ())
    }

    /// G15 §2e (G13 T7): one fresh connection, session preludes, main
    /// batch, best-effort postludes — the Showplan delivery mechanism.
    /// `SET SHOWPLAN_XML` must be the ONLY statement in its batch and is
    /// session-scoped, which is why `query()` (fresh connection per call)
    /// cannot deliver it (design curation item 1).
    ///
    /// Walks every result set the main `sql` batch produces. If exactly one
    /// of them has a single column named `Microsoft SQL Server 2005 XML
    /// Showplan`, that result set is returned; otherwise the LAST result
    /// set walked is returned (fail-open on the name — the needs-
    /// verification flag from G13 §1b, bounded by "wrong text handed to a
    /// parser that fails closed").
    ///
    /// **BOUNDED MEMORY, by construction (G15 T7 review MAJOR fix).** Under
    /// `SET STATISTICS XML ON` the main `sql` batch genuinely EXECUTES and
    /// can return its own (potentially huge) data result set(s) BEFORE the
    /// plan-XML set — so this method can never simply accumulate every
    /// result set into memory the way the original G13/T2 draft did. Two
    /// independent bounds now hold simultaneously:
    /// 1. **Structural: at most two result sets are ever held at once.**
    ///    The confirmed Showplan-named set (once found) is kept for the
    ///    rest of the walk; every OTHER set is tracked only as the single
    ///    "current fallback candidate", which is REPLACED (dropping the
    ///    previous candidate's batches) as soon as a newer set is walked —
    ///    never a growing `Vec` of every set seen. This alone caps
    ///    "how many sets" but not "how big is one set".
    /// 2. **Per-set row cap: `max_rows`.** Every individual result set's
    ///    own fetch loop is capped at `max_rows` rows (checked as each
    ///    batch arrives, before it's appended) — exceeding it aborts the
    ///    WHOLE call with a clean `QueryError` (Czech message; postludes
    ///    still run, same as every other error path) rather than silently
    ///    truncating or continuing to grow. `None` disables the cap
    ///    (existing docker/integration tests that don't care about it use
    ///    this); `run_mssql_plan_inner` (dbc-ui) always passes `Some(_)` —
    ///    see its own `PLAN_ROW_CAP` doc comment for the exact value and
    ///    rationale.
    ///
    /// Together: peak memory is bounded to at most `2 * max_rows` rows'
    /// worth of batches, REGARDLESS of what the user's SQL does — routing
    /// arbitrary user SQL through this function (via `SET STATISTICS XML
    /// ON`'s actual-execution path) is now safe as long as the caller
    /// supplies a real `max_rows`. (Unlike `query()`, this method still has
    /// no streaming back-pressure — the chosen set's batches are handed
    /// over only after the whole walk completes — but "no back-pressure,
    /// bounded to ~2*max_rows rows" is a very different risk profile than
    /// the previous "no bound at all".)
    ///
    /// G15 T7 integration fix (found wiring `run_mssql_plan`, "reality
    /// wins" — not a T2 grounding-text deviation, a genuine build error):
    /// `&self` (not `&mut self`) made this structurally impossible to call
    /// from ANY future spawned via `tokio::spawn`/`Runtime::spawn` (which
    /// requires `Send`), because the returned future retains `&'a Self` for
    /// its own lifetime, and `&'a MssqlConnection: Send` requires
    /// `MssqlConnection: Sync` — which it can never be, since `exec_conn`'s
    /// `odbc_api::Connection` wraps a raw ODBC handle (`*mut c_void`, never
    /// `Sync`). `&mut self` fixes this at zero behavior cost: the body only
    /// ever READS `self.conn_str` (cloned once, up front) and never touched
    /// `exec_conn` (this method always dials a brand-new connection,
    /// independent of the persistent `exec_conn` `execute()`/`query()` use)
    /// — `&mut Self: Send` requires only `Self: Send`, which already holds
    /// (`Box<dyn Connection>` already relies on it).
    pub async fn query_with_session(
        &mut self,
        prelude: &[String],
        sql: &str,
        postlude: &[String],
        max_rows: Option<usize>,
        cancel: CancelToken,
    ) -> Result<QueryStream, QueryError> {
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (schema_tx, schema_rx) =
            tokio::sync::oneshot::channel::<Result<SchemaRef, QueryError>>();
        let conn_str = self.conn_str.clone();
        let prelude: Vec<String> = prelude.to_vec();
        let sql = sql.to_owned();
        let postlude: Vec<String> = postlude.to_vec();

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
                run_postludes(&conn, &postlude);
                let _ = schema_tx.send(Err(cancelled_err()));
                return;
            }

            // Each prelude statement is its OWN batch (design §2e step 3).
            for p in &prelude {
                match conn.execute(p, (), None) {
                    Ok(Some(cursor)) => drop(cursor), // unexpected but drained-by-drop
                    Ok(None) => {}
                    Err(e) => {
                        let err = odbc_err(e);
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(err));
                        return;
                    }
                }
            }

            let mut cursor = match conn.execute(&sql, (), None) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    run_postludes(&conn, &postlude);
                    let _ = schema_tx
                        .send(Err(QueryError::msg("statement produced no result set")));
                    return;
                }
                Err(e) => {
                    let err = odbc_err(e);
                    run_postludes(&conn, &postlude);
                    let _ = schema_tx.send(Err(err));
                    return;
                }
            };

            // Walk every result set; select as we go (G15 T7 review MAJOR
            // fix — see this method's doc comment for the full bounded-
            // memory contract). `named_match` is the confirmed
            // Showplan-named set, kept for the rest of the walk once
            // found; `fallback` is the CURRENT candidate for "last set
            // walked" — replaced (dropping the previous candidate's
            // batches) every iteration, never accumulated. At most two
            // sets' batches are ever alive at once.
            let mut named_match: Option<(SchemaRef, Vec<RecordBatch>)> = None;
            let mut fallback: Option<(SchemaRef, Vec<RecordBatch>)> = None;
            loop {
                if cancel.is_cancelled() {
                    run_postludes(&conn, &postlude);
                    let _ = schema_tx.send(Err(cancelled_err()));
                    return;
                }

                let col_names: Vec<String> = match cursor.column_names() {
                    Ok(it) => match it.collect::<Result<Vec<_>, _>>() {
                        Ok(v) => v,
                        Err(e) => {
                            let err = odbc_err(e);
                            run_postludes(&conn, &postlude);
                            let _ = schema_tx.send(Err(err));
                            return;
                        }
                    },
                    Err(e) => {
                        let err = odbc_err(e);
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(err));
                        return;
                    }
                };
                let ncols = col_names.len();
                let is_showplan =
                    ncols == 1 && col_names[0] == "Microsoft SQL Server 2005 XML Showplan";
                let schema: SchemaRef = Arc::new(Schema::new(
                    col_names
                        .iter()
                        .map(|n| Field::new(n, DataType::Utf8, true))
                        .collect::<Vec<_>>(),
                ));

                let mut buffers = match wide::build(&mut cursor, BATCH_ROWS, QUERY_MAX_STR_LEN) {
                    Ok((b, _ncols)) => b,
                    Err(e) => {
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(e));
                        return;
                    }
                };
                let mut block_cursor = match cursor.bind_buffer(&mut buffers) {
                    Ok(c) => c,
                    Err(e) => {
                        let err = odbc_err(e);
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(err));
                        return;
                    }
                };

                let mut batches: Vec<RecordBatch> = Vec::new();
                let mut rows_in_set: usize = 0;
                loop {
                    if cancel.is_cancelled() {
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(cancelled_err()));
                        return;
                    }
                    match block_cursor.fetch() {
                        Ok(Some(batch)) => {
                            rows_in_set += batch.num_rows();
                            if let Some(cap) = max_rows {
                                if rows_in_set > cap {
                                    // G15 T7 review MAJOR fix: abort the WHOLE
                                    // call rather than silently truncating —
                                    // same fail-closed posture every other
                                    // row cap in this codebase uses (e.g.
                                    // dbc-diff's DIFF_ROW_CAP). Postludes
                                    // still run, same as every other error
                                    // path here.
                                    run_postludes(&conn, &postlude);
                                    let _ = schema_tx.send(Err(QueryError::msg(format!(
                                        "výsledek přesáhl limit {cap} řádků — plán/analýza byla zamítnuta, aby se předešlo vyčerpání paměti"
                                    ))));
                                    return;
                                }
                            }
                            let mut builders: Vec<StringBuilder> =
                                (0..ncols).map(|_| StringBuilder::new()).collect();
                            for (col_index, b) in builders.iter_mut().enumerate() {
                                let slice = batch.column(col_index);
                                for row_index in 0..batch.num_rows() {
                                    match wide::cell_text(slice, row_index) {
                                        Some(s) => b.append_value(s),
                                        None => b.append_null(),
                                    }
                                }
                            }
                            let arrays: Vec<ArrayRef> = builders
                                .into_iter()
                                .map(|mut b| Arc::new(b.finish()) as ArrayRef)
                                .collect();
                            match RecordBatch::try_new(schema.clone(), arrays) {
                                Ok(rb) => batches.push(rb),
                                Err(e) => {
                                    run_postludes(&conn, &postlude);
                                    let _ = schema_tx.send(Err(QueryError::msg(e.to_string())));
                                    return;
                                }
                            }
                        }
                        Ok(None) => break, // this result set exhausted
                        Err(e) => {
                            let err = odbc_err(e);
                            run_postludes(&conn, &postlude);
                            let _ = schema_tx.send(Err(err));
                            return;
                        }
                    }
                }

                let (next_cursor, _buffers) = match block_cursor.unbind() {
                    Ok(x) => x,
                    Err(e) => {
                        let err = odbc_err(e);
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(err));
                        return;
                    }
                };
                // Structural memory bound (see this method's doc comment):
                // the first Showplan-named set wins PERMANENTLY once found
                // (never overwritten); every other set only ever replaces
                // the single `fallback` candidate — the PREVIOUS
                // candidate's `Vec<RecordBatch>` is dropped right here.
                if is_showplan && named_match.is_none() {
                    named_match = Some((schema, batches));
                } else {
                    fallback = Some((schema, batches));
                }

                match next_cursor.more_results() {
                    Ok(Some(next)) => {
                        cursor = next;
                        continue;
                    }
                    Ok(None) => break, // no more result sets
                    Err(e) => {
                        let err = odbc_err(e);
                        run_postludes(&conn, &postlude);
                        let _ = schema_tx.send(Err(err));
                        return;
                    }
                }
            }

            // Postludes run best-effort, ALWAYS, after the walk completes
            // (design §2e step 6) — the connection drop that follows is
            // the real backstop (ODBC disconnect rolls back any still-open
            // transaction; session settings can never leak).
            run_postludes(&conn, &postlude);

            let (schema, batches) = match named_match.or(fallback) {
                Some(x) => x,
                None => {
                    let _ = schema_tx.send(Err(QueryError::msg("no result sets produced")));
                    return;
                }
            };

            let _ = schema_tx.send(Ok(schema));
            for b in batches {
                if tx.blocking_send(Ok(b)).is_err() {
                    break; // consumer gone
                }
            }
        });

        let columns = schema_rx.await.map_err(|_| QueryError::msg("driver task died"))??;
        Ok(QueryStream { columns, batches: rx })
    }
}

/// Runs every postlude statement best-effort (design §2e step 6): errors
/// (and any unexpected result-set cursor) are discarded via `let _ =` —
/// the connection drop that follows this call in every caller is the real
/// backstop (ODBC disconnect rolls back any still-open transaction, and
/// session settings set by a prelude can never leak to a reused
/// connection, since this driver never reuses connections across
/// `query_with_session` calls).
fn run_postludes(conn: &odbc_api::Connection<'_>, postlude: &[String]) {
    for p in postlude {
        let _ = conn.execute(p, (), None);
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

            let mut buffers = match wide::build(&mut cursor, BATCH_ROWS, QUERY_MAX_STR_LEN) {
                Ok((b, _ncols)) => b,
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
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
                        for (col_index, b) in builders.iter_mut().enumerate() {
                            let slice = batch.column(col_index);
                            for row_index in 0..batch.num_rows() {
                                match wide::cell_text(slice, row_index) {
                                    Some(s) => b.append_value(s),
                                    None => b.append_null(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// T2 Step 1 API-shape smoke test: `probe()` must be a plain blocking
    /// method (not `async fn`), callable from ordinary sync code with no
    /// executor — `open_config`'s `spawn_blocking` closure (T3) calls it
    /// this way. No live server is required: a connection string with an
    /// unreachable host/port simply fails fast inside `connect()`, which
    /// is itself the point — the call type-checks and returns
    /// `Result<(), QueryError>` without `.await` or a `tokio` runtime.
    #[test]
    fn probe_is_callable_from_a_non_async_fn() {
        let c = MssqlConnection::from_connection_string(
            "Driver={ODBC Driver 18 for SQL Server};Server=tcp:127.0.0.1,1;Database=x;Uid=x;Pwd=x;",
        );
        let _: Result<(), QueryError> = c.probe();
    }

    /// Security invariant (Global Constraints, "passwords"): `probe()`'s
    /// error path must never leak the password into the error text.
    /// `odbc_err` renders only the driver's diagnostic record text — this
    /// pins that a failed connect (unreachable host/port, same shape as
    /// `probe_is_callable_from_a_non_async_fn` above) never echoes back a
    /// distinctive password planted in the connection string. Non-live,
    /// fast, deterministic enough: connecting to 127.0.0.1:1 fails via
    /// "connection refused" (or an equivalent immediate driver error)
    /// well before any server-side auth exchange could occur.
    #[test]
    fn probe_error_never_contains_the_password() {
        let distinctive_password = "sUp3r$ecretZzz9000";
        let c = MssqlConnection::from_connection_string(format!(
            "Driver={{ODBC Driver 18 for SQL Server}};Server=tcp:127.0.0.1,1;Database=x;Uid=x;Pwd={distinctive_password};"
        ));
        let err = c.probe().expect_err("connecting to 127.0.0.1:1 must fail");
        assert!(
            !err.message.contains(distinctive_password),
            "probe() error text must never contain the password, got: {}",
            err.message
        );
    }
}
