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
    /// the app's write path — ONLY the sandbox Apply flow, the
    /// server-monitor's confirmed kill action (G9: `pg_terminate_backend` /
    /// `KILL <spid>`, confirm-dialog-gated, refused on read-only
    /// connections), the ANALYZE-on-a-write sequence (G13:
    /// `QueryRunner::run_analyze_write` — a dedicated connection, BEGIN …
    /// the user's write wrapped in `EXPLAIN ANALYZE` … ROLLBACK, ALWAYS,
    /// never COMMIT — confirm-dialog-gated, refused on read-only
    /// connections), and G11's backup/restore methods (`runner.rs`:
    /// `run_mssql_backup` — `BACKUP DATABASE`, allowed on read-only, the
    /// ONE documented exception since it only reads the source database;
    /// `run_mssql_restore` — `SET SINGLE_USER` → `RESTORE DATABASE` → `SET
    /// MULTI_USER` over one dedicated connection, hard-blocked on
    /// read-only, no override; `run_sqlite_backup` — `VACUUM INTO`, allowed
    /// on read-only) may call it. `run_sqlite_restore` is NOT in this list —
    /// it never calls `execute()` at all, restoring via a plain
    /// magic-header-checked `fs::copy` instead. Each of the above is a
    /// named, gated method issuing its own fixed, non-ad-hoc statement(s)
    /// over one dedicated connection, sequentially — NOT necessarily a
    /// BEGIN…COMMIT transaction in the sense the paragraph below describes:
    /// `run_mssql_restore`'s three statements (`SET SINGLE_USER` → `RESTORE
    /// DATABASE` → `SET MULTI_USER`) are plain sequential `execute()` calls
    /// on the SAME connection, not wrapped in an explicit transaction —
    /// T-SQL does not allow `RESTORE DATABASE` inside one.
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
    async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError>;
}
