use async_trait::async_trait;
use crate::{cancel::CancelToken, error::QueryError, schema::SchemaSnapshot, stream::QueryStream};

#[async_trait]
pub trait Connection: Send {
    /// Contract on the returned `QueryStream::batches` channel: once the
    /// driver sends an `Err(QueryError)` batch, it MUST stop sending any
    /// further items and drop its `Sender` so the channel closes. Consumers
    /// (e.g. the UI's query runner) rely on this to treat "stream ended
    /// after an Err" as the terminal state for that error — they do not
    /// keep draining past it. A driver that sends more `Ok` batches after
    /// an `Err`, or that fails to drop the sender, can cause a consumer
    /// that stops on first Err to leave the sender's task blocked forever
    /// on a full channel, or to interleave post-error data with an
    /// already-reported failure.
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError>;
    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError>;

    /// Executes a non-returning statement, reporting affected rows. This is
    /// the app's write path, governed by a PATTERN, not a single caller:
    /// every write reaches `execute` only through (a) a confirm modal
    /// showing the exact SQL that will run, (b) a runner-owned method with
    /// explicit transaction discipline, and (c) a read-only guard enforced
    /// at the runner choke point — the SHARED `dbc-ui`'s
    /// `runner::guard_not_read_only` for every caller below except kill,
    /// which uses its own equivalent direct read-only check so it can carry
    /// the design's mandated message text (`MONITOR_READ_ONLY_KILL_MSG`,
    /// design §0/§9.1's belt-and-braces gate — no server-side enforcement
    /// exists for kill on either engine).
    ///
    /// Sanctioned runner callers: `run_write_transaction` (sandbox Apply);
    /// the server-monitor's confirmed kill action (G9:
    /// `pg_terminate_backend` / `KILL <spid>`, confirm-dialog-gated);
    /// `run_analyze_write` (G13's ANALYZE-on-a-write sequence — its own
    /// `execute` calls are BEGIN/ROLLBACK transaction control only, over a
    /// dedicated connection; the user's write itself is dispatched via
    /// `query()`, wrapped in `EXPLAIN ANALYZE`, and ALWAYS rolled back,
    /// never committed); `run_script` (G12 script-runner write statements
    /// plus its own BEGIN/COMMIT/ROLLBACK transaction control);
    /// `run_csv_import` (G12, batched CSV `INSERT`s plus transaction
    /// control); and `connect_and_run_many` (G12 editor multi-statement —
    /// its per-statement read-only rejection is guard (c), via the shared
    /// guard). G11's backup/restore methods are also sanctioned:
    /// `run_mssql_backup` (`BACKUP DATABASE`, allowed on read-only — the
    /// ONE documented exception to guard (c), since it only reads the
    /// source database); `run_mssql_restore` (`SET SINGLE_USER` → `RESTORE
    /// DATABASE` → `SET MULTI_USER` over one dedicated connection,
    /// hard-blocked on read-only, no override — its three statements are
    /// plain sequential `execute()` calls on the SAME connection, NOT
    /// wrapped in an explicit transaction, because T-SQL does not allow
    /// `RESTORE DATABASE` inside one); and `run_sqlite_backup` (`VACUUM
    /// INTO`, allowed on read-only). `run_sqlite_restore` is NOT in this
    /// list — it never calls `execute()` at all, restoring via a plain
    /// magic-header-checked `fs::copy` instead. No other code may call
    /// this method.
    ///
    /// `run_mssql_plan` (G15 §2e, T7) is the MSSQL face of the
    /// already-sanctioned analyze-write pattern above, but is deliberately
    /// NOT added to the caller list itself: its tx control (the fused
    /// `XACT_ABORT`+`BEGIN TRANSACTION`, and the `ROLLBACK` that runs
    /// ALWAYS) travels as `MssqlConnection::query_with_session`'s
    /// prelude/postlude strings over a dedicated, driver-owned ODBC
    /// connection — never through this trait's `execute()` at all. Its
    /// discipline is the same (never commits, rollback best-effort on
    /// every path including every error branch, one dedicated connection
    /// per call), just delivered by a different mechanism because `SET
    /// SHOWPLAN_XML`/`SET STATISTICS XML` are session-scoped settings this
    /// trait's `query()` (fresh connection per call) cannot carry.
    ///
    /// Transactions are per-connection: a caller driving `BEGIN` … `COMMIT`/
    /// `ROLLBACK` MUST issue every statement in that sequence over the SAME
    /// `Connection` instance, sequentially. Implementations must never
    /// re-open or pool connections underneath `execute` — doing so would
    /// silently split a transaction across separate server-side sessions.
    ///
    /// Engine divergence callers MUST respect (T1 review issue 1,
    /// empirically verified): after a failed statement inside an open
    /// transaction, SQLite leaves the transaction open and usable, while
    /// PostgreSQL aborts it — every further statement fails with "current
    /// transaction is aborted" until ROLLBACK. A transaction driver must
    /// therefore stop at the FIRST error and roll back; it must not attempt
    /// to continue, and it must tolerate the ROLLBACK itself failing
    /// (dropping the connection aborts the transaction server-side on both
    /// engines).
    ///
    /// Session-sharing caveat (issue 2): on PostgreSQL, `query()` shares the
    /// same server session; a caller in an open transaction must not
    /// interleave `query()` calls on this instance. The Apply flow satisfies
    /// this by opening a DEDICATED connection used exclusively for its
    /// BEGIN…COMMIT sequence and dropped immediately after.
    ///
    /// The script runner's own read statements are the sanctioned exception:
    /// they run sequentially, fully drained, over this same dedicated
    /// connection inside the script's own transaction — the caveat forbids
    /// UNRELATED interleaving, not a script's own ordered statements.
    async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError>;
}
