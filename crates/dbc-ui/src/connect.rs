use std::time::Duration;

use dbc_core::{Connection, QueryError};
use dbc_driver_postgres::PostgresConnection;
use dbc_driver_sqlite::SqliteConnection;
use dbc_state::{ConnectionConfig, Engine};

use crate::tunnel::Tunnel;

/// Fallback bound for `tokio_postgres::Config::connect_timeout` when a saved
/// connection doesn't set `timeout_secs` (that field otherwise doubles as
/// the query-side watchdog in `runner::connect_and_run`). Keeps the TCP
/// handshake from hanging for the OS's own default timeout (tens of seconds
/// to minutes on a black-holed/firewalled host) — see task-8-review.md
/// issue #1.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Dispatch a connection string to the right driver.
///
/// `postgres://` / `postgresql://` URLs go to the Postgres driver (connected
/// via `block_on` on the given runtime handle); anything else is treated as
/// a SQLite file path.
///
/// `block_on` here must be called from a thread that is NOT already driving
/// the given runtime's async tasks (e.g. from inside `spawn_blocking`, or —
/// historically, before Task 8 — directly on the UI thread). Task 8 moved
/// the only caller (`runner::connect_and_run`) into `spawn_blocking`, so this
/// no longer blocks the UI thread; the function itself is otherwise
/// unchanged so the CLI-arg startup path keeps working exactly as before.
pub fn open(
    url: &str,
    runtime: &tokio::runtime::Handle,
) -> Result<Box<dyn Connection>, QueryError> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let url = url.to_owned();
        let conn = runtime.block_on(async move { PostgresConnection::connect(&url).await })?;
        Ok(Box::new(conn))
    } else {
        Ok(Box::new(SqliteConnection::new(url)))
    }
}

/// A live connection plus whatever else must stay alive for it to keep
/// working — currently just an optional SSH tunnel. Dropping this drops the
/// tunnel's child `ssh` process after the connection, tearing the forward
/// down.
pub struct OpenConnection {
    pub conn: Box<dyn Connection>,
    pub _tunnel: Option<Tunnel>,
}

/// Connects using a saved [`ConnectionConfig`] (Task 7's connection manager),
/// as opposed to [`open`]'s CLI-arg connection string.
///
/// - Postgres: built via `tokio_postgres::Config`'s builder API rather than
///   formatting a `postgres://user:pass@host:port/db` URL string — a
///   password containing `@`, `/`, or other URL-special characters would
///   otherwise have to be percent-encoded (and a bug there would silently
///   corrupt the URL rather than fail loudly). The builder API takes the
///   password as a separate field, so no encoding step exists to get wrong.
///   `SET SESSION CHARACTERISTICS` isn't needed for server-side read-only
///   enforcement either: `options("-c default_transaction_read_only=on")`
///   applies it for the lifetime of the connection, before any client SQL
///   runs.
/// - SQLite: `database` is the file path; server-side read-only enforcement
///   uses `SqliteConnection::new_with_options(path, true)`
///   (`SQLITE_OPEN_READ_ONLY`), not just the client-side `is_read_statement`
///   guard (see `dbc-driver-sqlite`'s `open_conn`).
/// - An `ssh` block on `cfg` opens a [`Tunnel`] first and rewrites the
///   target host/port to `127.0.0.1:{tunnel.local_port()}`; the tunnel is
///   returned alongside the connection so its lifetime is tied to it
///   (dropping `OpenConnection` kills the tunnel's child process).
///
/// This function performs blocking I/O (`Tunnel::open`'s child-process spawn
/// and up-to-10s poll loop, plus `runtime.block_on` for the Postgres
/// handshake) and must be called from a context where blocking is legal —
/// `spawn_blocking`, not a runtime worker thread. See `runner::connect_and_run`.
/// The whole sequence is bounded end-to-end: the tunnel step (when present)
/// caps out at 10s, and the Postgres handshake itself carries a
/// `connect_timeout` (`cfg.timeout_secs`, or `DEFAULT_CONNECT_TIMEOUT_SECS`
/// if unset) — an unreachable/firewalled host can no longer hang this
/// function for the OS's own (much longer, platform-dependent) TCP timeout.
///
/// SECURITY: `secret` (the connection password) is never logged here and
/// never appears in an error message — Postgres/SQLite driver errors carry
/// only server-provided text, and this function never formats `secret` into
/// a string itself (the builder API takes it as a field, not text it
/// concatenates).
pub fn open_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
    runtime: &tokio::runtime::Handle,
) -> Result<OpenConnection, QueryError> {
    match cfg.engine {
        Engine::Mssql => {
            // Permanent behaviour (not a Task 8 stub): the MSSQL driver is a
            // separate roadmap item entirely, per the brief.
            Err(QueryError::msg("MSSQL driver zatím není k dispozici"))
        }
        Engine::Sqlite => {
            let conn = SqliteConnection::new_with_options(cfg.database.clone(), cfg.read_only);
            Ok(OpenConnection { conn: Box::new(conn), _tunnel: None })
        }
        Engine::Postgres => {
            let default_port = 5432u16;
            let (target_host, target_port, tunnel) = if let Some(ssh) = &cfg.ssh {
                let port = cfg.port.unwrap_or(default_port);
                let tunnel = Tunnel::open(ssh, &cfg.host, port).map_err(QueryError::msg)?;
                ("127.0.0.1".to_string(), tunnel.local_port(), Some(tunnel))
            } else {
                (cfg.host.clone(), cfg.port.unwrap_or(default_port), None)
            };

            let mut config = tokio_postgres::Config::new();
            config
                .host(&target_host)
                .port(target_port)
                .dbname(&cfg.database)
                .user(&cfg.user)
                .connect_timeout(Duration::from_secs(
                    cfg.timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
                ));
            if let Some(pw) = &secret {
                config.password(pw);
            }
            if cfg.read_only {
                // Server-side enforcement (Task 6 security review
                // requirement): applies for the whole session, before any
                // client SQL runs, independent of the client-side
                // `is_read_statement` guard.
                config.options("-c default_transaction_read_only=on");
            }

            let conn = runtime
                .block_on(async move { PostgresConnection::connect_with_config(config).await })?;
            Ok(OpenConnection { conn: Box::new(conn), _tunnel: tunnel })
        }
    }
}
