//! G11: backup/restore command construction (pure) + external-process
//! orchestration (T3, appended below). Pure half has zero I/O — no
//! `std::process`, no `std::fs` reads beyond what's handed in as `&[u8]`.

use dbc_state::{ConnectionConfig, Engine};

fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// G15 T8 HARD GATE ITEM 1 — THE authoritative account of this gate's
/// status; `build_backup_sql`/`build_verify_backup_file_exists_sql`/
/// `build_verify_database_online_sql` (below) and
/// `runner.rs::mssql_backup_restore_round_trip_live`'s doc comment all
/// summarize and point back here rather than each re-deriving their own
/// theory — keep them in sync with THIS text if the story changes again.
///
/// Whether the 🗄/♻ backup/restore affordances are shown at all for a
/// given engine. `true` for Postgres/Sqlite (the G11 baseline, live and
/// validated for a long time). `false` for MSSQL — NOT because the
/// `BACKUP DATABASE`/`RESTORE DATABASE` T-SQL is wrong (both commands were
/// confirmed correct, live, via `sqlcmd`), but because the round trip is
/// UNRELIABLE through this driver's ODBC execution path specifically, and
/// — important, read this before trusting any ONE run — the exact failure
/// mode is NOT fully characterized:
/// - `BACKUP DATABASE WITH ... STATS = n` reliably aborted server-side
///   (SQL Server error 3041) while `Connection::execute` still reported
///   `Ok`, isolated by a controlled A/B comparison over the SAME
///   connection (identical statement, `STATS` present vs. absent) —
///   `build_backup_sql`'s `STATS` clause was removed as a result. This
///   part IS solid: a confirmed, reproducible, controlled finding, not a
///   guess.
/// - That fix alone did NOT make the round trip reliably green. Live runs
///   AFTER removing `STATS` still intermittently failed — sometimes at the
///   `BACKUP` step (the file never gets written despite `execute()`
///   reporting `Ok`, same class of lie `STATS` caused, apparently not
///   `STATS`-exclusive after all), sometimes at the `RESTORE` step (the
///   database gets stuck in `RESTORING` and never reaches `ONLINE` even
///   after a bounded poll — see `build_verify_database_online_sql`). A
///   run can ALSO fully pass — this was observed too, more than once —
///   which is exactly why "it passed this run" is not evidence of
///   reliability on its own.
/// - `run_mssql_backup_inner`/`run_mssql_restore_inner` both gained
///   verification steps this task (`xp_fileexist`, an `ONLINE` poll) so a
///   failure is reported LOUDLY rather than silently lied about — real,
///   permanent value regardless of the gate's state — but neither is a fix
///   for the underlying intermittency.
///
/// Decision: the gate stays OFF for 0.18.0 — the exact "if not feasible
/// cleanly, GATE it" contingency this task's instructions called for.
/// Un-gating needs BOTH a real root cause (most likely in `run_execute`'s
/// diagnostic-record handling around `SQL_SUCCESS_WITH_INFO`, or a
/// msodbcsql18-Linux/odbc-api interaction — not yet found) AND a follow-up
/// soak test (the round trip run enough times in a row, on fresh
/// containers, to actually measure a failure rate instead of eyeballing
/// one run) — a future task, not something to paper over here.
/// `mssql_backup_restore_round_trip_live` (runner.rs) is the promotion
/// contract: it must go green RELIABLY, across repeated runs, before this
/// function's `Mssql` arm flips.
///
/// G16 T6 ON-flip: Duckdb un-gated after the T4 embedded suite (round
/// trip, magic pin, read-only matrix) went green. Mssql stays gated per
/// its own note above.
pub fn backup_restore_available(engine: Engine) -> bool {
    !matches!(engine, Engine::Mssql)
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
    /// Exercised by `pure_tests` (`pg_dump_args_plain_format_omits_compress`)
    /// and reachable through this type's public API, but T6's dispatch
    /// currently only ever constructs `Custom` (compress=6) — the plan's
    /// "Postgres format radio" UI toggle was a scope trim (see this phase's
    /// final report); `#[allow(dead_code)]` documents that, rather than
    /// papering over an actual bug, until a format picker lands.
    #[allow(dead_code)]
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

/// `BACKUP DATABASE [db] TO DISK = N'server-path' WITH FORMAT`. Uses
/// `dbc_core::quote_ident_d(Dialect::Mssql, …)` (bracket style, `]`
/// doubled) for the database name — these builders are MSSQL-only T-SQL, so
/// the dialect is a fixed constant, not threaded from a caller.
///
/// G15 T8 live-found fix: NO `STATS = n` clause (a prior version of this
/// builder had `WITH FORMAT, STATS = 10`, matched by
/// `run_mssql_backup_inner`'s own review-era comment). `STATS` makes SQL
/// Server emit periodic "N percent processed" informational TDS messages
/// while the backup runs; in a controlled A/B comparison (the IDENTICAL
/// `BACKUP DATABASE` issued over the SAME connection, `STATS = 10` present
/// vs. absent) dropping it reliably fixed the failure `Connection::execute`
/// still reported `Ok` for. This is a real, confirmed contributing cause —
/// **not the whole story**: the round trip still intermittently fails even
/// without `STATS` (both at this BACKUP step and at RESTORE) — see
/// `backup::backup_restore_available`'s doc comment, the authoritative
/// account of what is and isn't understood about this, and why the
/// backup/restore feature gate stays off for 0.18.0 regardless of this
/// fix. Kept anyway: it's a genuine improvement (removes one confirmed
/// failure trigger) and has no functional cost — nothing in this codebase
/// parses/displays the STATS progress messages.
pub fn build_backup_sql(database: &str, server_path: &str) -> String {
    format!(
        "BACKUP DATABASE {} TO DISK = N{} WITH FORMAT",
        dbc_core::quote_ident_d(dbc_core::Dialect::Mssql, database),
        sql_string_literal(server_path)
    )
}

/// `RESTORE DATABASE [db] FROM DISK = N'server-path' WITH REPLACE`. No
/// `STATS` clause — same live-found reason as `build_backup_sql` (not
/// independently re-verified broken for RESTORE specifically, but dropped
/// for consistency and to not carry the same risk into the restore path).
pub fn build_restore_sql(database: &str, server_path: &str) -> String {
    format!(
        "RESTORE DATABASE {} FROM DISK = N{} WITH REPLACE",
        dbc_core::quote_ident_d(dbc_core::Dialect::Mssql, database),
        sql_string_literal(server_path)
    )
}

/// G15 T8 HARD GATE ITEM 1 fix: `EXEC master.dbo.xp_fileexist N'server-path'`
/// — a dedicated extended stored proc that returns a `(File Exists, File is
/// a Directory, Parent Directory Exists)` result set. Used by
/// `run_mssql_backup_inner` to VERIFY a `BACKUP DATABASE` call actually
/// wrote a file, rather than trusting `Connection::execute`'s `Ok` alone —
/// `execute()` reporting `Ok` for a `BACKUP DATABASE` that did NOT
/// actually write a file is a real, live-observed failure mode (see
/// `backup::backup_restore_available`'s doc comment for the full,
/// honestly-still-not-fully-characterized story — an earlier version of
/// THIS comment blamed a "database not yet ready" timing race specific to
/// backing up a just-created database, which turned out to be wrong: the
/// same silent-`Ok`-but-no-file failure was later observed live against
/// databases that had existed for a while too). A silently-failed backup
/// reported as success is worse than a loud failure, so the backup path
/// gets this belt-and-braces file-existence check regardless of whether
/// the underlying cause is ever fully root-caused; generalizing
/// diagnostic-record inspection to every `execute()` call site was judged
/// out of scope/too risky to rush.
pub fn build_verify_backup_file_exists_sql(server_path: &str) -> String {
    format!("EXEC master.dbo.xp_fileexist N{}", sql_string_literal(server_path))
}

/// G15 T8: post-`RESTORE` sanity check — `sys.databases.state_desc` for
/// `database` must read back `ONLINE`. Belt-and-braces alongside
/// `build_verify_backup_file_exists_sql`: a `RESTORE DATABASE` that
/// reports `Ok` via `Connection::execute` can still leave the database
/// stuck in `RESTORING` (never reaching `ONLINE`, even after a bounded
/// wait — see `run_mssql_restore_inner`'s doc comment) — the SAME class of
/// "execute() lied" risk `build_verify_backup_file_exists_sql` guards on
/// the backup side, not merely a defensive "just in case" addition. See
/// `backup::backup_restore_available`'s doc comment for the full account
/// of why this alone doesn't make the round trip reliable.
pub fn build_verify_database_online_sql(database: &str) -> String {
    format!(
        "SELECT state_desc FROM sys.databases WHERE name = N{}",
        sql_string_literal(database)
    )
}

/// `ALTER DATABASE [db] SET SINGLE_USER WITH ROLLBACK IMMEDIATE` (`multi:
/// false`) or `... SET MULTI_USER` (`multi: true`) — design §3.
///
/// G15 T8 whole-branch review NIT fix: this is MSSQL-only T-SQL (same as
/// every other builder in this section) but used to quote the database
/// name with the pg-only `dbc_core::quote_ident` (double quotes) instead
/// of `quote_ident_d(Dialect::Mssql, ...)` (brackets) — every sibling
/// builder here (`build_backup_sql`/`build_restore_sql`/
/// `build_verify_database_online_sql`) already gets this right.
pub fn build_single_user_sql(database: &str, multi: bool) -> String {
    let mode = if multi {
        "MULTI_USER"
    } else {
        "SINGLE_USER WITH ROLLBACK IMMEDIATE"
    };
    format!("ALTER DATABASE {} SET {mode}", dbc_core::quote_ident_d(dbc_core::Dialect::Mssql, database))
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

// --- DuckDB -----------------------------------------------------------------

/// G16 §7: DuckDB's supported online single-file-copy idiom — there is no
/// `VACUUM INTO`; `ATTACH` + `COPY FROM DATABASE` + `DETACH` over ONE
/// dedicated connection is the engine-blessed equivalent (copying a live
/// DuckDB file directly risks exactly the WAL/open-writer corruption
/// `VACUUM INTO` exists to avoid on sqlite). Pure builder: `dest_path`
/// single-quote-escaped by `''`-doubling (same as `build_vacuum_into_sql`);
/// `src_db_name` (fetched at RUN time via `SELECT current_database()` —
/// DuckDB names a file database after its file stem, but asking the engine
/// beats duplicating that rule client-side) goes through
/// `dbc_core::quote_ident` (pg-style `"…"` doubling — DuckDB's identifier
/// quoting exactly). These three statements are sanctioned `execute()`
/// callers under the EXISTING G11 backup entry (amended in
/// dbc-core/src/connection.rs this task — an amendment, not a new entry).
pub fn build_duckdb_backup_sql(src_db_name: &str, dest_path: &str) -> Vec<String> {
    let escaped = sql_string_literal(dest_path);
    let src = dbc_core::quote_ident(src_db_name);
    vec![
        format!("ATTACH {escaped} AS __dbc_backup"),
        format!("COPY FROM DATABASE {src} TO __dbc_backup"),
        "DETACH __dbc_backup".to_string(),
    ]
}

/// G16 §7: a DuckDB database file's main header carries the ASCII bytes
/// `DUCK` at byte offset 8 (bytes 0..8 are a block checksum). NOT trusted
/// from documentation alone — verified against a freshly created database
/// by `duckdb_backup_end_to_end_round_trip` (runner.rs).
pub const DUCKDB_MAGIC_OFFSET: usize = 8;

/// The four magic bytes at [`DUCKDB_MAGIC_OFFSET`].
pub const DUCKDB_MAGIC: &[u8; 4] = b"DUCK";

/// Never panics on a short slice — same posture as `sqlite_magic_header_ok`.
pub fn duckdb_magic_ok(bytes: &[u8]) -> bool {
    bytes.len() >= DUCKDB_MAGIC_OFFSET + DUCKDB_MAGIC.len()
        && &bytes[DUCKDB_MAGIC_OFFSET..DUCKDB_MAGIC_OFFSET + DUCKDB_MAGIC.len()] == DUCKDB_MAGIC
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

/// T6 binding carry-forward (connection-identity staleness across an async
/// window — a file-save/open dialog can take arbitrarily long): `true` only
/// when the connection id a backup/restore dialog was opened for is STILL
/// present in the caller's current connection list at confirm/dispatch time
/// — i.e. it wasn't deleted while the OS file dialog was open. The caller
/// (`main.rs`) re-resolves a FRESH `ConnectionConfig` from this same lookup
/// rather than reusing anything captured at dialog-open time, so an edited
/// connection (read-only toggled, password changed) is picked up too, not
/// just a deleted one. Mirrors this codebase's established
/// `conn_identity_matches` convention (`main.rs`), specialized to backup/
/// restore's "dispatch by a specific connection id" shape — unlike the
/// Apply dialog, a backup/restore dialog never targets "whatever connection
/// is currently active", so `current_conn_identity()` itself isn't the
/// right comparison target here.
pub fn backup_dispatch_allowed(captured_id: &str, current_ids: &[String]) -> bool {
    !captured_id.is_empty() && current_ids.iter().any(|id| id == captured_id)
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    // G15 T8 HARD GATE ITEM 1: gated off for Mssql pending a real fix — see
    // `backup_restore_available`'s doc comment.
    #[test]
    fn backup_restore_available_gates_mssql_only() {
        assert!(backup_restore_available(Engine::Postgres));
        assert!(backup_restore_available(Engine::Sqlite));
        assert!(!backup_restore_available(Engine::Mssql));
        // G16 T6 ON-flip: un-gated after the T4 embedded suite went green.
        assert!(backup_restore_available(Engine::Duckdb));
    }

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
            mssql: None,
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
        let opts = PgRestoreOptions { create_db: true, ..Default::default() };
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
        // Brackets don't treat `"` specially — only `]` needs doubling.
        let sql = build_backup_sql("my\"db", r"D:\Backups\mydb.bak");
        assert_eq!(
            sql,
            "BACKUP DATABASE [my\"db] TO DISK = N'D:\\Backups\\mydb.bak' WITH FORMAT"
        );
    }

    // G15 T4 required golden string: `]` inside the database name is
    // doubled, proving the bracket-quoting switch.
    #[test]
    fn backup_sql_doubles_embedded_closing_bracket() {
        let sql = build_backup_sql("we]ird", r"D:\Backups\mydb.bak");
        assert_eq!(
            sql,
            "BACKUP DATABASE [we]]ird] TO DISK = N'D:\\Backups\\mydb.bak' WITH FORMAT"
        );
    }

    // G15 T8 live-found fix: no `STATS` clause — see `build_backup_sql`'s
    // doc comment for the full root-cause writeup (STATS's progress
    // messages reliably made live BACKUP DATABASE abort through this
    // driver's execution path).
    #[test]
    fn backup_sql_has_no_stats_clause() {
        assert!(!build_backup_sql("db", "path").contains("STATS"));
    }

    #[test]
    fn restore_sql_shape() {
        let sql = build_restore_sql("mydb", r"D:\Backups\mydb.bak");
        assert_eq!(
            sql,
            "RESTORE DATABASE [mydb] FROM DISK = N'D:\\Backups\\mydb.bak' WITH REPLACE"
        );
    }

    #[test]
    fn single_user_sql_both_directions() {
        // G15 T8 whole-branch review NIT fix: brackets, not double quotes —
        // this is MSSQL-only T-SQL.
        assert_eq!(
            build_single_user_sql("mydb", false),
            "ALTER DATABASE [mydb] SET SINGLE_USER WITH ROLLBACK IMMEDIATE"
        );
        assert_eq!(build_single_user_sql("mydb", true), "ALTER DATABASE [mydb] SET MULTI_USER");
    }

    /// Golden bracket-doubling case, same convention `backup_sql_doubles_
    /// embedded_closing_bracket` already pins for `build_backup_sql`.
    #[test]
    fn single_user_sql_doubles_embedded_closing_bracket() {
        assert_eq!(
            build_single_user_sql("we]ird", false),
            "ALTER DATABASE [we]]ird] SET SINGLE_USER WITH ROLLBACK IMMEDIATE"
        );
    }

    #[test]
    fn path_quote_doubling_for_embedded_single_quote() {
        let sql = build_backup_sql("db", r"D:\o'brien\mydb.bak");
        assert!(sql.contains("D:\\o''brien\\mydb.bak"));
    }

    // G15 T8 HARD GATE ITEM 1 fix: verification-query builders.
    #[test]
    fn verify_backup_file_exists_sql_shape() {
        assert_eq!(
            build_verify_backup_file_exists_sql("/var/opt/mssql/data/db.bak"),
            "EXEC master.dbo.xp_fileexist N'/var/opt/mssql/data/db.bak'"
        );
    }

    #[test]
    fn verify_backup_file_exists_sql_escapes_embedded_quote() {
        let sql = build_verify_backup_file_exists_sql(r"D:\o'brien\mydb.bak");
        assert!(sql.contains("D:\\o''brien\\mydb.bak"));
    }

    #[test]
    fn verify_database_online_sql_shape() {
        assert_eq!(
            build_verify_database_online_sql("mydb"),
            "SELECT state_desc FROM sys.databases WHERE name = N'mydb'"
        );
    }

    #[test]
    fn verify_database_online_sql_escapes_embedded_quote() {
        let sql = build_verify_database_online_sql("o'brien");
        assert!(sql.contains("N'o''brien'"));
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

    // --- DuckDB backup SQL + magic (G16 T4) ---
    #[test]
    fn duckdb_backup_sql_shape_and_quoting() {
        let stmts = build_duckdb_backup_sql("analytics", r"D:\zálohy\o'brien.duckdb");
        assert_eq!(stmts, vec![
            r"ATTACH 'D:\zálohy\o''brien.duckdb' AS __dbc_backup".to_string(),
            "COPY FROM DATABASE \"analytics\" TO __dbc_backup".to_string(),
            "DETACH __dbc_backup".to_string(),
        ]);
        // A hostile db name is quote_ident-escaped, never interpolated raw.
        let weird = build_duckdb_backup_sql("we\"ird", "d.duckdb");
        assert!(weird[1].contains("\"we\"\"ird\""), "got: {}", weird[1]);
    }

    #[test]
    fn duckdb_magic_ok_bounds_and_offset() {
        let mut good = vec![0u8; 16];
        good[8..12].copy_from_slice(b"DUCK");
        assert!(duckdb_magic_ok(&good));
        assert!(!duckdb_magic_ok(b"DUCK")); // magic at offset 0 is NOT a duckdb file
        assert!(!duckdb_magic_ok(&good[..11])); // short slice never panics
        assert!(!duckdb_magic_ok(&[]));
        assert!(!sqlite_magic_header_ok(&good)); // the two sniffs never overlap
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

    // --- backup_dispatch_allowed (connection-identity staleness) ---
    #[test]
    fn backup_dispatch_allowed_when_id_still_present() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert!(backup_dispatch_allowed("a", &ids));
    }

    #[test]
    fn backup_dispatch_refused_when_id_no_longer_present() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert!(!backup_dispatch_allowed("deleted-while-dialog-open", &ids));
    }

    #[test]
    fn backup_dispatch_refused_on_empty_captured_id() {
        assert!(!backup_dispatch_allowed("", &["a".to_string()]));
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
/// SECURITY (CWE-427, binary planting):
/// - **G11 T4 review MAJOR 1** (returning an absolute path, not a bare
///   name): callers (`resolve_tool_path`, `runner.rs`) pass this string
///   straight to `Command::new`, and on Windows `CreateProcess` searches
///   the application directory and the CURRENT WORKING DIRECTORY *before*
///   PATH when given a bare name — a planted `pg_dump.exe` sitting in a
///   writable CWD would run instead of the real tool, and since
///   `PGPASSWORD` is set on that spawned child's environment, the planted
///   binary would receive the real database password. Returning the
///   fully-resolved path (this function's whole point) bypasses that
///   CWD/app-dir search order entirely — `CreateProcess` uses a path
///   containing a directory separator as-is, never re-searching it.
/// - **Final whole-branch review MAJOR** (the `where` probe's OWN search
///   order): the fix above only helps once `find_on_path` has already
///   returned the RIGHT path — but plain `where <name>` on Windows *itself*
///   searches the current directory before PATH (same precedence
///   `CreateProcess` uses for a bare name), so a planted binary sitting in
///   this process's CWD would already win at the `where` step, and the
///   "absolute path" this function hands back would just BE the planted
///   binary's absolute path. `where $PATH:<name>` restricts the search to
///   directories listed in `%PATH%` only, skipping the CWD entirely —
///   empirically confirmed (reviewer's probe): a planted CWD binary yields
///   "Could not find files", while `where $PATH:cmd` correctly resolves to
///   `C:\Windows\System32\cmd.exe`. The Unix `which` branch is unaffected —
///   `which` has never searched the CWD unless `.` is explicitly listed in
///   `$PATH` (a user/shell configuration choice outside this function's
///   control, not a `which`-specific search-order footgun).
pub fn find_on_path(name: &str) -> Option<String> {
    #[cfg(windows)]
    let probe = Command::new("where").arg(format!("$PATH:{name}")).output();
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
        //
        // SECURITY (CWE-427, final whole-branch review MAJOR): this also
        // exercises the `where $PATH:<name>` argument form (not plain
        // `where <name>`) — a CWD-planting probe is deliberately NOT added
        // here (mutating this test process's current directory is global,
        // shared state that would be flaky under parallel test execution —
        // same reasoning the review that requested this fix itself
        // accepted); the reviewer's own manual probe already empirically
        // confirmed `where $PATH:cmd` resolves to
        // `C:\Windows\System32\cmd.exe` while a planted CWD binary yields
        // "Could not find files" — see `find_on_path`'s doc comment for
        // the full writeup.
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

// --- UI-facing session state (T6) -------------------------------------
//
// Lives here (not connections_ui.rs) so backup.rs stays the single home for
// every backup/restore type — mirrors plan.rs's (G13) "one file, pure half
// then UI-adjacent half" convention this plan's Architecture section
// commits to.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    Backup,
    Restore,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackupStatus {
    /// Restore only — the typed-database-name confirm step. Backup skips
    /// straight to `Running` (design §2 vs §3: only Restore gets the
    /// GitHub-delete-repo-pattern typed-name friction).
    Confirming,
    Running,
    Succeeded,
    Failed(String),
    Cancelled,
}

/// Cancel switch for whatever is actually running. `RefCell`-wrapped
/// (DEVIATION from the plan's literal `pub cancel: Rc<dyn Fn()>` — grounded
/// below) since the real cancellation closure isn't known until dispatch:
/// `BackupSession` is constructed by `open_backup_dialog`/`open_restore_dialog`
/// (main.rs) before any process/task exists to cancel, and a `Confirming`
/// Restore session genuinely has nothing to cancel yet. `start_backup`/
/// `start_restore` fill this slot in once the real `BackupHandle::cancel`
/// (Postgres) exists; MSSQL/SQLite runs never fill it (T4's runner methods
/// for those two engines expose no cancel hook — see `BackupSession`'s doc
/// comment) so it stays `None` for their whole run, and `cancel_now` is
/// always safe to call regardless: a no-op before dispatch, after a
/// terminal state, or for an engine with nothing to cancel.
pub type CancelSlot = std::rc::Rc<std::cell::RefCell<Option<std::rc::Rc<dyn Fn()>>>>;

/// Cap on retained log lines in a `BackupSession`'s `log` — same fixed-cap
/// posture as `tabs::SCRIPT_LOG_CAP`, not user-tunable. Review MINOR 2 fix:
/// `render_backup_restore_panel` re-clones every retained line on every
/// render frame while `Running`, and `pg_dump -v` emits one line PER
/// OBJECT — on a huge schema an unbounded log would grow without limit and
/// get fully re-cloned each frame, O(n²) cumulative cost over the run's
/// lifetime. 500 (half `SCRIPT_LOG_CAP`) is enough to show meaningful
/// recent progress without that growth.
pub const BACKUP_LOG_CAP: usize = 500;

/// `BackupSession.log`'s value type — the retained lines PLUS whether any
/// were ever evicted past `BACKUP_LOG_CAP` (drives the panel's "… (starší
/// řádky zahozeny)" notice, review MINOR 2). Eviction here only bounds what
/// this UI panel keeps in memory to redraw — it never truncates what the
/// actual `pg_dump`/`pg_restore`/`psql` process itself streamed or did.
#[derive(Debug, Default, Clone)]
pub struct BackupLogState {
    pub lines: std::collections::VecDeque<String>,
    pub truncated: bool,
}

pub type BackupLog = std::rc::Rc<std::cell::RefCell<BackupLogState>>;

/// Appends `line` to `log`, evicting the oldest entry past `BACKUP_LOG_CAP`
/// — same eviction posture as `tabs::ScriptRunState::push_log`. The ONLY
/// place a line is ever added to a `BackupSession.log` (main.rs's two
/// `BackupEvent::Log` dispatch-loop arms both call this instead of pushing
/// directly).
pub fn push_backup_log(log: &BackupLog, line: String) {
    let mut state = log.borrow_mut();
    state.lines.push_back(line);
    if state.lines.len() > BACKUP_LOG_CAP {
        state.lines.pop_front();
        state.truncated = true;
    }
}

/// UI session state for one backup/restore run, held by
/// `connections_ui::ModalState::BackupRestore`. `Clone` is cheap (every
/// field is either `Copy`, a `String`, or an `Rc`/`Entity` handle) — GPUI's
/// `ModalState::clone()`-per-render convention (every other `ModalState`
/// arm does the same) relies on this.
#[derive(Clone)]
pub struct BackupSession {
    pub kind: BackupKind,
    pub engine: dbc_state::Engine,
    /// `ConnectionConfig.id` this session was opened for — NOT necessarily
    /// the app's currently-active connection (the dropdown's 🗄/♻ icons work
    /// on ANY row, not just the active one). Re-checked via
    /// `backup_dispatch_allowed` before every dispatch (staleness guard).
    pub connection_id: String,
    pub connection_name: String,
    pub database: String,
    pub log: BackupLog,
    pub status: std::rc::Rc<std::cell::RefCell<BackupStatus>>,
    pub started_at: std::time::Instant,
    pub cancel: CancelSlot,
    /// `Some` only during `Confirming` for a Restore session — the typed
    /// database-name field; `None` for Backup (no typed-confirm friction).
    pub confirm_input: Option<gpui::Entity<crate::connections_ui::TextField>>,
    pub expected_name: String,
    /// The redacted command/SQL text shown by the confirm/running panel
    /// (§3-novela: "show exactly what will run before dispatch").
    pub command_line: String,
    /// Full local path this run reads from (restore) or writes to (backup)
    /// — kept separately from `command_line` (the display string) so
    /// `main.rs` can build the T7 history description without re-parsing it.
    pub target_path: String,
}

impl BackupSession {
    /// Invokes whatever cancel hook is currently installed — a no-op before
    /// dispatch (`cancel` still empty), once a run has already reached a
    /// terminal state, or for an engine (MSSQL/SQLite) with no real cancel
    /// hook at all. Safe to call unconditionally from every teardown path.
    pub fn cancel_now(&self) {
        if let Some(f) = self.cancel.borrow().as_ref() {
            f();
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(*self.status.borrow(), BackupStatus::Running)
    }

    /// `true` only once a REAL cancel hook is installed (Postgres —
    /// `run_backup_now`/`run_restore_now` wire a `BackupHandle::cancel`
    /// closure into `cancel` right after spawning). `false` before dispatch,
    /// for a terminal session, AND for the whole lifetime of an MSSQL/SQLite
    /// run — those two engines have no OS child process to kill, only a
    /// `tokio` task driving `Connection::execute`/`fs::copy`, and T4's
    /// runner methods for them expose no cancel hook at all (review MAJOR
    /// finding: the caller must never pretend otherwise).
    pub fn can_cancel(&self) -> bool {
        self.cancel.borrow().is_some()
    }
}

/// Review MAJOR fix: whether a teardown path (an explicit "Zrušit" click,
/// `close_modal`, `switch_to_connection`, or the app-quit hook) should flip
/// a session's UI-visible `status` from `Running` to `Cancelled`. `true`
/// only when there is BOTH a real cancel hook installed (`can_cancel`) AND
/// the run is still actually `Running` — flipping status for a
/// non-cancellable (MSSQL/SQLite) session while its `SET SINGLE_USER ->
/// RESTORE -> SET MULTI_USER` sequence or `fs::copy` keeps running in the
/// background would (a) lie to the user ("přerušeno uživatelem" while it's
/// actually still running), (b) make `finish_backup_restore`'s own
/// `Running`-only guard silently DROP the real outcome once it eventually
/// arrives — no history record of a restore that actually happened — and
/// (c) let the now-"terminal"-looking (but actually still running) modal
/// be closed and a SECOND overlapping write dispatched against the same
/// database, defeating the single-modal invariant. Pure so this decision
/// is unit-tested directly rather than only through GPUI-context-dependent
/// `AppView` methods (main.rs's `cancel_backup_restore`/
/// `cancel_active_backup_if_running` are thin callers of this).
pub fn should_cancel_on_teardown(can_cancel: bool, is_running: bool) -> bool {
    can_cancel && is_running
}

/// Review MAJOR fix, second half: whether a terminal event (Postgres'
/// `Finished`/`Failed`, or an MSSQL/SQLite oneshot's `Ok`/`Err`) should
/// update `status` and record a history entry — `true` only while `status`
/// is still `Running`. Deliberately independent of whether the modal that
/// started this run is still `self.modal` (`AppView::finish_backup_restore`
/// never reads `self.modal` at all, on purpose) — a user closing or
/// switching away from a non-cancellable (MSSQL/SQLite) session's modal
/// (safe to allow — see `should_cancel_on_teardown`'s doc comment) must
/// NOT suppress the real outcome once the background task actually
/// completes: as long as nothing wrongly flipped `status` away from
/// `Running` in the meantime (which is exactly what
/// `should_cancel_on_teardown` now prevents for those two engines), this
/// still returns `true` and the run is still recorded.
pub fn should_record_terminal_event(status_is_running: bool) -> bool {
    status_is_running
}

#[cfg(test)]
mod session_tests {
    use super::*;

    fn session(status: BackupStatus) -> BackupSession {
        BackupSession {
            kind: BackupKind::Backup,
            engine: dbc_state::Engine::Postgres,
            connection_id: "c1".into(),
            connection_name: "demo".into(),
            database: "shop".into(),
            log: std::rc::Rc::new(std::cell::RefCell::new(BackupLogState::default())),
            status: std::rc::Rc::new(std::cell::RefCell::new(status)),
            started_at: std::time::Instant::now(),
            cancel: std::rc::Rc::new(std::cell::RefCell::new(None)),
            confirm_input: None,
            expected_name: String::new(),
            command_line: String::new(),
            target_path: String::new(),
        }
    }

    #[test]
    fn cancel_now_is_a_no_op_when_no_hook_installed() {
        let s = session(BackupStatus::Running);
        s.cancel_now(); // must not panic
    }

    #[test]
    fn cancel_now_invokes_the_installed_hook_exactly_once_per_call() {
        let s = session(BackupStatus::Running);
        let calls = std::rc::Rc::new(std::cell::RefCell::new(0));
        let calls2 = calls.clone();
        *s.cancel.borrow_mut() = Some(std::rc::Rc::new(move || *calls2.borrow_mut() += 1));
        s.cancel_now();
        s.cancel_now();
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn is_running_matrix() {
        assert!(session(BackupStatus::Running).is_running());
        assert!(!session(BackupStatus::Confirming).is_running());
        assert!(!session(BackupStatus::Succeeded).is_running());
        assert!(!session(BackupStatus::Failed("x".into())).is_running());
        assert!(!session(BackupStatus::Cancelled).is_running());
    }

    // --- review MAJOR fix: can_cancel / should_cancel_on_teardown ---
    #[test]
    fn can_cancel_is_false_until_a_hook_is_installed() {
        let s = session(BackupStatus::Running);
        assert!(!s.can_cancel());
        *s.cancel.borrow_mut() = Some(std::rc::Rc::new(|| {}));
        assert!(s.can_cancel());
    }

    #[test]
    fn should_cancel_on_teardown_matrix() {
        // Postgres-shaped (real hook) — cancel only while actually Running.
        assert!(should_cancel_on_teardown(true, true));
        assert!(!should_cancel_on_teardown(true, false));
        // MSSQL/SQLite-shaped (no hook, ever) — NEVER flip status, even
        // while Running — the whole point of this fix.
        assert!(!should_cancel_on_teardown(false, true));
        assert!(!should_cancel_on_teardown(false, false));
    }

    #[test]
    fn should_record_terminal_event_only_while_still_running() {
        // The case the review's fix restores: a non-cancellable session
        // whose modal was closed (or the app quit) never has its status
        // wrongly flipped away from Running by `should_cancel_on_teardown`
        // — so once the real terminal event arrives, it's still recorded.
        assert!(should_record_terminal_event(true));
        // A cancelled (or already-terminal) session's late-arriving event
        // must NOT overwrite the outcome a second time.
        assert!(!should_record_terminal_event(false));
    }

    // --- review MINOR 2 fix: bounded log retention ---
    #[test]
    fn push_backup_log_retains_lines_under_the_cap_untruncated() {
        let log: BackupLog = std::rc::Rc::new(std::cell::RefCell::new(BackupLogState::default()));
        for i in 0..10 {
            push_backup_log(&log, format!("line {i}"));
        }
        let state = log.borrow();
        assert_eq!(state.lines.len(), 10);
        assert!(!state.truncated);
        assert_eq!(state.lines.front().map(String::as_str), Some("line 0"));
        assert_eq!(state.lines.back().map(String::as_str), Some("line 9"));
    }

    #[test]
    fn push_backup_log_evicts_oldest_past_the_cap_and_marks_truncated() {
        let log: BackupLog = std::rc::Rc::new(std::cell::RefCell::new(BackupLogState::default()));
        for i in 0..(BACKUP_LOG_CAP + 50) {
            push_backup_log(&log, format!("line {i}"));
        }
        let state = log.borrow();
        assert_eq!(state.lines.len(), BACKUP_LOG_CAP);
        assert!(state.truncated);
        // Oldest 50 lines (0..50) evicted — the retained front is line 50.
        assert_eq!(state.lines.front().map(String::as_str), Some("line 50"));
        assert_eq!(
            state.lines.back().map(String::as_str),
            Some(format!("line {}", BACKUP_LOG_CAP + 49).as_str())
        );
    }
}
