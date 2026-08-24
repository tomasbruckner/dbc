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
//! - DuckDB PROCESS-CONCURRENCY limitation (G16): the MCP server is a
//!   separate process, and DuckDB allows two processes on one file only
//!   when BOTH are read-only — the app holding a read-write root means the
//!   MCP open fails with the driver's translated `locked` error (and vice
//!   versa); that error already names the situation in human terms, no
//!   additional handling here.

use std::time::Duration;

use dbc_core::{Connection, QueryError};
use dbc_driver_duckdb::DuckdbConnection;
use dbc_driver_postgres::PostgresConnection;
use dbc_driver_sqlite::SqliteConnection;
use dbc_state::{ConnectionConfig, Engine};

const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;
const DEFAULT_PG_PORT: u16 = 5432;

/// Twin of `dbc-ui`'s `connect::is_in_memory_duckdb_path` — deliberately
/// duplicated, not shared (this file's module doc: near-duplicate of the
/// GUI connect path, a fix to one should prompt checking the twin).
///
/// Prefix match, not equality (T3 review finding 1): DuckDB also accepts
/// the NAMED in-memory form `:memory:name` — same trap, same refusal.
fn is_in_memory_duckdb_path(path: &str) -> bool {
    let trimmed = path.trim();
    trimmed.is_empty() || trimmed.starts_with(":memory:")
}

/// Opens a saved connection for MCP use: always read-only at the driver
/// layer, never SSH-tunneled, never MSSQL; DuckDB always read-only.
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
            // G15 T8 whole-branch review NIT fix: this comment used to say
            // "the MSSQL driver isn't merged on main yet", which went stale
            // the moment G15 landed dbc-driver-mssql and wired it into
            // dbc-ui's own open_config. MCP intentionally refuses MSSQL —
            // this crate's own connect path (this file's module doc: "a
            // near-duplicate of dbc-ui's open_config, not a shared
            // abstraction") has simply never been extended to build an
            // MSSQL connection string/config, independent of whatever
            // dbc-ui's own MSSQL feature gates are doing. Not a stale
            // placeholder to "catch up" passively — a deliberate scope
            // decision for a future task to lift on purpose.
            Err(QueryError::msg("MSSQL zatím není přes MCP podporován"))
        }
        Engine::Sqlite => {
            // `true` unconditionally: MCP has no write path, so it is
            // always at least as restrictive as `cfg.read_only`.
            let conn = SqliteConnection::new_with_options(cfg.database.clone(), true);
            Ok(Box::new(conn))
        }
        Engine::Duckdb => {
            if is_in_memory_duckdb_path(&cfg.database) {
                return Err(QueryError::msg(
                    "in-memory DuckDB databáze není podporována — zadejte cestu k souboru",
                ));
            }
            // `true` unconditionally: MCP has no write path, so it is
            // always at least as restrictive as `cfg.read_only` — same
            // posture as the Sqlite arm above. PROCESS-CONCURRENCY
            // limitation: see this module's doc comment (DuckDB allows two
            // processes on one file only when BOTH are read-only; the
            // driver's translated `locked` error surfaces verbatim).
            let conn = DuckdbConnection::new_with_options(cfg.database.clone(), true);
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

    /// G16 T3: DuckDB over MCP is forced read-only regardless of
    /// `cfg.read_only`, and a `:memory:` path is refused outright.
    #[tokio::test]
    async fn duckdb_forced_read_only_and_memory_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Driver fixture quirk: no pre-existing file — DuckDB creates the
        // database itself (a pre-existing empty file is not a valid db).
        let db = dir.path().join("m.duckdb");
        {
            // Seeded via query(), NOT the write entry point: the
            // no_write_path_regression lint (tools.rs) forbids that call
            // shape anywhere in this crate's source, test code included —
            // and the driver happily runs DDL/DML through query() on a
            // read-write root (auto-commit), which is all the seeding
            // needs (the sqlite twin above seeds via rusqlite for the
            // same reason).
            let mut seed = dbc_driver_duckdb::DuckdbConnection::new(&db);
            for sql in ["CREATE TABLE t(id INTEGER)", "INSERT INTO t VALUES (1)"] {
                let mut s = seed.query(sql, CancelToken::new()).await.unwrap();
                while let Some(item) = s.batches.recv().await {
                    item.unwrap();
                }
            }
        } // seed's root drops here — frees the path for the read-only open
        let mut cfg = sqlite_cfg(&db, false); // reuse the fixture, flip engine:
        cfg.engine = Engine::Duckdb;
        let mut conn = open_for_mcp(&cfg, None).await.unwrap();

        let mut stream = conn.query("SELECT id FROM t", CancelToken::new()).await.unwrap();
        let mut rows = 0usize;
        while let Some(item) = stream.batches.recv().await {
            rows += item.unwrap().num_rows();
        }
        assert_eq!(rows, 1);

        // Write refused — read-only forced regardless of cfg.read_only=false.
        let mut saw_error = false;
        match conn.query("INSERT INTO t VALUES (2)", CancelToken::new()).await {
            Ok(mut s) => {
                while let Some(item) = s.batches.recv().await {
                    if item.is_err() {
                        saw_error = true;
                    }
                }
            }
            Err(_) => saw_error = true,
        }
        assert!(saw_error, "MCP DuckDB connection must refuse the write");

        cfg.database = ":memory:".into();
        assert!(open_for_mcp(&cfg, None).await.is_err());
        // T3 review finding 1: the NAMED in-memory form is refused too.
        cfg.database = ":memory:analytics".into();
        assert!(open_for_mcp(&cfg, None).await.is_err());
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
