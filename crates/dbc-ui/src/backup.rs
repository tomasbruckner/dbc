//! G11: backup/restore command construction (pure) + external-process
//! orchestration (T3, appended below). Pure half has zero I/O — no
//! `std::process`, no `std::fs` reads beyond what's handed in as `&[u8]`.

use dbc_state::ConnectionConfig;

fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// --- Postgres argument builders -------------------------------------------

/// SECURITY: libpq's `-d`/`--dbname` argument is special-cased by
/// `pg_dump`/`pg_restore`/`psql` — if the value contains `=` (a
/// `keyword=value` conninfo string) or looks like a `scheme://` URI, it is
/// parsed as a FULL connection string that can override `host`/`port`/
/// `sslmode`/etc., not merely name a database. `ConnectionConfig.database`
/// is free-text with no validation elsewhere in this codebase, so a value
/// like `"dbname=x host=evil.example.com sslmode=disable"` would silently
/// redirect the spawned tool to an attacker-controlled host WHILE
/// `PGPASSWORD` is still set on that same child process's environment —
/// exfiltrating the real database password to the attacker's server. This
/// function rejects any dbname that could be reinterpreted this way, fail
/// closed, before it is ever placed into an argv. CONSIDERED AND RULED
/// OUT: `host`/`port`/`user` are NOT exposed to this same class of
/// injection — they are passed as their OWN separate `-h`/`-p`/`-U`
/// arguments, and libpq's conninfo-or-URI special-case parsing applies
/// only to the value given via `-d`/`--dbname`, never to `-h`/`-p`/`-U`.
pub fn validate_pg_dbname(name: &str) -> Result<(), String> {
    if name.contains('=') || name.contains("://") {
        Err(format!(
            "neplatný název databáze (obsahuje '=' nebo '://', což by mohlo být vyloženo jako connection string): {name}"
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgDumpFormat {
    Custom,
    Plain,
}

#[derive(Debug, Clone)]
pub struct PgBackupOptions {
    pub format: PgDumpFormat,
    /// `-Fc` only; ignored (never emitted) for `Plain` — design §2.
    pub compress: u8,
}

/// `pg_dump -h host -p port -U user -d database --format=c|p --file=<path>
/// [--compress=N] -v` (design §2: `-v` is what makes pg_dump emit the
/// per-object progress lines the log pane shows). PGPASSWORD is NEVER part
/// of this Vec — see the SECURITY test below. `cfg.database` is validated
/// via `validate_pg_dbname` first (SECURITY: conninfo/URI injection via
/// `-d` — see that function's doc comment); a rejected name never reaches
/// argv.
pub fn build_pg_dump_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    opts: &PgBackupOptions,
    out_path: &str,
) -> Result<Vec<String>, String> {
    validate_pg_dbname(&cfg.database)?;
    let mut args = vec![
        "-h".to_string(),
        target_host.to_string(),
        "-p".to_string(),
        target_port.to_string(),
        "-U".to_string(),
        cfg.user.clone(),
        "-d".to_string(),
        cfg.database.clone(),
        format!(
            "--format={}",
            match opts.format {
                PgDumpFormat::Custom => "c",
                PgDumpFormat::Plain => "p",
            }
        ),
        format!("--file={out_path}"),
    ];
    if opts.format == PgDumpFormat::Custom {
        args.push(format!("--compress={}", opts.compress.min(9)));
    }
    args.push("-v".to_string());
    Ok(args)
}

#[derive(Debug, Clone)]
pub struct PgRestoreOptions {
    pub clean_if_exists: bool,
    pub create_db: bool,
    pub no_owner_no_privileges: bool,
    pub single_transaction: bool,
}

impl Default for PgRestoreOptions {
    fn default() -> Self {
        Self {
            clean_if_exists: true,
            create_db: false,
            no_owner_no_privileges: true,
            single_transaction: true,
        }
    }
}

/// `pg_restore -h host -p port -U user -d database [--clean --if-exists]
/// [--create] [--no-owner --no-privileges] [-1] <dump_path>` — design §3.
/// `cfg.database` is validated via `validate_pg_dbname` first (SECURITY:
/// conninfo/URI injection via `-d` — see that function's doc comment).
pub fn build_pg_restore_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    opts: &PgRestoreOptions,
    dump_path: &str,
) -> Result<Vec<String>, String> {
    validate_pg_dbname(&cfg.database)?;
    let mut args = vec![
        "-h".to_string(),
        target_host.to_string(),
        "-p".to_string(),
        target_port.to_string(),
        "-U".to_string(),
        cfg.user.clone(),
        "-d".to_string(),
        cfg.database.clone(),
    ];
    if opts.clean_if_exists {
        args.push("--clean".to_string());
        args.push("--if-exists".to_string());
    }
    if opts.create_db {
        args.push("--create".to_string());
    }
    if opts.no_owner_no_privileges {
        args.push("--no-owner".to_string());
        args.push("--no-privileges".to_string());
    }
    if opts.single_transaction {
        args.push("-1".to_string());
    }
    args.push(dump_path.to_string());
    Ok(args)
}

/// `psql -h host -p port -U user -d database -f <dump_path>` — plain-SQL
/// restore, design §3 ("no equivalent transaction flag is forced").
/// `cfg.database` is validated via `validate_pg_dbname` first (SECURITY:
/// conninfo/URI injection via `-d` — see that function's doc comment).
pub fn build_psql_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    dump_path: &str,
) -> Result<Vec<String>, String> {
    validate_pg_dbname(&cfg.database)?;
    Ok(vec![
        "-h".to_string(),
        target_host.to_string(),
        "-p".to_string(),
        target_port.to_string(),
        "-U".to_string(),
        cfg.user.clone(),
        "-d".to_string(),
        cfg.database.clone(),
        "-f".to_string(),
        dump_path.to_string(),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat {
    Custom,
    Plain,
}

/// Sniffs the dump's first bytes for pg_restore's `PGDMP` custom-format
/// magic (design §3: "detects which by reading the dump's first bytes...
/// rather than trusting a file extension"); anything else is Plain.
pub fn detect_dump_format(bytes: &[u8]) -> DumpFormat {
    if bytes.starts_with(b"PGDMP") {
        DumpFormat::Custom
    } else {
        DumpFormat::Plain
    }
}

// --- MSSQL T-SQL builders --------------------------------------------------

/// `BACKUP DATABASE "db" TO DISK = N'server-path' WITH FORMAT, STATS = 10`.
/// Uses `dbc_core::quote_ident` (double-quote style) for the database name —
/// same caveat G7's plan already documented for MSSQL bracket-quoting: the
/// bracket-aware `admin_sql::quote_ident_for` does not exist as code
/// anywhere in this repo yet (only as a G10 plan document), and MSSQL is
/// entirely unwired at `connect::open_config` regardless (see this plan's
/// Spec section), so this SQL text is built and unit-tested but never
/// actually sent to a live MSSQL server by anything in this codebase today.
/// Follow-up once both land: switch to `admin_sql::quote_ident_for`.
pub fn build_backup_sql(database: &str, server_path: &str) -> String {
    format!(
        "BACKUP DATABASE {} TO DISK = N{} WITH FORMAT, STATS = 10",
        dbc_core::quote_ident(database),
        sql_string_literal(server_path)
    )
}

/// `RESTORE DATABASE "db" FROM DISK = N'server-path' WITH REPLACE, STATS = 10`.
pub fn build_restore_sql(database: &str, server_path: &str) -> String {
    format!(
        "RESTORE DATABASE {} FROM DISK = N{} WITH REPLACE, STATS = 10",
        dbc_core::quote_ident(database),
        sql_string_literal(server_path)
    )
}

/// `ALTER DATABASE "db" SET SINGLE_USER WITH ROLLBACK IMMEDIATE` (`multi:
/// false`) or `... SET MULTI_USER` (`multi: true`) — design §3.
pub fn build_single_user_sql(database: &str, multi: bool) -> String {
    let mode = if multi {
        "MULTI_USER"
    } else {
        "SINGLE_USER WITH ROLLBACK IMMEDIATE"
    };
    format!("ALTER DATABASE {} SET {mode}", dbc_core::quote_ident(database))
}

// --- SQLite -----------------------------------------------------------------

/// `"SQLite format 3\0"` — design CURATION item 4.
pub const SQLITE_MAGIC_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// True only when `bytes` is at least 16 bytes and its first 16 bytes are
/// byte-for-byte `SQLITE_MAGIC_HEADER`. Never panics on a short slice.
pub fn sqlite_magic_header_ok(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && &bytes[..16] == SQLITE_MAGIC_HEADER
}

/// `VACUUM INTO 'dest-path'` — design §2, `''`-doubling for embedded quotes.
pub fn build_vacuum_into_sql(dest_path: &str) -> String {
    format!("VACUUM INTO {}", sql_string_literal(dest_path))
}

// --- Shared read-only gate + redaction + confirm ---------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupOp {
    Backup,
    Restore,
}

/// Design CURATION item 2: the ONE documented exemption from the read-only
/// guard. `true` only for `Backup` — `Restore` is NEVER exempt, on any
/// engine, for any connection.
pub fn backup_exempt_from_read_only(op: BackupOp) -> bool {
    matches!(op, BackupOp::Backup)
}

/// The gate every T4 runner method calls first, before any I/O. Mirrors
/// `runner::guard_not_read_only`'s shape (same message, same
/// `Result<(), String>` — T4 maps this into a `QueryError` at the call
/// site since this module has no `dbc_core` error-type dependency of its
/// own beyond re-exported `QueryError`, kept separate here so this whole
/// module stays free of any `Connection`/async dependency).
pub fn guard_backup_restore_read_only(op: BackupOp, read_only: bool) -> Result<(), String> {
    if read_only && !backup_exempt_from_read_only(op) {
        Err("připojení je jen pro čtení".to_string())
    } else {
        Ok(())
    }
}

/// Replaces every occurrence of `secret` (when non-empty) with `***` —
/// applied to both the user-facing command-line echo and to any spawn
/// error string before it is ever surfaced (SECURITY requirement).
pub fn redact_secret(text: &str, secret: Option<&str>) -> String {
    match secret {
        Some(s) if !s.is_empty() => text.replace(s, "***"),
        _ => text.to_string(),
    }
}

/// `program` + `args` joined with spaces, then redacted — what the confirm
/// modal and the log pane's first line show the user.
pub fn display_command_line(program: &str, args: &[String], secret: Option<&str>) -> String {
    let joined = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    redact_secret(&joined, secret)
}

/// GitHub-delete-repo-pattern exact match (design §3) — case-sensitive,
/// no trimming (the user must type the exact name shown).
pub fn confirm_matches(typed: &str, expected: &str) -> bool {
    typed == expected
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    fn cfg() -> ConnectionConfig {
        ConnectionConfig {
            id: "c1".into(),
            name: "demo".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Postgres,
            host: "db.internal".into(),
            port: Some(5432),
            database: "shop".into(),
            user: "alice".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
        }
    }

    // --- SECURITY: PGPASSWORD never in argv ---
    const NASTY_PASSWORD: &str = "p'ss\"w$ord --format=evil";

    #[test]
    fn pg_dump_args_never_contain_the_password() {
        let opts = PgBackupOptions {
            format: PgDumpFormat::Custom,
            compress: 6,
        };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.backup").unwrap();
        assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
    }

    #[test]
    fn pg_restore_args_never_contain_the_password() {
        let args = build_pg_restore_args(
            &cfg(),
            "127.0.0.1",
            15432,
            &PgRestoreOptions::default(),
            r"D:\bk\shop.backup",
        )
        .unwrap();
        assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
    }

    #[test]
    fn psql_args_never_contain_the_password() {
        let args = build_psql_args(&cfg(), "127.0.0.1", 15432, r"D:\bk\shop.sql").unwrap();
        assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
    }

    // --- exact argv shape ---
    #[test]
    fn pg_dump_args_custom_format_includes_compress_and_verbose() {
        let opts = PgBackupOptions {
            format: PgDumpFormat::Custom,
            compress: 6,
        };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.backup").unwrap();
        assert_eq!(
            args,
            vec![
                "-h",
                "127.0.0.1",
                "-p",
                "15432",
                "-U",
                "alice",
                "-d",
                "shop",
                "--format=c",
                r"--file=D:\bk\shop.backup",
                "--compress=6",
                "-v",
            ]
        );
    }

    #[test]
    fn pg_dump_args_plain_format_omits_compress() {
        let opts = PgBackupOptions {
            format: PgDumpFormat::Plain,
            compress: 6,
        };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.sql").unwrap();
        assert!(!args.iter().any(|a| a.starts_with("--compress")));
        assert!(args.contains(&"--format=p".to_string()));
    }

    #[test]
    fn pg_dump_compress_clamped_to_9() {
        let opts = PgBackupOptions {
            format: PgDumpFormat::Custom,
            compress: 200,
        };
        let args = build_pg_dump_args(&cfg(), "h", 1, &opts, "f").unwrap();
        assert!(args.contains(&"--compress=9".to_string()));
    }

    #[test]
    fn pg_restore_default_options_shape() {
        let args = build_pg_restore_args(
            &cfg(),
            "127.0.0.1",
            15432,
            &PgRestoreOptions::default(),
            r"D:\bk\shop.backup",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "-h",
                "127.0.0.1",
                "-p",
                "15432",
                "-U",
                "alice",
                "-d",
                "shop",
                "--clean",
                "--if-exists",
                "--no-owner",
                "--no-privileges",
                "-1",
                r"D:\bk\shop.backup",
            ]
        );
    }

    #[test]
    fn pg_restore_all_options_off_is_bare() {
        let opts = PgRestoreOptions {
            clean_if_exists: false,
            create_db: false,
            no_owner_no_privileges: false,
            single_transaction: false,
        };
        let args = build_pg_restore_args(&cfg(), "h", 1, &opts, "f.backup").unwrap();
        assert_eq!(
            args,
            vec!["-h", "h", "-p", "1", "-U", "alice", "-d", "shop", "f.backup"]
        );
    }

    #[test]
    fn pg_restore_create_db_adds_flag() {
        let mut opts = PgRestoreOptions::default();
        opts.create_db = true;
        let args = build_pg_restore_args(&cfg(), "h", 1, &opts, "f.backup").unwrap();
        assert!(args.contains(&"--create".to_string()));
    }

    #[test]
    fn psql_args_shape() {
        let args = build_psql_args(&cfg(), "127.0.0.1", 15432, r"D:\bk\shop.sql").unwrap();
        assert_eq!(
            args,
            vec!["-h", "127.0.0.1", "-p", "15432", "-U", "alice", "-d", "shop", "-f", r"D:\bk\shop.sql"]
        );
    }

    // --- SECURITY: -d/--dbname conninfo/URI injection (validate_pg_dbname) ---
    #[test]
    fn validate_pg_dbname_rejects_the_exfiltration_probe() {
        assert!(validate_pg_dbname("dbname=x host=evil.example.com sslmode=disable").is_err());
    }

    #[test]
    fn validate_pg_dbname_rejects_bare_keyword_value_form() {
        assert!(validate_pg_dbname("dbname=x").is_err());
    }

    #[test]
    fn validate_pg_dbname_rejects_uri_form() {
        assert!(validate_pg_dbname("postgresql://evil.example.com/x").is_err());
        assert!(validate_pg_dbname("postgres://evil.example.com/x").is_err());
    }

    #[test]
    fn validate_pg_dbname_accepts_normal_names() {
        assert!(validate_pg_dbname("my_db").is_ok());
        assert!(validate_pg_dbname("shop prod ěščř").is_ok());
    }

    #[test]
    fn build_pg_dump_args_rejects_injectable_dbname() {
        let mut c = cfg();
        c.database = "dbname=x host=evil.example.com sslmode=disable".into();
        let opts = PgBackupOptions {
            format: PgDumpFormat::Custom,
            compress: 6,
        };
        assert!(build_pg_dump_args(&c, "127.0.0.1", 15432, &opts, "f").is_err());
    }

    #[test]
    fn build_pg_restore_args_rejects_injectable_dbname() {
        let mut c = cfg();
        c.database = "postgresql://evil.example.com/x".into();
        assert!(build_pg_restore_args(&c, "127.0.0.1", 15432, &PgRestoreOptions::default(), "f").is_err());
    }

    #[test]
    fn build_psql_args_rejects_injectable_dbname() {
        let mut c = cfg();
        c.database = "dbname=x".into();
        assert!(build_psql_args(&c, "127.0.0.1", 15432, "f").is_err());
    }

    // --- dump format sniff ---
    #[test]
    fn detects_custom_format_via_pgdmp_magic() {
        assert_eq!(detect_dump_format(b"PGDMP\x01\x0e\x00rest"), DumpFormat::Custom);
    }

    #[test]
    fn treats_anything_else_as_plain() {
        assert_eq!(
            detect_dump_format(b"-- pg_dump plain SQL\nCREATE TABLE"),
            DumpFormat::Plain
        );
        assert_eq!(detect_dump_format(b""), DumpFormat::Plain);
        assert_eq!(detect_dump_format(b"PGDM"), DumpFormat::Plain); // short prefix match, not full magic
    }

    // --- MSSQL SQL builders ---
    #[test]
    fn backup_sql_shape_and_quoting() {
        let sql = build_backup_sql("my\"db", r"D:\Backups\mydb.bak");
        assert_eq!(
            sql,
            "BACKUP DATABASE \"my\"\"db\" TO DISK = N'D:\\Backups\\mydb.bak' WITH FORMAT, STATS = 10"
        );
    }

    #[test]
    fn restore_sql_shape() {
        let sql = build_restore_sql("mydb", r"D:\Backups\mydb.bak");
        assert_eq!(
            sql,
            "RESTORE DATABASE \"mydb\" FROM DISK = N'D:\\Backups\\mydb.bak' WITH REPLACE, STATS = 10"
        );
    }

    #[test]
    fn single_user_sql_both_directions() {
        assert_eq!(
            build_single_user_sql("mydb", false),
            "ALTER DATABASE \"mydb\" SET SINGLE_USER WITH ROLLBACK IMMEDIATE"
        );
        assert_eq!(build_single_user_sql("mydb", true), "ALTER DATABASE \"mydb\" SET MULTI_USER");
    }

    #[test]
    fn path_quote_doubling_for_embedded_single_quote() {
        let sql = build_backup_sql("db", r"D:\o'brien\mydb.bak");
        assert!(sql.contains("D:\\o''brien\\mydb.bak"));
    }

    // --- SQLite magic header ---
    #[test]
    fn magic_header_ok_on_real_prefix() {
        let mut bytes = SQLITE_MAGIC_HEADER.to_vec();
        bytes.extend_from_slice(&[0u8; 100]);
        assert!(sqlite_magic_header_ok(&bytes));
    }

    #[test]
    fn magic_header_rejects_short_file() {
        assert!(!sqlite_magic_header_ok(b"SQLite fo"));
    }

    #[test]
    fn magic_header_rejects_wrong_bytes() {
        assert!(!sqlite_magic_header_ok(b"not a sqlite file at all, just text"));
    }

    #[test]
    fn magic_header_empty_never_panics() {
        assert!(!sqlite_magic_header_ok(b""));
    }

    #[test]
    fn vacuum_into_sql_shape_and_quoting() {
        assert_eq!(
            build_vacuum_into_sql(r"D:\bk\shop.sqlite"),
            "VACUUM INTO 'D:\\bk\\shop.sqlite'"
        );
        assert_eq!(
            build_vacuum_into_sql(r"D:\o'brien.sqlite"),
            "VACUUM INTO 'D:\\o''brien.sqlite'"
        );
    }

    // --- read-only exemption (design CURATION item 2, REQUIRED) ---
    #[test]
    fn backup_is_exempt_restore_is_not() {
        assert!(backup_exempt_from_read_only(BackupOp::Backup));
        assert!(!backup_exempt_from_read_only(BackupOp::Restore));
    }

    #[test]
    fn guard_matrix() {
        assert!(guard_backup_restore_read_only(BackupOp::Backup, true).is_ok());
        assert!(guard_backup_restore_read_only(BackupOp::Backup, false).is_ok());
        assert!(guard_backup_restore_read_only(BackupOp::Restore, false).is_ok());
        let err = guard_backup_restore_read_only(BackupOp::Restore, true).unwrap_err();
        assert!(!err.is_empty());
    }

    // --- redaction ---
    #[test]
    fn redact_secret_replaces_every_occurrence() {
        let text = "PGPASSWORD=hunter2 && pg_dump ... error: auth failed for hunter2";
        let redacted = redact_secret(text, Some("hunter2"));
        assert!(!redacted.contains("hunter2"));
        assert_eq!(redacted.matches("***").count(), 2);
    }

    #[test]
    fn redact_secret_none_or_empty_is_passthrough() {
        assert_eq!(redact_secret("hello", None), "hello");
        assert_eq!(redact_secret("hello", Some("")), "hello");
    }

    #[test]
    fn display_command_line_redacts_and_never_leaks_via_args_anyway() {
        let args = build_pg_dump_args(
            &cfg(),
            "h",
            1,
            &PgBackupOptions {
                format: PgDumpFormat::Custom,
                compress: 6,
            },
            "f",
        )
        .unwrap();
        let line = display_command_line("pg_dump", &args, Some(NASTY_PASSWORD));
        assert!(!line.contains(NASTY_PASSWORD));
    }

    // --- confirm_matches ---
    #[test]
    fn confirm_matches_exact_and_case_sensitive() {
        assert!(confirm_matches("shop_prod", "shop_prod"));
        assert!(!confirm_matches("Shop_Prod", "shop_prod"));
        assert!(!confirm_matches("shop_prod ", "shop_prod"));
        assert!(!confirm_matches("", "shop_prod"));
    }
}

// --- Process half: spawn/stream/kill-on-drop + PATH discovery -------------
//
// Generalized from `tunnel.rs`'s `Tunnel`/`spawn_ssh`/`ssh_binary` shape
// (`Stdio::null()`/`Stdio::piped()`/`CREATE_NO_WINDOW` reused verbatim) to
// any external program rather than a single hardcoded `ssh` binary — see
// this module's own doc header and G11 T3's plan Grounding section.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc::Sender, Arc, Mutex};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One log line, or a terminal outcome — the shape `runner.rs` (T4) streams
/// over an `mpsc::Sender` exactly like `QueryEvent` already streams query
/// progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupEvent {
    Log(String),
    Finished,
    Failed(String),
}

/// Shared, `Clone`-able kill switch for a spawned external process — held
/// by the UI (T6) so "Zrušit" can reach across the `spawn_blocking` thread
/// boundary. `cancel()` is idempotent and safe to call after the process
/// has already exited on its own (a no-op in that case).
#[derive(Clone)]
pub struct BackupHandle {
    child: Arc<Mutex<Option<Child>>>,
}

impl BackupHandle {
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                // On Windows, `child.kill()` alone only terminates the
                // immediate process (e.g. `cmd.exe`, or `pg_dump` itself if
                // it ever shells out to a helper). Any grandchild it spawned
                // inherits our piped stderr handle by default, which keeps
                // that pipe open — and `run_and_stream`'s read loop
                // blocked — until the orphaned grandchild exits on its own.
                // `taskkill /T /F` kills the whole process tree so a cancel
                // is prompt regardless of what the external tool spawns
                // underneath itself.
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    let pid = child.id();
                    let _ = Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .creation_flags(CREATE_NO_WINDOW)
                        .status();
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Spawns `program` with `args`, `PGPASSWORD` (when `password` is `Some`)
/// set on the CHILD'S ENVIRONMENT ONLY, stdin closed, stderr piped and
/// streamed line-by-line as `BackupEvent::Log` — mirrors
/// `tunnel.rs::spawn_ssh`'s `Stdio` shape and `CREATE_NO_WINDOW` use
/// exactly, generalized from one fixed binary (`ssh`) to any program.
///
/// DEVIATION from the plan's literal sample (documented here since it's
/// security/robustness load-bearing, not cosmetic): the sample had this
/// function block its OWN calling thread for the entire process lifetime
/// and only hand back `BackupHandle` on its final return — which means a
/// concurrent caller could never actually observe/cancel a still-running
/// process, since by the time the handle is available the process (and
/// thus the whole point of `cancel()`) is already over. Here, `spawn()`
/// happens synchronously (fast, still safe to call from a
/// `tokio::task::spawn_blocking` context per `Tunnel::open`'s convention —
/// see T4), then the line-streaming + `wait()` work moves onto a
/// dedicated, internally-owned `std::thread`, and `run_and_stream` itself
/// returns the `BackupHandle` as soon as the child is confirmed spawned —
/// matching this module's own doc comment ("held by the UI so 'Zrušit' can
/// reach across the thread boundary") and T4's own grounding note that the
/// handle must be available "once run_and_stream has actually spawned the
/// child", i.e. before the first log line, not after the last one.
///
/// Every line sent as `BackupEvent::Log`/`BackupEvent::Failed` is passed
/// through `redact_secret` first (SECURITY requirement — defense in depth
/// even though `password` is never in `args`).
pub fn run_and_stream(
    program: &str,
    args: &[String],
    password: Option<&str>,
    tx: &Sender<BackupEvent>,
) -> BackupHandle {
    let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let handle = BackupHandle { child: slot.clone() };

    // SECURITY / robustness (forward-noted from T2 review): reject an
    // interior NUL byte in the program name or any argument BEFORE ever
    // touching `Command` — `OsStr`'s Windows wide-string conversion would
    // itself refuse this at spawn time, but failing explicitly here keeps
    // the failure message clear and Czech-language-consistent instead of
    // surfacing a raw OS conversion error, and guarantees no panic on a
    // user-supplied path (e.g. a restore-source path picked via a file
    // dialog) regardless of platform.
    if program.contains('\0') || args.iter().any(|a| a.contains('\0')) {
        let _ = tx.send(BackupEvent::Failed(redact_secret(
            "neplatná cesta nebo argument: obsahuje NUL bajt",
            password,
        )));
        return handle;
    }

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(pw) = password {
        // SECURITY: env-only, never argv (see `args` above), never logged
        // (this line itself never gets sent as a BackupEvent).
        cmd.env("PGPASSWORD", pw);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(BackupEvent::Failed(redact_secret(
                &format!("failed to spawn '{program}': {e}"),
                password,
            )));
            return handle;
        }
    };

    let stderr = child.stderr.take();
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(child);
    }

    // Streaming + wait run on a dedicated thread so `run_and_stream` can
    // return `handle` to the caller right away — see the DEVIATION note on
    // this function's doc comment above.
    let tx_bg = tx.clone();
    let slot_bg = slot.clone();
    let password_owned = password.map(|p| p.to_string());
    let program_owned = program.to_string();
    std::thread::spawn(move || {
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let _ = tx_bg.send(BackupEvent::Log(redact_secret(&l, password_owned.as_deref())));
                    }
                    Err(_) => break, // pipe closed (process exited/killed) — fall through to wait()
                }
            }
        }

        // Take the child back out to `wait()` on it — a concurrent
        // `cancel()` may have already taken (and killed+waited) it, in
        // which case `taken` is `None` and this call is a no-op (the
        // process is already reaped).
        let taken = slot_bg.lock().ok().and_then(|mut g| g.take());
        match taken {
            Some(mut c) => match c.wait() {
                Ok(status) if status.success() => {
                    let _ = tx_bg.send(BackupEvent::Finished);
                }
                Ok(status) => {
                    let _ = tx_bg.send(BackupEvent::Failed(redact_secret(
                        &format!("{program_owned} skončil s chybou ({status})"),
                        password_owned.as_deref(),
                    )));
                }
                Err(e) => {
                    let _ = tx_bg.send(BackupEvent::Failed(redact_secret(
                        &format!("{program_owned}: {e}"),
                        password_owned.as_deref(),
                    )));
                }
            },
            None => {
                // Cancelled mid-stream — `cancel()` already killed+waited it.
                let _ = tx_bg.send(BackupEvent::Failed("přerušeno uživatelem".to_string()));
            }
        }
    });

    handle
}

/// Same PATH-probe shape as `tunnel.rs::ssh_binary` (`Command::new("where")`
/// on Windows), generalized to any program name — design §1.
///
/// SECURITY (CWE-427, binary planting — G11 T4 review MAJOR 1): returns the
/// FULLY RESOLVED path, never a bare name. `where`/`which` print the
/// resolved absolute path to stdout; this function reads and returns the
/// FIRST line (their own "first match wins" precedence — the same one the
/// shell's own command lookup uses when more than one PATH entry matches).
/// Handing back a bare name here would be unsafe: callers (`resolve_tool_path`,
/// `runner.rs`) pass this string straight to `Command::new`, and on Windows
/// `CreateProcess` searches the application directory and the CURRENT
/// WORKING DIRECTORY *before* PATH — a planted `pg_dump.exe` sitting in a
/// writable CWD would run instead of the real tool, and since `PGPASSWORD`
/// is set on that spawned child's environment, the planted binary would
/// receive the real database password. Returning the fully-resolved path
/// bypasses that CWD/app-dir search order entirely (`CreateProcess` uses a
/// path containing a directory separator as-is, never re-searching it).
pub fn find_on_path(name: &str) -> Option<String> {
    #[cfg(windows)]
    let probe = Command::new("where").arg(name).output();
    #[cfg(not(windows))]
    let probe = Command::new("which").arg(name).output();
    let output = probe.ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// Given already-discovered `(version_dir_path, mtime)` pairs (design §1:
/// `C:\Program Files\PostgreSQL\*`), picks the entry whose final path
/// component parses as the numerically highest `u32`; ties broken by mtime
/// (later wins); non-numeric final components are ignored entirely. `None`
/// on an empty or all-non-numeric input. Pure (mtime is supplied by the
/// caller, T4, which does the actual `std::fs::read_dir` — see T4's
/// Grounding for why this keeps the function itself hermetically
/// testable).
pub fn pick_highest_version_dir(dirs: &[(String, std::time::SystemTime)]) -> Option<String> {
    dirs.iter()
        .filter_map(|(path, mtime)| {
            let last = std::path::Path::new(path).file_name()?.to_str()?;
            last.parse::<u32>().ok().map(|v| (v, *mtime, path.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
        .map(|(_, _, path)| path)
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn missing_binary_is_a_failed_event_not_a_panic() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_and_stream("definitely-not-a-real-binary-xyz", &[], None, &tx);
        let ev = rx.recv().unwrap();
        assert!(matches!(ev, BackupEvent::Failed(msg) if msg.contains("definitely-not-a-real-binary-xyz")));
    }

    #[test]
    fn missing_binary_error_never_contains_the_password() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_and_stream("definitely-not-a-real-binary-xyz", &[], Some("hunter2"), &tx);
        let ev = rx.recv().unwrap();
        if let BackupEvent::Failed(msg) = ev {
            assert!(!msg.contains("hunter2"));
        } else {
            panic!("expected Failed");
        }
    }

    // --- SECURITY: interior NUL byte in program/arg rejected before spawn ---
    #[test]
    fn program_with_interior_nul_is_rejected_before_spawn() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_and_stream("cmd\0evil", &[], None, &tx);
        let ev = rx.recv().unwrap();
        assert!(matches!(ev, BackupEvent::Failed(_)));
    }

    #[test]
    fn arg_with_interior_nul_is_rejected_before_spawn() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_and_stream("cmd", &["/C".to_string(), "echo\0evil".to_string()], None, &tx);
        let ev = rx.recv().unwrap();
        assert!(matches!(ev, BackupEvent::Failed(_)));
    }

    #[test]
    fn interior_nul_rejection_never_contains_the_password() {
        let (tx, rx) = std::sync::mpsc::channel();
        run_and_stream("cmd", &["\0".to_string()], Some("hunter2"), &tx);
        let ev = rx.recv().unwrap();
        if let BackupEvent::Failed(msg) = ev {
            assert!(!msg.contains("hunter2"));
        } else {
            panic!("expected Failed");
        }
    }

    // Real-subprocess test using a universally-available Windows command —
    // same trick tunnel.rs's own test suite relies on (a fixed system
    // binary, not pg_dump) so this runs in ordinary `cargo test`, no docker,
    // no external tool install.
    #[test]
    #[cfg(windows)]
    fn real_spawn_streams_stdout_lines_as_log_events_and_finishes() {
        let (tx, rx) = std::sync::mpsc::channel();
        // `cmd /C echo line1 & echo line2` writes to STDOUT, not stderr —
        // redirect with `1>&2` so it lands on the piped stream this function
        // actually reads.
        run_and_stream(
            "cmd",
            &["/C".to_string(), "echo line1 1>&2 & echo line2 1>&2".to_string()],
            None,
            &tx,
        );
        let mut lines = Vec::new();
        let mut finished = false;
        while let Ok(ev) = rx.recv() {
            match ev {
                BackupEvent::Log(l) => lines.push(l),
                BackupEvent::Finished => {
                    finished = true;
                    break;
                }
                BackupEvent::Failed(m) => panic!("unexpected failure: {m}"),
            }
        }
        assert!(finished);
        assert!(lines.iter().any(|l| l.contains("line1")));
        assert!(lines.iter().any(|l| l.contains("line2")));
    }

    #[test]
    #[cfg(windows)]
    fn cancel_kills_a_long_running_process_before_it_finishes() {
        let (tx, rx) = std::sync::mpsc::channel();
        let program = "cmd".to_string();
        let args = vec!["/C".to_string(), "ping -n 30 127.0.0.1 >nul".to_string()];
        let handle_slot: std::sync::Arc<std::sync::Mutex<Option<BackupHandle>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let handle_slot2 = handle_slot.clone();
        let t = std::thread::spawn(move || {
            let h = run_and_stream(&program, &args, None, &tx);
            *handle_slot2.lock().unwrap() = Some(h);
        });
        // Give the process a moment to actually spawn before cancelling.
        std::thread::sleep(Duration::from_millis(300));
        // Poll for the handle to exist (spawn happens early in run_and_stream).
        for _ in 0..50 {
            if let Some(h) = handle_slot.lock().unwrap().as_ref() {
                h.cancel();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let start = std::time::Instant::now();
        let ev = rx.recv().unwrap();
        t.join().unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(25),
            "cancel should end the 30s ping almost immediately"
        );
        assert!(matches!(ev, BackupEvent::Failed(_)));
    }

    #[test]
    fn find_on_path_finds_a_universally_present_binary() {
        // `cmd.exe` (Windows) is always on PATH in this repo's CI/dev
        // environment (the tool this test itself just spawned above).
        #[cfg(windows)]
        {
            let resolved = find_on_path("cmd").expect("cmd must be on PATH");
            // SECURITY (CWE-427, T4 review MAJOR 1): must be the fully
            // resolved path, never a bare name — see `find_on_path`'s doc
            // comment.
            assert!(
                std::path::Path::new(&resolved).is_absolute(),
                "expected an absolute path, got: {resolved}"
            );
        }
    }

    #[test]
    fn find_on_path_missing_binary_is_false() {
        assert!(find_on_path("definitely-not-a-real-binary-xyz").is_none());
    }

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn picks_highest_numeric_version() {
        let dirs = vec![
            (r"C:\Program Files\PostgreSQL\14".to_string(), t(100)),
            (r"C:\Program Files\PostgreSQL\16".to_string(), t(200)),
            (r"C:\Program Files\PostgreSQL\9".to_string(), t(50)),
        ];
        assert_eq!(
            pick_highest_version_dir(&dirs).as_deref(),
            Some(r"C:\Program Files\PostgreSQL\16")
        );
    }

    #[test]
    fn ignores_non_numeric_dirs() {
        let dirs = vec![
            (r"C:\Program Files\PostgreSQL\16".to_string(), t(100)),
            (r"C:\Program Files\PostgreSQL\pgAdmin 4".to_string(), t(999)),
        ];
        assert_eq!(
            pick_highest_version_dir(&dirs).as_deref(),
            Some(r"C:\Program Files\PostgreSQL\16")
        );
    }

    #[test]
    fn ties_broken_by_later_mtime() {
        let dirs = vec![
            (r"C:\A\16".to_string(), t(100)),
            (r"C:\B\16".to_string(), t(200)),
        ];
        assert_eq!(pick_highest_version_dir(&dirs).as_deref(), Some(r"C:\B\16"));
    }

    #[test]
    fn empty_or_all_non_numeric_is_none() {
        assert_eq!(pick_highest_version_dir(&[]), None);
        assert_eq!(pick_highest_version_dir(&[(r"C:\x\abc".to_string(), t(1))]), None);
    }
}
