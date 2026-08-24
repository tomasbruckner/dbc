//! `dbc-mcp`'s own connect logic — deliberately a near-duplicate of
//! `dbc-ui/src/connect.rs::open_config` (~40 lines), not a shared
//! abstraction (see that file's own cross-reference comment; a fix to one
//! path should prompt checking the twin — design doc §1 "Deliberate
//! duplication"). Differences from the GUI's path, both load-bearing for
//! MCP's read-only guarantee:
//!
//! - Read-only is forced at the driver layer UNCONDITIONALLY, regardless of
//!   `ConnectionConfig::read_only` — MCP has no write path at all, so it is
//!   always at least as restrictive as whatever the saved config asks for
//!   (design doc §4 layer 2).
//! - No `Tunnel`/SSH support at all: a config with `ssh: Some(_)` is
//!   rejected outright (§1 non-goal, v1).
//! - Called directly `.await`ed, no `runtime.block_on`: every caller here
//!   is itself an async `#[tool]` method already running on the MCP
//!   server's own tokio runtime, unlike `dbc-ui` which calls the twin from
//!   a blocking context and needs `block_on`.

use std::time::Duration;

use dbc_core::{Connection, QueryError};
use dbc_driver_postgres::PostgresConnection;
use dbc_driver_sqlite::SqliteConnection;
use dbc_state::{ConnectionConfig, Engine};

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;
const DEFAULT_PG_PORT: u16 = 5432;

/// Opens a saved connection for MCP use: always read-only at the driver
/// layer, never SSH-tunneled, never MSSQL (not shipped yet).
///
/// SECURITY: `secret` is never logged and never appears in an error
/// message — same discipline as `dbc-ui`'s `open_config`.
pub async fn open_for_mcp(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<Box<dyn Connection>, QueryError> {
    if cfg.ssh.is_some() {
        return Err(QueryError::msg(
            "SSH-tunneled connections are not available over MCP (v1 non-goal)",
        ));
    }
    match cfg.engine {
        Engine::Mssql => {
            // Same message dbc-ui's open_config uses for the same case —
            // the MSSQL driver isn't merged on main yet.
            Err(QueryError::msg("MSSQL driver zatím není k dispozici"))
        }
        Engine::Sqlite => {
            // `true` unconditionally: MCP has no write path, so it is
            // always at least as restrictive as `cfg.read_only`.
            let conn = SqliteConnection::new_with_options(cfg.database.clone(), true);
            Ok(Box::new(conn))
        }
        Engine::Postgres => {
            let mut config = tokio_postgres::Config::new();
            config
                .host(&cfg.host)
                .port(cfg.port.unwrap_or(DEFAULT_PG_PORT))
                .dbname(&cfg.database)
                .user(&cfg.user)
                .connect_timeout(Duration::from_secs(
                    cfg.timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
                ))
                // Unconditional, unlike dbc-ui's `if cfg.read_only`: MCP
                // forces this regardless of the saved flag (§4 layer 2).
                .options("-c default_transaction_read_only=on");
            if let Some(pw) = &secret {
                config.password(pw);
            }
            let conn = PostgresConnection::connect_with_config(config).await?;
            Ok(Box::new(conn))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::CancelToken;

    fn sqlite_cfg(db_path: &std::path::Path, read_only: bool) -> ConnectionConfig {
        ConnectionConfig {
            id: "c1".into(),
            name: "t".into(),
            folder: vec![],
            engine: Engine::Sqlite,
            host: String::new(),
            port: None,
            database: db_path.to_string_lossy().into_owned(),
            user: String::new(),
            read_only,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    // T3's core assertion: a connection whose saved config says
    // `read_only: false` still rejects a write when opened via
    // `open_for_mcp` — the driver-layer force wins regardless.
    #[tokio::test]
    async fn forces_read_only_even_when_config_says_read_write() {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(f.path()).unwrap();
            conn.execute_batch("CREATE TABLE t(id INTEGER)").unwrap();
        }
        let cfg = sqlite_cfg(f.path(), false);
        let mut conn = open_for_mcp(&cfg, None).await.unwrap();

        let mut select = conn.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        while select.batches.recv().await.is_some() {}

        let mut saw_error = false;
        match conn.query("INSERT INTO t(id) VALUES (1)", CancelToken::new()).await {
            Ok(mut s) => {
                while let Some(item) = s.batches.recv().await {
                    if item.is_err() {
                        saw_error = true;
                    }
                }
            }
            Err(_) => saw_error = true,
        }
        assert!(saw_error, "expected the INSERT to be rejected by the forced read-only connection");
    }

    #[tokio::test]
    async fn ssh_tunneled_connection_rejected() {
        let mut cfg = sqlite_cfg(std::path::Path::new("unused.db"), true);
        cfg.ssh = Some(dbc_state::SshTunnelConfig {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            key_path: None,
        });
        // `Box<dyn Connection>` (the `Ok` variant) doesn't implement
        // `Debug`, so `Result::unwrap_err` can't be used here — same
        // pattern dbc-driver-sqlite's own tests use.
        let err = match open_for_mcp(&cfg, None).await {
            Ok(_) => panic!("expected an ssh-tunneled connection to be rejected"),
            Err(e) => e,
        };
        assert!(err.message.to_lowercase().contains("ssh"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn mssql_engine_rejected_with_clear_message() {
        let mut cfg = sqlite_cfg(std::path::Path::new("unused.db"), true);
        cfg.engine = Engine::Mssql;
        let err = match open_for_mcp(&cfg, None).await {
            Ok(_) => panic!("expected the MSSQL engine to be rejected"),
            Err(e) => e,
        };
        assert!(!err.message.is_empty());
    }
}
