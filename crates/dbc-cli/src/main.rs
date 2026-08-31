//! `dbc` — the command line over the connections saved in the app.
//!
//! Same `config.toml`, same vault, same workspace pointer as the GUI: a
//! connection set up by clicking is a connection you can name in a script,
//! and `dbc query prod` and the app's „prod" are the same server by
//! construction rather than by convention.
//!
//! ## What it will not do
//!
//! Write, unless told to for that one invocation (`--write`), and never at
//! all against a connection the app marks read-only. See `policy` for why
//! the flag is not just a convenience.
//!
//! ## Layering
//!
//! Everything decidable without I/O is in its own module and unit-tested:
//! `args` (the command line), `pick` (which connection), `policy` (may it
//! run), `render` (what comes out). This file is the part that opens
//! things, and it is deliberately the thin part.

mod args;
mod hist;
mod pick;
mod policy;
mod render;
mod vault_key;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dbc_core::{CancelToken, Connection, Dialect};
use dbc_state::{AppConfig, Engine, Vault};

use args::{Args, Command, SqlSource};
use render::Table;

/// Usage error — the argv was wrong. Distinct from a runtime failure so a
/// script can tell „I called it wrong" from „the database said no".
const EXIT_USAGE: u8 = 2;
const EXIT_ERROR: u8 = 1;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dbc: {}", e.message);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dbc: {message}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Where `config.toml` and the vault live for this invocation.
///
/// The workspace pointer is honoured exactly as the GUI and `dbc-mcp`
/// honour it, and a BROKEN pointer refuses rather than falling back to the
/// profile — silently serving the profile's „prod" to someone who thinks
/// they are in a workspace is the one failure mode this whole mechanism
/// exists to prevent. Explicit `--config`/`--vault` still win, so there is
/// always a way through.
fn resolve_paths(a: &Args) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    use dbc_state::workspace::Resolution;
    // History is machine-local in BOTH modes (§W5), so it comes from the
    // same resolution as the rest and simply never points into a shared
    // workspace folder.
    let (config, vault, history) = match dbc_state::workspace::resolve() {
        Resolution::Profile(p) => (p.config, p.vault, p.history),
        Resolution::Workspace { paths, .. } => (paths.config, paths.vault, paths.history),
        Resolution::Broken { root, reason } => {
            let named = root
                .map(|r| r.display().to_string())
                .unwrap_or_else(|| "ukazatel na pracovní prostor je nečitelný".to_string());
            let reason = dbc_state::workspace::one_line_reason(&reason);
            match (&a.config, &a.vault) {
                (Some(c), Some(v)) => (c.clone(), v.clone(), dbc_state::default_history_path()),
                _ => {
                    return Err(format!(
                        "{named} ({reason})\nspusť aplikaci a vyber pracovní prostor znovu, \
                         nebo zadej --config a --vault"
                    ))
                }
            }
        }
    };
    Ok((a.config.clone().unwrap_or(config), a.vault.clone().unwrap_or(vault), history))
}

/// A missing `config.toml` reads as „no connections saved yet" — that is
/// what `AppConfig::load` already does, and a first run has nothing to
/// report. Anything else is refused rather than rescued: falling back to
/// an empty config would answer `dbc connections` with „nothing saved"
/// for a file that is merely locked, and that answer is indistinguishable
/// from the truth.
fn load_config(path: &Path) -> Result<AppConfig, String> {
    AppConfig::load(path)
        .map_err(|e| format!("config.toml ({}) nejde přečíst: {}", path.display(), e.message))
}

fn dialect_for(engine: Engine) -> Dialect {
    match engine {
        Engine::Postgres => Dialect::Postgres,
        Engine::Mssql => Dialect::Mssql,
        Engine::Sqlite | Engine::Duckdb => Dialect::Sqlite,
    }
}

fn run(a: Args) -> Result<(), String> {
    match &a.command {
        Command::Help => {
            print!("{}", args::USAGE);
            return Ok(());
        }
        Command::Version => {
            println!("dbc {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let (config_path, vault_path, history_path) = resolve_paths(&a)?;

    match &a.command {
        Command::Help | Command::Version => unreachable!("handled above"),
        Command::Login => return login(&vault_path),
        Command::Logout => {
            vault_key::forget_key()?;
            println!("uložený klíč trezoru smazán");
            return Ok(());
        }
        _ => {}
    }

    let config = load_config(&config_path)?;

    if matches!(a.command, Command::Connections) {
        print!("{}", render::render(&connections_table(&config), a.format));
        return Ok(());
    }

    // Everything below needs an actual connection.
    let asked_for = match &a.command {
        Command::Databases { conn } | Command::Tables { conn, .. } | Command::Query { conn, .. } => {
            conn.as_str()
        }
        _ => unreachable!("handled above"),
    };
    let cfg = pick::pick(&config.connections, asked_for)
        .map_err(|e| e.message(asked_for))?
        .clone();

    // DECIDED BEFORE ANYTHING IS OPENED. A refused write must not have
    // cost a master-password prompt and a connection first — the refusal
    // depends only on the SQL, the dialect and the connection's saved
    // read-only flag, all of which are known here, and being asked for a
    // password before being told „no" teaches people to type it reflexively.
    let plan = match &a.command {
        Command::Query { sql, write, .. } => {
            let text = read_sql(sql)?;
            Some(
                policy::plan(&text, dialect_for(cfg.engine), cfg.read_only, *write)
                    .map_err(|e| e.message())?,
            )
        }
        _ => None,
    };

    // The vault is opened only when there is a secret to fetch. A SQLite
    // file has no password, and demanding a master password to list its
    // tables would be theatre.
    let vault = if dbc_state::engine_is_file_based(cfg.engine) || !Vault::exists(&vault_path) {
        None
    } else {
        Some(vault_key::unlock(&vault_path)?)
    };
    let secret = dbc_connect::resolve_secret_for_connect(vault.as_ref(), &cfg);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    // `databases` is the one command that must NOT be pinned to a specific
    // database: it asks the server what it has, from wherever it lands.
    let mut target = cfg.clone();
    if !matches!(a.command, Command::Databases { .. }) {
        target.database = pick::database_for(&cfg, a.database.as_deref());
    }

    let opened = dbc_connect::open_config(&target, secret, runtime.handle())
        .map_err(|e| e.message)?;
    let mut conn = opened.conn;

    let table = match &a.command {
        Command::Databases { .. } => Some(databases_table(&runtime, &mut *conn, &target)?),
        Command::Tables { schema, .. } => {
            Some(tables_table(&runtime, &mut *conn, schema.as_deref())?)
        }
        Command::Query { .. } => {
            let plan = plan.as_deref().expect("built above for exactly this command");
            // The label the app's history panel will show. `--db` is part
            // of it: „produkce" and „produkce/sklad" are different runs
            // and a history that called them both „produkce" would be
            // lying about which server was touched.
            let label = dbc_state::conn_label(
                &cfg.name,
                a.database.as_deref().filter(|db| *db != cfg.database),
            );
            let mut recorder = hist::Recorder::open(&history_path);
            query_table(&runtime, &mut *conn, plan, &a, &mut recorder, &label)?
        }
        _ => unreachable!("handled above"),
    };
    // `None` = a batch of pure writes. Their affected-row counts already
    // went to stderr; printing an empty table with a made-up column would
    // put a fake result in the pipe.
    if let Some(table) = table {
        print!("{}", render::render(&table, a.format));
    }
    Ok(())
}

fn login(vault_path: &Path) -> Result<(), String> {
    if !Vault::exists(vault_path) {
        return Err("trezor zatím neexistuje — vytvoř ho v aplikaci uložením hesla".to_string());
    }
    let password = vault_key::prompt_master()?;
    let vault = Vault::unlock(vault_path, &password).map_err(|e| e.message)?;
    // What is stored is what Argon2id DERIVES from the password, never the
    // password: a stolen key unlocks this machine's vault, a stolen
    // password unlocks everywhere it was reused.
    let key = vault.export_key();
    vault_key::store_key(&key)?;
    println!("klíč trezoru uložen — `dbc` teď půjde volat i ze skriptu (`dbc logout` ho smaže)");
    Ok(())
}

fn connections_table(config: &AppConfig) -> Table {
    let mut t = Table::new(vec![
        "jméno".into(),
        "engine".into(),
        "host".into(),
        "databáze".into(),
        "režim".into(),
        "id".into(),
    ]);
    for c in &config.connections {
        let host = if dbc_state::engine_is_file_based(c.engine) {
            String::new()
        } else {
            match c.port {
                Some(p) => format!("{}:{}", c.host, p),
                None => c.host.clone(),
            }
        };
        t.push_str_row(vec![
            c.name.clone(),
            format!("{:?}", c.engine),
            host,
            c.database.clone(),
            if c.read_only { "jen čtení".into() } else { "čtení i zápis".into() },
            c.id.clone(),
        ]);
    }
    t
}

fn databases_table(
    runtime: &tokio::runtime::Runtime,
    conn: &mut dyn Connection,
    cfg: &dbc_state::ConnectionConfig,
) -> Result<Table, String> {
    let mut t = Table::new(vec!["databáze".into()]);
    let Some(sql) = dbc_connect::db_list_sql(cfg.engine) else {
        // One file, one database — no round trip to make.
        t.push_str_row(vec![cfg.database.clone()]);
        return Ok(t);
    };
    let drained = runtime
        .block_on(drain(conn, sql, dbc_connect::DB_LIST_CAP + 1))
        .map_err(|e| e.message)?;
    let (rows, truncated) = dbc_connect::truncate_db_list(
        drained.rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect(),
    );
    for name in rows {
        t.push_str_row(vec![name]);
    }
    t.truncated = truncated;
    Ok(t)
}

fn tables_table(
    runtime: &tokio::runtime::Runtime,
    conn: &mut dyn Connection,
    schema_filter: Option<&str>,
) -> Result<Table, String> {
    let snapshot = runtime.block_on(conn.schema()).map_err(|e| e.message)?;
    let mut t = Table::new(vec![
        "schéma".into(),
        "jméno".into(),
        "druh".into(),
        "sloupců".into(),
    ]);
    for table in &snapshot.tables {
        let schema = table.schema.clone().unwrap_or_default();
        if let Some(want) = schema_filter {
            if !schema.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        t.push_str_row(vec![
            schema,
            table.name.clone(),
            format!("{:?}", table.kind),
            table.columns.len().to_string(),
        ]);
    }
    Ok(t)
}

/// A drained result: column names plus stringified cells.
struct Drained {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    truncated: bool,
}

/// Pull a query's stream into memory, stopping at `row_limit`.
///
/// The cap is applied HERE rather than by rewriting the SQL, so it holds
/// for statements no auto-limit could safely touch — and the last batch is
/// sliced rather than dropped, so the count lands exactly on the limit
/// instead of somewhere below it.
async fn drain(
    conn: &mut dyn Connection,
    sql: &str,
    row_limit: usize,
) -> Result<Drained, dbc_core::QueryError> {
    let cancel = CancelToken::new();
    let mut stream = conn.query(sql, cancel.clone()).await?;
    let columns: Vec<String> =
        stream.columns.fields().iter().map(|f| f.name().to_string()).collect();
    let mut buf = dbc_buffer::ResultBuffer::with_cap(stream.columns.clone(), row_limit.max(1));
    let mut truncated = false;
    loop {
        match stream.batches.recv().await {
            Some(Ok(batch)) => {
                let remaining = row_limit.saturating_sub(buf.row_count());
                if remaining == 0 {
                    truncated = true;
                    cancel.cancel();
                    break;
                }
                let slice =
                    if batch.num_rows() > remaining { batch.slice(0, remaining) } else { batch };
                buf.push(slice).map_err(|e| dbc_core::QueryError::msg(e.to_string()))?;
                if buf.row_count() >= row_limit {
                    truncated = true;
                    cancel.cancel();
                    break;
                }
            }
            Some(Err(e)) => return Err(e),
            None => break,
        }
    }
    let ncols = buf.column_count();
    let mut rows = Vec::with_capacity(buf.row_count());
    for r in 0..buf.row_count() {
        let mut row = Vec::with_capacity(ncols);
        for c in 0..ncols {
            row.push(if buf.cell_is_null(r, c) { None } else { Some(buf.cell_text(r, c)) });
        }
        rows.push(row);
    }
    Ok(Drained { columns, rows, truncated })
}

/// Run a planned batch.
///
/// The LAST statement that returns rows is what gets printed. A `.sql`
/// file that ends in a `SELECT` therefore prints that select, which is
/// what a person running a script expects; writes report their affected
/// row counts on stderr as they go, so a pipe carrying the result stays
/// clean. A batch with no reads in it at all returns `None` — stdout stays
/// empty rather than carrying an invented header.
fn query_table(
    runtime: &tokio::runtime::Runtime,
    conn: &mut dyn Connection,
    plan: &[policy::Stmt],
    a: &Args,
    recorder: &mut hist::Recorder,
    conn_label: &str,
) -> Result<Option<Table>, String> {
    let row_limit = if a.row_limit == 0 { usize::MAX } else { a.row_limit };
    let timeout = std::time::Duration::from_secs(a.timeout_secs);
    let mut last: Option<Table> = None;
    for (i, stmt) in plan.iter().enumerate() {
        let started_at = hist::Recorder::now_secs();
        let clock = std::time::Instant::now();
        let expired = || {
            dbc_core::QueryError::msg(format!(
                "příkaz {} překročil {} s (--timeout)",
                i + 1,
                a.timeout_secs
            ))
        };

        // Each statement of a batch is its own history row. A `.sql` file
        // is a sequence of things that happened, not one thing, and a
        // single row holding all of it could not be clicked back into the
        // editor as anything runnable.
        // `(rows to print, rows to record)` — a write has no table but
        // does have an affected-row count, and history wants that number
        // just as much as a select's.
        let outcome: Result<(Option<Table>, Option<i64>), String> = if stmt.is_read {
            runtime
                .block_on(async {
                    tokio::time::timeout(timeout, drain(conn, &stmt.sql, row_limit))
                        .await
                        .map_err(|_| expired())?
                })
                .map(|drained| {
                    let mut t = Table::new(drained.columns);
                    t.rows = drained.rows;
                    t.truncated = drained.truncated;
                    let counted = t.rows.len() as i64;
                    (Some(t), Some(counted))
                })
                .map_err(|e| e.message)
        } else {
            let cancel = CancelToken::new();
            runtime
                .block_on(async {
                    tokio::time::timeout(timeout, conn.execute(&stmt.sql, cancel))
                        .await
                        .map_err(|_| expired())?
                })
                .map(|affected| {
                    eprintln!("příkaz {}: {} řádků změněno", i + 1, affected);
                    (None, Some(affected as i64))
                })
                .map_err(|e| e.message)
        };
        let ms = Some(clock.elapsed().as_millis() as i64);

        // A FAILED statement is recorded too, with its error — that is the
        // run you most want to find again, and it is what the app's own
        // recorder does.
        match &outcome {
            Ok((_, rows)) => recorder.record(&stmt.sql, conn_label, started_at, ms, *rows, None),
            Err(e) => recorder.record(&stmt.sql, conn_label, started_at, ms, None, Some(e)),
        }
        // Only NOW may the error end the batch: recorded first, propagated
        // second.
        if let (Some(table), _) = outcome? {
            last = Some(table);
        }
    }
    Ok(last)
}

fn read_sql(source: &SqlSource) -> Result<String, String> {
    match source {
        SqlSource::Text(t) => Ok(t.clone()),
        SqlSource::File(p) => std::fs::read_to_string(p)
            .map_err(|e| format!("soubor {} nejde přečíst: {e}", p.display())),
        SqlSource::Stdin => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("standardní vstup nejde přečíst: {e}"))?;
            Ok(buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use args::Format;

    #[test]
    fn every_engine_maps_to_a_dialect() {
        assert_eq!(dialect_for(Engine::Postgres), Dialect::Postgres);
        assert_eq!(dialect_for(Engine::Mssql), Dialect::Mssql);
        assert_eq!(dialect_for(Engine::Sqlite), Dialect::Sqlite);
        assert_eq!(dialect_for(Engine::Duckdb), Dialect::Sqlite);
    }

    /// A usage error and a runtime error must not share an exit code — a
    /// script's `if dbc …` needs to tell „I called it wrong" from „the
    /// server said no".
    #[test]
    fn the_two_failure_exit_codes_are_distinct_and_not_success() {
        assert_ne!(EXIT_USAGE, EXIT_ERROR);
        assert_ne!(EXIT_USAGE, 0);
        assert_ne!(EXIT_ERROR, 0);
    }

    #[test]
    fn sql_comes_back_verbatim_from_text_and_from_a_file() {
        assert_eq!(read_sql(&SqlSource::Text("select 1".into())).unwrap(), "select 1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.sql");
        std::fs::write(&path, "select 42;\n").unwrap();
        assert_eq!(read_sql(&SqlSource::File(path)).unwrap(), "select 42;\n");
    }

    #[test]
    fn a_missing_file_names_the_path_it_could_not_read() {
        let e = read_sql(&SqlSource::File(PathBuf::from("nope-does-not-exist.sql"))).unwrap_err();
        assert!(e.contains("nope-does-not-exist.sql"), "{e}");
    }

    #[test]
    fn the_connections_table_reports_the_read_only_flag_and_hides_no_secret() {
        let mut config = AppConfig::default();
        config.connections.push(dbc_state::ConnectionConfig {
            id: "conn-1".into(),
            name: "prod".into(),
            folder: vec![],
            engine: Engine::Postgres,
            host: "db.example".into(),
            port: Some(5432),
            database: "sales".into(),
            user: "app".into(),
            read_only: true,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        });
        let out = render::render(&connections_table(&config), Format::Table);
        assert!(out.contains("prod"), "{out}");
        assert!(out.contains("db.example:5432"), "{out}");
        assert!(out.contains("jen čtení"), "{out}");
        // Nothing about a password may ever reach stdout — not the value,
        // not a placeholder that implies one is there.
        assert!(!out.to_lowercase().contains("heslo"), "{out}");
        assert!(!out.contains("password"), "{out}");
    }

    /// A file engine has no host to print, and printing the saved `host`
    /// field (which is meaningless there) would read as a real server.
    #[test]
    fn a_file_engine_shows_no_host() {
        let mut config = AppConfig::default();
        config.connections.push(dbc_state::ConnectionConfig {
            id: "conn-2".into(),
            name: "local".into(),
            folder: vec![],
            engine: Engine::Sqlite,
            host: "leftover".into(),
            port: None,
            database: "C:/data/app.db".into(),
            user: String::new(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        });
        let out = render::render(&connections_table(&config), Format::Table);
        assert!(!out.contains("leftover"), "{out}");
        assert!(out.contains("app.db"), "{out}");
    }
}
