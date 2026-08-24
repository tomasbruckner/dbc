//! G9: engine-specific monitor SQL. Every SELECT list is written so every
//! value the client needs is already a number or text (durations as
//! `extract(epoch ...)::float8` / `DATEDIFF(SECOND, ...)`) — never a
//! native timestamp the client would have to parse (design §2 parse
//! strategy). Column ORDER is monitor.rs's parse contract — change both
//! together.

pub mod pg {
    /// `[active, idle, max_conn]`
    pub const CONNECTIONS: &str = "\
SELECT
  count(*) FILTER (WHERE state = 'active') AS active,
  count(*) FILTER (WHERE state = 'idle')   AS idle,
  current_setting('max_connections')::int  AS max_conn
FROM pg_stat_activity WHERE datname = current_database()";

    /// `[waiting, deadlocks_since_reset]`
    pub const LOCKS: &str = "\
SELECT
  (SELECT count(*) FROM pg_locks WHERE NOT granted) AS waiting,
  (SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()) AS deadlocks_since_reset";

    /// `[data_bytes]` — split from WAL_SIZE so a pg_ls_waldir permission
    /// error can't take the data half down with it (design §2 caveat).
    pub const DATA_SIZE: &str = "SELECT pg_database_size(current_database()) AS data_bytes";

    /// `[wal_bytes]` — requires pg_monitor/superuser; failure degrades the
    /// WAL half of the tile to "n/a" (design §2 caveat).
    pub const WAL_SIZE: &str =
        "SELECT coalesce(sum(size), 0)::bigint AS wal_bytes FROM pg_ls_waldir()";

    /// `[cache_hit_pct, uptime_secs, xact_total]` — xact_total is CUMULATIVE;
    /// the client computes the TPS delta (monitor::compute_rate).
    pub const PERF: &str = "\
SELECT
  round(100.0 * sum(blks_hit) / NULLIF(sum(blks_hit) + sum(blks_read), 0), 2) AS cache_hit_pct,
  extract(epoch FROM now() - pg_postmaster_start_time())::bigint AS uptime_secs,
  sum(xact_commit + xact_rollback) AS xact_total
FROM pg_stat_database";

    /// `[pid, user, application, client, state, duration_secs, query]` —
    /// excludes the monitor's own session (design §2).
    pub const RUNNING: &str = "\
SELECT pid, usename AS \"user\", application_name AS application, client_addr::text AS client,
       state, extract(epoch FROM now() - query_start)::float8 AS duration_secs, query
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid()
ORDER BY duration_secs DESC NULLS LAST
LIMIT 200";

    /// `[waiter_pid, blocker_pid, wait_secs, waiter_query, blocker_query]`
    /// — the standard "who blocks whom" pg_locks self-join (design §2).
    pub const BLOCKING: &str = "\
SELECT blocked_activity.pid AS waiter_pid, blocking_activity.pid AS blocker_pid,
       extract(epoch FROM now() - blocked_activity.query_start)::float8 AS wait_secs,
       blocked_activity.query AS waiter_query, blocking_activity.query AS blocker_query
FROM pg_locks blocked
JOIN pg_stat_activity blocked_activity ON blocked_activity.pid = blocked.pid
JOIN pg_locks blocking
  ON blocking.locktype IS NOT DISTINCT FROM blocked.locktype
 AND blocking.database  IS NOT DISTINCT FROM blocked.database
 AND blocking.relation  IS NOT DISTINCT FROM blocked.relation
 AND blocking.page      IS NOT DISTINCT FROM blocked.page
 AND blocking.tuple     IS NOT DISTINCT FROM blocked.tuple
 AND blocking.transactionid IS NOT DISTINCT FROM blocked.transactionid
 AND blocking.classid   IS NOT DISTINCT FROM blocked.classid
 AND blocking.objid     IS NOT DISTINCT FROM blocked.objid
 AND blocking.objsubid  IS NOT DISTINCT FROM blocked.objsubid
 AND blocking.pid <> blocked.pid
JOIN pg_stat_activity blocking_activity ON blocking_activity.pid = blocking.pid
WHERE NOT blocked.granted AND blocking.granted";

    /// `[schema, table, data_bytes, index_bytes, toast_bytes, row_estimate]`
    pub const TABLES: &str = "\
SELECT n.nspname AS schema, c.relname AS \"table\",
       pg_relation_size(c.oid) AS data_bytes,
       pg_indexes_size(c.oid) AS index_bytes,
       CASE WHEN c.reltoastrelid <> 0 THEN pg_relation_size(c.reltoastrelid) ELSE 0 END AS toast_bytes,
       c.reltuples::bigint AS row_estimate
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
ORDER BY pg_relation_size(c.oid) + pg_indexes_size(c.oid) DESC";
}

/// The live MSSQL monitor refresh set (design §3), 11 statements —
/// `runner::run_monitor_refresh`'s Mssql arm (G15 T6) drives every one of
/// these via `drain_rows` and threads the results through
/// `monitor::merge_mssql_connections`/`merge_mssql_locks`/
/// `split_mssql_size`/`merge_mssql_perf` into the pg-shaped tiles
/// `assemble_snapshot` already knows how to degrade per-tile.
///
/// Correction (G15 T6): this module doc used to say these constants were
/// "NOT runnable — no driver exists" and carried a PERMANENT `dead_code`
/// allow. Both are obsolete — `dbc-driver-mssql` landed (T2/T3) and every
/// constant here is now called from real, complete code (this crate's
/// `#[allow(dead_code)]` is gone). What's still gated is reachability, not
/// code existence: `monitor::monitor_available` returns `false` for Mssql
/// until T8's flip (gated on the XACT_ABORT matrix running green on a real
/// machine, Global Constraints) — so `open_monitor` never dispatches a
/// refresh against an MSSQL connection until then, but the SQL/merge code
/// itself is exactly what will run the moment it does.
pub mod mssql {
    pub const CONNECTIONS: &str = "\
SELECT
  SUM(CASE WHEN status = 'running'  THEN 1 ELSE 0 END) AS active,
  SUM(CASE WHEN status = 'sleeping' THEN 1 ELSE 0 END) AS idle
FROM sys.dm_exec_sessions WHERE is_user_process = 1";

    /// value_in_use = 0 means "unlimited/dynamic" -> UI max = None.
    pub const CONNECTIONS_MAX: &str =
        "SELECT value_in_use AS max_conn FROM sys.configurations WHERE name = 'user connections'";

    pub const LOCKS_WAITING: &str =
        "SELECT COUNT(*) AS waiting FROM sys.dm_tran_locks WHERE request_status = 'WAIT'";

    /// Misleading name: despite "/sec" this is a CUMULATIVE counter (design §3).
    pub const DEADLOCKS: &str = "\
SELECT cntr_value AS deadlocks_since_reset FROM sys.dm_os_performance_counters
WHERE counter_name = 'Number of Deadlocks/sec' AND instance_name = '_Total'";

    /// Data vs log split — the sp_spaceused equivalent (design §3).
    pub const SIZE: &str = "\
SELECT
  SUM(CASE WHEN type_desc = 'ROWS' THEN size ELSE 0 END) * 8 * 1024 AS data_bytes,
  SUM(CASE WHEN type_desc = 'LOG'  THEN size ELSE 0 END) * 8 * 1024 AS log_bytes
FROM sys.database_files";

    pub const CACHE_HIT: &str = "\
SELECT (a.cntr_value * 1.0 / NULLIF(b.cntr_value, 0)) * 100 AS cache_hit_pct
FROM sys.dm_os_performance_counters a, sys.dm_os_performance_counters b
WHERE a.counter_name = 'Buffer cache hit ratio' AND b.counter_name = 'Buffer cache hit ratio base'";

    pub const UPTIME: &str =
        "SELECT DATEDIFF(SECOND, sqlserver_start_time, GETDATE()) AS uptime_secs FROM sys.dm_os_sys_info";

    /// Cumulative; client-side delta, same as pg's xact_total.
    pub const XACT_TOTAL: &str = "\
SELECT cntr_value AS xact_total FROM sys.dm_os_performance_counters
WHERE counter_name = 'Transactions/sec' AND instance_name = '_Total'";

    /// TOP 200 mirrors the pg LIMIT (design §8 perf caveat).
    pub const RUNNING: &str = "\
SELECT TOP 200 r.session_id AS pid, s.login_name AS [user], s.program_name AS application,
       s.host_name AS client, r.status AS state,
       DATEDIFF(SECOND, r.start_time, GETDATE()) AS duration_secs, t.text AS query
FROM sys.dm_exec_requests r
JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t
WHERE r.session_id <> @@SPID
ORDER BY duration_secs DESC";

    /// blocker_query is NULL when the blocker is idle-in-transaction (no
    /// active request row) — inherent to the DMV, not a bug (design §3).
    pub const BLOCKING: &str = "\
SELECT r.session_id AS waiter_pid, r.blocking_session_id AS blocker_pid,
       r.wait_time / 1000.0 AS wait_secs, tw.text AS waiter_query, tb.text AS blocker_query
FROM sys.dm_exec_requests r
CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) tw
OUTER APPLY (SELECT sql_handle FROM sys.dm_exec_requests br WHERE br.session_id = r.blocking_session_id) bx
OUTER APPLY sys.dm_exec_sql_text(bx.sql_handle) tb
WHERE r.blocking_session_id <> 0";

    /// Set-based sp_spaceused equivalent, no per-table EXEC loop (design §3).
    ///
    /// G15 T6 smoke-test fix (live docker MSSQL, not just this crate's
    /// lexical `every_mssql_monitor_query_starts_with_select` check, which
    /// can't catch a semantic T-SQL error): the `ORDER BY` used to
    /// reference the `data_bytes`/`index_bytes` SELECT-list ALIASES inside
    /// an arithmetic expression (`ORDER BY data_bytes + index_bytes DESC`)
    /// — T-SQL allows a bare alias in `ORDER BY` but NOT an alias inside a
    /// larger expression combined with `GROUP BY`, and failed live with
    /// "Msg 207 ... Invalid column name 'data_bytes'"/`'index_bytes'`.
    /// Fixed by repeating the underlying aggregate expressions in
    /// `ORDER BY` instead of referencing the aliases — verified live
    /// against a real (dockerized) SQL Server 2022 instance, including
    /// with an actual user table present.
    pub const TABLES: &str = "\
SELECT OBJECT_SCHEMA_NAME(ps.object_id) AS [schema], OBJECT_NAME(ps.object_id) AS [table],
       SUM(CASE WHEN ps.index_id IN (0,1) THEN ps.in_row_data_page_count ELSE 0 END) * 8 * 1024 AS data_bytes,
       SUM(CASE WHEN ps.index_id > 1 THEN ps.used_page_count ELSE 0 END) * 8 * 1024 AS index_bytes,
       SUM(ps.lob_used_page_count + ps.row_overflow_used_page_count) * 8 * 1024 AS toast_bytes,
       MAX(CASE WHEN ps.index_id IN (0,1) THEN ps.row_count ELSE 0 END) AS row_estimate
FROM sys.dm_db_partition_stats ps
JOIN sys.tables t ON t.object_id = ps.object_id
GROUP BY ps.object_id
ORDER BY SUM(CASE WHEN ps.index_id IN (0,1) THEN ps.in_row_data_page_count ELSE 0 END) * 8 * 1024
       + SUM(CASE WHEN ps.index_id > 1 THEN ps.used_page_count ELSE 0 END) * 8 * 1024 DESC";
}

/// See the Interfaces doc comment. `debug_assert` mirrors sandbox.rs's
/// "must never be constructed wrong" posture for values that only ever
/// originate from our own parsed results (design §6 pid validation).
pub fn kill_sql(engine: dbc_state::Engine, pid: i64) -> Option<String> {
    debug_assert!(pid > 0, "pid must come from a fetched RunningQueryRow/BlockingNode");
    match engine {
        dbc_state::Engine::Postgres => Some(format!("SELECT pg_terminate_backend({pid})")),
        dbc_state::Engine::Mssql => Some(format!("KILL {pid}")),
        dbc_state::Engine::Sqlite => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::is_read_statement;

    /// Design §8 T2: regression guard — a future edit that turns any
    /// monitor query into a write must fail CI, not just review.
    #[test]
    fn every_pg_monitor_query_is_read_only_per_guards() {
        for (name, sql) in [
            ("CONNECTIONS", pg::CONNECTIONS),
            ("LOCKS", pg::LOCKS),
            ("DATA_SIZE", pg::DATA_SIZE),
            ("WAL_SIZE", pg::WAL_SIZE),
            ("PERF", pg::PERF),
            ("RUNNING", pg::RUNNING),
            ("BLOCKING", pg::BLOCKING),
            ("TABLES", pg::TABLES),
        ] {
            assert!(is_read_statement(sql), "pg::{name} must pass is_read_statement");
        }
    }

    /// Design §8 T3: leading-keyword smoke only — is_read_statement's
    /// keyword set is pg/sqlite-flavoured and NOT authoritative for T-SQL,
    /// so this asserts the weaker property that holds regardless.
    #[test]
    fn every_mssql_monitor_query_starts_with_select() {
        for (name, sql) in [
            ("CONNECTIONS", mssql::CONNECTIONS),
            ("CONNECTIONS_MAX", mssql::CONNECTIONS_MAX),
            ("LOCKS_WAITING", mssql::LOCKS_WAITING),
            ("DEADLOCKS", mssql::DEADLOCKS),
            ("SIZE", mssql::SIZE),
            ("CACHE_HIT", mssql::CACHE_HIT),
            ("UPTIME", mssql::UPTIME),
            ("XACT_TOTAL", mssql::XACT_TOTAL),
            ("RUNNING", mssql::RUNNING),
            ("BLOCKING", mssql::BLOCKING),
            ("TABLES", mssql::TABLES),
        ] {
            assert!(
                sql.trim_start().to_ascii_uppercase().starts_with("SELECT"),
                "mssql::{name} must lead with SELECT"
            );
        }
    }

    #[test]
    fn kill_sql_per_engine() {
        assert_eq!(
            kill_sql(dbc_state::Engine::Postgres, 1234),
            Some("SELECT pg_terminate_backend(1234)".to_string())
        );
        assert_eq!(kill_sql(dbc_state::Engine::Mssql, 55), Some("KILL 55".to_string()));
        assert_eq!(kill_sql(dbc_state::Engine::Sqlite, 1), None);
    }

    /// Documents design §0's rejected alternative: the pg kill statement
    /// PASSES is_read_statement (leading SELECT, no WRITE_KEYWORDS token),
    /// which is exactly WHY it must never travel through query() — the
    /// read-only guard there would not catch it. If this test ever fails,
    /// the §0 rationale changed and the routing decision must be revisited.
    #[test]
    fn pg_kill_statement_would_evade_the_read_guard_hence_execute_only() {
        let sql = kill_sql(dbc_state::Engine::Postgres, 1).unwrap();
        assert!(is_read_statement(&sql));
    }
}
