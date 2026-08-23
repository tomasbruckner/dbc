//! G11: backup/restore command construction (pure) + external-process
//! orchestration (T3, appended below). Pure half has zero I/O — no
//! `std::process`, no `std::fs` reads beyond what's handed in as `&[u8]`.

use dbc_state::ConnectionConfig;

fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// --- Postgres argument builders -------------------------------------------

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
/// of this Vec — see the SECURITY test below.
pub fn build_pg_dump_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    opts: &PgBackupOptions,
    out_path: &str,
) -> Vec<String> {
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
    args
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
pub fn build_pg_restore_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    opts: &PgRestoreOptions,
    dump_path: &str,
) -> Vec<String> {
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
    args
}

/// `psql -h host -p port -U user -d database -f <dump_path>` — plain-SQL
/// restore, design §3 ("no equivalent transaction flag is forced").
pub fn build_psql_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    dump_path: &str,
) -> Vec<String> {
    vec![
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
    ]
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
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.backup");
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
        );
        assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
    }

    #[test]
    fn psql_args_never_contain_the_password() {
        let args = build_psql_args(&cfg(), "127.0.0.1", 15432, r"D:\bk\shop.sql");
        assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
    }

    // --- exact argv shape ---
    #[test]
    fn pg_dump_args_custom_format_includes_compress_and_verbose() {
        let opts = PgBackupOptions {
            format: PgDumpFormat::Custom,
            compress: 6,
        };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.backup");
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
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.sql");
        assert!(!args.iter().any(|a| a.starts_with("--compress")));
        assert!(args.contains(&"--format=p".to_string()));
    }

    #[test]
    fn pg_dump_compress_clamped_to_9() {
        let opts = PgBackupOptions {
            format: PgDumpFormat::Custom,
            compress: 200,
        };
        let args = build_pg_dump_args(&cfg(), "h", 1, &opts, "f");
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
        );
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
        let args = build_pg_restore_args(&cfg(), "h", 1, &opts, "f.backup");
        assert_eq!(
            args,
            vec!["-h", "h", "-p", "1", "-U", "alice", "-d", "shop", "f.backup"]
        );
    }

    #[test]
    fn pg_restore_create_db_adds_flag() {
        let mut opts = PgRestoreOptions::default();
        opts.create_db = true;
        let args = build_pg_restore_args(&cfg(), "h", 1, &opts, "f.backup");
        assert!(args.contains(&"--create".to_string()));
    }

    #[test]
    fn psql_args_shape() {
        let args = build_psql_args(&cfg(), "127.0.0.1", 15432, r"D:\bk\shop.sql");
        assert_eq!(
            args,
            vec!["-h", "127.0.0.1", "-p", "15432", "-U", "alice", "-d", "shop", "-f", r"D:\bk\shop.sql"]
        );
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
        );
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
