use std::time::Duration;

use dbc_core::{Connection, QueryError};
use dbc_driver_mssql::{MssqlConfig, MssqlConnection};
use dbc_driver_postgres::{PgConfig, PostgresConnection};
use dbc_driver_sqlite::SqliteConnection;
use dbc_state::{ConnectionConfig, Engine};

use crate::tunnel::Tunnel;

/// Fallback bound for `PgConfig::connect_timeout` when a saved
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
/// - Postgres: built via `dbc_driver_postgres::PgConfig`'s (a re-export of
///   `tokio_postgres::Config` — G1 follow-up #5, final-review.md: dbc-ui
///   must not depend on the driver protocol crate directly) builder API
///   rather than formatting a `postgres://user:pass@host:port/db` URL
///   string — a password containing `@`, `/`, or other URL-special
///   characters would otherwise have to be percent-encoded (and a bug there
///   would silently corrupt the URL rather than fail loudly). The builder
///   API takes the password as a separate field, so no encoding step exists
///   to get wrong.
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
/// concatenates). MSSQL (G15 T3): the password lives ONLY in the in-memory
/// ODBC connection string built by `mssql_connection_from_config`
/// (`escape_odbc_value` brace-wraps hostile values so it round-trips); it is
/// never persisted, never logged, never formatted into an error (`probe`
/// surfaces driver diagnostic records only). No DSN, ever.
pub fn open_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
    runtime: &tokio::runtime::Handle,
) -> Result<OpenConnection, QueryError> {
    match cfg.engine {
        Engine::Mssql => {
            // READ-ONLY POSTURE (driver integration note 5, G15 §1a):
            // there is NO server-side read-only mode to set —
            // ApplicationIntent=ReadOnly only routes AG secondaries; on a
            // standalone instance it accepts writes. Client-side
            // `is_read_statement` + the SHARED runner guard are the ONLY
            // enforcement for MSSQL, unlike pg
            // (default_transaction_read_only=on) and sqlite
            // (SQLITE_OPEN_READ_ONLY). Nothing server-side is set here.
            //
            // SECURITY: the password lives only in the in-memory ODBC
            // connection string (escape_odbc_value brace-wraps hostile
            // values); it is never persisted, never logged, never
            // formatted into an error (probe surfaces driver diagnostic
            // records only — REQUIRED negative test in T8's
            // mssql_docker_tests). No DSN, ever.
            let conn = mssql_connection_from_config(cfg, secret)?;
            // Eager handshake: bad host/credentials fail HERE (probe is
            // plain blocking code; this arm already runs on a
            // blocking-legal thread — no block_on needed).
            conn.probe().map_err(mssql_im002_hint)?;
            Ok(OpenConnection { conn: Box::new(conn), _tunnel: None })
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

            let mut config = PgConfig::new();
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

/// Shared MSSQL builder — used by `open_config`'s arm AND (T7)
/// `runner::run_mssql_plan`. Refusals first, before touching the
/// vault-provided secret's destination string. NO probe here — callers
/// decide (open_config probes eagerly; run_mssql_plan lets
/// query_with_session's own connect fail naturally).
pub(crate) fn mssql_connection_from_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<MssqlConnection, QueryError> {
    // §1d: Encrypt=yes + a 127.0.0.1 tunnel endpoint makes the server
    // cert's hostname never match, so a tunneled MSSQL connection only
    // works with TrustServerCertificate=yes — an untested encryption
    // downgrade path. Fail honest; same message pattern as the
    // backup-over-tunnel gates in main.rs.
    if cfg.ssh.is_some() {
        return Err(QueryError::msg(
            "SSH tunel pro MSSQL zatím není podporován — použij přímé připojení",
        ));
    }
    // §0 non-goal: SQL auth only in v1 (no Trusted_Connection).
    if cfg.user.trim().is_empty() {
        return Err(QueryError::msg(
            "MSSQL: zadejte uživatele — ověření přes Windows účet zatím není podporováno",
        ));
    }
    let opts = cfg.mssql.clone().unwrap_or_default();
    let mut mssql_cfg = MssqlConfig::new(
        cfg.host.clone(),
        cfg.port.unwrap_or(1433),
        cfg.database.clone(),
        cfg.user.clone(),
        secret.unwrap_or_default(),
    )
    .encrypt(opts.encrypt)
    .trust_server_certificate(opts.trust_server_certificate)
    // Same 15s fallback bound the pg arm uses, rendered as ODBC
    // `Connection Timeout` so an unreachable host fails inside the same
    // envelope instead of hanging for the OS TCP timeout.
    .connect_timeout_sec(
        cfg.timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS).min(u32::MAX as u64) as u32,
    );
    if let Some(driver) = opts.driver.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        mssql_cfg = mssql_cfg.driver(driver.to_string());
    }
    Ok(MssqlConnection::new(&mssql_cfg))
}

/// §1c: SQLSTATE IM002 ("data source name not found and no default
/// driver specified") is the exact failure an uninstalled msodbcsql18
/// produces. Best-effort sugar, never load-bearing — the original
/// diagnostic is appended, and a non-IM002 error passes through
/// untouched. Detection checks the structured code first (odbc_err puts
/// the bare SQLSTATE there) and falls back to a substring match.
pub(crate) fn mssql_im002_hint(e: QueryError) -> QueryError {
    let is_im002 = e.code.as_deref() == Some("IM002") || e.message.contains("IM002");
    if !is_im002 {
        return e;
    }
    QueryError {
        code: e.code.clone(),
        message: format!(
            "ODBC Driver 18 for SQL Server není nainstalován — nainstalujte balíček \
             msodbcsql18 (nebo v nastavení připojení zadejte název nainstalovaného \
             driveru): {}",
            e.message
        ),
        position: e.position,
    }
}

#[cfg(test)]
mod mssql_connect_tests {
    use super::*;
    use dbc_state::{Engine, MssqlOptions, SshTunnelConfig};

    fn base_cfg() -> ConnectionConfig {
        ConnectionConfig {
            id: "c1".into(),
            name: "mssql".into(),
            folder: vec![],
            engine: Engine::Mssql,
            host: "localhost".into(),
            port: Some(1433),
            database: "master".into(),
            user: "sa".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    #[test]
    fn mssql_ssh_config_is_refused_before_any_io() {
        let mut cfg = base_cfg();
        cfg.ssh = Some(SshTunnelConfig {
            host: "bastion".into(),
            port: 22,
            user: "tomas".into(),
            key_path: None,
        });
        let err = match mssql_connection_from_config(&cfg, None) {
            Err(e) => e,
            Ok(_) => panic!("expected the SSH refusal"),
        };
        assert_eq!(
            err.message,
            "SSH tunel pro MSSQL zatím není podporován — použij přímé připojení"
        );
    }

    #[test]
    fn mssql_empty_user_is_refused_with_integrated_auth_message() {
        let mut cfg = base_cfg();
        cfg.user = "  ".into();
        let err = match mssql_connection_from_config(&cfg, None) {
            Err(e) => e,
            Ok(_) => panic!("expected the integrated-auth refusal"),
        };
        assert_eq!(
            err.message,
            "MSSQL: zadejte uživatele — ověření přes Windows účet zatím není podporováno"
        );
    }

    #[test]
    fn mssql_connection_from_config_applies_options_and_defaults() {
        let mut cfg = base_cfg();
        cfg.mssql = Some(MssqlOptions {
            encrypt: false,
            trust_server_certificate: true,
            driver: Some("ODBC Driver 17 for SQL Server".into()),
        });
        // Just proves the builder succeeds and refusals above don't fire —
        // the rendered connection string is exercised by dbc-driver-mssql's
        // own `MssqlConfig` tests; this is the plumbing seam.
        assert!(mssql_connection_from_config(&cfg, Some("pw".into())).is_ok());
    }

    #[test]
    fn im002_hint_wraps_only_im002() {
        let e = QueryError { code: Some("IM002".into()), message: "driver not found".into(), position: None };
        let wrapped = mssql_im002_hint(e);
        assert!(wrapped.message.contains("ODBC Driver 18 for SQL Server není nainstalován"));
        assert!(wrapped.message.contains("driver not found"));

        let e = QueryError { code: Some("08001".into()), message: "certificate chain".into(), position: None };
        let untouched = mssql_im002_hint(e.clone());
        assert_eq!(untouched.message, e.message);
    }
}
