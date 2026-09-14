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
mod targets_cli;
mod vault_key;

use std::path::Path;
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
fn resolve_paths(a: &Args) -> Result<dbc_state::workspace::Paths, String> {
    use dbc_state::workspace::Resolution;
    // History is machine-local in BOTH modes (§W5), so it comes from the
    // same resolution as the rest and simply never points into a shared
    // workspace folder.
    let mut paths = match dbc_state::workspace::resolve() {
        Resolution::Profile(p) => p,
        Resolution::Workspace { paths, .. } => paths,
        Resolution::Broken { root, reason } => {
            let named = root
                .map(|r| r.display().to_string())
                .unwrap_or_else(|| "ukazatel na pracovní prostor je nečitelný".to_string());
            let reason = dbc_state::workspace::one_line_reason(&reason);
            match (&a.config, &a.vault) {
                (Some(c), Some(v)) => dbc_state::workspace::Paths {
                    config: c.clone(),
                    vault: v.clone(),
                    // A broken pointer says nothing about where the rest of
                    // the context lives, and the two overrides do not cover
                    // them. Beside the config the user named is the only
                    // defensible guess.
                    views: c.with_file_name("views.toml"),
                    params: c.with_file_name("params.toml"),
                    history: dbc_state::default_history_path(),
                },
                _ => {
                    return Err(format!(
                        "{named} ({reason})\nspusť aplikaci a vyber pracovní prostor znovu, \
                         nebo zadej --config a --vault"
                    ))
                }
            }
        }
    };
    if let Some(c) = &a.config {
        // `views` and `params` follow the config rather than staying at the
        // profile's. `--config` names a CONTEXT, and the two files are keyed
        // by the connection ids inside that config — leaving them pointed at
        // `%APPDATA%\dbc` would have `dbc export --config <elsewhere>` bundle
        // one machine's connections with another context's column widths.
        // Only `export`/`import` read them, so nothing else notices.
        paths.views = c.with_file_name("views.toml");
        paths.params = c.with_file_name("params.toml");
        paths.config = c.clone();
    }
    if let Some(v) = &a.vault {
        paths.vault = v.clone();
    }
    Ok(paths)
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

pub(crate) fn dialect_for(engine: Engine) -> Dialect {
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

    let paths = resolve_paths(&a)?;
    let (config_path, vault_path, history_path) =
        (paths.config.clone(), paths.vault.clone(), paths.history.clone());

    match &a.command {
        Command::Help | Command::Version => unreachable!("handled above"),
        Command::Login => return login(&vault_path),
        Command::Logout => {
            vault_key::forget_key()?;
            println!("uložený klíč trezoru smazán");
            return Ok(());
        }
        // Both open no database and need no password: a bundle is files,
        // and the vault inside it is never unsealed.
        Command::Export { file } => return export(&paths, file),
        Command::Import { file, force } => return import(&paths, file, *force),
        _ => {}
    }

    let config = load_config(&config_path)?;

    if matches!(a.command, Command::Connections) {
        print!("{}", render::render(&connections_table(&config), a.format));
        return Ok(());
    }

    // Every query — one positional connection or many `--on` targets — goes
    // through the same path, so the safety order below holds for both.
    if let Command::Query { conn, targets, targets_file, sql, write } = &a.command {
        let ctx = QueryContext {
            a: &a,
            config: &config,
            vault_path: &vault_path,
            history_path: &history_path,
        };
        return query(&ctx, conn.as_deref(), targets, targets_file.as_deref(), sql, *write);
    }

    // Everything below needs an actual connection.
    let asked_for = match &a.command {
        Command::Databases { conn } | Command::Tables { conn, .. } => conn.as_str(),
        _ => unreachable!("handled above"),
    };
    let cfg = pick::pick(&config.connections, asked_for)
        .map_err(|e| e.message(asked_for))?
        .clone();

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
        Command::Databases { .. } => databases_table(&runtime, &mut *conn, &target)?,
        Command::Tables { schema, .. } => tables_table(&runtime, &mut *conn, schema.as_deref())?,
        _ => unreachable!("handled above"),
    };
    print!("{}", render::render(&table, a.format));
    Ok(())
}

/// What `query` needs from the invocation besides the command itself.
struct QueryContext<'a> {
    a: &'a Args,
    config: &'a AppConfig,
    vault_path: &'a Path,
    history_path: &'a Path,
}

/// `dbc query`, over one target or many. The order is a safety property
/// (spec §4), not a convenience:
///
/// 1. every named connection is looked up in the config;
/// 2. the run is DECIDED from the SQL and the saved read-only flags — a
///    refused write must not have cost a master-password prompt or a
///    connection, and being asked for a password before being told „no"
///    teaches people to type it reflexively;
/// 3. the vault is opened once, and only if some connection needs it;
/// 4. globs are expanded live, one enumeration per connection;
/// 5. everything runs through the shared fan-out.
///
/// The positional form (`dbc query prod --db sklad`) is exactly one target
/// and prints what it always printed; only the `--on` form gets the
/// per-target sections.
fn query(
    ctx: &QueryContext,
    conn: Option<&str>,
    targets: &[String],
    targets_file: Option<&Path>,
    sql: &SqlSource,
    write: bool,
) -> Result<(), String> {
    let a = ctx.a;
    // 1. Targets against the config: the positional connection (+ `--db`,
    // taken verbatim), or the `--on` / `--on-file` texts.
    let resolved = match conn {
        Some(c) => vec![targets_cli::resolve_positional(ctx.config, c, a.database.as_deref())?],
        None => {
            let mut texts = targets.to_vec();
            if let Some(f) = targets_file {
                texts.extend(targets_cli::read_targets_file(f)?);
            }
            // A file of nothing but comments must not be a run of nothing
            // that exits 0 — that is the run a script would never notice.
            if texts.is_empty() {
                return Err(match targets_file {
                    Some(f) => format!(
                        "žádný cíl ke spuštění — soubor {} neobsahuje žádný conn/db",
                        f.display()
                    ),
                    None => "žádný cíl ke spuštění".to_string(),
                });
            }
            targets_cli::resolve_texts(ctx.config, &texts)?
        }
    };

    // 2. DECIDED BEFORE ANYTHING IS OPENED.
    let text = read_sql(sql)?;
    targets_cli::preflight(&text, &resolved, write)?;

    // 3. The vault, once, only if some connection needs a secret.
    let needs_vault = resolved.iter().any(|r| !dbc_state::engine_is_file_based(r.cfg.engine))
        && Vault::exists(ctx.vault_path);
    let vault = if needs_vault { Some(vault_key::unlock(ctx.vault_path)?) } else { None };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    // 4. Globs, live — the same enumeration `dbc databases` prints.
    let list_dbs = |cfg: &dbc_state::ConnectionConfig| -> Result<(Vec<String>, bool), String> {
        let secret = dbc_connect::resolve_secret_for_connect(vault.as_ref(), cfg);
        let opened =
            dbc_connect::open_config(cfg, secret, runtime.handle()).map_err(|e| e.message)?;
        let mut conn = opened.conn;
        let t = databases_table(&runtime, &mut *conn, cfg)?;
        let names = t.rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        Ok((names, t.truncated))
    };
    let plan_targets = targets_cli::expand(resolved, list_dbs)?;

    // 5. Run, then record. History is written here rather than inside the
    // targets because the recorder is not `Send`, and each target ran on
    // a runtime worker.
    let reports =
        run_targets_cli(&runtime, ctx.config, vault.as_ref(), &text, write, a, plan_targets)?;
    let mut recorder = hist::Recorder::open(ctx.history_path);
    for r in &reports {
        for h in &r.history {
            recorder.record(&h.sql, &r.label, h.started_at, h.ms, h.rows, h.error.as_deref());
        }
        for (i, n) in &r.affected {
            eprintln!("{}: příkaz {}: {} řádků změněno", r.label, i + 1, n);
        }
    }

    if conn.is_some() && reports.len() == 1 {
        // The pre-existing shape on stdout, byte for byte; a failure is
        // the plain `dbc: …` line and exit 1, as it always was.
        let r = &reports[0];
        if let Some(e) = &r.error {
            return Err(e.clone());
        }
        // `None` = a batch of pure writes. Their affected-row counts already
        // went to stderr; printing an empty table with a made-up column
        // would put a fake result in the pipe.
        if let Some(t) = &r.table {
            print!("{}", render::render(t, a.format));
        }
        return Ok(());
    }

    for r in &reports {
        if let Some(e) = &r.error {
            eprintln!("{}: chyba: {e}", r.label);
        }
    }
    print!("{}", render::render_targets(&reports, a.format));
    let failed = reports.iter().filter(|r| r.error.is_some()).count();
    if failed > 0 {
        return Err(format!("{failed} z {} cílů selhalo", reports.len()));
    }
    Ok(())
}

/// Every target on its own connection, at most `MAX_PARALLEL_TARGETS` at
/// once, each producing one `TargetReport`. The body is `run_one_target`,
/// which runs on a runtime worker instead of under `block_on`, so four
/// targets really are in flight together.
fn run_targets_cli(
    runtime: &tokio::runtime::Runtime,
    config: &AppConfig,
    vault: Option<&Vault>,
    sql: &str,
    write: bool,
    a: &Args,
    targets: Vec<dbc_connect::targets::Target>,
) -> Result<Vec<targets_cli::TargetReport>, String> {
    use dbc_connect::fanout::{run_targets, TargetEvent, MAX_PARALLEL_TARGETS};

    // One plan per target: its own dialect and read-only flag. The
    // preflight already decided the run may happen; this only re-derives
    // the statement list each target executes.
    let mut jobs = Vec::with_capacity(targets.len());
    for t in &targets {
        let saved = config
            .connections
            .iter()
            .find(|c| c.id == t.conn_id)
            .ok_or_else(|| format!("připojení {} zmizelo z configu", t.conn_name))?;
        let mut cfg = saved.clone();
        cfg.database = t.database.clone();
        let plan = policy::plan(sql, dialect_for(cfg.engine), cfg.read_only, write)
            .map_err(|e| e.message())?;
        let secret = dbc_connect::resolve_secret_for_connect(vault, &cfg);
        jobs.push(Some((t.label(), cfg, secret, plan)));
    }
    let n = jobs.len();
    let jobs = std::sync::Arc::new(std::sync::Mutex::new(jobs));
    let limits = Limits {
        row_limit: if a.row_limit == 0 { usize::MAX } else { a.row_limit },
        timeout_secs: a.timeout_secs,
    };
    let handle = runtime.handle().clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut reports: Vec<Option<targets_cli::TargetReport>> = (0..n).map(|_| None).collect();
    runtime.block_on(async {
        let driver = tokio::spawn(run_targets(
            n,
            MAX_PARALLEL_TARGETS,
            CancelToken::new(),
            move |ix, inner| {
                let job = jobs.lock().unwrap().get_mut(ix).and_then(Option::take);
                let handle = handle.clone();
                async move {
                    let Some((label, cfg, secret, plan)) = job else { return false };
                    let report = run_one_target(&handle, label, cfg, secret, &plan, limits).await;
                    let ok = report.error.is_none();
                    // The body owns the only sender; it drops when this
                    // future ends, which is what lets the fan-out finish.
                    let _ = inner.send(report).await;
                    ok
                }
            },
            tx,
        ));
        // The channel closes once every sender is gone, i.e. once the
        // fan-out and all its forwarders have ended.
        while let Some(ev) = rx.recv().await {
            if let TargetEvent::Inner { target_ix, event } = ev {
                reports[target_ix] = Some(event);
            }
        }
        let _ = driver.await;
    });
    Ok(reports
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            r.unwrap_or_else(|| {
                targets_cli::TargetReport::failed(targets[i].label(), "cíl neproběhl".into())
            })
        })
        .collect())
}

/// The per-target caps, copied out of `Args` so the fan-out body can be
/// `'static`.
#[derive(Clone, Copy)]
struct Limits {
    row_limit: usize,
    timeout_secs: u64,
}

/// One target: connect, run the planned batch, report.
///
/// The LAST statement that returns rows is what gets printed. A `.sql`
/// file that ends in a `SELECT` therefore prints that select, which is
/// what a person running a script expects; writes collect their affected
/// row counts for stderr, so a pipe carrying the result stays clean. The
/// first failing statement ends the target — recorded first, with its
/// error, then reported — and clears any earlier result, so an error never
/// arrives with a half-result beside it.
async fn run_one_target(
    handle: &tokio::runtime::Handle,
    label: String,
    cfg: dbc_state::ConnectionConfig,
    secret: Option<String>,
    plan: &[policy::Stmt],
    limits: Limits,
) -> targets_cli::TargetReport {
    let clock_all = std::time::Instant::now();
    // `open_config` does blocking I/O (tunnels, handshakes) and says so.
    let h = handle.clone();
    let opened =
        tokio::task::spawn_blocking(move || dbc_connect::open_config(&cfg, secret, &h)).await;
    let mut opened = match opened {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return targets_cli::TargetReport::failed(label, e.message),
        Err(_) => return targets_cli::TargetReport::failed(label, "připojování selhalo".into()),
    };
    let conn = &mut *opened.conn;
    let mut report = targets_cli::TargetReport {
        label,
        table: None,
        error: None,
        affected: Vec::new(),
        history: Vec::new(),
        elapsed_ms: 0,
    };
    let timeout = std::time::Duration::from_secs(limits.timeout_secs);
    for (i, stmt) in plan.iter().enumerate() {
        let started_at = hist::Recorder::now_secs();
        let clock = std::time::Instant::now();
        let expired =
            || format!("příkaz {} překročil {} s (--timeout)", i + 1, limits.timeout_secs);
        // `(rows to print, rows to record)` — a write has no table but
        // does have an affected-row count, and history wants that number
        // just as much as a select's.
        let outcome: Result<(Option<Table>, Option<i64>), String> = if stmt.is_read {
            match tokio::time::timeout(timeout, drain(conn, &stmt.sql, limits.row_limit)).await {
                Err(_) => Err(expired()),
                Ok(Err(e)) => Err(e.message),
                Ok(Ok(d)) => {
                    let mut t = Table::new(d.columns);
                    t.rows = d.rows;
                    t.truncated = d.truncated;
                    let n = t.rows.len() as i64;
                    Ok((Some(t), Some(n)))
                }
            }
        } else {
            match tokio::time::timeout(timeout, conn.execute(&stmt.sql, CancelToken::new())).await
            {
                Err(_) => Err(expired()),
                Ok(Err(e)) => Err(e.message),
                Ok(Ok(n)) => {
                    report.affected.push((i, n));
                    Ok((None, Some(n as i64)))
                }
            }
        };
        let ms = Some(clock.elapsed().as_millis() as i64);
        // Each statement of a batch is its own history row, a FAILED one
        // too — that is the run you most want to find again.
        report.history.push(targets_cli::HistRow {
            sql: stmt.sql.clone(),
            started_at,
            ms,
            rows: outcome.as_ref().ok().and_then(|(_, rows)| *rows),
            error: outcome.as_ref().err().cloned(),
        });
        match outcome {
            Ok((Some(t), _)) => report.table = Some(t),
            Ok(_) => {}
            Err(e) => {
                report.table = None;
                report.error = Some(e);
                break;
            }
        }
    }
    report.elapsed_ms = clock_all.elapsed().as_millis();
    report
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

/// `dbc export <soubor>` — write the portable bundle.
///
/// Deliberately does NOT ask for the master password. Nothing is decrypted:
/// the vault goes into the file as the sealed envelope it already is. Asking
/// would imply a protection the export does not add, and would stop the
/// command working from a script for no gain — the file on disk was already
/// readable to whoever runs this.
fn export(paths: &dbc_state::workspace::Paths, file: &Path) -> Result<(), String> {
    let bundle =
        dbc_state::bundle::build(paths, env!("CARGO_PKG_VERSION")).map_err(|e| e.message)?;
    let summary = dbc_state::bundle::summary(&bundle).map_err(|e| e.message)?;
    dbc_state::bundle::write(&bundle, file).map_err(|e| e.message)?;

    println!("{} — {}", file.display(), plural_connections(summary.connections.len()));
    if summary.has_vault {
        println!("trezor je uvnitř, pořád zašifrovaný — hesla v souboru čitelná nejsou");
        println!("na druhém počítači ho otevřeš tím samým master heslem jako tady");
    } else {
        println!("trezor zatím neexistuje, takže v souboru žádná hesla nejsou");
    }
    println!("nepřenáší se: historie, hodnoty parametrů, log, cesty k psql/sqlcmd");
    Ok(())
}

/// `dbc import <soubor>` — replace this context's settings with a bundle's.
///
/// The `--force` gate is on the CONFIG's existence, not on the vault's: the
/// config is what carries the connections, so its presence is what makes
/// this a replacement rather than a first setup. A CLI cannot put a confirm
/// dialog in front of a destructive act, so the flag is the dialog — and the
/// refusal names the file that would have been overwritten, because that is
/// the fact needed to decide.
fn import(paths: &dbc_state::workspace::Paths, file: &Path, force: bool) -> Result<(), String> {
    // Read and validate FIRST: a bundle that turns out to be unusable must
    // not have caused a backup, a warning, or a single write.
    let bundle = dbc_state::bundle::read(file).map_err(|e| e.message)?;
    let summary = dbc_state::bundle::summary(&bundle).map_err(|e| e.message)?;

    if paths.config.exists() && !force {
        return Err(format!(
            "{} už existuje a import ho nahradí ({} v balíčku) — přidej --force, \
             pokud to je záměr; původní soubory se odloží stranou",
            paths.config.display(),
            plural_connections(summary.connections.len())
        ));
    }

    let applied = dbc_state::bundle::apply(&bundle, paths).map_err(|e| e.message)?;

    println!("nastaveno z {} — {}", file.display(), plural_connections(summary.connections.len()));
    for name in &summary.connections {
        println!("  {name}");
    }
    if summary.has_vault {
        println!("trezor nahrazen — otevírá se master heslem z původního počítače");
    }
    if applied.backed_up.is_empty() {
        println!("nic se nepřepisovalo, profil byl prázdný");
    } else {
        println!("původní soubory odloženy stranou:");
        for (_, bak) in &applied.backed_up {
            println!("  {}", bak.display());
        }
    }
    Ok(())
}

/// `připojení` is a neuter `-í` noun: one form for every count, so this is
/// only here to keep the number and the word together at each call site.
fn plural_connections(n: usize) -> String {
    format!("{n} připojení")
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
    use std::path::PathBuf;

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

    /// A `dbc query` invocation without argv: the command is what the
    /// parser would have produced, the paths are a temp dir's, and the
    /// vault path does NOT exist — so a vault prompt is impossible and a
    /// connect attempt is the only way a test here could stall.
    struct Harness {
        args: Args,
        config: AppConfig,
        dir: tempfile::TempDir,
    }

    impl Harness {
        fn new(config: AppConfig, command: Command, database: Option<&str>) -> Harness {
            Harness {
                args: Args {
                    command,
                    database: database.map(str::to_string),
                    format: Format::Csv,
                    row_limit: args::DEFAULT_ROW_LIMIT,
                    timeout_secs: args::DEFAULT_TIMEOUT_SECS,
                    config: None,
                    vault: None,
                },
                config,
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn run(&self) -> Result<(), String> {
            let Command::Query { conn, targets, targets_file, sql, write } = &self.args.command
            else {
                unreachable!("harness builds a query")
            };
            let ctx = QueryContext {
                a: &self.args,
                config: &self.config,
                vault_path: &self.dir.path().join("no-such-vault.bin"),
                history_path: &self.dir.path().join("history.sqlite"),
            };
            query(&ctx, conn.as_deref(), targets, targets_file.as_deref(), sql, *write)
        }
    }

    fn conn_cfg(id: &str, name: &str, engine: Engine, database: &str, ro: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: id.into(),
            name: name.into(),
            folder: vec![],
            engine,
            // TEST-NET-3 (RFC 5737): never routable. Meaningless for a
            // file engine, which is exactly the point — a file engine
            // never looks at it.
            host: "203.0.113.1".into(),
            port: Some(5432),
            database: database.into(),
            user: "u".into(),
            read_only: ro,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    fn on(targets: &[&str], targets_file: Option<PathBuf>, sql: &str, write: bool) -> Command {
        Command::Query {
            conn: None,
            targets: targets.iter().map(|s| s.to_string()).collect(),
            targets_file,
            sql: SqlSource::Text(sql.into()),
            write,
        }
    }

    /// Spec §4 ordering, through the REAL `query()`: a read-only server
    /// connection on an unreachable host, named with a glob (which would
    /// need the server's database list), and a write. The refusal must
    /// come from the config alone — long before the connect timeout, and
    /// with no vault to unlock. A connect attempt here would take the
    /// driver's full timeout and fail the bound.
    #[test]
    fn query_refuses_a_read_only_write_before_any_connect_or_vault() {
        let mut config = AppConfig::default();
        config.connections.push(conn_cfg("c1", "archiv", Engine::Postgres, "db", true));
        let h = Harness::new(config, on(&["archiv/klient_*"], None, "delete from t", true), None);
        let clock = std::time::Instant::now();
        let e = h.run().unwrap_err();
        assert!(e.contains("jen pro čtení") && e.contains("archiv"), "{e}");
        assert!(clock.elapsed() < std::time::Duration::from_secs(2), "a connection was attempted");
    }

    /// `--on-file` with nothing but comments is not a run of nothing that
    /// exits 0; it is an error naming the file.
    #[test]
    fn query_refuses_an_empty_target_list() {
        let mut config = AppConfig::default();
        config.connections.push(conn_cfg("c1", "prod", Engine::Sqlite, "x.db", false));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("targets.txt");
        std::fs::write(&file, "# nothing here\n\n   \n").unwrap();
        let h = Harness::new(config, on(&[], Some(file.clone()), "select 1", false), None);
        let e = h.run().unwrap_err();
        assert!(e.contains("žádný cíl"), "{e}");
        assert!(e.contains(&file.display().to_string()), "{e}");
    }

    /// The positional form resolves the NAME verbatim — a slash in it is
    /// not a `conn/db` separator — and then really runs, end to end, on a
    /// SQLite file the test owns.
    #[test]
    fn a_positional_connection_named_with_a_slash_runs() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db").display().to_string();
        let mut config = AppConfig::default();
        config.connections.push(conn_cfg("c1", "a/b", Engine::Sqlite, &db, false));
        let command = Command::Query {
            conn: Some("a/b".into()),
            targets: vec![],
            targets_file: None,
            sql: SqlSource::Text("select 1 as one".into()),
            write: false,
        };
        let h = Harness::new(config, command, None);
        h.run().unwrap();
        // Same name with `--db`: the value is the database, taken as is
        // (glob characters are illegal in a Windows file name, so the
        // `k_*` → `Named` half lives in `targets_cli`'s unit test). A file
        // that does not exist yet is created by SQLite, which is how the
        // run proves which path it opened.
        let literal = dir.path().join("k_x.db").display().to_string();
        let mut config = AppConfig::default();
        config.connections.push(conn_cfg("c1", "a/b", Engine::Sqlite, &db, false));
        let command = Command::Query {
            conn: Some("a/b".into()),
            targets: vec![],
            targets_file: None,
            sql: SqlSource::Text("select 1 as one".into()),
            write: false,
        };
        Harness::new(config, command, Some(&literal)).run().unwrap();
        assert!(std::path::Path::new(&literal).exists());
    }
}
