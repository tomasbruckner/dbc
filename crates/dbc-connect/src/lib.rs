//! Opening a saved connection — ONE implementation, for every engine.
//!
//! This used to live inside `dbc-ui`, which made it unreachable from any
//! other binary: `dbc-mcp` grew its own near-duplicate (read-only, and
//! refusing MSSQL outright), and a CLI would have been a third copy of the
//! same four `match cfg.engine` arms. The security notes below are the
//! reason that mattered — they are per-engine posture statements about
//! where a password may live and what read-only actually enforces, and
//! three drifting copies of those is exactly the shape of bug this
//! codebase spends its audits preventing.
//!
//! What is NOT here: policy. This crate opens what it is told to open. The
//! read-only guard, the auto-limit, the confirm-before-write rule and the
//! row caps all live with their callers, because a GUI's confirm dialog
//! and a CLI's `--write` flag are the same rule wearing different clothes
//! and neither belongs in a connection opener.

pub mod tunnel;

use tunnel::Tunnel;

use std::time::Duration;

use dbc_core::{Connection, QueryError};
use dbc_driver_duckdb::DuckdbConnection;
use dbc_driver_mssql::{MssqlConfig, MssqlConnection};
use dbc_driver_postgres::{PgConfig, PostgresConnection};
use dbc_driver_sqlite::SqliteConnection;
use dbc_state::{ConnectionConfig, Engine, Vault};


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
/// Design §6: the DB-list cap. The listing SQL carries a `2001`-row cap in
/// its own dialect (`LIMIT` / `TOP`) and `drain_all_rows` is capped at
/// `DB_LIST_CAP + 1` as a second belt — the sentinel 2001st row is how
/// `truncate_db_list` detects "there were more" without a COUNT round-trip.
pub const DB_LIST_CAP: usize = 2000;

/// Design §3.2: excludes templates AND `datallowconn = false` —
/// deliberately stricter than `admin_sql`'s sizes query (templates only):
/// a database you cannot connect to must not render as an expandable row.
pub const PG_DB_LIST_SQL: &str = "SELECT datname FROM pg_catalog.pg_database \
     WHERE NOT datistemplate AND datallowconn ORDER BY datname LIMIT 2001";

/// Design §3.2: ONLINE databases only (state = 0). System DBs
/// (master/msdb/model/tempdb) are INCLUDED — DataGrip precedent; hiding
/// them would surprise admins, and they are just rows until expanded.
/// `TOP (2001)`, not `LIMIT` — resolved deviation 2.
pub const MSSQL_DB_LIST_SQL: &str =
    "SELECT TOP (2001) name FROM sys.databases WHERE state = 0 ORDER BY name";

/// The listing SQL for `engine`, or `None` for the file engines — those
/// have no list to fetch: the file IS the database.
pub fn db_list_sql(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Postgres => Some(PG_DB_LIST_SQL),
        Engine::Mssql => Some(MSSQL_DB_LIST_SQL),
        Engine::Sqlite | Engine::Duckdb => None,
    }
}

/// Pure half of the truncation contract: keep at most `DB_LIST_CAP` names,
/// flag whether anything was dropped (the caller renders the disclosure
/// Notice row, design §6).
pub fn truncate_db_list(mut names: Vec<String>) -> (Vec<String>, bool) {
    if names.len() > DB_LIST_CAP {
        names.truncate(DB_LIST_CAP);
        (names, true)
    } else {
        (names, false)
    }
}

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
        Engine::Duckdb => {
            // File-based engine: `database` is the file path; host/port/
            // user/password and `cfg.ssh` are ignored, byte-for-byte the
            // Sqlite arm's posture (no new divergence, no new error). No
            // vault secret is ever fetched for this engine
            // (`connections_ui::engine_is_file_based` keeps the prompt
            // away at every call site).
            if is_in_memory_duckdb_path(&cfg.database) {
                return Err(QueryError::msg(
                    "in-memory DuckDB databáze není podporována — zadejte cestu k souboru",
                ));
            }
            // Dual read-only enforcement, same as sqlite: engine-side
            // AccessMode::ReadOnly here (driver-proven by its
            // read_only_connection_rejects_writes tests), plus the SHARED
            // client-side guards at the runner choke point. Registry
            // semantics the UI inherits (all driver-implemented): same
            // file+mode roots are shared and fine; opposite-mode opens
            // fail with the driver's Czech mixed-mode error; another
            // PROCESS holding the file fails with the translated `locked`
            // error (PID-scrubbed). All surfaced VERBATIM.
            let conn = DuckdbConnection::new_with_options(cfg.database.clone(), cfg.read_only);
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

/// G16 §3: `:memory:` (and an empty path) is refused for DuckDB BEFORE the
/// driver is ever constructed — this app opens a fresh connection per
/// dispatch and the driver's per-path registry holds only a `Weak`, so an
/// in-memory database's entire contents would be torn down the moment each
/// dispatch's last connection drops: an empty database on every single
/// query, a data-eating trap rather than a feature. Revisit only if the
/// app ever grows a held-connection mode.
///
/// Prefix match, not equality (T3 review finding 1): DuckDB also accepts
/// the NAMED in-memory form `:memory:name` — same trap, same refusal.
pub fn is_in_memory_duckdb_path(path: &str) -> bool {
    let trimmed = path.trim();
    trimmed.is_empty() || trimmed.starts_with(":memory:")
}

/// G15 T8 HARD GATE ITEM 2: the two `mssql_connection_from_config`
/// refusals below (SSH tunnel, empty/integrated-auth user) need no secret
/// at all to decide — `true` here means that function will refuse `cfg`
/// before ever looking at whatever secret it's handed. Extracted as its
/// own predicate so callers can check it BEFORE fetching the vault secret,
/// not just before USING it: the review finding was that every call site
/// (`main.rs`/`connections_ui.rs`, ~8 of them) unconditionally called
/// `vault.get_secret(&cfg.id)` first and only afterward built the
/// `ConnectSpec`/called this function, so a config that was always going
/// to be refused still had its plaintext secret pulled out of the vault
/// and held in memory for no reason — see [`resolve_secret_for_connect`].
pub fn mssql_connect_refusal(cfg: &ConnectionConfig) -> bool {
    cfg.engine == Engine::Mssql && (cfg.ssh.is_some() || cfg.user.trim().is_empty())
}

/// The get_secret call every "resolve a saved connection's secret to
/// attempt a connect" call site should use instead of reaching into
/// `vault` directly — skips the vault lookup entirely when
/// [`mssql_connect_refusal`] already knows `cfg` will be refused before
/// any secret is used, otherwise behaves exactly like the
/// `vault.and_then(|v| v.get_secret(&cfg.id))` pattern it replaces (same
/// `None`-on-no-vault/no-entry semantics for every non-refused config,
/// MSSQL or not).
///
/// G16 (T3 review finding 2): file-based engines (Sqlite/Duckdb — the
/// `connections_ui::engine_is_file_based` predicate is the authority)
/// short-circuit to `None` too. No password exists for these engines and
/// `open_config` ignores whatever secret it's handed, so pulling a
/// plaintext secret out of the vault and holding it in memory (possible
/// when a config's id has a stale vault entry, e.g. a pg config
/// hand-switched to duckdb) would be a needless exposure — the same G15
/// hard-gate-item-2 hygiene the MSSQL refusal branch above exists for.
pub fn resolve_secret_for_connect(vault: Option<&Vault>, cfg: &ConnectionConfig) -> Option<String> {
    if mssql_connect_refusal(cfg) {
        return None;
    }
    if dbc_state::engine_is_file_based(cfg.engine) {
        return None;
    }
    vault.and_then(|v| v.get_secret(&cfg.id))
}

/// Shared MSSQL builder — used by `open_config`'s arm AND (T7)
/// `runner::run_mssql_plan`. Refusals first, before touching the
/// vault-provided secret's destination string. NO probe here — callers
/// decide (open_config probes eagerly; run_mssql_plan lets
/// query_with_session's own connect fail naturally).
///
/// These same two checks are ALSO exposed standalone as
/// [`mssql_connect_refusal`] — kept duplicated rather than calling it from
/// here, because inlined `if`s that construct and return the exact,
/// caller-facing `QueryError` text are clearer to read at THIS call site
/// than routing through a boolean predicate would be; `mssql_connect_refusal`
/// exists for callers that need the yes/no answer before they even have a
/// secret to pass in here, per its own doc comment.
/// Vytažené z `mssql_connection_from_config` (pwchange T3): tentýž config
/// build — včetně obou refusalů a timeout defaultu — potřebuje i změna
/// hesla při loginu ([`change_mssql_password`]), která NEotevírá
/// `MssqlConnection`.
/// Turns what a person typed into Host and Port into the pair the ODBC
/// `Server=tcp:<host>,<port>` value is built from.
///
/// It exists because that value used to be `format!("tcp:{host},{port}")`
/// over the raw fields, and the network layer's complaint about the result
/// is famously unhelpful: „SQL Server Network Interfaces: Connection string
/// is not valid [87]", from which a user reasonably concluded their
/// password might be too long (2026-09-01). Nothing about the address is in
/// that sentence, and the address is the only thing it is ever about.
///
/// So the shapes people actually type are accepted rather than concatenated
/// into nonsense:
///
/// * surrounding whitespace, which a paste brings along;
/// * a `tcp:` prefix, because that is what the connection string itself
///   looks like and copying it back in is a natural thing to do;
/// * `host,1433` and `host:1433` — the SQL Server and the everything-else
///   spelling of „host and port together". Either supplies the port, and
///   contradicting the Port field is refused rather than silently picking
///   one.
///
/// And the shapes that cannot work are refused HERE, naming the field, in
/// Czech, before a driver gets to describe them as a bad connection string.
///
/// IPv6 is left alone: a literal is bracketed (`[::1]`) and full of colons,
/// so anything bracketed or with more than one colon is passed through
/// untouched instead of being torn apart at the wrong one.
pub fn normalise_mssql_host(
    host: &str,
    port: Option<u16>,
) -> Result<(String, u16), QueryError> {
    let trimmed = host.trim();
    // `get`, not slicing: `host` is arbitrary user text and `[..4]` panics
    // in the middle of a multi-byte character.
    let stripped = match trimmed.get(..4) {
        Some(p) if p.eq_ignore_ascii_case("tcp:") => trimmed[4..].trim(),
        _ => trimmed,
    };
    if stripped.is_empty() {
        return Err(QueryError::msg(
            "MSSQL: vyplň Host — adresa serveru chybí (jméno nebo IP, port patří do pole Port)",
        ));
    }

    let bracketed_or_ipv6 = stripped.contains('[') || stripped.matches(':').count() > 1;
    let split_at = stripped
        .rfind(',')
        .or_else(|| if bracketed_or_ipv6 { None } else { stripped.rfind(':') });

    let (addr, embedded_port) = match split_at {
        Some(i) => {
            let (a, rest) = stripped.split_at(i);
            let p = rest[1..].trim();
            let parsed = p.parse::<u16>().ok().filter(|n| *n != 0).ok_or_else(|| {
                QueryError::msg(format!(
                    "MSSQL: v poli Host je za oddělovačem „{p}“, což není číslo portu —                      napiš do Hostu jen jméno serveru nebo IP a port dej do pole Port"
                ))
            })?;
            (a.trim(), Some(parsed))
        }
        None => (stripped, None),
    };

    if addr.is_empty() {
        return Err(QueryError::msg(
            "MSSQL: v poli Host je jen port — doplň jméno serveru nebo IP",
        ));
    }
    if addr.chars().any(char::is_whitespace) {
        return Err(QueryError::msg(format!(
            "MSSQL: Host „{addr}“ obsahuje mezeru — adresa serveru žádnou mít nemůže"
        )));
    }

    let resolved = match (embedded_port, port) {
        (Some(from_host), Some(from_field)) if from_host != from_field => {
            return Err(QueryError::msg(format!(
                "MSSQL: port je zadaný dvakrát a pokaždé jinak — v Hostu {from_host},                  v poli Port {from_field}. Nech ho jen na jednom místě."
            )))
        }
        (Some(from_host), _) => from_host,
        (None, Some(from_field)) => from_field,
        (None, None) => 1433,
    };
    Ok((addr.to_string(), resolved))
}

pub fn mssql_config_from_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<MssqlConfig, QueryError> {
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
    let (host, port) = normalise_mssql_host(&cfg.host, cfg.port)?;
    let mut mssql_cfg = MssqlConfig::new(
        host,
        port,
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
    Ok(mssql_cfg)
}

pub fn mssql_connection_from_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<MssqlConnection, QueryError> {
    Ok(MssqlConnection::new(&mssql_config_from_config(cfg, secret)?))
}

/// pwchange (spec §3): `cfg`ovo heslo pro connect string = NOVÉ heslo,
/// staré jde do `SQL_COPT_SS_OLDPWD`. Volá se VÝHRADNĚ přes
/// `QueryRunner::change_mssql_password` (sankcionovaná cesta — UI nikdy
/// nesahá na driver přímo).
pub fn change_mssql_password(
    cfg: &ConnectionConfig,
    old_password: &str,
    new_password: &str,
) -> Result<(), QueryError> {
    let mssql_cfg = mssql_config_from_config(cfg, Some(new_password.to_string()))?;
    dbc_driver_mssql::change_password_at_connect(&mssql_cfg, old_password)
}

/// §1c: SQLSTATE IM002 ("data source name not found and no default
/// driver specified") is the exact failure an uninstalled msodbcsql18
/// produces. Best-effort sugar, never load-bearing — the original
/// diagnostic is appended, and a non-IM002 error passes through
/// untouched. Detection checks the structured code first (odbc_err puts
/// the bare SQLSTATE there) and falls back to a substring match.
pub fn mssql_im002_hint(e: QueryError) -> QueryError {
    let is_im002 = e.code.as_deref() == Some("IM002") || e.message.contains("IM002");
    if !is_im002 {
        return e;
    }
    QueryError {
        code: e.code.clone(),
        message: format!(
            "ODBC Driver 18 for SQL Server není nainstalován — stáhněte ho z \
             https://learn.microsoft.com/sql/connect/odbc/download-odbc-driver-for-sql-server \
             (nebo v nastavení připojení zadejte název nainstalovaného driveru): {}",
            e.message
        ),
        position: e.position,
    }
}

#[cfg(test)]
mod duckdb_connect_tests {
    use super::*;
    use dbc_core::CancelToken;

    fn duckdb_cfg(path: &str, read_only: bool) -> ConnectionConfig {
        ConnectionConfig {
            id: "d1".into(),
            name: "duck".into(),
            folder: vec![],
            engine: Engine::Duckdb,
            host: String::new(),
            port: None,
            database: path.into(),
            user: String::new(),
            read_only,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    /// Driver fixture quirk (its own test suite's convention): give DuckDB
    /// a path where NO file exists yet — it must create the database
    /// itself; a pre-existing empty temp file is not a valid database.
    fn fresh_db_path(dir: &tempfile::TempDir) -> String {
        dir.path().join("t.duckdb").to_string_lossy().into_owned()
    }

    #[test]
    fn in_memory_duckdb_path_matcher() {
        assert!(is_in_memory_duckdb_path(":memory:"));
        assert!(is_in_memory_duckdb_path("  :memory:  "));
        assert!(is_in_memory_duckdb_path(""));
        assert!(is_in_memory_duckdb_path("   "));
        // T3 review finding 1: DuckDB's NAMED in-memory form is the same
        // data-eating trap — refused too.
        assert!(is_in_memory_duckdb_path(":memory:analytics"));
        assert!(is_in_memory_duckdb_path("  :memory:analytics  "));
        assert!(!is_in_memory_duckdb_path(r"D:\data\analytics.duckdb"));
    }

    /// T3 review finding 2: a file-based config whose id has a (stale)
    /// vault entry must never have that plaintext secret pulled out of
    /// the vault — no password exists for these engines and open_config
    /// ignores the secret anyway. Behavioral proof with a REAL vault
    /// holding a planted entry: the same id yields the secret for a pg
    /// config (sanity: the entry is really there and reachable) and None
    /// for the duckdb/sqlite spellings.
    #[test]
    fn resolve_secret_for_connect_never_fetches_for_file_based_engines() {
        let dir = tempfile::tempdir().unwrap();
        let vault_path = dir.path().join("vault.bin");
        let mut vault = Vault::create(&vault_path, "master").unwrap();
        vault.set_secret("d1", "stale-secret").unwrap();

        let mut cfg = duckdb_cfg(r"D:\data\analytics.duckdb", false);
        assert_eq!(resolve_secret_for_connect(Some(&vault), &cfg), None);
        cfg.engine = Engine::Sqlite;
        assert_eq!(resolve_secret_for_connect(Some(&vault), &cfg), None);
        // Sanity: the planted entry IS reachable for an engine that uses
        // secrets — proves the None above is the short-circuit, not a
        // missing entry.
        cfg.engine = Engine::Postgres;
        assert_eq!(
            resolve_secret_for_connect(Some(&vault), &cfg),
            Some("stale-secret".to_string())
        );
    }

    #[test]
    fn duckdb_memory_path_is_refused_before_driver_construction() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = match open_config(&duckdb_cfg(":memory:", false), None, rt.handle()) {
            Err(e) => e,
            Ok(_) => panic!("expected the :memory: refusal"),
        };
        assert_eq!(
            err.message,
            "in-memory DuckDB databáze není podporována — zadejte cestu k souboru"
        );
    }

    #[test]
    fn duckdb_open_config_round_trips_a_select() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        let mut opened = open_config(&duckdb_cfg(&path, false), None, rt.handle()).unwrap();
        rt.block_on(async {
            let mut stream =
                opened.conn.query("SELECT 1 AS one", CancelToken::new()).await.unwrap();
            let mut rows = 0usize;
            while let Some(item) = stream.batches.recv().await {
                rows += item.unwrap().num_rows();
            }
            assert_eq!(rows, 1);
        });
    }

    /// Proves the arm passes cfg.read_only through to the ENGINE
    /// (AccessMode::ReadOnly), not just the client-side guard.
    #[test]
    fn duckdb_read_only_config_refuses_writes_at_the_engine() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        {
            // Create the database read-write first (read-only can't create
            // a missing file), then drop it so the path's root is free.
            let mut rw = open_config(&duckdb_cfg(&path, false), None, rt.handle()).unwrap();
            rt.block_on(async {
                rw.conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
            });
        }
        let mut ro = open_config(&duckdb_cfg(&path, true), None, rt.handle()).unwrap();
        rt.block_on(async {
            let err = ro.conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await;
            assert!(err.is_err(), "AccessMode::ReadOnly must refuse the write engine-side");
        });
    }

    /// The driver's mixed-mode policy surfaces VERBATIM through
    /// open_config's arm — same path+opposite mode while any instance of
    /// the first is alive (design §3 registry semantics).
    #[test]
    fn duckdb_mixed_mode_same_path_surfaces_the_driver_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        let mut rw = open_config(&duckdb_cfg(&path, false), None, rt.handle()).unwrap();
        rt.block_on(async {
            // Bind the rw root (roots bind lazily on first use).
            rw.conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
            let handle = tokio::runtime::Handle::current();
            // Construction succeeds — the refusal fires on first use.
            let mut ro = open_config(&duckdb_cfg(&path, true), None, &handle).unwrap();
            let err = match ro.conn.query("SELECT 1", CancelToken::new()).await {
                Err(e) => e,
                Ok(_) => panic!("expected the mixed-mode refusal"),
            };
            assert_eq!(err.code.as_deref(), Some("mixed-access-mode"));
            assert!(err.message.contains("již otevřena v jiném režimu"), "got: {}", err.message);
        });
        drop(rw);
    }
}

#[cfg(test)]
mod mssql_host_tests {
    use super::*;

    /// The plain case must come through byte for byte — this runs in front
    /// of every existing MSSQL connection in the world, so „it also does
    /// nothing when there is nothing to do" is the first thing to pin.
    #[test]
    fn an_ordinary_host_and_port_are_left_exactly_alone() {
        assert_eq!(
            normalise_mssql_host("production-sql.example.internal", Some(1113)).unwrap(),
            ("production-sql.example.internal".to_string(), 1113)
        );
        assert_eq!(
            normalise_mssql_host("20.224.189.174", Some(7333)).unwrap(),
            ("20.224.189.174".to_string(), 7333)
        );
    }

    #[test]
    fn no_port_anywhere_means_the_sql_server_default() {
        assert_eq!(normalise_mssql_host("srv", None).unwrap(), ("srv".to_string(), 1433));
    }

    /// The shapes a person types or pastes, all of which used to be glued
    /// straight into `tcp:{host},{port}` and rejected by SNI as „Connection
    /// string is not valid [87]".
    #[test]
    fn the_shapes_people_actually_type_are_accepted() {
        for (host, port, want_host, want_port) in [
            ("  srv  ", Some(1113u16), "srv", 1113u16),
            ("tcp:srv", Some(1113), "srv", 1113),
            ("TCP:srv", Some(1113), "srv", 1113),
            ("srv,1113", None, "srv", 1113),
            ("srv:1113", None, "srv", 1113),
            ("tcp:srv,1113", None, "srv", 1113),
            // Same port twice is agreement, not a conflict.
            ("srv,1113", Some(1113), "srv", 1113),
        ] {
            assert_eq!(
                normalise_mssql_host(host, port).unwrap(),
                (want_host.to_string(), want_port),
                "{host:?} + {port:?}"
            );
        }
    }

    /// Refused HERE, in Czech, naming the field — instead of by a driver
    /// that will only say the connection string is invalid.
    #[test]
    fn what_cannot_work_is_refused_with_a_message_about_the_field() {
        let cases = [
            ("", None, "vyplň Host"),
            ("   ", None, "vyplň Host"),
            ("tcp:", None, "vyplň Host"),
            ("srv,abc", None, "není číslo portu"),
            ("srv,0", None, "není číslo portu"),
            ("srv,99999", None, "není číslo portu"),
            (",1113", None, "jen port"),
            ("my server", None, "obsahuje mezeru"),
        ];
        for (host, port, needle) in cases {
            let e = normalise_mssql_host(host, port).unwrap_err().to_string();
            assert!(e.contains(needle), "{host:?} gave {e:?}, expected {needle:?}");
        }
    }

    /// Two different ports is the one case where guessing would be worse
    /// than refusing: either choice silently ignores something the user
    /// typed on purpose.
    #[test]
    fn a_port_given_twice_and_differently_is_refused_rather_than_guessed() {
        let e = normalise_mssql_host("srv,1113", Some(1433)).unwrap_err().to_string();
        assert!(e.contains("1113") && e.contains("1433"), "{e}");
    }

    /// An IPv6 literal is bracketed and full of colons; tearing it apart at
    /// the last one would turn a valid address into a bad one.
    #[test]
    fn an_ipv6_literal_is_not_split_at_a_colon() {
        assert_eq!(
            normalise_mssql_host("[2001:db8::1]", Some(1433)).unwrap(),
            ("[2001:db8::1]".to_string(), 1433)
        );
        // …but a comma still means „and the port is", even for IPv6.
        assert_eq!(
            normalise_mssql_host("[2001:db8::1],1113", None).unwrap(),
            ("[2001:db8::1]".to_string(), 1113)
        );
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

    /// G15 T8 HARD GATE ITEM 2: `mssql_connect_refusal` must agree with
    /// `mssql_connection_from_config`'s own two refusal `if`s exactly — a
    /// mismatch either direction would be wrong (either the vault gets
    /// skipped for a config that would actually connect, or a config that
    /// will be refused still triggers a needless secret fetch at the call
    /// site, defeating the point).
    #[test]
    fn mssql_connect_refusal_matches_mssql_connection_from_config_ssh_and_empty_user_checks() {
        let mut ssh_cfg = base_cfg();
        ssh_cfg.ssh = Some(SshTunnelConfig { host: "bastion".into(), port: 22, user: "tomas".into(), key_path: None });
        assert!(mssql_connect_refusal(&ssh_cfg));
        assert!(mssql_connection_from_config(&ssh_cfg, Some("pw".into())).is_err());

        let mut empty_user_cfg = base_cfg();
        empty_user_cfg.user = "   ".into();
        assert!(mssql_connect_refusal(&empty_user_cfg));
        assert!(mssql_connection_from_config(&empty_user_cfg, Some("pw".into())).is_err());

        let ok_cfg = base_cfg();
        assert!(!mssql_connect_refusal(&ok_cfg));
        assert!(mssql_connection_from_config(&ok_cfg, Some("pw".into())).is_ok());
    }

    /// A non-MSSQL config is NEVER refused by `mssql_connect_refusal` —
    /// the SSH/empty-user checks are MSSQL-specific (pg supports SSH
    /// tunnels and its own auth story; this predicate must not
    /// accidentally start gating them).
    #[test]
    fn mssql_connect_refusal_is_false_for_non_mssql_engines() {
        let mut pg_cfg = base_cfg();
        pg_cfg.engine = Engine::Postgres;
        pg_cfg.user = String::new();
        pg_cfg.ssh = Some(SshTunnelConfig { host: "bastion".into(), port: 22, user: "tomas".into(), key_path: None });
        assert!(!mssql_connect_refusal(&pg_cfg));
    }

    /// `resolve_secret_for_connect` with `vault: None` — the "no vault
    /// unlocked" case every call site already handles via
    /// `self.vault.as_ref()` on an `Option::None`. Refused configs still
    /// short-circuit to `None` the same way; non-refused configs also get
    /// `None` here since there's no vault to consult, same end result as
    /// today's `self.vault.as_ref().and_then(...)` on `None`, just without
    /// ever attempting the lookup for the refused case.
    #[test]
    fn resolve_secret_for_connect_short_circuits_refused_mssql_config_without_a_vault() {
        let mut ssh_cfg = base_cfg();
        ssh_cfg.ssh = Some(SshTunnelConfig { host: "bastion".into(), port: 22, user: "tomas".into(), key_path: None });
        assert_eq!(resolve_secret_for_connect(None, &ssh_cfg), None);

        let ok_cfg = base_cfg();
        assert_eq!(resolve_secret_for_connect(None, &ok_cfg), None);
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

    /// pwchange T3: změna hesla jde přes STEJNÝ config-builder jako
    /// connect — oba refusaly (SSH tunel, prázdný uživatel) platí i pro
    /// ni a extrakce `mssql_config_from_config` nesmí změnit chování
    /// `mssql_connection_from_config` (okolní testy to jistí).
    #[test]
    fn change_mssql_password_propagates_config_refusals() {
        let mut cfg = base_cfg();
        cfg.user = String::new();
        let err = change_mssql_password(&cfg, "old", "new").unwrap_err();
        assert!(err.message.contains("zadejte uživatele"), "{err}");

        let mut cfg = base_cfg();
        cfg.ssh = Some(SshTunnelConfig {
            host: "bastion".into(),
            port: 22,
            user: "tomas".into(),
            key_path: None,
        });
        let err = change_mssql_password(&cfg, "old", "new").unwrap_err();
        assert!(err.message.contains("SSH tunel"), "{err}");
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

#[cfg(test)]
mod db_list_tests {
    use super::*;

    /// The two catalog queries are saved-behaviour contracts (design §3.2):
    /// pg excludes templates AND `datallowconn = false` (deliberately
    /// stricter than admin_sql's sizes query — a db you cannot connect to
    /// must not render as expandable); MSSQL takes ONLINE only (state = 0)
    /// and deliberately INCLUDES system DBs (DataGrip precedent). Each cap
    /// is dialect-native: `LIMIT` is not T-SQL, `TOP` is not Postgres —
    /// resolved deviation 2.
    #[test]
    fn db_list_sql_texts_are_pinned() {
        assert_eq!(
            PG_DB_LIST_SQL,
            "SELECT datname FROM pg_catalog.pg_database \
             WHERE NOT datistemplate AND datallowconn ORDER BY datname LIMIT 2001"
        );
        assert_eq!(
            MSSQL_DB_LIST_SQL,
            "SELECT TOP (2001) name FROM sys.databases WHERE state = 0 ORDER BY name"
        );
    }

    #[test]
    fn truncate_db_list_caps_at_2000_and_flags() {
        let names: Vec<String> = (0..2001).map(|i| format!("db{i:04}")).collect();
        let (kept, truncated) = truncate_db_list(names);
        assert_eq!(kept.len(), DB_LIST_CAP);
        assert!(truncated);
        let (kept, truncated) = truncate_db_list(vec!["a".into(), "b".into()]);
        assert_eq!(kept.len(), 2);
        assert!(!truncated);
        // Exactly at the cap: NOT truncated (the +1 sentinel row is the signal).
        let names: Vec<String> = (0..2000).map(|i| format!("db{i:04}")).collect();
        assert!(!truncate_db_list(names).1);
    }

    /// The file engines have no list to ask for — one file, one database.
    #[test]
    fn the_file_engines_have_no_listing_sql() {
        assert!(db_list_sql(Engine::Sqlite).is_none());
        assert!(db_list_sql(Engine::Duckdb).is_none());
        assert_eq!(db_list_sql(Engine::Postgres), Some(PG_DB_LIST_SQL));
        assert_eq!(db_list_sql(Engine::Mssql), Some(MSSQL_DB_LIST_SQL));
    }
}
