use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringBuilder};
use dbc_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use dbc_core::{
    CancelToken, ColumnInfo, Connection, ConstraintInfo, FkRef, IndexInfo, QueryError, QueryStream,
    SchemaSnapshot, SequenceInfo, TableInfo, TableKind, BATCH_ROWS, CHANNEL_CAPACITY,
};
use duckdb::types::{Value, ValueRef};

pub struct DuckdbConnection {
    path: PathBuf,
    read_only: bool,
    /// The shared "root" connection for `path` (see [`RegistryEntry`]),
    /// bound lazily on first use via [`DuckdbConnection::get_or_init_root`]
    /// and cached for the lifetime of this instance. Holding the `Arc` keeps
    /// the underlying database open (and the process-wide [`registry`] entry
    /// alive) for as long as this `DuckdbConnection` exists, even between
    /// calls where nothing is actively running.
    root: Option<Arc<RegistryEntry>>,
    /// Lazily-cloned-from-root connection reused across [`Connection::
    /// execute`] calls so a `BEGIN … COMMIT`/`ROLLBACK` sequence runs over
    /// one underlying DuckDB session rather than being silently split across
    /// separate ones (which would drop the in-progress transaction). Taken
    /// out of the `Option` and moved into `spawn_blocking`, then put back —
    /// safe because `execute` takes `&mut self`, so there is never
    /// concurrent access. `query`/`schema` are unaffected and keep cloning a
    /// fresh session off `root` per call, as before. Mirrors
    /// `dbc-driver-sqlite::SqliteConnection::exec_conn`.
    exec_conn: Option<duckdb::Connection>,
}

impl DuckdbConnection {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), read_only: false, root: None, exec_conn: None }
    }

    /// Like [`DuckdbConnection::new`], but opens the database with DuckDB's
    /// `access_mode = READ_ONLY` config (instead of the default `AUTOMATIC`)
    /// when `read_only` is set — server-side enforcement of a connection's
    /// read-only flag, so a client-side guard bypass can't still mutate the
    /// file. Mirrors sqlite's `SQLITE_OPEN_READ_ONLY` swap in
    /// `SqliteConnection::new_with_options`. See dbc-ui's `connect::open_config`,
    /// which selects this constructor when `ConnectionConfig::read_only` is
    /// set.
    ///
    /// Mixed-mode policy: if `path` already has a root open in this process
    /// under the OTHER access mode, the first `query`/`schema`/`execute`
    /// call on this instance fails with a clear "already open in a different
    /// mode" error rather than silently reusing it — see the doc comment on
    /// [`mixed_mode_error`] for why both directions (requesting read-only
    /// against an existing read-write root, and vice versa) are refused
    /// uniformly instead of one of them being downgraded transparently.
    pub fn new_with_options(path: impl Into<PathBuf>, read_only: bool) -> Self {
        Self { path: path.into(), read_only, root: None, exec_conn: None }
    }

    /// Returns the shared root connection for `self.path`, creating (and
    /// registering) it on first use. Cached in `self.root` afterwards, so
    /// every later call on this instance — and every mixed-mode check — only
    /// happens once per instance, not once per `query`/`schema`/`execute`
    /// call.
    async fn get_or_init_root(&mut self) -> Result<Arc<RegistryEntry>, QueryError> {
        if let Some(root) = &self.root {
            return Ok(root.clone());
        }
        let path = self.path.clone();
        let read_only = self.read_only;
        let root = tokio::task::spawn_blocking(move || get_or_create_root(&path, read_only))
            .await
            .map_err(|_| QueryError::msg("driver task died"))??;
        self.root = Some(root.clone());
        Ok(root)
    }
}

fn q_err(e: duckdb::Error) -> QueryError {
    QueryError { code: None, message: e.to_string(), position: None }
}

/// `duckdb::Connection::open` mirrored with `AccessMode::ReadOnly` swapped in
/// for the default `Automatic` access mode when `read_only` is set. Called
/// exactly once per database file per process, from [`get_or_create_root`] —
/// everything else clones off the resulting root instead of calling this
/// again (see the [`RegistryEntry`] doc comment for why).
fn open_conn(path: &Path, read_only: bool) -> duckdb::Result<duckdb::Connection> {
    if read_only {
        let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
        duckdb::Connection::open_with_flags(path, config)
    } else {
        duckdb::Connection::open(path)
    }
}

/// One process-wide "root" `duckdb::Connection` per database file, shared by
/// every [`DuckdbConnection`] instance pointed at that file.
///
/// Empirically verified (DuckDB 1.10504.0, and reproduced by
/// `two_independent_opens_of_the_same_file_conflict` below): a second,
/// independent `duckdb::Connection::open()` of a path that's already open
/// ANYWHERE in this process fails outright — `AccessMode::Automatic`/
/// `ReadWrite` takes an exclusive lock even for a plain `SELECT`, so this
/// isn't limited to write conflicts. (The one exception: two connections
/// both opened with `AccessMode::ReadOnly` *do* coexist at the OS/engine
/// level — but this driver doesn't special-case that, see
/// [`mixed_mode_error`].) That breaks realistic app usage outright: a
/// second browser tab on the same file, the Apply flow's dedicated write
/// connection coexisting with the primary browse connection, or `schema()`
/// racing `query()` from two instances would all fail with a raw OS lock
/// error.
///
/// The fix: `duckdb::Connection::try_clone()` (verified against the
/// vendored source, `duckdb-1.10504.0/src/lib.rs`) calls `duckdb_connect` on
/// the SAME shared `Arc<Mutex<DatabaseHandle>>` the original connection
/// holds — it does NOT reopen the file, so it never re-takes the OS lock.
/// Every `query`/`schema`/`execute` call therefore gets its own DuckDB
/// session via `RegistryEntry::try_clone` off one shared root, instead of
/// calling `open_conn` itself. Each clone has independent transaction state
/// (proven by `transaction_isolated_between_clones_of_shared_root` below),
/// so this doesn't change any of the transaction-isolation guarantees
/// `dbc_core::Connection::execute`'s doc comment already requires.
///
/// Lifetime: the registry stores only a [`Weak`] reference; each
/// `DuckdbConnection` instance holds a strong `Arc` in `self.root` for as
/// long as it exists (see [`DuckdbConnection::get_or_init_root`]), which is
/// what actually keeps the root — and the underlying open database — alive.
/// Once the last instance pointed at a given path is dropped, the `Weak`
/// expires and [`get_or_create_root`] opens a fresh root next time (dead
/// entries are swept opportunistically on each lookup).
struct RegistryEntry {
    /// `Connection` is `!Sync` (see its doc comment in duckdb-rs), so a
    /// `std::sync::Mutex` — not just an `Arc` — is required to call
    /// `try_clone(&self)` from arbitrary blocking-pool threads.
    root: Mutex<duckdb::Connection>,
    read_only: bool,
}

impl RegistryEntry {
    fn try_clone(&self) -> duckdb::Result<duckdb::Connection> {
        let guard = self.root.lock().unwrap_or_else(|e| e.into_inner());
        guard.try_clone()
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<RegistryEntry>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<RegistryEntry>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Best-effort canonicalization for registry keys, so `"./x.db"` and
/// `"x.db"` opened from the same working directory (or any other two paths
/// that resolve to the same file) share one registry entry. Falls back
/// step-by-step when the target doesn't exist yet (a brand-new database:
/// `std::fs::canonicalize` requires the path to exist) down to the path
/// exactly as given, which is still correct for the common case where every
/// caller spells the same database path identically.
fn canonical_key(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            std::fs::canonicalize(parent).map(|p| p.join(name)).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// Translates a raw file-open failure into an actionable message with no
/// leaked process internals. DuckDB's own error text for a genuine
/// cross-process lock conflict embeds this process's exe path and PID (at
/// least on Windows: `"...The process cannot access the file because it is
/// being used by another process.\r\n\nFile is already open in\n<exe path>
/// (PID 1234)"`) — accurate for debugging, useless and a little bit of an
/// info leak for an end user. Only that specific shape is rewritten;
/// anything else (permission denied, missing directory, corrupt file, ...)
/// passes through [`q_err`] unchanged.
fn translate_open_error(e: duckdb::Error, path: &Path) -> QueryError {
    let lower = e.to_string().to_lowercase();
    if lower.contains("already open") || lower.contains("being used by another process") {
        QueryError {
            code: Some("locked".into()),
            message: format!("databázový soubor je právě používán jiným procesem: {}", path.display()),
            position: None,
        }
    } else {
        q_err(e)
    }
}

/// Mixed-mode policy: a path already open in one access mode (read-only or
/// read-write) within this process refuses a request for the OTHER mode,
/// rather than silently reusing the existing root under the requested
/// instance's nominal `read_only` flag.
///
/// This is deliberate, not just the simpler option:
/// - If the existing root is `AccessMode::ReadOnly` and this instance wants
///   read-write, there is no way to satisfy that by cloning from it — DuckDB
///   fixes access mode at the DATABASE level, so every clone off a
///   read-only root is read-only too, no matter what this driver does.
///   Erroring is the only correct choice here.
/// - If the existing root is read-write and this instance asked for
///   read-only, silently handing it a read-write clone anyway would mean
///   its writes are only blocked by dbc-ui's `is_read_statement` guard, not
///   by DuckDB itself — exactly the server-side-enforcement guarantee
///   `read_only_connection_rejects_execute_writes` exists to protect (see
///   sqlite driver's "Task 6 security review requirement" precedent). That
///   would be a SILENT downgrade from engine-enforced to app-enforced
///   read-only, which is the one thing this function must never do.
///
/// So both directions error, uniformly, with a value the caller can show
/// the user (e.g. "close the other tab/connection first") instead of one
/// direction failing loudly and the other failing silently.
fn mixed_mode_error(path: &Path, existing_read_only: bool, requested_read_only: bool) -> QueryError {
    let mode = |ro: bool| if ro { "jen pro čtení" } else { "čtení a zápis" };
    QueryError {
        code: Some("mixed-access-mode".into()),
        message: format!(
            "databáze je již otevřena v jiném režimu ({}); požadováno: {}: {}",
            mode(existing_read_only),
            mode(requested_read_only),
            path.display()
        ),
        position: None,
    }
}

/// Looks up (or lazily creates) the shared root for `path`, enforcing the
/// mixed-mode policy documented on [`mixed_mode_error`]. See the
/// [`RegistryEntry`] doc comment for the overall design this is part of.
fn get_or_create_root(path: &Path, read_only: bool) -> Result<Arc<RegistryEntry>, QueryError> {
    let key = canonical_key(path);
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    // Opportunistic cleanup: drop registry entries whose root has already
    // been torn down (last strong `Arc` dropped) rather than growing the map
    // forever across the process's lifetime.
    map.retain(|_, w| w.strong_count() > 0);

    if let Some(existing) = map.get(&key).and_then(Weak::upgrade) {
        if existing.read_only != read_only {
            return Err(mixed_mode_error(path, existing.read_only, read_only));
        }
        return Ok(existing);
    }

    let conn = open_conn(path, read_only).map_err(|e| translate_open_error(e, path))?;
    let entry = Arc::new(RegistryEntry { root: Mutex::new(conn), read_only });
    map.insert(key, Arc::downgrade(&entry));
    Ok(entry)
}

/// Formats a DuckDB `DATE32` (days since 1970-01-01) as `YYYY-MM-DD`, via
/// Howard Hinnant's `civil_from_days` — self-contained so this crate doesn't
/// need a chrono dependency (see the ground rules in the driver's task doc:
/// dbc-core, dbc-buffer, tokio, duckdb only).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn format_date32(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a count of microseconds-since-midnight as `HH:MM:SS[.ffffff]`.
fn format_time_of_day(micros: i64) -> String {
    let micros = micros.rem_euclid(86_400_000_000);
    let us = micros % 1_000_000;
    let total_secs = micros / 1_000_000;
    let s = total_secs % 60;
    let m = (total_secs / 60) % 60;
    let h = total_secs / 3600;
    if us == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{us:06}")
    }
}

fn format_timestamp(micros_since_epoch: i64) -> String {
    let days = micros_since_epoch.div_euclid(86_400_000_000);
    let time_micros = micros_since_epoch.rem_euclid(86_400_000_000);
    format!("{} {}", format_date32(days as i32), format_time_of_day(time_micros))
}

/// Converts a single cell to its text representation for the UI grid, or
/// `None` for SQL NULL. Mirrors `dbc-driver-sqlite::value_to_text`: scalar
/// types get a natural rendering (numbers via `to_string()`; dates/times/
/// timestamps reconstructed from DuckDB's epoch-based integer encodings
/// above); blobs get a size placeholder like sqlite's `<blob N B>`. Compound
/// types (LIST/STRUCT/MAP/ARRAY/UNION/INTERVAL) fall back to `ValueRef`'s
/// `Debug` rendering as a decode-error-style placeholder — accurate but not
/// pretty-printed as SQL, which is an acceptable tradeoff for a preview grid
/// cell that most query results will never hit.
fn value_to_text(v: ValueRef<'_>) -> Option<String> {
    use ValueRef::*;
    match v {
        Null => None,
        // `as_str` handles both Text and Enum (dictionary-decoded) values.
        Text(_) | Enum(..) => v.as_str().ok().map(|s| s.to_string()),
        Boolean(b) => Some(b.to_string()),
        TinyInt(i) => Some(i.to_string()),
        SmallInt(i) => Some(i.to_string()),
        Int(i) => Some(i.to_string()),
        BigInt(i) => Some(i.to_string()),
        HugeInt(i) => Some(i.to_string()),
        UTinyInt(i) => Some(i.to_string()),
        USmallInt(i) => Some(i.to_string()),
        UInt(i) => Some(i.to_string()),
        UBigInt(i) => Some(i.to_string()),
        Float(f) => Some(f.to_string()),
        Double(f) => Some(f.to_string()),
        Decimal(d) => Some(d.to_string()),
        Blob(b) => Some(format!("<blob {} B>", b.len())),
        Date32(days) => Some(format_date32(days)),
        Time64(unit, t) => Some(format_time_of_day(unit.to_micros(t))),
        Timestamp(unit, t) => Some(format_timestamp(unit.to_micros(t))),
        other => Some(format!("{other:?}")),
    }
}

/// Decodes a `duckdb_constraints()`/`duckdb_indexes()`-style LIST(VARCHAR)
/// value (e.g. `constraint_column_names`) into a plain `Vec<String>`.
/// `duckdb-rs` has no generic `FromSql` for `Vec<String>` (only `Vec<u8>` for
/// blobs), so this goes through the owned `Value::List` representation by
/// hand.
fn list_of_strings(v: ValueRef<'_>) -> Vec<String> {
    match v.to_owned() {
        Value::List(items) => items
            .into_iter()
            .map(|it| match it {
                Value::Text(s) => s,
                other => format!("{other:?}"),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Parses DuckDB's `duckdb_indexes().expressions` column: a bracketed,
/// comma-separated textual rendering of the index's key expressions (e.g.
/// `[a, b]` for plain columns, `['"weird col"', a]` when an identifier needs
/// quoting) — empirically confirmed against DuckDB 1.10504.0, since
/// `duckdb_indexes()` has no structured column-list field (unlike sqlite's
/// `PRAGMA index_info` or postgres's `pg_index.indkey`). Simple top-level
/// split on `", "`; an expression containing a literal `", "` (e.g. a
/// function call with multiple arguments) would misparse — accepted
/// limitation, same spirit as the postgres driver's expression-index
/// handling.
fn parse_index_expressions(expr: &str) -> Vec<String> {
    let trimmed = expr.trim();
    let inner = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(trimmed);
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(", ")
        .map(|tok| {
            let tok = tok.trim();
            let tok = tok.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(tok);
            let tok = tok.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(tok);
            tok.replace("\"\"", "\"")
        })
        .collect()
}

#[async_trait]
impl Connection for DuckdbConnection {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError> {
        let root = self.get_or_init_root().await?;
        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let (schema_tx, schema_rx) = tokio::sync::oneshot::channel::<Result<SchemaRef, QueryError>>();
        let sql = sql.to_owned();
        let handle = tokio::runtime::Handle::current();

        tokio::task::spawn_blocking(move || {
            // Cloning off the shared root (see the `RegistryEntry` doc
            // comment) rather than `open_conn`-ing this path again — a
            // second independent open of an already-open path fails
            // outright on this engine.
            let conn = match root.try_clone() {
                Ok(c) => c,
                Err(e) => {
                    let _ = schema_tx.send(Err(q_err(e)));
                    return;
                }
            };
            // Watcher: protocol-level interrupt when the token fires.
            // Unlike sqlite's per-statement interrupt handle, DuckDB's
            // `interrupt_handle()` is tied to the `Connection` and applies
            // to whatever statement is currently executing on it — obtained
            // up front, before `prepare`/`query`, same effect either way
            // since this connection is used for exactly one query.
            let interrupt = conn.interrupt_handle();
            let watcher_cancel = cancel.clone();
            let watcher = handle.spawn(async move {
                watcher_cancel.cancelled().await;
                interrupt.interrupt();
            });

            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => {
                    let _ = schema_tx.send(Err(q_err(e)));
                    watcher.abort();
                    return;
                }
            };

            // Unlike rusqlite, DuckDB's column metadata (`Statement::
            // column_names`) panics unless the statement has already been
            // executed at least once — so `query()` doubles as both "run
            // the statement" and "learn its shape" here. It also runs the
            // statement to completion (DuckDB materializes the Arrow result
            // before returning), so a cancel fired while this call is
            // blocking surfaces right here as an `Err`, not later in the
            // row loop below.
            let mut rows = match stmt.query([]) {
                Ok(r) => r,
                Err(e) => {
                    let err = if cancel.is_cancelled() {
                        QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
                    } else {
                        q_err(e)
                    };
                    let _ = schema_tx.send(Err(err));
                    watcher.abort();
                    return;
                }
            };

            let col_names = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
            let schema: SchemaRef = Arc::new(Schema::new(
                col_names.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect::<Vec<_>>(),
            ));
            let _ = schema_tx.send(Ok(schema.clone()));

            let ncols = col_names.len();
            let mut builders: Vec<StringBuilder> = (0..ncols).map(|_| StringBuilder::new()).collect();
            let mut in_batch = 0usize;

            let flush = |builders: &mut Vec<StringBuilder>| -> RecordBatch {
                let arrays: Vec<ArrayRef> =
                    builders.iter_mut().map(|b| Arc::new(b.finish()) as ArrayRef).collect();
                RecordBatch::try_new(schema.clone(), arrays).expect("schema matches builders")
            };

            loop {
                // Cooperative check, same rationale as sqlite's: even though
                // the query already fully ran by the time we get here (see
                // above), checking between rows keeps this loop consistent
                // with the other drivers and costs nothing.
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
                            if tx.blocking_send(Ok(flush(&mut builders))).is_err() {
                                break;
                            }
                            in_batch = 0;
                        }
                    }
                    Ok(None) => {
                        if in_batch > 0 {
                            let _ = tx.blocking_send(Ok(flush(&mut builders)));
                        }
                        break;
                    }
                    Err(e) => {
                        let err = if cancel.is_cancelled() {
                            QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
                        } else {
                            q_err(e)
                        };
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

    /// Executes a non-returning statement over a connection kept open across
    /// calls (see [`DuckdbConnection::exec_conn`]), so `BEGIN … COMMIT`/
    /// `ROLLBACK` sequences issued via successive `execute` calls run over
    /// the same DuckDB handle. `duckdb::Connection::execute` returns the
    /// changed-row count directly for DML (`0` for `BEGIN`/`COMMIT`/DDL).
    ///
    /// Engine divergence (empirically verified against DuckDB 1.10504.0, see
    /// the `mid_transaction_error_aborts_like_postgres` test below): after a
    /// failed statement inside an open transaction, DuckDB — like
    /// PostgreSQL, unlike SQLite — invalidates the transaction. Every
    /// further statement fails (DuckDB reports "current transaction is
    /// aborted" verbatim, same message text as Postgres) until `ROLLBACK`.
    /// Callers must follow the same stop-at-first-error-and-roll-back
    /// discipline the trait doc mandates for Postgres.
    ///
    /// Locking note (empirically verified against DuckDB 1.10504.0, and the
    /// reason this driver keeps a process-wide [`RegistryEntry`] per file
    /// rather than calling `duckdb::Connection::open` per call like sqlite's
    /// driver does): a second independent `open()` of a path that's already
    /// open anywhere in this process fails outright, so `exec_conn` here is
    /// a clone off the shared root (`RegistryEntry::try_clone`), not an
    /// independently-opened connection. This is consistent with — and now
    /// mechanically enforces — the trait doc's existing requirement that
    /// `execute`'s BEGIN…COMMIT sequence run over a connection whose
    /// transaction state isn't shared with `query()`/`schema()` calls on
    /// another instance: `transaction_isolated_between_clones_of_shared_root`
    /// below proves clones off the same root have independent transactions,
    /// same as separate sessions on any other engine.
    async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError> {
        if cancel.is_cancelled() {
            return Err(QueryError {
                code: Some("cancelled".into()),
                message: "query cancelled".into(),
                position: None,
            });
        }
        let root = self.get_or_init_root().await?;
        let conn = match self.exec_conn.take() {
            Some(c) => c,
            None => root.try_clone().map_err(q_err)?,
        };
        let sql = sql.to_owned();
        let (result, conn) = tokio::task::spawn_blocking(move || {
            let result = conn.execute(&sql, []).map(|n| n as u64).map_err(q_err);
            (result, conn)
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?;
        self.exec_conn = Some(conn);
        result
    }

    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        let root = self.get_or_init_root().await?;
        tokio::task::spawn_blocking(move || {
            let conn = root.try_clone().map_err(q_err)?;
            let mut tables: Vec<TableInfo> = Vec::new();
            // DuckDB catalog object ids (table_oid/view_oid) are unique
            // across object kinds, so tables and views share one lookup.
            let mut oid_idx: HashMap<i64, usize> = HashMap::new();

            // Base tables. `internal`/`temporary` exclude DuckDB's own
            // catalog/system objects and session-local TEMP tables — the
            // DuckDB analogue of the sqlite driver's `sqlite_` prefix skip
            // and the postgres driver's `SCHEMA_EXCLUDE`.
            let mut stmt = conn
                .prepare(
                    "SELECT schema_name, table_name, table_oid, sql FROM duckdb_tables() \
                     WHERE NOT internal AND NOT temporary ORDER BY schema_name, table_name",
                )
                .map_err(q_err)?;
            let rows: Vec<(String, String, i64, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (schema, name, oid, sql) in rows {
                oid_idx.insert(oid, tables.len());
                tables.push(TableInfo {
                    schema: Some(schema),
                    name,
                    kind: TableKind::Table,
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    ddl: sql,
                });
            }

            // Views: DDL comes straight from `duckdb_views().sql` (DuckDB
            // echoes the original CREATE VIEW text), unlike Postgres, which
            // has no server-side "get view DDL" and needs
            // `pg_get_viewdef` reconstruction.
            let mut stmt = conn
                .prepare(
                    "SELECT schema_name, view_name, view_oid, sql FROM duckdb_views() \
                     WHERE NOT internal AND NOT temporary ORDER BY schema_name, view_name",
                )
                .map_err(q_err)?;
            let rows: Vec<(String, String, i64, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (schema, name, oid, sql) in rows {
                oid_idx.insert(oid, tables.len());
                tables.push(TableInfo {
                    schema: Some(schema),
                    name,
                    kind: TableKind::View,
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    ddl: sql,
                });
            }

            // Columns, in `column_index` order (the `ORDER BY` below makes
            // push order == declaration order, same trick sqlite's PRAGMA
            // loop relies on via `pk` sequence numbers).
            let mut stmt = conn
                .prepare(
                    "SELECT table_oid, column_name, data_type, is_nullable, column_default \
                     FROM duckdb_columns() WHERE NOT internal ORDER BY table_oid, column_index",
                )
                .map_err(q_err)?;
            let rows: Vec<(i64, String, String, bool, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (oid, name, data_type, nullable, default) in rows {
                if let Some(&t_idx) = oid_idx.get(&oid) {
                    tables[t_idx].columns.push(ColumnInfo {
                        name,
                        data_type,
                        nullable,
                        default,
                        is_pk: false,
                        fk: None,
                    });
                }
            }

            // Constraints: PRIMARY KEY / FOREIGN KEY / UNIQUE / CHECK.
            // `duckdb_constraints()` also emits one "NOT NULL" row per
            // NOT-NULL column; skipped here since `ColumnInfo::nullable`
            // above already captures it and it isn't a named/listable
            // constraint the UI would want to show.
            //
            // Target schema for FKs is left `None`: `duckdb_constraints()`
            // exposes `referenced_table` but no `referenced_schema`, and
            // guessing "same schema as the referencing table" would be
            // faking data DuckDB doesn't actually give us (unlike sqlite,
            // where there IS no cross-schema concept, so `None` there is
            // simply correct rather than a guess).
            let mut stmt = conn
                .prepare(
                    "SELECT table_oid, constraint_type, constraint_text, constraint_column_names, \
                            constraint_name, referenced_table, referenced_column_names \
                     FROM duckdb_constraints()",
                )
                .map_err(q_err)?;
            let mut rows = stmt.query([]).map_err(q_err)?;
            while let Some(row) = rows.next().map_err(q_err)? {
                let oid: i64 = row.get(0).map_err(q_err)?;
                let Some(&t_idx) = oid_idx.get(&oid) else { continue };
                let ctype: String = row.get(1).map_err(q_err)?;
                let ctext: String = row.get(2).map_err(q_err)?;
                let cols = list_of_strings(row.get_ref(3).map_err(q_err)?);
                let cname: Option<String> = row.get(4).map_err(q_err)?;
                let rtable: Option<String> = row.get(5).map_err(q_err)?;
                let rcols = list_of_strings(row.get_ref(6).map_err(q_err)?);

                match ctype.as_str() {
                    "NOT NULL" => continue,
                    "PRIMARY KEY" => {
                        for col_name in &cols {
                            if let Some(c) =
                                tables[t_idx].columns.iter_mut().find(|c| &c.name == col_name)
                            {
                                c.is_pk = true;
                            }
                        }
                        tables[t_idx].constraints.push(ConstraintInfo {
                            name: cname.unwrap_or_default(),
                            kind: "PRIMARY KEY".to_string(),
                            definition: ctext,
                        });
                    }
                    "FOREIGN KEY" => {
                        if let Some(rtable) = &rtable {
                            for (col_name, r_col) in cols.iter().zip(rcols.iter()) {
                                if let Some(c) =
                                    tables[t_idx].columns.iter_mut().find(|c| &c.name == col_name)
                                {
                                    c.fk = Some(FkRef {
                                        schema: None,
                                        table: rtable.clone(),
                                        column: r_col.clone(),
                                    });
                                }
                            }
                        }
                        tables[t_idx].constraints.push(ConstraintInfo {
                            name: cname.unwrap_or_default(),
                            kind: "FOREIGN KEY".to_string(),
                            definition: ctext,
                        });
                    }
                    other => {
                        tables[t_idx].constraints.push(ConstraintInfo {
                            name: cname.unwrap_or_default(),
                            kind: other.to_string(),
                            definition: ctext,
                        });
                    }
                }
            }

            // Secondary indexes. `duckdb_indexes()` has no structured
            // column-list field (only `sql` and a textual `expressions`
            // rendering) and never lists the hidden index backing a PRIMARY
            // KEY (see `parse_index_expressions` doc comment and
            // `is_primary` in DuckDB's own docs, "always false") — so no
            // PK-backing-index filter is needed here, unlike postgres's
            // `NOT i.indisprimary`.
            let mut stmt = conn
                .prepare("SELECT table_oid, index_name, is_unique, expressions FROM duckdb_indexes()")
                .map_err(q_err)?;
            let rows: Vec<(i64, String, bool, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;
            for (oid, name, unique, expr) in rows {
                if let Some(&t_idx) = oid_idx.get(&oid) {
                    let columns = expr.as_deref().map(parse_index_expressions).unwrap_or_default();
                    tables[t_idx].indexes.push(IndexInfo { name, columns, unique });
                }
            }

            // Sequences: schema-level objects, not attached to any table.
            let mut stmt = conn
                .prepare(
                    "SELECT schema_name, sequence_name FROM duckdb_sequences() WHERE NOT temporary \
                     ORDER BY schema_name, sequence_name",
                )
                .map_err(q_err)?;
            let sequences: Vec<SequenceInfo> = stmt
                .query_map([], |r| Ok(SequenceInfo { schema: Some(r.get(0)?), name: r.get(1)? }))
                .map_err(q_err)?
                .collect::<duckdb::Result<Vec<_>>>()
                .map_err(q_err)?;

            // DuckDB has no `CREATE TRIGGER` support at all, and routine/
            // macro introspection (`duckdb_functions()`, `CREATE MACRO`) is
            // not implemented — left empty rather than faking data, per the
            // task's "leave genuinely-unavailable collections empty" rule.
            Ok(SchemaSnapshot { tables, routines: Vec::new(), triggers: Vec::new(), sequences })
        })
        .await
        .map_err(|_| QueryError::msg("driver task died"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{CancelToken, Connection};

    fn fixture_db() -> tempfile::TempPath {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        // DuckDB refuses to open a path that already exists as an empty
        // file with unexpected content in some versions; remove it first so
        // `Connection::open` creates a fresh database at that path.
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER, name TEXT);").unwrap();
        let mut stmt = conn.prepare("INSERT INTO t(id, name) VALUES (?, ?)").unwrap();
        for value in 1..=5000i64 {
            stmt.execute(duckdb::params![value, format!("n{value}")]).unwrap();
        }
        path
    }

    #[tokio::test]
    async fn streams_all_rows_in_batches() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
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
    async fn null_values_round_trip_as_none() {
        let f = fixture_db();
        let conn = duckdb::Connection::open(&*f).unwrap();
        conn.execute_batch("INSERT INTO t(id, name) VALUES (9999, NULL)").unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&*f);
        let mut s = c.query("SELECT name FROM t WHERE id = 9999", CancelToken::new()).await.unwrap();
        use dbc_core::arrow::array::Array;
        let mut saw_null = false;
        while let Some(b) = s.batches.recv().await {
            let b = b.unwrap();
            let col = b.column(0).as_any().downcast_ref::<dbc_core::arrow::array::StringArray>().unwrap();
            for i in 0..col.len() {
                if col.is_null(i) {
                    saw_null = true;
                }
            }
        }
        assert!(saw_null, "expected a NULL name cell");
    }

    #[tokio::test]
    async fn sql_error_is_a_value() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let err = match c.query("SELECT * FROM missing_table", CancelToken::new()).await {
            Ok(_) => panic!("expected an error querying a missing table"),
            Err(e) => e,
        };
        assert!(err.message.to_lowercase().contains("missing_table"));
    }

    #[tokio::test]
    async fn cancel_interrupts_long_query() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let cancel = CancelToken::new();
        let watcher_cancel = cancel.clone();
        // DuckDB runs `query()` to completion before returning control (see
        // the `Connection::query` doc comment), so cancellation has to land
        // WHILE that call is still in flight rather than after — spawn the
        // cancel from a concurrent task instead of calling it inline like
        // sqlite's equivalent test does.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            watcher_cancel.cancel();
        });
        // Triple cross join of a 5000-row table = 125e9 row evaluations —
        // CPU-bound enough to not finish within the 200ms head start above
        // even under heavy parallel-test contention. `count(*)` rather than
        // selecting a column: DuckDB's `query()` materializes its full
        // result before this driver can stream from it (see the `query`
        // doc comment above), so selecting `a.id` here would try to
        // materialize on the order of a terabyte of output before ever
        // reaching the interrupt check — `count(*)` does the same
        // combinatorial CPU work but returns exactly one row, regardless of
        // whether the interrupt lands in time.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            c.query("SELECT count(*) FROM t a, t b, t c", cancel),
        )
        .await
        .expect("query was not interrupted within 30s");

        let err = match result {
            Ok(_) => panic!("expected the interrupted query to fail"),
            Err(e) => e,
        };
        assert_eq!(err.code.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn schema_lists_tables_and_columns() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new(&*f);
        let snap = c.schema().await.unwrap();
        let t = snap.tables.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[0].name, "id");
    }

    #[tokio::test]
    async fn schema_handles_reserved_word_and_spaced_table_names() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"order\" (id INTEGER, total DOUBLE);
             CREATE TABLE \"weird name\" (a TEXT, b TEXT);",
        )
        .unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&path);
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

    #[tokio::test]
    async fn read_only_connection_allows_select() {
        let f = fixture_db();
        let mut c = DuckdbConnection::new_with_options(&*f, true);
        let mut s = c
            .query("SELECT id FROM t ORDER BY id LIMIT 1", CancelToken::new())
            .await
            .unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn read_only_connection_rejects_writes() {
        // Server-side enforcement, mirroring sqlite's
        // `read_only_connection_rejects_writes`: `AccessMode::ReadOnly` must
        // reject a write regardless of any client-side `is_read_statement`
        // guard.
        let f = fixture_db();
        let mut c = DuckdbConnection::new_with_options(&*f, true);
        let mut s = match c.query("INSERT INTO t(id, name) VALUES (9999, 'x')", CancelToken::new()).await {
            Ok(s) => s,
            Err(e) => {
                assert!(e.message.to_lowercase().contains("read"), "expected a read-only error, got: {}", e.message);
                return;
            }
        };
        let mut saw_error = false;
        while let Some(item) = s.batches.recv().await {
            if let Err(e) = item {
                assert!(e.message.to_lowercase().contains("read"), "expected a read-only error, got: {}", e.message);
                saw_error = true;
            }
        }
        assert!(saw_error, "expected the write to be rejected by the read-only connection");
    }

    #[tokio::test]
    async fn read_only_connection_rejects_execute_writes() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut w = DuckdbConnection::new(&path);
            w.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        }
        let mut c = DuckdbConnection::new_with_options(&path, true);
        let err = c
            .execute("INSERT INTO t(id) VALUES (1)", CancelToken::new())
            .await
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("readonly") || msg.contains("read-only") || msg.contains("read only"),
            "expected a read-only rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn full_catalog() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE customers(id INTEGER PRIMARY KEY, name VARCHAR NOT NULL DEFAULT 'x');
             CREATE TABLE orders(id INTEGER PRIMARY KEY, cid INTEGER NOT NULL REFERENCES customers(id));
             CREATE INDEX idx_orders_cid ON orders(cid);
             CREATE VIEW v_orders AS SELECT id FROM orders;
             CREATE SEQUENCE seq_test;",
        )
        .unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&path);
        let snap = c.schema().await.unwrap();

        assert!(snap.tables.len() >= 2, "expected at least 2 tables, got {}", snap.tables.len());

        let customers = snap.tables.iter().find(|t| t.name == "customers").unwrap();
        assert_eq!(customers.kind, TableKind::Table);
        assert_eq!(customers.columns.len(), 2);
        let id_col = customers.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id_col.is_pk);
        let name_col = customers.columns.iter().find(|c| c.name == "name").unwrap();
        assert!(!name_col.nullable);
        assert!(customers.ddl.is_some());
        assert!(customers.ddl.as_ref().unwrap().to_uppercase().contains("CREATE TABLE"));

        let orders = snap.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.kind, TableKind::Table);
        let cid_col = orders.columns.iter().find(|c| c.name == "cid").unwrap();
        assert!(!cid_col.nullable);
        assert!(cid_col.fk.is_some());
        let fk = cid_col.fk.as_ref().unwrap();
        assert_eq!(fk.table, "customers");
        assert_eq!(fk.column, "id");

        assert!(!orders.indexes.is_empty());
        let idx = orders.indexes.iter().find(|i| i.name == "idx_orders_cid").unwrap();
        assert_eq!(idx.columns, vec!["cid"]);
        assert!(!idx.unique);

        let v_orders = snap.tables.iter().find(|t| t.name == "v_orders").unwrap();
        assert_eq!(v_orders.kind, TableKind::View);
        assert_eq!(v_orders.columns.len(), 1);
        assert!(v_orders.ddl.is_some());
        assert!(v_orders.ddl.as_ref().unwrap().to_uppercase().contains("CREATE VIEW"));

        assert!(snap.sequences.iter().any(|s| s.name == "seq_test"));
    }

    #[tokio::test]
    async fn execute_reports_affected_rows() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let mut c = DuckdbConnection::new(&path);

        let n = c
            .execute("CREATE TABLE t(id INTEGER, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 0);

        let n = c
            .execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 1);
        let n = c
            .execute("INSERT INTO t(id, name) VALUES (2, 'b')", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 1);

        let n = c.execute("UPDATE t SET name = 'z'", CancelToken::new()).await.unwrap();
        assert_eq!(n, 2);

        let n = c
            .execute("DELETE FROM t WHERE id = 9999", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn execute_in_transaction_rolls_back() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut c = DuckdbConnection::new(&path);
            c.execute("CREATE TABLE t(id INTEGER, name TEXT)", CancelToken::new()).await.unwrap();

            c.execute("BEGIN", CancelToken::new()).await.unwrap();
            c.execute("INSERT INTO t(id, name) VALUES (1, 'a')", CancelToken::new()).await.unwrap();
            c.execute("ROLLBACK", CancelToken::new()).await.unwrap();
            // `c` (and its persistent `exec_conn`) is dropped at the end of
            // this block. DuckDB holds an exclusive file lock for as long as
            // any `Connection` to the file is open (see the locking note on
            // `Connection::execute` above), so the verification query below
            // needs a connection of its own, opened after this one closes.
        }

        let mut verify = DuckdbConnection::new(&path);
        // The insert must not be visible — same underlying connection must
        // have been used for BEGIN/INSERT/ROLLBACK for the rollback to take
        // effect; a fresh connection per call would have auto-committed the
        // INSERT before ROLLBACK ever ran.
        let mut s = verify.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 0, "row inserted inside the rolled-back transaction must be absent");
    }

    #[tokio::test]
    async fn execute_in_transaction_commits() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut c = DuckdbConnection::new(&path);
            c.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();

            c.execute("BEGIN", CancelToken::new()).await.unwrap();
            c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();
            c.execute("COMMIT", CancelToken::new()).await.unwrap();
        }

        let mut verify = DuckdbConnection::new(&path);
        let mut s = verify.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 1);
    }

    /// T1-review-style engine divergence proof (see the `execute` doc
    /// comment on the trait impl above): unlike SQLite, DuckDB aborts an
    /// open transaction on the first error — every subsequent statement in
    /// that transaction fails until `ROLLBACK`, matching Postgres's
    /// behavior rather than SQLite's "transaction stays open and usable"
    /// one. Empirically confirmed against DuckDB 1.10504.0.
    #[tokio::test]
    async fn mid_transaction_error_aborts_like_postgres() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let mut c = DuckdbConnection::new(&path);
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();

        c.execute("BEGIN", CancelToken::new()).await.unwrap();
        c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();

        // Duplicate primary key: the first error inside the transaction.
        let first_error = c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await;
        assert!(first_error.is_err(), "expected the duplicate-key insert to fail");

        // A perfectly valid statement, issued on the SAME connection right
        // after the error: DuckDB rejects it too, because the transaction
        // itself is now invalidated (Postgres-style), unlike sqlite where
        // this would succeed.
        let after_error = c.execute("INSERT INTO t(id) VALUES (2)", CancelToken::new()).await;
        let err = after_error.expect_err("DuckDB must abort the transaction after the first error");
        assert!(
            err.message.to_lowercase().contains("transaction"),
            "expected a transaction-aborted style error, got: {}",
            err.message
        );

        // Rollback still works and leaves the connection usable afterwards.
        c.execute("ROLLBACK", CancelToken::new()).await.unwrap();
        let n = c.execute("INSERT INTO t(id) VALUES (3)", CancelToken::new()).await.unwrap();
        assert_eq!(n, 1);
    }

    // --- Registry / locking architecture (review round 1) ---
    //
    // Reviewer-found scenarios that broke realistic app usage under the old
    // "every call independently `duckdb::Connection::open()`s the file"
    // design: (a) query() on an instance with a live exec_conn, (b) the
    // Apply flow's dedicated write connection coexisting with the primary
    // browse connection, (c) two plain SELECTs from two instances (two
    // tabs), (d) schema() racing query() across instances. All four are
    // exactly "N `DuckdbConnection` instances, same path, at least one of
    // them holding a connection open" — proven fixed below by exercising
    // each shape directly against the shared-root registry.

    /// Empirically verified finding this whole registry exists to encode:
    /// `RegistryEntry::try_clone` sessions are isolated from each other's
    /// open transactions exactly like separate sessions on any other engine
    /// (Postgres-style read-committed-ish visibility), NOT like re-entering
    /// the same session. Also covers reviewer scenario (b): a "writer"
    /// instance (standing in for the Apply flow's dedicated connection) and
    /// a "reader" instance (the primary browse connection) coexist on the
    /// same file without either failing to open.
    #[tokio::test]
    async fn transaction_isolated_between_clones_of_shared_root() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();

        let mut writer = DuckdbConnection::new(&path);
        writer.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        writer.execute("BEGIN", CancelToken::new()).await.unwrap();
        writer.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();

        let mut reader = DuckdbConnection::new(&path);
        let mut s = reader.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 0, "uncommitted row must not be visible from a different instance/session");

        writer.execute("COMMIT", CancelToken::new()).await.unwrap();

        let mut s = reader.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(b) = s.batches.recv().await {
            rows += b.unwrap().num_rows();
        }
        assert_eq!(rows, 1, "committed row must become visible to the other instance");
    }

    /// Reviewer scenario (a): `query()` on the SAME instance whose own
    /// `exec_conn` is still open mid-transaction must not fail with a file
    /// lock error — it clones off the same shared root exec_conn came from.
    #[tokio::test]
    async fn query_on_same_instance_succeeds_while_exec_conn_open() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();

        let mut c = DuckdbConnection::new(&path);
        c.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        c.execute("BEGIN", CancelToken::new()).await.unwrap();
        c.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();

        // Must not error just because exec_conn is still open.
        let mut s = c.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        while let Some(b) = s.batches.recv().await {
            b.unwrap();
        }

        c.execute("COMMIT", CancelToken::new()).await.unwrap();
    }

    /// Reviewer scenario (c): two plain SELECTs from two instances (two
    /// tabs) on the same file, neither ever calling `execute()`. Failed
    /// under the old design even for reads, because the default
    /// `AccessMode::Automatic` root takes an exclusive lock regardless.
    #[tokio::test]
    async fn two_plain_selects_from_two_instances_coexist() {
        let f = fixture_db();
        let mut tab_a = DuckdbConnection::new(&*f);
        let mut tab_b = DuckdbConnection::new(&*f);

        let mut sa = tab_a.query("SELECT id FROM t ORDER BY id LIMIT 1", CancelToken::new()).await.unwrap();
        let mut sb = tab_b.query("SELECT id FROM t ORDER BY id DESC LIMIT 1", CancelToken::new()).await.unwrap();

        let mut rows_a = 0usize;
        while let Some(b) = sa.batches.recv().await {
            rows_a += b.unwrap().num_rows();
        }
        let mut rows_b = 0usize;
        while let Some(b) = sb.batches.recv().await {
            rows_b += b.unwrap().num_rows();
        }
        assert_eq!(rows_a, 1);
        assert_eq!(rows_b, 1);
    }

    /// Reviewer scenario (d): `schema()` on one instance racing `query()` on
    /// another, same file.
    #[tokio::test]
    async fn schema_races_query_across_instances() {
        let f = fixture_db();
        let mut browser = DuckdbConnection::new(&*f);
        let mut inspector = DuckdbConnection::new(&*f);

        let (query_result, schema_result) = tokio::join!(
            browser.query("SELECT id FROM t LIMIT 1", CancelToken::new()),
            inspector.schema()
        );
        let mut s = query_result.unwrap();
        while let Some(b) = s.batches.recv().await {
            b.unwrap();
        }
        let snap = schema_result.unwrap();
        assert!(snap.tables.iter().any(|t| t.name == "t"));
    }

    /// Mixed-mode policy proof (see `mixed_mode_error`'s doc comment for the
    /// reasoning): a read-only instance and a read-write instance pointed at
    /// the SAME path within this process must not silently share a root —
    /// the later one gets a clear, actionable error instead.
    #[tokio::test]
    async fn mixed_access_mode_is_rejected() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        {
            let mut w = DuckdbConnection::new(&path);
            w.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        }

        let mut rw = DuckdbConnection::new(&path);
        rw.execute("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await.unwrap();

        // A read-only instance on the SAME path, while `rw`'s root is still
        // alive, must be rejected rather than silently downgraded.
        let mut ro = DuckdbConnection::new_with_options(&path, true);
        let err = ro.query("SELECT id FROM t", CancelToken::new()).await.unwrap_err();
        assert_eq!(err.code.as_deref(), Some("mixed-access-mode"));
        assert!(err.message.contains("jiném režimu"), "expected the Czech mixed-mode message, got: {}", err.message);
    }

    /// Direct reproduction of the raw OS lock conflict this whole registry
    /// exists to route around (bypassing the registry deliberately, via raw
    /// `duckdb::Connection::open` calls, exactly like the old per-call
    /// `open_conn` design did) — proving `translate_open_error` turns
    /// DuckDB's PID/exe-path-bearing message into the clean Czech one.
    #[tokio::test]
    async fn two_independent_opens_of_the_same_file_conflict_is_translated() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();

        let _first = duckdb::Connection::open(&path).unwrap();
        let second = duckdb::Connection::open(&path);
        let raw_err = match second {
            Ok(_) => {
                // Some platforms/filesystems may tolerate this; nothing to
                // translate if DuckDB itself didn't conflict.
                return;
            }
            Err(e) => e,
        };
        let raw_msg = raw_err.to_string();
        let translated = translate_open_error(raw_err, &path);
        assert_eq!(translated.code.as_deref(), Some("locked"));
        assert!(translated.message.contains("jiným procesem"));
        assert!(!translated.message.to_lowercase().contains("pid"), "translated message must not leak the raw OS text: {raw_msg}");
    }

    // --- Value-type rendering (review round 1: regression protection for
    // civil_from_days and friends, cross-checked against Python). ---

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-25567), (1900, 1, 1));
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn format_date32_renders_iso() {
        assert_eq!(format_date32(0), "1970-01-01");
        assert_eq!(format_date32(-25567), "1900-01-01");
        assert_eq!(format_date32(19723), "2024-01-01");
    }

    #[test]
    fn format_time_of_day_renders_hms() {
        assert_eq!(format_time_of_day(0), "00:00:00");
        assert_eq!(format_time_of_day(49_530_000_000), "13:45:30");
        assert_eq!(format_time_of_day(49_530_123_456), "13:45:30.123456");
    }

    #[test]
    fn format_timestamp_handles_negative_epoch() {
        // 1969-12-31 23:59:59 — one second before the epoch.
        assert_eq!(format_timestamp(-1_000_000), "1969-12-31 23:59:59");
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn parse_index_expressions_handles_quoted_and_empty() {
        assert_eq!(parse_index_expressions("[a, b]"), vec!["a", "b"]);
        assert_eq!(parse_index_expressions("['\"weird col\"', a]"), vec!["weird col", "a"]);
        assert_eq!(parse_index_expressions("[]"), Vec::<String>::new());
        assert_eq!(parse_index_expressions(""), Vec::<String>::new());
    }

    #[tokio::test]
    async fn value_type_rendering_for_date_time_timestamp_decimal_blob_hugeint() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.into_temp_path();
        std::fs::remove_file(&path).ok();
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE types_test (
                 d DATE, t TIME, ts TIMESTAMP, dec DECIMAL(10,2), b BLOB, h HUGEINT
             );
             INSERT INTO types_test VALUES (
                 '2024-01-15', '13:45:30', '2024-01-15 13:45:30.5', 123.45,
                 'hello'::BLOB, 100000000000000000000
             );",
        )
        .unwrap();
        drop(conn);

        let mut c = DuckdbConnection::new(&path);
        let mut s = c.query("SELECT d, t, ts, dec, b, h FROM types_test", CancelToken::new()).await.unwrap();
        use dbc_core::arrow::array::{Array, StringArray};

        let mut batch_opt = None;
        while let Some(b) = s.batches.recv().await {
            batch_opt = Some(b.unwrap());
        }
        let batch = batch_opt.expect("expected one row back");
        assert_eq!(batch.num_rows(), 1);

        let col = |i: usize| -> String {
            batch.column(i).as_any().downcast_ref::<StringArray>().unwrap().value(0).to_string()
        };
        assert_eq!(col(0), "2024-01-15");
        assert_eq!(col(1), "13:45:30");
        assert_eq!(col(2), "2024-01-15 13:45:30.500000");
        assert_eq!(col(3), "123.45");
        assert_eq!(col(4), "<blob 5 B>");
        assert_eq!(col(5), "100000000000000000000");
    }
}
