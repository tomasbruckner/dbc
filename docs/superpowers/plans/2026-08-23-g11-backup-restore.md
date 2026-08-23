# G11 Backup & Restore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers for every task, a **sonnet** adversarial code review per task before it's considered done — with a **security-focused** review pass specifically for T2 (redaction), T3 (spawn/env), T4 (runner sanctioned-write methods) and T5 (the docker validation) — and a **default-model** final review once all tasks land, mirroring this repo's staffing convention for multi-task phases (G7/G9/G13).

**Goal:** Whole-database backup/restore per engine, one UI surface over three fundamentally different mechanics: Postgres via external `pg_dump`/`pg_restore`/`psql` binaries (secrets via `PGPASSWORD` env, never argv, never logged), MSSQL via server-side `BACKUP DATABASE`/`RESTORE DATABASE` T-SQL run over the existing query path, SQLite via `VACUUM INTO`/guarded file copy. Backup is a documented, narrow exception to the read-only gate (it only reads the source database); restore is hard-blocked on read-only connections everywhere, no override, and gated behind a GitHub-delete-repo-style typed-database-name confirm modal.

**Architecture:** One new pure-then-impure module, `crates/dbc-ui/src/backup.rs`, following `plan.rs`'s (G13) "pure half, then the layer around it" colocation convention: command/SQL-string builders, dump-format sniffing, the SQLite magic-header check, the `backup_exempt_from_read_only` predicate, and the typed-name `confirm_matches` check are pure and unit-tested with zero I/O in the first half; a thin external-process spawn/stream/kill-on-drop layer — generalized from `tunnel.rs`'s `Tunnel`/`spawn_ssh`/`ssh_binary` shape — sits in the second half and is exercised by a small number of real-subprocess integration tests (mirroring `tunnel.rs::missing_binary_is_a_value_error`). `dbc-state::config::AppConfig` grows a `tool_paths: ToolPaths` field (`pg_dump`/`pg_restore`/`psql`, all three — the design's own curation fixed a gap where `psql` was missing). `QueryRunner` (`runner.rs`) grows one generic external-tool method (`run_external_tool`, used for all three Postgres binaries — dump, restore, and the plain-SQL `psql` path share identical spawn/stream/redact mechanics, so one method suffices rather than three near-duplicates) plus four sanctioned write methods for the two non-external-process engines (`run_mssql_backup`, `run_mssql_restore`, `run_sqlite_backup`, `run_sqlite_restore`) — all five added to `dbc-core::Connection::execute`'s sanctioned-caller doc comment. `connections_ui::ModalState` grows a fifth arm, `BackupRestore`, reusing the confirm/progress overlay shape the other four arms already establish; two new icon-button affordances land on `dropdown_item` (the connection dropdown row — see Global Constraints for why this replaces the design's assumed-but-nonexistent "context menu"); two new `PaletteAction` variants gate on an active connection. `dbc-state::history::HistoryDb` grows an additive `kind` column and a new `add_with_kind` method (the existing `add` keeps its exact signature, now a thin `kind = "query"` wrapper) so backup/restore runs show up in the History panel with a small badge.

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; no new primitive beyond what `grid.rs::start_export`/G12's plan already demonstrate — `cx.prompt_for_new_path` for the save dialog, `cx.prompt_for_paths(PathPromptOptions{..})` for the open dialog), `tokio` (workspace dep, existing feature set `["rt-multi-thread", "sync", "time", "macros"]` is sufficient — external-process spawning uses plain `std::process::Command` inside `tokio::task::spawn_blocking`, the exact pattern `connect::open_config`'s doc comment already mandates for `Tunnel::open`; **no new crate dependency** for T1–T4), `testcontainers-modules = { version = "0.13", features = ["postgres"] }` (new **dev**-dependency of `dbc-ui` only, T5 — same version `dbc-driver-postgres`'s own integration tests already pin).

**Spec:** `docs/superpowers/specs/drafts/g11-backup-restore-design.md` — the CURATION block (top of file, dated 2026-08-23) is binding. The five curation points and how this plan satisfies each:
1. `psql` was missing from `ToolPaths` → added alongside `pg_dump`/`pg_restore` in T1.
2. Backup-on-read-only is an explicit, separately-tested exemption (`backup_exempt_from_read_only`, T2) — never achieved by weakening `is_read_statement` or `guard_not_read_only`. Restore stays hard-blocked, no override (T2 test + T4 runner-level belt-and-braces test).
3. The MSSQL SINGLE_USER/RESTORE/MULTI_USER sequence, MSSQL BACKUP, and SQLite `VACUUM INTO`/file-replace are named, sanctioned runner-owned methods added to `Connection::execute`'s doc-comment caller list (T4).
4. SQLite restore reads the source file's first 16 bytes and requires the `SQLite format 3\0` magic header before any `fs::copy` (T2, pure; wired in T4).
5. PGPASSWORD env-only mechanism (T3) — never argv, never logged; the argv echoed to the user is built from the args `Vec` only and additionally passed through a redaction pass (T2's `redact_secret`) as defense in depth.

This plan also grounds and, where the design's assumptions don't match the actual code on this branch, corrects three things (each expanded in its own task's Grounding section, summarized here):
- **No context menu exists anywhere in `dbc-ui`.** The design's §4 assumes "the connection dropdown's per-item context menu (right-click, same menu that already hosts folder/favourite actions)" and a separate "connection manager's row context menu." Neither exists: `crates/dbc-ui/src/connections_ui.rs`'s only per-connection UI is `dropdown_item` (lines 1624–1683), which exposes per-row actions as small always-visible icon buttons (★ favourite, ✎ edit), each wired with `cx.stop_propagation()` so its click doesn't also fire the row's own connect handler — not a menu. There is also no separate "connection manager" panel anywhere in this codebase; the dropdown IS the only connection-management surface. T6 adds two more icon buttons to this same row, following the exact established pattern, rather than inventing a context-menu component that has no precedent at this pinned GPUI rev.
- **MSSQL is completely unwired**, not merely "the driver phase is separate" (design's own words undersell this): `crates/dbc-ui/src/connect.rs:94-99`, `open_config`'s `Engine::Mssql` arm is a permanent, non-stub `Err(QueryError::msg("MSSQL driver zatím není k dispozici"))` (the doc comment there explicitly calls this "Permanent behaviour (not a Task 8 stub)"). Every MSSQL code path this plan adds (`run_mssql_backup`/`run_mssql_restore` in T4) is real, reachable code — not dead code gated behind a driver-phase flag — but calling it against a saved MSSQL connection fails at the very first `open_spec` step with the exact same error every other MSSQL feature in this app already produces today. No special-casing is needed or added; T4's tests prove this fail-fast behavior directly. The design's curation item 5 asks for a "STATS=10 message-surfacing spike" inside T5 — this plan drops that spike entirely (not deferred, dropped) because there is no way to spike against a live MSSQL session in a codebase where MSSQL connections cannot be opened at all; T4 implements MSSQL backup/restore with spinner-plus-elapsed-timer progress only, matching the design's own documented no-verification fallback, and records this as a resolved (not open) question.
- **The app never holds a long-lived `Connection` for a saved connection.** The design's SQLite/MSSQL restore mechanisms both open with "(1) app drops its OWN cached `Connection` handle(s) for this connection id from `AppView` state first." No such registry exists: `runner.rs::connect_and_run`/`fetch_schema`/`fetch_lookup`/`run_write_transaction` all build a **fresh** `ConnectSpec` from `self.config`/`self.vault` and open-then-drop a connection for exactly one run (`opened` goes out of scope at the end of each `_inner` function — see e.g. `run_write_transaction_inner`'s own doc comment, runner.rs:414-431). There is nothing for T4/T6 to proactively close before a SQLite file-copy or an MSSQL `SET SINGLE_USER` — the design's step (1) in both restore mechanisms is moot on this codebase's actual architecture and is dropped, not implemented. The residual risk this leaves (a query that happens to be in-flight at the exact moment of restore) is the same risk the design's own §6 already documents for "no cross-app coordination" and is inherited as-is, not newly introduced.

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`. File dialogs use `PathPromptOptions` **without file filters** — confirmed at this pinned rev by G12's plan grounding (`crates/gpui/src/platform.rs:2139-2148`: `PathPromptOptions { files: bool, directories: bool, multiple: bool, prompt: Option<SharedString> }`, no filter field; the Windows `file_open_dialog` impl never calls `SetFileTypes`) — this plan does not attempt a `.backup`/`.sql`/`.sqlite` filter; any extension mismatch is caught client-side after the pick, same posture G12 already established for `.sql`.
- **SECURITY — passwords to external tools ONLY via child-process env, never argv, never logged.** `PGPASSWORD` is set on the spawned `std::process::Command`'s environment block only (`Command::env`), read from the vault at spawn time (`Vault::get_secret`), and is NEVER pushed into the `args: Vec<String>` that both (a) becomes the real argv AND (b) is what the UI echoes back to the user (with `***` substituted for the raw secret — see redaction below). **REQUIRED tests** (T2, T3, T5 — all three levels, non-negotiable): (1) every `build_*_args` pure function has a test asserting `!args.iter().any(|a| a.contains(&password))` for a representative password containing SQL-special/shell-special characters; (2) `redact_secret`/`display_command_line` have tests proving a secret embedded in an arbitrary string is replaced with `***`; (3) T5's docker test asserts that the error text produced by a *deliberately failed* `pg_dump` spawn (wrong password) does not contain the real password anywhere in its `message` field.
- Command lines shown to the user display `***` in place of the secret — `backup::display_command_line` (T2) is the ONLY function that formats a program+args pair for UI display; it is called with the real secret value so it can find-and-replace it, even though the secret is never part of `args` itself (defense in depth against a future arg-builder bug).
- **No credentials/result data in history or logs.** History entries (T7) store a synthetic, secret-free description string (e.g. `-- BACKUP demo -> D:\backups\demo-20260823-141200.backup (pg_dump -Fc, compress=6)`), never the command line's raw form, never a password.
- **Restore = write-class action → §3-novela, binding project-wide (restated per this plan, same posture as G9/G12/G13's own restatement):** every write this plan adds reaches its effect only through (a) a confirm modal showing the exact (redacted) command/SQL, (b) a runner-owned method with explicit transaction/sequence discipline, (c) the shared read-only gate. For MSSQL/SQLite this is literally `Connection::execute` gated by `backup::guard_backup_restore_read_only` (T2) inside a T4 runner method; for Postgres, `pg_restore`/`psql` is an external process (not `execute()`) but is gated by the exact same predicate before ever spawning, and dispatched only from `ModalState::BackupRestore`'s typed-name-confirmed "Obnovit" button (T6). **Backup is the one documented exception** (design CURATION item 2): `backup_exempt_from_read_only` (T2) returns `true` only for `BackupOp::Backup`, never for `BackupOp::Restore` — restore is unconditionally blocked on a read-only connection, no override, mirroring the exact posture G9/G10/G12 already established for their own write paths. **REQUIRED test** (T2, non-negotiable): a read-only connection + `BackupOp::Restore` is refused by `guard_backup_restore_read_only` with no I/O attempted — proven the same way `run_write_transaction_refuses_read_only_connection_without_connecting` (runner.rs:701-734) proves it today, i.e. a test that never constructs a `ConnectSpec`/calls `open_spec` in the failing path (T4).
- **SQLite source magic-header check is binding** (design CURATION item 4): `backup::sqlite_magic_header_ok` (T2, pure) reads the first 16 bytes of the picked restore-source file and requires the exact `SQLite format 3\0` byte sequence before any `fs::copy` runs (T4). A file that fails the check is refused with `"soubor není SQLite databáze"` — no copy is ever attempted.
- **`execute()`'s doc comment is stale as of this plan** (`crates/dbc-core/src/connection.rs:19-20`, currently "This is the app's write path — ONLY the sandbox Apply flow may call it") — G5's Apply flow (`run_write_transaction`) has in fact already been joined by other sanctioned callers on other in-flight branches (G9's kill flow, G12/G13's write paths, per those plans' own Global Constraints); this plan's amendment (T4) adds `run_mssql_backup`, `run_mssql_restore`, `run_sqlite_backup`, and `run_sqlite_restore` to that list. Whichever of those other phases' amendments has already landed by the time T4 executes, T4 re-reads the doc comment by symbol (not by assuming its exact current wording) and appends rather than overwrites.
- **Task-ordering / single-writer files** (binding, same posture every G6/G9/G12/G13-class plan in this repo states): `crates/dbc-ui/src/runner.rs` and `crates/dbc-ui/src/main.rs` are edited concurrently by other in-flight phases on separate branches. T4 (`runner.rs`, `connection.rs` doc-only) and T6 (`main.rs`, `connections_ui.rs`, `palette.rs`) are **serialized tail tasks** — dispatched only after whatever G9/G10/G12/G13 `runner.rs`/`main.rs` work has already merged to `main`, with this branch rebased onto that merge first; re-locate every line reference in this plan by symbol name, not line number, after that rebase. T1 (`dbc-state::config`), T2/T3 (the new `backup.rs` file — touched by no other in-flight phase), and T7 (`dbc-state::history`, additive) have no such ordering constraint and can start immediately in parallel worktrees.
- Errors are values; no panics on any I/O or user-data path. A missing tool, a failed spawn, a malformed dump-format header, a too-short restore-source file — all degrade to `Result::Err`/a status string, never a panic.
- Tests green before every commit: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state -p dbc-ui` (plus `-p dbc-core` for T4's doc-only change) must pass with the task's new tests included. T5's docker+real-`pg_dump` tests are `#[ignore]`d and run explicitly via `-- --ignored`.
- Version bump at merge (phase-numbered convention — `dbc-ui` is `0.6.0` at time of writing on this branch): `dbc-ui` → `0.11.0` at T6's tail commit. `dbc-state` stays `0.1.0` (satellite-crate convention — every other satellite crate in this repo is pinned at `0.1.0` regardless of phase).
- UI strings are Czech (labels, statuses, error messages, tooltips) — English only in code/comments/tests.

### Task dependency graph

| Task | Files | Depends on | Notes |
|---|---|---|---|
| T1 | `crates/dbc-state/src/config.rs` | — | `ToolPaths` + persistence; parallel-eligible immediately |
| T2 | `crates/dbc-ui/src/backup.rs` (new, pure half), `crates/dbc-ui/src/main.rs` (`mod backup;`) | — | pure command/SQL builders + redaction + guards; parallel-eligible immediately, independent of T1 |
| T3 | `crates/dbc-ui/src/backup.rs` (append, process half) | T2 | spawn/stream/kill-on-drop skeleton; needs T2's arg builders only for its own tests |
| T4 | `crates/dbc-ui/src/runner.rs`, `crates/dbc-core/src/connection.rs` (doc only) | T1, T2, T3 | **serialized tail** — runner.rs; after G9/G10/G12/G13 merge |
| T5 | `crates/dbc-ui/Cargo.toml` (dev-dep), `crates/dbc-ui/tests/backup_docker.rs` (new) | T4 | docker + real `pg_dump`/`pg_restore` required, `#[ignore]`d |
| T6 | `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/palette.rs`, `crates/dbc-ui/src/main.rs` | T4 | **serialized tail** — main.rs; after T4, same merge-ordering constraint |
| T7 | `crates/dbc-state/src/history.rs`, `crates/dbc-ui/src/history_panel.rs`, `crates/dbc-ui/src/main.rs` | T4 (shape only) | history integration; developed in parallel with T6, its `main.rs` call sites land alongside T6's |

**Parallelization:** T1 and T2 start immediately and in parallel (disjoint files, no shared state). T3 follows T2 (same file, appends). T4 is the runner-owned-methods tail — start as soon as T1+T2+T3 are merged, but its OWN merge waits for the cross-phase `runner.rs` ordering constraint above. T5 (docker validation) depends only on T4. T6 and T7 both depend on T4's method shapes; T7's `HistoryDb` schema/method work (`history.rs`) is fully independent of T6 and can proceed in its own worktree, but T7's two `main.rs` call sites (recording a backup/restore run) are small and land in the same tail window as T6 to avoid a second `main.rs` rebase.

---

### Task 1 (T1): `ToolPaths` — tool detection persistence

**Files:**
- Modify: `crates/dbc-state/src/config.rs`

**Interfaces:**
- Produces (consumed by T4's tool-resolution code and T6's "Nastavit cestu…" flow):
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolPaths {
    #[serde(default)]
    pub pg_dump: Option<String>,
    #[serde(default)]
    pub pg_restore: Option<String>,
    /// Design CURATION item 1: was missing from the design's own §1 sketch
    /// of `ToolPaths` even though §3's plain-SQL restore pipes through
    /// `psql` — added here with identical shape/detection/override to
    /// `pg_dump`/`pg_restore`.
    #[serde(default)]
    pub psql: Option<String>,
}
```
- `AppConfig` grows one field:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub favourite_objects: Vec<FavouriteObject>,
    /// Global, not per-connection (an installed tool is a machine property,
    /// not a connection property) — design §1.
    #[serde(default)]
    pub tool_paths: ToolPaths,
}
```

**Grounding:** `crates/dbc-state/src/config.rs` (read in full above) already has the exact `#[serde(default)]`-on-every-field-plus-struct-level-`Default`-derive shape this needs — `AppConfig::load`/`save` (lines 70-85) are untouched, TOML roundtrips a new `[tool_paths]` table automatically via `toml`/`serde`'s derive, and an OLD config file with no `[tool_paths]` section at all loads fine (proven by the existing `old_config_without_favourites_loads` test at config.rs:202-215, same mechanism). `Default` is required on `ToolPaths` for `#[serde(default)]` at the `AppConfig` field level to compile.

- [ ] **Step 1: Write the failing tests** (`crates/dbc-state/src/config.rs`, inside the existing `#[cfg(test)] mod tests`):
```rust
fn sample_with_tools() -> AppConfig {
    let mut c = sample();
    c.tool_paths = ToolPaths {
        pg_dump: Some(r"C:\Program Files\PostgreSQL\16\bin\pg_dump.exe".into()),
        pg_restore: Some(r"C:\Program Files\PostgreSQL\16\bin\pg_restore.exe".into()),
        psql: Some(r"C:\Program Files\PostgreSQL\16\bin\psql.exe".into()),
    };
    c
}

#[test]
fn tool_paths_roundtrip_save_load() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    sample_with_tools().save(&p).unwrap();
    let loaded = AppConfig::load(&p).unwrap();
    assert_eq!(loaded, sample_with_tools());
}

#[test]
fn tool_paths_defaults_to_none_when_absent_from_old_config() {
    let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"
"#;
    let config: AppConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.tool_paths, ToolPaths::default());
    assert_eq!(config.tool_paths.psql, None);
}
```

- [ ] **Step 2: Run to see it fail**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state tool_paths`
  Expected: compile error (`ToolPaths` doesn't exist, `AppConfig` has no `tool_paths` field).

- [ ] **Step 3: Implement** the `ToolPaths` struct and the `AppConfig.tool_paths` field exactly as in the Interfaces block above (place `ToolPaths` immediately above `AppConfig` in the file, alongside `FavouriteObject`).

- [ ] **Step 4: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state`
  Expected: all existing tests still pass (the two new ones plus the full pre-existing suite), zero warnings.

- [ ] **Step 5: Commit**
```bash
git add crates/dbc-state/src/config.rs
git commit -m "feat: ToolPaths config for pg_dump/pg_restore/psql (G11 T1)"
```

---

### Task 2 (T2): `backup.rs` pure half — command/SQL builders, redaction, guards

**Files:**
- Create: `crates/dbc-ui/src/backup.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `mod backup;` — alphabetically between `mod autocomplete;` and `mod connect;`)

**Interfaces:**
- Consumes: `dbc_core::{quote_ident, QueryError}`, `dbc_state::{ConnectionConfig, Engine}`.
- Produces (consumed by T3's tests, T4, T6):
```rust
// --- Postgres argument builders -------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgDumpFormat { Custom, Plain }

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
    cfg: &ConnectionConfig, target_host: &str, target_port: u16,
    opts: &PgBackupOptions, out_path: &str,
) -> Vec<String>;

#[derive(Debug, Clone)]
pub struct PgRestoreOptions {
    pub clean_if_exists: bool,       // default true
    pub create_db: bool,             // default false
    pub no_owner_no_privileges: bool,// default true
    pub single_transaction: bool,    // default true
}
impl Default for PgRestoreOptions;

/// `pg_restore -h host -p port -U user -d database [--clean --if-exists]
/// [--create] [--no-owner --no-privileges] [-1] <dump_path>` — design §3.
pub fn build_pg_restore_args(
    cfg: &ConnectionConfig, target_host: &str, target_port: u16,
    opts: &PgRestoreOptions, dump_path: &str,
) -> Vec<String>;

/// `psql -h host -p port -U user -d database -f <dump_path>` — plain-SQL
/// restore, design §3 ("no equivalent transaction flag is forced").
pub fn build_psql_args(
    cfg: &ConnectionConfig, target_host: &str, target_port: u16, dump_path: &str,
) -> Vec<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat { Custom, Plain }

/// Sniffs the dump's first bytes for pg_restore's `PGDMP` custom-format
/// magic (design §3: "detects which by reading the dump's first bytes...
/// rather than trusting a file extension"); anything else is Plain.
pub fn detect_dump_format(bytes: &[u8]) -> DumpFormat;

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
pub fn build_backup_sql(database: &str, server_path: &str) -> String;

/// `RESTORE DATABASE "db" FROM DISK = N'server-path' WITH REPLACE, STATS = 10`.
pub fn build_restore_sql(database: &str, server_path: &str) -> String;

/// `ALTER DATABASE "db" SET SINGLE_USER WITH ROLLBACK IMMEDIATE` (`multi:
/// false`) or `... SET MULTI_USER` (`multi: true`) — design §3.
pub fn build_single_user_sql(database: &str, multi: bool) -> String;

// --- SQLite -----------------------------------------------------------------

/// `"SQLite format 3\0"` — design CURATION item 4.
pub const SQLITE_MAGIC_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// True only when `bytes` is at least 16 bytes and its first 16 bytes are
/// byte-for-byte `SQLITE_MAGIC_HEADER`. Never panics on a short slice.
pub fn sqlite_magic_header_ok(bytes: &[u8]) -> bool;

/// `VACUUM INTO 'dest-path'` — design §2, `''`-doubling for embedded quotes.
pub fn build_vacuum_into_sql(dest_path: &str) -> String;

// --- Shared read-only gate + redaction + confirm ---------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupOp { Backup, Restore }

/// Design CURATION item 2: the ONE documented exemption from the read-only
/// guard. `true` only for `Backup` — `Restore` is NEVER exempt, on any
/// engine, for any connection.
pub fn backup_exempt_from_read_only(op: BackupOp) -> bool;

/// The gate every T4 runner method calls first, before any I/O. Mirrors
/// `runner::guard_not_read_only`'s shape (same message, same
/// `Result<(), String>` — T4 maps this into a `QueryError` at the call
/// site since this module has no `dbc_core` error-type dependency of its
/// own beyond re-exported `QueryError`, kept separate here so this whole
/// module stays free of any `Connection`/async dependency).
pub fn guard_backup_restore_read_only(op: BackupOp, read_only: bool) -> Result<(), String>;

/// Replaces every occurrence of `secret` (when non-empty) with `***` —
/// applied to both the user-facing command-line echo and to any spawn
/// error string before it is ever surfaced (SECURITY requirement).
pub fn redact_secret(text: &str, secret: Option<&str>) -> String;

/// `program` + `args` joined with spaces, then redacted — what the confirm
/// modal and the log pane's first line show the user.
pub fn display_command_line(program: &str, args: &[String], secret: Option<&str>) -> String;

/// GitHub-delete-repo-pattern exact match (design §3) — case-sensitive,
/// no trimming (the user must type the exact name shown).
pub fn confirm_matches(typed: &str, expected: &str) -> bool;
```

**Grounding:**
- `dbc_core::quote_ident`/`quote_qualified` (`crates/dbc-core/src/ddl.rs:42-51`, re-exported at `dbc-core/src/lib.rs:12`) are the SAME functions `sandbox.rs` imports for its own SQL generation (`crates/dbc-ui/src/sandbox.rs:1,197`) — no second quoting function is invented here.
- `ConnectionConfig` (`crates/dbc-state/src/config.rs:33-51`) fields used: `.user`, `.database`. `target_host`/`target_port` are passed in SEPARATELY rather than read from `cfg.host`/`cfg.port` because an SSH-tunneled connection's real dial target is `127.0.0.1:{tunnel.local_port()}`, not `cfg.host`/`cfg.port` (same distinction `connect::open_config`, lines 106-112, already makes for the Postgres driver connection itself) — T4's caller resolves this the same way `open_config` does before calling into `build_pg_dump_args`/`build_pg_restore_args`.
- `''`-doubling for the MSSQL string-literal path and the SQLite `VACUUM INTO` path mirrors `sandbox.rs::sql_value`'s exact convention (`crates/dbc-ui/src/sandbox.rs:175-193`, `format!("'{}'", s.replace('\'', "''"))`) — a small private helper `fn sql_string_literal(s: &str) -> String` is added to `backup.rs` rather than importing `sandbox`'s (that one also handles the numeric-vs-quoted-value heuristic this module never needs).
- `PGDMP` magic bytes: `pg_dump`'s custom archive format (`-Fc`) always begins with the 5 ASCII bytes `PGDMP` — this is `pg_dump`'s own documented archive-format signature (design §3's own basis for the sniff), independent of dump content.

```rust
//! G11: backup/restore command construction (pure) + external-process
//! orchestration (T3, appended below). Pure half has zero I/O — no
//! `std::process`, no `std::fs` reads beyond what's handed in as `&[u8]`.

use dbc_state::ConnectionConfig;

fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgDumpFormat { Custom, Plain }

#[derive(Debug, Clone)]
pub struct PgBackupOptions {
    pub format: PgDumpFormat,
    pub compress: u8,
}

pub fn build_pg_dump_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    opts: &PgBackupOptions,
    out_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "-h".to_string(), target_host.to_string(),
        "-p".to_string(), target_port.to_string(),
        "-U".to_string(), cfg.user.clone(),
        "-d".to_string(), cfg.database.clone(),
        format!("--format={}", match opts.format { PgDumpFormat::Custom => "c", PgDumpFormat::Plain => "p" }),
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
        Self { clean_if_exists: true, create_db: false, no_owner_no_privileges: true, single_transaction: true }
    }
}

pub fn build_pg_restore_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    opts: &PgRestoreOptions,
    dump_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "-h".to_string(), target_host.to_string(),
        "-p".to_string(), target_port.to_string(),
        "-U".to_string(), cfg.user.clone(),
        "-d".to_string(), cfg.database.clone(),
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

pub fn build_psql_args(
    cfg: &ConnectionConfig,
    target_host: &str,
    target_port: u16,
    dump_path: &str,
) -> Vec<String> {
    vec![
        "-h".to_string(), target_host.to_string(),
        "-p".to_string(), target_port.to_string(),
        "-U".to_string(), cfg.user.clone(),
        "-d".to_string(), cfg.database.clone(),
        "-f".to_string(), dump_path.to_string(),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat { Custom, Plain }

pub fn detect_dump_format(bytes: &[u8]) -> DumpFormat {
    if bytes.starts_with(b"PGDMP") { DumpFormat::Custom } else { DumpFormat::Plain }
}

pub fn build_backup_sql(database: &str, server_path: &str) -> String {
    format!(
        "BACKUP DATABASE {} TO DISK = N{} WITH FORMAT, STATS = 10",
        dbc_core::quote_ident(database),
        sql_string_literal(server_path)
    )
}

pub fn build_restore_sql(database: &str, server_path: &str) -> String {
    format!(
        "RESTORE DATABASE {} FROM DISK = N{} WITH REPLACE, STATS = 10",
        dbc_core::quote_ident(database),
        sql_string_literal(server_path)
    )
}

pub fn build_single_user_sql(database: &str, multi: bool) -> String {
    let mode = if multi { "MULTI_USER" } else { "SINGLE_USER WITH ROLLBACK IMMEDIATE" };
    format!("ALTER DATABASE {} SET {mode}", dbc_core::quote_ident(database))
}

pub const SQLITE_MAGIC_HEADER: &[u8; 16] = b"SQLite format 3\0";

pub fn sqlite_magic_header_ok(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && &bytes[..16] == SQLITE_MAGIC_HEADER
}

pub fn build_vacuum_into_sql(dest_path: &str) -> String {
    format!("VACUUM INTO {}", sql_string_literal(dest_path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupOp { Backup, Restore }

pub fn backup_exempt_from_read_only(op: BackupOp) -> bool {
    matches!(op, BackupOp::Backup)
}

pub fn guard_backup_restore_read_only(op: BackupOp, read_only: bool) -> Result<(), String> {
    if read_only && !backup_exempt_from_read_only(op) {
        Err("připojení je jen pro čtení".to_string())
    } else {
        Ok(())
    }
}

pub fn redact_secret(text: &str, secret: Option<&str>) -> String {
    match secret {
        Some(s) if !s.is_empty() => text.replace(s, "***"),
        _ => text.to_string(),
    }
}

pub fn display_command_line(program: &str, args: &[String], secret: Option<&str>) -> String {
    let joined = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    redact_secret(&joined, secret)
}

pub fn confirm_matches(typed: &str, expected: &str) -> bool {
    typed == expected
}
```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/backup.rs`, `#[cfg(test)] mod pure_tests`):
```rust
#[cfg(test)]
mod pure_tests {
    use super::*;

    fn cfg() -> ConnectionConfig {
        ConnectionConfig {
            id: "c1".into(), name: "demo".into(), folder: Vec::new(),
            engine: dbc_state::Engine::Postgres, host: "db.internal".into(),
            port: Some(5432), database: "shop".into(), user: "alice".into(),
            read_only: false, timeout_secs: None, auto_limit: None, ssh: None,
            favourite: false,
        }
    }

    // --- SECURITY: PGPASSWORD never in argv ---
    const NASTY_PASSWORD: &str = "p'ss\"w$ord --format=evil";

    #[test]
    fn pg_dump_args_never_contain_the_password() {
        let opts = PgBackupOptions { format: PgDumpFormat::Custom, compress: 6 };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.backup");
        assert!(!args.iter().any(|a| a.contains(NASTY_PASSWORD)));
    }

    #[test]
    fn pg_restore_args_never_contain_the_password() {
        let args = build_pg_restore_args(&cfg(), "127.0.0.1", 15432, &PgRestoreOptions::default(), r"D:\bk\shop.backup");
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
        let opts = PgBackupOptions { format: PgDumpFormat::Custom, compress: 6 };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.backup");
        assert_eq!(
            args,
            vec![
                "-h", "127.0.0.1", "-p", "15432", "-U", "alice", "-d", "shop",
                "--format=c", r"--file=D:\bk\shop.backup", "--compress=6", "-v",
            ]
        );
    }

    #[test]
    fn pg_dump_args_plain_format_omits_compress() {
        let opts = PgBackupOptions { format: PgDumpFormat::Plain, compress: 6 };
        let args = build_pg_dump_args(&cfg(), "127.0.0.1", 15432, &opts, r"D:\bk\shop.sql");
        assert!(!args.iter().any(|a| a.starts_with("--compress")));
        assert!(args.contains(&"--format=p".to_string()));
    }

    #[test]
    fn pg_dump_compress_clamped_to_9() {
        let opts = PgBackupOptions { format: PgDumpFormat::Custom, compress: 200 };
        let args = build_pg_dump_args(&cfg(), "h", 1, &opts, "f");
        assert!(args.contains(&"--compress=9".to_string()));
    }

    #[test]
    fn pg_restore_default_options_shape() {
        let args = build_pg_restore_args(&cfg(), "127.0.0.1", 15432, &PgRestoreOptions::default(), r"D:\bk\shop.backup");
        assert_eq!(
            args,
            vec![
                "-h", "127.0.0.1", "-p", "15432", "-U", "alice", "-d", "shop",
                "--clean", "--if-exists", "--no-owner", "--no-privileges", "-1",
                r"D:\bk\shop.backup",
            ]
        );
    }

    #[test]
    fn pg_restore_all_options_off_is_bare() {
        let opts = PgRestoreOptions { clean_if_exists: false, create_db: false, no_owner_no_privileges: false, single_transaction: false };
        let args = build_pg_restore_args(&cfg(), "h", 1, &opts, "f.backup");
        assert_eq!(args, vec!["-h", "h", "-p", "1", "-U", "alice", "-d", "shop", "f.backup"]);
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
        assert_eq!(args, vec!["-h", "127.0.0.1", "-p", "15432", "-U", "alice", "-d", "shop", "-f", r"D:\bk\shop.sql"]);
    }

    // --- dump format sniff ---
    #[test]
    fn detects_custom_format_via_pgdmp_magic() {
        assert_eq!(detect_dump_format(b"PGDMP\x01\x0e\x00rest"), DumpFormat::Custom);
    }

    #[test]
    fn treats_anything_else_as_plain() {
        assert_eq!(detect_dump_format(b"-- pg_dump plain SQL\nCREATE TABLE"), DumpFormat::Plain);
        assert_eq!(detect_dump_format(b""), DumpFormat::Plain);
        assert_eq!(detect_dump_format(b"PGDM"), DumpFormat::Plain); // short prefix match, not full magic
    }

    // --- MSSQL SQL builders ---
    #[test]
    fn backup_sql_shape_and_quoting() {
        let sql = build_backup_sql("my\"db", r"D:\Backups\mydb.bak");
        assert_eq!(sql, "BACKUP DATABASE \"my\"\"db\" TO DISK = N'D:\\Backups\\mydb.bak' WITH FORMAT, STATS = 10");
    }

    #[test]
    fn restore_sql_shape() {
        let sql = build_restore_sql("mydb", r"D:\Backups\mydb.bak");
        assert_eq!(sql, "RESTORE DATABASE \"mydb\" FROM DISK = N'D:\\Backups\\mydb.bak' WITH REPLACE, STATS = 10");
    }

    #[test]
    fn single_user_sql_both_directions() {
        assert_eq!(build_single_user_sql("mydb", false), "ALTER DATABASE \"mydb\" SET SINGLE_USER WITH ROLLBACK IMMEDIATE");
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
        assert_eq!(build_vacuum_into_sql(r"D:\bk\shop.sqlite"), "VACUUM INTO 'D:\\bk\\shop.sqlite'");
        assert_eq!(build_vacuum_into_sql(r"D:\o'brien.sqlite"), "VACUUM INTO 'D:\\o''brien.sqlite'");
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
        let args = build_pg_dump_args(&cfg(), "h", 1, &PgBackupOptions { format: PgDumpFormat::Custom, compress: 6 }, "f");
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
```

- [ ] **Step 2: Run to see it fail**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui backup::pure_tests`
  Expected: compile error (`backup` module doesn't exist).

- [ ] **Step 3: Implement** — write the full code block from the Interfaces/Grounding sections above into `crates/dbc-ui/src/backup.rs`, and add `mod backup;` to `crates/dbc-ui/src/main.rs`'s mod list (alphabetically between `mod autocomplete;` and `mod connect;`). Since nothing consumes this module's public items yet, add `#[allow(dead_code)] // consumed from T3/T4 on` on the `mod backup;` line to keep the build warning-free in the interim.

- [ ] **Step 4: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui backup::`
  Expected: all tests in `pure_tests` pass, zero warnings.

- [ ] **Step 5: Commit**
```bash
git add crates/dbc-ui/src/backup.rs crates/dbc-ui/src/main.rs
git commit -m "feat: backup.rs pure command/SQL builders + redaction + read-only guard (G11 T2)"
```

---

### Task 3 (T3): `backup.rs` process half — spawn/stream/kill-on-drop + tool detection

**Files:**
- Modify: `crates/dbc-ui/src/backup.rs` (append)

**Interfaces:**
- Consumes: T2's builders (tests only), `std::process::{Command, Stdio, Child}`.
- Produces (consumed by T4):
```rust
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// One log line, or a terminal outcome — the shape `runner.rs` (T4) streams
/// over an `mpsc::Sender` exactly like `QueryEvent` already streams query
/// progress (`runner.rs:10-15`).
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
    child: std::sync::Arc<std::sync::Mutex<Option<Child>>>,
}
impl BackupHandle {
    pub fn cancel(&self);
}

/// Spawns `program` with `args`, `PGPASSWORD` (when `password` is
/// `Some`) set on the CHILD'S ENVIRONMENT ONLY, stdin closed, stderr
/// piped and streamed line-by-line as `BackupEvent::Log` — mirrors
/// `tunnel.rs::spawn_ssh`'s `Stdio` shape and `CREATE_NO_WINDOW` use
/// exactly, generalized from one fixed binary (`ssh`) to any program.
/// MUST be called from a blocking-safe context (`tokio::task::spawn_blocking`
/// — see T4) since this function blocks its calling thread for the entire
/// process lifetime, exactly like `Tunnel::open`'s doc comment already
/// mandates for its own blocking poll loop.
///
/// Every line sent as `BackupEvent::Log`/`BackupEvent::Failed` is passed
/// through `redact_secret` first (SECURITY requirement — defense in depth
/// even though `password` is never in `args`).
pub fn run_and_stream(
    program: &str,
    args: &[String],
    password: Option<&str>,
    tx: &std::sync::mpsc::Sender<BackupEvent>,
) -> BackupHandle;

/// Same PATH-probe shape as `tunnel.rs::ssh_binary` (`Command::new("where")`
/// on Windows), generalized to any program name — design §1.
pub fn find_on_path(name: &str) -> bool;

/// Given already-discovered `(version_dir_path, mtime)` pairs (design §1:
/// `C:\Program Files\PostgreSQL\*`), picks the entry whose final path
/// component parses as the numerically highest `u32`; ties broken by mtime
/// (later wins); non-numeric final components are ignored entirely. `None`
/// on an empty or all-non-numeric input. Pure (mtime is supplied by the
/// caller, T4, which does the actual `std::fs::read_dir` — see T4's
/// Grounding for why this keeps the function itself hermetically testable).
pub fn pick_highest_version_dir(dirs: &[(String, std::time::SystemTime)]) -> Option<String>;
```

**Grounding:**
- `tunnel.rs`'s `spawn_ssh` (lines 122-138) is the exact `Stdio::null()`/`Stdio::piped()`/`CREATE_NO_WINDOW` shape reused here verbatim, generalized from a hardcoded `ssh_binary()` to any `program: &str` — the design explicitly calls for this ("reuses `tunnel.rs`'s shape... dedicated struct that kills the child on `Drop`"). Unlike `Tunnel` (which owns its `Child` directly and is dropped when the tunnel itself is dropped), `BackupHandle` wraps the `Child` in `Arc<Mutex<Option<Child>>>` because the UI (T6) needs to trigger a kill from the GPUI foreground thread while the actual process I/O (reading stderr lines) runs on a SEPARATE `spawn_blocking` thread inside `run_and_stream` — a plain owned `Child` can't be shared across that boundary the way `Tunnel`'s single-owner `Drop` impl can.
- `tunnel.rs::ssh_binary` (lines 143-159) is the exact `where`/`which` PATH-probe shape `find_on_path` generalizes — note the ORIGINAL caches its one result in a `static OnceLock<bool>` because it only ever probes for `"ssh"`; `find_on_path` here takes `name: &str` and does NOT cache (three different tool names — `pg_dump`/`pg_restore`/`psql` — each probed independently, and infrequently, at spawn time, per design §1: "resolved independently... a user may only have one on PATH").
- `read_stderr_tail` (tunnel.rs:163-175) reads whatever's ALREADY buffered without blocking further — `run_and_stream` differs deliberately: it blocks reading the FULL stream line-by-line via `std::io::BufRead::lines()` on the child's piped stderr, for the whole process lifetime, since (unlike the tunnel's one-shot "get the last error on failure" use) the whole POINT here is showing every line to the user as it arrives (design §2: "a scrolling read-only log pane... auto-scrolled").
- `std::sync::mpsc::Sender` (NOT `tokio::sync::mpsc::Sender`) is used for `run_and_stream`'s `tx` parameter deliberately: this function's entire body runs on a `spawn_blocking` thread (never inside an async task), so a plain blocking std channel avoids any `.blocking_send()` panic-on-wrong-context footgun a `tokio::sync::mpsc::Sender` would introduce; T4's runner method bridges this std-channel side to the `tokio::sync::mpsc::Receiver<BackupEvent>` its own public API returns, via a short forwarding loop — see T4's Grounding.
- `pick_highest_version_dir`'s signature deliberately takes `(String, SystemTime)` pairs rather than doing its own `std::fs::read_dir`/`.metadata()` calls, DEVIATING from the design's "pure function `pick_highest_version_dir(dirs: &[String])`" sketch by adding the mtime as an explicit input — this keeps the tie-break rule (design §1: "ties broken by directory mtime") fully unit-testable with synthetic `SystemTime` values (`UNIX_EPOCH + Duration::from_secs(n)`) with ZERO filesystem access, which a `dirs: &[String]`-only signature could not achieve without either dropping the mtime tie-break or making the function impure. The actual `std::fs::read_dir(r"C:\Program Files\PostgreSQL")` call producing these pairs lives in T4 (`resolve_tool_path`), the one place in this plan that genuinely needs to touch that directory.

```rust
// (appended to crates/dbc-ui/src/backup.rs)

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc::Sender, Arc, Mutex};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupEvent {
    Log(String),
    Finished,
    Failed(String),
}

#[derive(Clone)]
pub struct BackupHandle {
    child: Arc<Mutex<Option<Child>>>,
}

impl BackupHandle {
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub fn run_and_stream(
    program: &str,
    args: &[String],
    password: Option<&str>,
    tx: &Sender<BackupEvent>,
) -> BackupHandle {
    let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let handle = BackupHandle { child: slot.clone() };

    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
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

    if let Some(stderr) = stderr {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    let _ = tx.send(BackupEvent::Log(redact_secret(&l, password)));
                }
                Err(_) => break, // pipe closed (process exited/killed) — fall through to wait()
            }
        }
    }

    // Take the child back out to `wait()` on it — a concurrent `cancel()`
    // may have already taken (and killed+waited) it, in which case `taken`
    // is `None` and this call is a no-op (the process is already reaped).
    let taken = slot.lock().ok().and_then(|mut g| g.take());
    match taken {
        Some(mut c) => match c.wait() {
            Ok(status) if status.success() => {
                let _ = tx.send(BackupEvent::Finished);
            }
            Ok(status) => {
                let _ = tx.send(BackupEvent::Failed(redact_secret(
                    &format!("{program} skončil s chybou ({status})"),
                    password,
                )));
            }
            Err(e) => {
                let _ = tx.send(BackupEvent::Failed(redact_secret(&format!("{program}: {e}"), password)));
            }
        },
        None => {
            // Cancelled mid-stream — `cancel()` already killed+waited it.
            let _ = tx.send(BackupEvent::Failed("přerušeno uživatelem".to_string()));
        }
    }

    handle
}

pub fn find_on_path(name: &str) -> bool {
    #[cfg(windows)]
    let probe = Command::new("where").arg(name).output();
    #[cfg(not(windows))]
    let probe = Command::new("which").arg(name).output();
    matches!(probe, Ok(o) if o.status.success())
}

pub fn pick_highest_version_dir(dirs: &[(String, std::time::SystemTime)]) -> Option<String> {
    dirs.iter()
        .filter_map(|(path, mtime)| {
            let last = std::path::Path::new(path).file_name()?.to_str()?;
            last.parse::<u32>().ok().map(|v| (v, *mtime, path.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
        .map(|(_, _, path)| path)
}
```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/backup.rs`, `#[cfg(test)] mod process_tests`):
```rust
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
                BackupEvent::Finished => { finished = true; break; }
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
        let handle_slot: std::sync::Arc<std::sync::Mutex<Option<BackupHandle>>> = std::sync::Arc::new(std::sync::Mutex::new(None));
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
        assert!(start.elapsed() < Duration::from_secs(25), "cancel should end the 30s ping almost immediately");
        assert!(matches!(ev, BackupEvent::Failed(_)));
    }

    #[test]
    fn find_on_path_finds_a_universally_present_binary() {
        // `cmd.exe` (Windows) is always on PATH in this repo's CI/dev
        // environment (the tool this test itself just spawned above).
        #[cfg(windows)]
        assert!(find_on_path("cmd"));
    }

    #[test]
    fn find_on_path_missing_binary_is_false() {
        assert!(!find_on_path("definitely-not-a-real-binary-xyz"));
    }

    fn t(secs: u64) -> SystemTime { UNIX_EPOCH + Duration::from_secs(secs) }

    #[test]
    fn picks_highest_numeric_version() {
        let dirs = vec![
            (r"C:\Program Files\PostgreSQL\14".to_string(), t(100)),
            (r"C:\Program Files\PostgreSQL\16".to_string(), t(200)),
            (r"C:\Program Files\PostgreSQL\9".to_string(), t(50)),
        ];
        assert_eq!(pick_highest_version_dir(&dirs).as_deref(), Some(r"C:\Program Files\PostgreSQL\16"));
    }

    #[test]
    fn ignores_non_numeric_dirs() {
        let dirs = vec![
            (r"C:\Program Files\PostgreSQL\16".to_string(), t(100)),
            (r"C:\Program Files\PostgreSQL\pgAdmin 4".to_string(), t(999)),
        ];
        assert_eq!(pick_highest_version_dir(&dirs).as_deref(), Some(r"C:\Program Files\PostgreSQL\16"));
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
```

- [ ] **Step 2: Run to see it fail**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui backup::process_tests`
  Expected: compile error (the process-half symbols don't exist yet).

- [ ] **Step 3: Implement** the process half exactly as in the Grounding code block above (append after the pure half from T2, same file).

- [ ] **Step 4: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui backup::`
  Expected: all of `pure_tests` (T2) and `process_tests` (T3) pass, zero warnings. `cancel_kills_a_long_running_process_before_it_finishes` is timing-sensitive but bounded well under its own 25s assertion; if it's ever flaky in CI, widen the initial `sleep`, not the assertion bound.

- [ ] **Step 5: Commit**
```bash
git add crates/dbc-ui/src/backup.rs
git commit -m "feat: backup.rs process spawn/stream/cancel + tool-path discovery (G11 T3)"
```

---

### Task 4 (T4): Runner-owned methods — `runner.rs` + `connection.rs` doc amendment

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`
- Modify: `crates/dbc-core/src/connection.rs` (doc comment only)

**Interfaces:**
- Consumes: T1's `ToolPaths`, T2's builders/guards, T3's `run_and_stream`/`BackupHandle`/`find_on_path`/`pick_highest_version_dir`.
- Produces (consumed by T6):
```rust
/// Resolves an external tool's path per design §1's three-step order:
/// (1) `configured` if `Some` — validated as an existing file HERE, at use
///     time, not at save time (a stale saved path surfaces as an error,
///     never silently falls through); (2) PATH via `backup::find_on_path`;
///     (3) glob `C:\Program Files\PostgreSQL\*\bin\<name>.exe`, highest
///     version wins (`backup::pick_highest_version_dir`). Returns the
///     literal program string to hand to `Command::new` — either a bare
///     name (PATH case, step 2) or a full path (steps 1/3).
pub fn resolve_tool_path(configured: Option<&str>, name: &str) -> Result<String, QueryError>;

impl QueryRunner {
    /// One generic external-tool runner, used for `pg_dump`, `pg_restore`,
    /// AND `psql` alike — their spawn/stream/redact mechanics are identical
    /// (design §2/§3 both reduce to "spawn a program with PGPASSWORD in its
    /// env, stream stderr as log lines"), so one method serves all three
    /// rather than three near-duplicates. Runs on a `spawn_blocking` thread
    /// (per `run_and_stream`'s contract) and forwards each `BackupEvent`
    /// over a `tokio::sync::mpsc::Receiver` the caller drains exactly like
    /// `connect_and_run`'s `QueryEvent` receiver (runner.rs:1057-ish,
    /// main.rs's `cx.spawn` loop).
    pub fn run_external_tool(
        &self,
        program: String,
        args: Vec<String>,
        password: Option<String>,
    ) -> (tokio::sync::mpsc::Receiver<backup::BackupEvent>, backup::BackupHandle);

    /// MSSQL `BACKUP DATABASE` — allowed on read-only (design CURATION item
    /// 2). Runs over ONE fresh connection (`open_spec`, dropped at the end),
    /// same one-shot shape `fetch_schema`/`test_connect` already use.
    pub fn run_mssql_backup(
        &self, spec: ConnectSpec, database: String, server_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>;

    /// MSSQL restore: SINGLE_USER -> RESTORE -> MULTI_USER, one dedicated
    /// connection, MULTI_USER always attempted even if RESTORE failed
    /// (best-effort try/finally shape — design §3). Hard-blocked on
    /// read-only, no override.
    pub fn run_mssql_restore(
        &self, spec: ConnectSpec, database: String, server_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>;

    /// SQLite `VACUUM INTO` via `Connection::execute` — allowed on
    /// read-only.
    pub fn run_sqlite_backup(
        &self, spec: ConnectSpec, dest_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>;

    /// SQLite restore: magic-header check (T2, pure) then `fs::copy` — no
    /// `Connection`/`ConnectSpec` involved at all (a plain file operation).
    /// Hard-blocked on read-only via the CALLER (T6 — see this method's
    /// Grounding for why the gate lives one level up here, uniquely among
    /// the four methods in this list).
    pub fn run_sqlite_restore(
        &self, db_path: String, backup_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>;
}
```

**Grounding:**
- **`execute()`'s doc-comment amendment** (`crates/dbc-core/src/connection.rs:19-20`): current text is `/// the app's write path — ONLY the sandbox Apply flow may call it.` This plan appends (does not delete existing sanctioned-caller text from any already-merged phase — re-read the comment by symbol before editing, per this plan's Global Constraints task-ordering note):
  ```rust
  /// Executes a non-returning statement, reporting affected rows. This is
  /// the app's write path. Sanctioned callers: the sandbox Apply flow
  /// (`runner::run_write_transaction`); G11's `run_mssql_backup`,
  /// `run_mssql_restore`, and `run_sqlite_backup` (`runner.rs`) — each a
  /// named, gated method per this file's own transaction-discipline
  /// contract below, never raw ad-hoc SQL.
  ```
  `run_sqlite_restore` is deliberately NOT added to this list — it never calls `execute()` at all (see its own bullet below).
- **`open_spec`** (runner.rs:480-497) is the shared dispatcher every one-shot method already uses (`fetch_schema`, `test_connect`, `fetch_lookup`) — `run_mssql_backup`/`run_mssql_restore`/`run_sqlite_backup` reuse it verbatim, inheriting its `spawn_blocking`-wrapped connect + panic-to-`QueryError` mapping for free. For MSSQL, `open_spec` against a `ConnectSpec::Config{cfg,..}` with `cfg.engine == Engine::Mssql` reaches `connect::open_config`'s `Engine::Mssql` arm and returns `Err(QueryError::msg("MSSQL driver zatím není k dispozici"))` immediately — this is the exact, ALREADY-EXISTING behavior every other MSSQL feature in this app has today; these two methods add no MSSQL-specific handling around that error, they simply surface it the same way `connect_and_run`'s `Err(e) => tx.send(QueryEvent::Failed(e))` arm already does for query runs against an MSSQL connection. **REQUIRED test**, T4: `run_mssql_backup`/`run_mssql_restore` against an `Engine::Mssql` `ConnectSpec` return exactly that error string.
- **`guard_not_read_only`/`spec_is_read_only`** (runner.rs:249-272) is the EXISTING general write-path gate — this plan does not touch it or weaken it (per Global Constraints). Every T4 method instead calls the NEW `backup::guard_backup_restore_read_only(op, spec_is_read_only(&spec))` — reusing `spec_is_read_only` (already `pub(crate)`-visible within `runner.rs`, no new helper needed) but swapping in the backup/restore-aware predicate instead of the unconditional one, exactly matching design CURATION item 2's "never by weakening the shared read-only guard... via ONE documented exemption predicate."
- **`run_sqlite_restore` has no `ConnectSpec` argument at all** — a deliberate, grounded deviation from the design's sketch (design §3 SQLite: "(1) drops every live `Connection`... (2) `fs::copy`... (3) marks disconnected"). As established in this plan's Spec section, `runner.rs` never holds a live `Connection` for a saved connection outside one in-flight run, so step (1) has nothing to close, and `run_sqlite_restore` needs neither a secret nor a network round-trip — just two local paths. Its read-only gate is therefore evaluated by the CALLER (T6), which already has `cfg.read_only` in hand from `self.config` before ever reaching this method — T6's Grounding shows the exact call site. This is the ONE of the four T4 methods whose gate isn't inline in `runner.rs` itself; flagged here so it isn't mistaken for an oversight.
- **Dedicated-connection MSSQL restore sequence**, `run_mssql_restore`: opens ONE connection via `open_spec`, then issues `build_single_user_sql(db, false)`, `build_restore_sql(db, path)`, `build_single_user_sql(db, true)` in order over that SAME connection (transaction-per-connection invariant, `Connection::execute`'s own doc comment, `crates/dbc-core/src/connection.rs:22-26`). The closing `MULTI_USER` statement is attempted even if `RESTORE` failed (best-effort, mirrors `drive_write_sequence`'s own "the ROLLBACK attempt's result is discarded" posture at runner.rs:319-322) — the FIRST failure among the three statements is what's returned to the caller; a subsequent `MULTI_USER` failure is logged nowhere further (there's no log sink at this layer) but does not override the original error.
- **Bridging `run_and_stream`'s `std::sync::mpsc::Sender` to a `tokio::sync::mpsc::Receiver`**, `run_external_tool`: `run_and_stream` (T3) blocks its OWN thread for the process's whole lifetime and can only push into a std channel from there. `run_external_tool` therefore: (a) creates a `tokio::sync::mpsc::channel(256)` for its return value, (b) creates a plain `std::sync::mpsc::channel()` for `run_and_stream` to write into, (c) spawns `run_and_stream` itself inside `self.handle().spawn_blocking(...)`, and (d) spawns a SEPARATE lightweight forwarding task (`self.runtime.spawn(async move { ... })`) that loops `std_rx.recv()` — itself run inside ANOTHER `spawn_blocking` per iteration (a blocking std `Receiver::recv()` inside a plain async task would block a runtime worker thread) — and re-sends each item into the tokio channel via `.send().await`. `BackupHandle` is returned to the caller SYNCHRONOUSLY (not through either channel) via a `tokio::sync::oneshot` set once `run_and_stream` has actually spawned the child (see the code below for the exact handshake) — the caller needs it immediately so "Zrušit" is wireable before the first log line even arrives.

```rust
// (added to crates/dbc-ui/src/runner.rs)

use crate::backup;

pub fn resolve_tool_path(configured: Option<&str>, name: &str) -> Result<String, QueryError> {
    if let Some(path) = configured {
        return if std::path::Path::new(path).is_file() {
            Ok(path.to_string())
        } else {
            Err(QueryError::msg(format!(
                "nakonfigurovaná cesta k {name} neexistuje: {path} — nastavte ji znovu"
            )))
        };
    }
    if backup::find_on_path(name) {
        return Ok(name.to_string());
    }
    let exe = format!("{name}.exe");
    let base = std::path::Path::new(r"C:\Program Files\PostgreSQL");
    let mut candidates: Vec<(String, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            if let Some(p) = entry.path().to_str() {
                candidates.push((p.to_string(), mtime));
            }
        }
    }
    let best_dir = backup::pick_highest_version_dir(&candidates);
    match best_dir {
        Some(dir) => {
            let full = std::path::Path::new(&dir).join("bin").join(&exe);
            if full.is_file() {
                Ok(full.to_string_lossy().to_string())
            } else {
                Err(QueryError::msg(format!("{name} nenalezen — nastavte cestu ručně")))
            }
        }
        None => Err(QueryError::msg(format!("{name} nenalezen — nastavte cestu ručně"))),
    }
}

impl QueryRunner {
    pub fn run_external_tool(
        &self,
        program: String,
        args: Vec<String>,
        password: Option<String>,
    ) -> (tokio::sync::mpsc::Receiver<backup::BackupEvent>, backup::BackupHandle) {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let (std_tx, std_rx) = std::sync::mpsc::channel::<backup::BackupEvent>();
        let (handle_tx, handle_rx) = std::sync::mpsc::channel::<backup::BackupHandle>();

        // The blocking spawn+stream loop — never runs on a runtime worker
        // thread (spawn_blocking contract, same as `Tunnel::open`'s own doc
        // comment mandates).
        self.handle().spawn_blocking(move || {
            let handle = backup::run_and_stream(&program, &args, password.as_deref(), &std_tx);
            let _ = handle_tx.send(handle);
        });

        // Forwarding loop: std channel -> tokio channel, off the UI thread.
        self.runtime.spawn(async move {
            loop {
                let next = tokio::task::spawn_blocking({
                    let std_rx_recv = std_rx.recv();
                    move || std_rx_recv
                })
                .await;
                match next {
                    Ok(Ok(ev)) => {
                        let terminal = matches!(ev, backup::BackupEvent::Finished | backup::BackupEvent::Failed(_));
                        if tx.send(ev).await.is_err() || terminal {
                            break;
                        }
                    }
                    _ => break, // sender dropped — run_and_stream returned
                }
            }
        });

        // Block briefly (this whole method is synchronous, called from the
        // UI thread) for the handle to arrive — `run_and_stream` sends it
        // the moment it has EITHER spawned the child OR failed to spawn at
        // all, so this never waits for the process to finish, only to start.
        let handle = handle_rx.recv().unwrap_or_else(|_| {
            // spawn_blocking task panicked before sending — degrade to a
            // handle whose cancel() is a harmless no-op rather than
            // panicking the UI thread.
            backup::BackupHandle::from_already_gone()
        });
        (rx, handle)
    }

    pub fn run_mssql_backup(
        &self,
        spec: ConnectSpec,
        database: String,
        server_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_mssql_backup_inner(spec, database, server_path, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    pub fn run_mssql_restore(
        &self,
        spec: ConnectSpec,
        database: String,
        server_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_mssql_restore_inner(spec, database, server_path, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    pub fn run_sqlite_backup(
        &self,
        spec: ConnectSpec,
        dest_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = run_sqlite_backup_inner(spec, dest_path, handle).await;
            let _ = tx.send(result);
        });
        rx
    }

    pub fn run_sqlite_restore(
        &self,
        db_path: String,
        backup_path: String,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || run_sqlite_restore_inner(&db_path, &backup_path))
                .await
                .unwrap_or_else(|_| Err(QueryError::msg("restore task panicked")));
            let _ = tx.send(result);
        });
        rx
    }
}

async fn run_mssql_backup_inner(
    spec: ConnectSpec,
    database: String,
    server_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Backup, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    let sql = backup::build_backup_sql(&database, &server_path);
    opened.conn.execute(&sql, CancelToken::new()).await?;
    Ok(())
}

async fn run_mssql_restore_inner(
    spec: ConnectSpec,
    database: String,
    server_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Restore, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();

    opened.conn.execute(&backup::build_single_user_sql(&database, false), cancel.clone()).await?;
    let restore_result = opened
        .conn
        .execute(&backup::build_restore_sql(&database, &server_path), cancel.clone())
        .await;
    // Best-effort MULTI_USER regardless of RESTORE's outcome (design §3,
    // step 4 "always runs even if step 3 fails") — its own result never
    // overrides `restore_result`.
    let _ = opened.conn.execute(&backup::build_single_user_sql(&database, true), cancel).await;
    restore_result.map(|_| ())
}

async fn run_sqlite_backup_inner(
    spec: ConnectSpec,
    dest_path: String,
    handle: tokio::runtime::Handle,
) -> Result<(), QueryError> {
    backup::guard_backup_restore_read_only(backup::BackupOp::Backup, spec_is_read_only(&spec))
        .map_err(QueryError::msg)?;
    let mut opened = open_spec(spec, handle).await?;
    opened.conn.execute(&backup::build_vacuum_into_sql(&dest_path), CancelToken::new()).await?;
    Ok(())
}

/// Design CURATION item 4, hard requirement: reads the first 16 bytes of
/// `backup_path` and refuses (no copy attempted) unless they are exactly
/// `backup::SQLITE_MAGIC_HEADER`.
fn run_sqlite_restore_inner(db_path: &str, backup_path: &str) -> Result<(), QueryError> {
    let mut header = [0u8; 16];
    let mut f = std::fs::File::open(backup_path).map_err(|e| QueryError::msg(e.to_string()))?;
    use std::io::Read;
    let n = f.read(&mut header).map_err(|e| QueryError::msg(e.to_string()))?;
    if !backup::sqlite_magic_header_ok(&header[..n]) {
        return Err(QueryError::msg("soubor není SQLite databáze"));
    }
    drop(f);
    std::fs::copy(backup_path, db_path).map_err(|e| QueryError::msg(e.to_string()))?;
    Ok(())
}
```

  A small addition to `backup::BackupHandle` (T3's type, extended here since `run_external_tool` needs a "process never actually started" degenerate case):
  ```rust
  impl BackupHandle {
      /// Used only by `run_external_tool`'s panic-recovery path — a handle
      /// whose `cancel()` is a harmless no-op because there was never a
      /// child to kill.
      pub fn from_already_gone() -> Self {
          Self { child: std::sync::Arc::new(std::sync::Mutex::new(None)) }
      }
  }
  ```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/runner.rs`, new `#[cfg(test)] mod backup_runner_tests`):
```rust
#[cfg(test)]
mod backup_runner_tests {
    use super::*;

    fn cfg(engine: dbc_state::Engine, read_only: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "x".into(), name: "x".into(), folder: Vec::new(), engine,
            host: String::new(), port: None, database: String::new(), user: String::new(),
            read_only, timeout_secs: None, auto_limit: None, ssh: None, favourite: false,
        }
    }

    #[test]
    fn resolve_tool_path_configured_but_missing_is_a_value_error() {
        let err = resolve_tool_path(Some(r"D:\definitely\not\real\pg_dump.exe"), "pg_dump").unwrap_err();
        assert!(err.message.contains("pg_dump"));
    }

    #[test]
    fn resolve_tool_path_no_config_and_not_on_path_and_no_glob_hit_is_friendly_error() {
        // "definitely-not-a-real-tool" is neither configured, on PATH, nor
        // under C:\Program Files\PostgreSQL — exercises the full fallthrough.
        let err = resolve_tool_path(None, "definitely-not-a-real-tool-xyz").unwrap_err();
        assert!(err.message.contains("nenalezen"));
    }

    // --- MSSQL: fails fast at open_spec, exactly like every other MSSQL
    // feature in this app today (Spec section grounding) — REQUIRED. ---
    #[tokio::test]
    async fn run_mssql_backup_against_mssql_engine_fails_with_the_standard_unwired_message() {
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, false)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_backup_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(err.message.contains("MSSQL driver zatím není k dispozici"));
    }

    #[tokio::test]
    async fn run_mssql_restore_against_mssql_engine_fails_with_the_standard_unwired_message() {
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, false)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_restore_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(err.message.contains("MSSQL driver zatím není k dispozici"));
    }

    // --- read-only gates, REQUIRED, no I/O attempted in the refusing path ---
    #[tokio::test]
    async fn mssql_restore_refuses_read_only_without_connecting() {
        let spec = ConnectSpec::Config {
            cfg: Box::new(cfg(dbc_state::Engine::Sqlite, true)), // engine irrelevant — guard fires first
            secret: None,
        };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_restore_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert_eq!(err.message, "připojení je jen pro čtení");
    }

    #[tokio::test]
    async fn mssql_backup_allowed_even_when_read_only_reaches_open_spec_not_the_guard() {
        // read_only=true + Backup must NOT be refused by the guard — the
        // MSSQL-unwired error proves the guard passed and open_spec was
        // actually reached.
        let spec = ConnectSpec::Config { cfg: Box::new(cfg(dbc_state::Engine::Mssql, true)), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_mssql_backup_inner(spec, "db".into(), r"D:\x.bak".into(), handle).await.unwrap_err();
        assert!(err.message.contains("MSSQL driver zatím není k dispozici"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn sqlite_backup_refuses_read_only_without_connecting() {
        let spec = ConnectSpec::Config {
            cfg: Box::new({ let mut c = cfg(dbc_state::Engine::Sqlite, true); c.database = "\0invalid".into(); c }),
            secret: None,
        };
        let handle = tokio::runtime::Handle::current();
        let err = run_sqlite_backup_inner(spec, r"D:\x.sqlite".into(), handle).await;
        // Backup is exempt from read-only, so this must NOT be the
        // read-only message — it must instead fail later (bad path), proving
        // the guard passed through and open_spec was actually attempted.
        assert_ne!(err.unwrap_err().message, "připojení je jen pro čtení");
    }

    // --- SQLite restore: magic header + real copy, temp files, no docker ---
    #[test]
    fn sqlite_restore_refuses_non_sqlite_source_without_copying() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("not_a_db.txt");
        std::fs::write(&src, b"hello world, not a database").unwrap();
        let dest = dir.path().join("target.sqlite");
        std::fs::write(&dest, b"ORIGINAL CONTENT").unwrap();

        let err = run_sqlite_restore_inner(dest.to_str().unwrap(), src.to_str().unwrap()).unwrap_err();
        assert_eq!(err.message, "soubor není SQLite databáze");
        // Original destination file must be untouched.
        assert_eq!(std::fs::read(&dest).unwrap(), b"ORIGINAL CONTENT");
    }

    #[test]
    fn sqlite_restore_copies_a_valid_sqlite_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("backup.sqlite");
        let mut content = backup::SQLITE_MAGIC_HEADER.to_vec();
        content.extend_from_slice(b"rest of a fake but header-valid sqlite file");
        std::fs::write(&src, &content).unwrap();
        let dest = dir.path().join("live.sqlite");
        std::fs::write(&dest, b"stale content").unwrap();

        run_sqlite_restore_inner(dest.to_str().unwrap(), src.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    // --- run_external_tool: end-to-end with a real (non-pg_dump) process ---
    #[tokio::test]
    async fn run_external_tool_streams_and_finishes_with_a_real_process() {
        let runner = QueryRunner::new();
        let (mut rx, _handle) = runner.run_external_tool(
            "cmd".to_string(),
            vec!["/C".to_string(), "echo hello 1>&2".to_string()],
            None,
        );
        let mut saw_log = false;
        let mut saw_finished = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                backup::BackupEvent::Log(l) if l.contains("hello") => saw_log = true,
                backup::BackupEvent::Finished => { saw_finished = true; break; }
                backup::BackupEvent::Failed(m) => panic!("unexpected failure: {m}"),
                _ => {}
            }
        }
        assert!(saw_log && saw_finished);
    }

    #[tokio::test]
    async fn run_external_tool_missing_binary_is_a_failed_event() {
        let runner = QueryRunner::new();
        let (mut rx, _handle) = runner.run_external_tool("definitely-not-a-real-binary-xyz".to_string(), vec![], None);
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, backup::BackupEvent::Failed(_)));
    }
}
```

- [ ] **Step 2: Run to see it fail**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui backup_runner_tests`
  Expected: compile errors (none of the new symbols exist in `runner.rs` yet).

- [ ] **Step 3: Implement** — add `use crate::backup;`, `resolve_tool_path`, the five new `QueryRunner` methods, and the four `*_inner` free functions exactly as in the Grounding code block above; extend `BackupHandle` with `from_already_gone` in `backup.rs`. Amend `crates/dbc-core/src/connection.rs`'s doc comment on `execute()` per the Grounding bullet above (re-read the CURRENT text first — do not assume it still reads exactly as quoted in this plan if another phase's amendment has already landed; append this plan's sentence rather than overwriting).

- [ ] **Step 4: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-ui`
  Expected: all pass, zero warnings, including every pre-existing test in both crates.

- [ ] **Step 5: Commit**
```bash
git add crates/dbc-ui/src/runner.rs crates/dbc-ui/src/backup.rs crates/dbc-core/src/connection.rs
git commit -m "feat: sanctioned backup/restore runner methods + tool-path resolution (G11 T4)"
```

---

### Task 5 (T5): Docker validation against real `pg_dump`/`pg_restore` + `postgres:16.13`

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml` (`[dev-dependencies]`: add `testcontainers-modules = { version = "0.13", features = ["postgres"] }`)
- Create: `crates/dbc-ui/tests/backup_docker.rs`

**Interfaces:** none new — this task is pure validation of T2–T4's real code against a real Postgres server and a real, locally-installed `pg_dump`/`pg_restore`.

**Grounding:**
- `testcontainers-modules = { version = "0.13", features = ["postgres"] }` is the exact version `dbc-driver-postgres/Cargo.toml:18` already pins (its own `tests/integration.rs` is the house pattern this task follows: `use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};`, `Postgres::default().start().await.unwrap()`, `node.get_host_port_ipv4(5432).await.unwrap()`) — adding it as a NEW dev-dependency of `dbc-ui` (not previously present, confirmed against `crates/dbc-ui/Cargo.toml` read above) introduces no version skew since it matches the pin already resolved in this workspace's `Cargo.lock`.
- **Image tag pinned to `postgres:16.13`** (per this plan's own mandate, not merely `Postgres::default()`'s floating default) via `testcontainers_modules::postgres::Postgres::default().with_tag("16.13")` — `Postgres`'s builder exposes `with_tag` (same builder-pattern API `dbc-driver-postgres`'s own test file doesn't need since it's fine with the default tag, but is documented on the `testcontainers-modules` `postgres::Postgres` type for exactly this purpose).
- **"open_spec not connect::open"** (this plan's own mandate, restated from the task brief): unlike `dbc-driver-postgres`'s integration tests (which call `PostgresConnection::connect(&url)` directly — a raw driver call, bypassing the app's own connection machinery entirely), this task's tests build a full `ConnectionConfig` pointed at the container's mapped host/port and drive everything through `QueryRunner`'s real, public methods (`run_mssql_backup`-sibling `run_external_tool`/`open_spec`-based helpers already used by T4) — proving the WHOLE app-level path (tool resolution, arg building with the real `ConnectionConfig`, PGPASSWORD env, spawn, log streaming, redaction) works end-to-end, not just the underlying driver.
- **Local `pg_dump`/`pg_restore` install is a real prerequisite**, same class of external requirement docker itself is — `resolve_tool_path(None, "pg_dump")` (T4) is called at the top of each test and the test is skipped-by-panic with a clear message if it fails, rather than silently no-op'ing (fail-loud philosophy, consistent with this codebase's `#[ignore]`-then-explain convention for docker tests rather than a runtime skip mechanism, which this repo's test suite doesn't otherwise use).
- **SECURITY REQUIRED test** (Global Constraints, item 3): a deliberately WRONG password against a real Postgres container makes `pg_dump` fail via its own auth rejection — the resulting `BackupEvent::Failed` message must not contain the real (wrong-but-still-a-real-string) password anywhere.

```rust
//! Docker + a local pg_dump/pg_restore install required.
//! Run with: cargo test -p dbc-ui -- --ignored

use dbc_state::{ConnectionConfig, Engine};
use dbc_ui::backup; // NOTE: if `backup`/`runner` aren't `pub` at the crate
                     // root, this test file is instead added under
                     // `crates/dbc-ui/src/` as a `#[cfg(test)]` module in
                     // `runner.rs` itself (like every other test in this
                     // crate) rather than `tests/` — resolved in Step 0
                     // below before writing the rest of this file, since
                     // `dbc-ui` is a binary crate (`main.rs`, no `lib.rs`)
                     // and `tests/*.rs` integration tests cannot import
                     // `dbc_ui::` at all in that shape.
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

async fn container_cfg(node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>, database: &str) -> ConnectionConfig {
    ConnectionConfig {
        id: "docker-pg".into(), name: "docker-pg".into(), folder: Vec::new(),
        engine: Engine::Postgres,
        host: "127.0.0.1".into(),
        port: Some(node.get_host_port_ipv4(5432).await.unwrap()),
        database: database.into(), user: "postgres".into(), read_only: false,
        timeout_secs: None, auto_limit: None, ssh: None, favourite: false,
    }
}

#[tokio::test]
#[ignore]
async fn real_pg_dump_backup_then_pg_restore_roundtrip() {
    let node = Postgres::default().with_tag("16.13").start().await.unwrap();
    let cfg = container_cfg(&node, "postgres").await;

    let pg_dump = crate::runner::resolve_tool_path(None, "pg_dump")
        .expect("pg_dump must be installed and resolvable for this test — see PostgreSQL client tools");
    let pg_restore = crate::runner::resolve_tool_path(None, "pg_restore")
        .expect("pg_restore must be installed and resolvable for this test");

    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("roundtrip.backup");

    let opts = backup::PgBackupOptions { format: backup::PgDumpFormat::Custom, compress: 6 };
    let args = backup::build_pg_dump_args(&cfg, &cfg.host, cfg.port.unwrap(), &opts, out_path.to_str().unwrap());

    let runner = crate::runner::QueryRunner::new();
    let (mut rx, _handle) = runner.run_external_tool(pg_dump, args, Some("postgres".to_string()));
    let mut finished = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            backup::BackupEvent::Finished => { finished = true; break; }
            backup::BackupEvent::Failed(m) => panic!("pg_dump failed: {m}"),
            backup::BackupEvent::Log(_) => {}
        }
    }
    assert!(finished, "pg_dump did not report Finished");
    assert!(out_path.is_file() && std::fs::metadata(&out_path).unwrap().len() > 0);

    // Sniff the real dump: must be detected as Custom (PGDMP magic).
    let head = std::fs::read(&out_path).unwrap();
    assert_eq!(backup::detect_dump_format(&head[..head.len().min(64)]), backup::DumpFormat::Custom);

    // Restore into a FRESH throwaway database (never overwrite the
    // container's own `postgres` db mid-test) — created via a raw
    // connection since restore-target creation is out of this plan's scope.
    let create_url = format!("postgres://postgres:postgres@127.0.0.1:{}/postgres", cfg.port.unwrap());
    let mut admin = dbc_driver_postgres::PostgresConnection::connect(&create_url).await.unwrap();
    dbc_core::Connection::execute(&mut admin, "CREATE DATABASE roundtrip_target", dbc_core::CancelToken::new())
        .await
        .unwrap();

    let mut restore_cfg = cfg.clone();
    restore_cfg.database = "roundtrip_target".into();
    let restore_opts = backup::PgRestoreOptions::default();
    let restore_args = backup::build_pg_restore_args(
        &restore_cfg, &restore_cfg.host, restore_cfg.port.unwrap(), &restore_opts, out_path.to_str().unwrap(),
    );
    let (mut rx2, _h2) = runner.run_external_tool(pg_restore, restore_args, Some("postgres".to_string()));
    let mut restored = false;
    while let Some(ev) = rx2.recv().await {
        match ev {
            backup::BackupEvent::Finished => { restored = true; break; }
            backup::BackupEvent::Failed(m) => panic!("pg_restore failed: {m}"),
            backup::BackupEvent::Log(_) => {}
        }
    }
    assert!(restored, "pg_restore did not report Finished");
}

#[tokio::test]
#[ignore]
async fn wrong_password_error_never_contains_the_real_password() {
    let node = Postgres::default().with_tag("16.13").start().await.unwrap();
    let cfg = container_cfg(&node, "postgres").await;
    let pg_dump = crate::runner::resolve_tool_path(None, "pg_dump")
        .expect("pg_dump must be installed and resolvable for this test");

    let out_dir = tempfile::tempdir().unwrap();
    let out_path = out_dir.path().join("should_not_exist.backup");
    let opts = backup::PgBackupOptions { format: backup::PgDumpFormat::Custom, compress: 0 };
    let args = backup::build_pg_dump_args(&cfg, &cfg.host, cfg.port.unwrap(), &opts, out_path.to_str().unwrap());

    let runner = crate::runner::QueryRunner::new();
    const WRONG_PASSWORD: &str = "definitely-the-wrong-password-42";
    let (mut rx, _handle) = runner.run_external_tool(
        crate::runner::resolve_tool_path(None, "pg_dump").unwrap(),
        args,
        Some(WRONG_PASSWORD.to_string()),
    );
    let mut saw_failure_text = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            backup::BackupEvent::Failed(m) => { saw_failure_text = m; break; }
            backup::BackupEvent::Finished => panic!("expected an auth failure with the wrong password, got Finished"),
            backup::BackupEvent::Log(l) => saw_failure_text.push_str(&l),
        }
    }
    assert!(!saw_failure_text.contains(WRONG_PASSWORD), "leaked password in: {saw_failure_text}");
    let _ = pg_dump; // silence unused-in-this-branch warning if the loop exits via Log accumulation only
}
```

- [ ] **Step 0: Resolve the module-visibility ambiguity flagged in the file's own header comment.** `dbc-ui` is currently a BINARY crate (`main.rs`, no `lib.rs` — confirmed: `crates/dbc-ui/src/` has no `lib.rs` in this repo today). `tests/*.rs` integration test files can only import a crate's PUBLIC library API, which a binary-only crate doesn't expose. Before writing the rest of this file: check whether `crates/dbc-ui/src/lib.rs` exists on this branch (another phase may have added one for its own testing needs). If it does NOT exist, do not add one just for this task (out of scope, a much bigger structural change than one docker test file) — instead move this entire test file's content into `crates/dbc-ui/src/runner.rs`'s own `#[cfg(test)]` area as a new `mod docker_tests { ... }`, `#[ignore]`d exactly the same way, using `super::*`/`crate::backup::*` imports instead of `dbc_ui::`. Delete the not-yet-created `crates/dbc-ui/tests/backup_docker.rs` path from this task's Files list if this branch is taken, and record the actual choice made in this task's commit message.

- [ ] **Step 1: Add the dev-dependency.** `crates/dbc-ui/Cargo.toml`, `[dev-dependencies]` — append `testcontainers-modules = { version = "0.13", features = ["postgres"] }`.

- [ ] **Step 2: Write the tests** exactly as in the Grounding code block above (adjusted per Step 0's resolution), in whichever location Step 0 settled on.

- [ ] **Step 3: Run against real docker + a real local Postgres client-tools install**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored`
  Expected: both new tests pass. If `pg_dump`/`pg_restore` aren't installed on the machine running this step, the test panics with the exact "must be installed" message from `resolve_tool_path` — install PostgreSQL client tools (or point `AppConfig.tool_paths` — not exercised by this specific test path, which deliberately calls `resolve_tool_path(None, ...)` to test PATH/glob discovery — a manual local run can instead call `resolve_tool_path(Some(path), ...)` while iterating) and re-run.

- [ ] **Step 4: Run the full non-docker suite once more to confirm nothing else broke**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
  Expected: unchanged pass count from before this task (the two new tests are `#[ignore]`d and don't run here), zero warnings.

- [ ] **Step 5: Commit**
```bash
git add crates/dbc-ui/Cargo.toml Cargo.lock crates/dbc-ui/tests/backup_docker.rs crates/dbc-ui/src/runner.rs
git commit -m "test: docker-validated pg_dump/pg_restore roundtrip + password-redaction proof (G11 T5)"
```

---

### Task 6 (T6): UI wiring — dropdown icon buttons, palette actions, confirm/progress modal

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs`
- Modify: `crates/dbc-ui/src/palette.rs`
- Modify: `crates/dbc-ui/src/main.rs`

**Interfaces:**
- Consumes: T4's `QueryRunner` methods, T2's `confirm_matches`/`display_command_line`/`detect_dump_format`, gpui's `PathPromptOptions`/`cx.prompt_for_paths`/`cx.prompt_for_new_path`.
- Produces:
```rust
// connections_ui.rs — ModalState grows a 5th arm (was 4: ConnectionDialog,
// MasterPasswordPrompt, CreateMasterPassword, QueryParams).
#[derive(Clone)]
pub enum ModalState {
    ConnectionDialog(ConnectionDialogUi),
    MasterPasswordPrompt { /* unchanged */ },
    CreateMasterPassword { /* unchanged */ },
    QueryParams { /* unchanged */ },
    BackupRestore(crate::backup::BackupSession),
}

// backup.rs — UI-facing session state (added here, not connections_ui.rs,
// so backup.rs stays the single home for every backup/restore type,
// mirroring plan.rs's (G13) "one file, pure half then UI-adjacent half"
// convention this plan's Architecture section already commits to).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BackupKind { Backup, Restore }

#[derive(Clone, PartialEq)]
pub enum BackupStatus { Confirming, Running, Succeeded, Failed(String), Cancelled }

#[derive(Clone)]
pub struct BackupSession {
    pub kind: BackupKind,
    pub engine: dbc_state::Engine,
    pub connection_name: String,
    pub database: String,
    pub log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    pub status: std::rc::Rc<std::cell::RefCell<BackupStatus>>,
    pub started_at: std::time::Instant,
    /// Triggers `BackupHandle::cancel()` (Postgres) or a `CancelToken`
    /// cancellation (MSSQL/SQLite) — type-erased so `BackupSession` doesn't
    /// need a variant per engine's cancellation mechanism.
    pub cancel: std::rc::Rc<dyn Fn()>,
    /// `Some` only during `Confirming` for a Restore session — the typed
    /// database-name field; `None` for Backup (no typed-confirm friction)
    /// and cleared once the session moves to `Running`.
    pub confirm_input: Option<Entity<crate::text_model::TextField>>,
    pub expected_name: String,
    pub command_line: String,
}
```

**Grounding:**
- **No context menu — icon buttons on `dropdown_item`.** Two new children added to `dropdown_item` (`connections_ui.rs:1624-1683`), following the EXACT `★`/`✎` pattern already there (own `.id(...)`, `cx.stop_propagation()` so the click doesn't also fire the row's connect handler at `dropdown_item`'s outer `on_click`):
  ```rust
  .child(
      div()
          .id(SharedString::from(format!("dropdown-item-backup-{}", c.id)))
          .px_1().cursor_pointer().text_color(rgb(0xa6adc8)).hover(|s| s.bg(rgb(0x45475a)))
          .child("🗄")
          .on_click(cx.listener(move |view, _, window, cx| {
              cx.stop_propagation();
              view.open_backup_dialog(backup_target.clone(), window, cx);
          })),
  )
  .child(
      div()
          .id(SharedString::from(format!("dropdown-item-restore-{}", c.id)))
          .px_1().cursor_pointer()
          .text_color(if c.read_only { rgb(0x6c7086) } else { rgb(0xa6adc8) })
          .hover(|s| s.bg(rgb(0x45475a)))
          .child("♻")
          .on_click(cx.listener(move |view, _, window, cx| {
              cx.stop_propagation();
              view.open_restore_dialog(restore_target.clone(), window, cx);
          })),
  )
  ```
  `open_restore_dialog` itself is what actually enforces the read-only block (tooltip-equivalent status text, since GPUI tooltips aren't otherwise used anywhere in this codebase — a `hover()`-only color dim plus an inline status message on click is the established affordance-for-disabled-state precedent here, matching `dropdown_item`'s existing icon-button conventions rather than inventing a tooltip component with no other call site in this repo).
- **File dialogs.** Backup (save): `cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name))` — IDENTICAL call shape to `grid.rs::start_export` (`grid.rs:1331`), including its four-armed `dialog.await` match (`Ok(Ok(Some(path)))` / `Ok(Ok(None))` cancelled / `Ok(Err(e))` dialog error / `Err(_canceled)` dialog unavailable) — reused verbatim rather than re-derived, EXCEPT this plan's fallback on dialog-unavailable is "abort with a status note" (design §2: "no Downloads-folder fallback here... a silent fallback path for a DESTRUCTIVE-adjacent, potentially multi-gigabyte backup file is the wrong default"), NOT `start_export`'s Downloads-folder fallback. Restore (open, picking the source file): `cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Obnovit ze zálohy".into()) })` — the `prompt_for_paths` sibling API, verified present at this pinned GPUI rev by G12's plan grounding (`crates/gpui/src/app.rs:1564-1569`, `crates/gpui/src/platform.rs:2139-2148`) — **no file-type filter** (per this plan's Global Constraints; a `.backup`/`.bak`/`.sqlite`-mismatched pick is caught after selection, e.g. via `detect_dump_format`/`sqlite_magic_header_ok` themselves, which double as the validation).
- **The typed-name confirm step lives INSIDE `ModalState::BackupRestore`**, not as a separate modal, so there is only ONE overlay transition per restore (Confirming → Running → terminal), matching every other modal in this file's single-panel-per-state shape (`render_modal_overlay`, `connections_ui.rs:1036-1041`, dispatches ONE panel per `ModalState` variant — adding a match arm here is what T6 does, not adding a second enum).
- **Esc-closability** (`main.rs::on_cancel_query`, lines 1474-1487): add one new arm to the existing `match &modal` — `ModalState::BackupRestore(session) => !matches!(*session.status.borrow(), backup::BackupStatus::Running)` — mirrors the design's "NOT closable while Running" rule using the exact same closability-predicate mechanism every other modal here already uses (no new Esc-handling code path invented).
- **Palette gating** (design §4c: "shown only when a connection is currently active"). `palette::fixed_actions()` (palette.rs:135-143) and `palette::rank_items()` (palette.rs:167-232, which calls `fixed_actions()` internally in BOTH its empty-query and non-empty-query branches) both grow a `connection_active: bool` parameter:
  ```rust
  pub fn fixed_actions(connection_active: bool) -> Vec<(String, PaletteAction)> {
      let mut v = vec![
          ("Spustit dotaz".to_string(), PaletteAction::RunQuery),
          ("Přepnout strom".to_string(), PaletteAction::ToggleTree),
          ("Přepnout historii".to_string(), PaletteAction::ToggleHistory),
          ("Nové spojení…".to_string(), PaletteAction::NewConnection),
          ("Obnovit schéma".to_string(), PaletteAction::RefreshSchema),
      ];
      if connection_active {
          v.push(("Zálohovat databázi…".to_string(), PaletteAction::BackupDatabase));
          v.push(("Obnovit databázi ze zálohy…".to_string(), PaletteAction::RestoreDatabase));
      }
      v
  }

  pub fn rank_items(
      query: &str, tables: &[TableSource], history: &[HistorySource],
      connections: &[ConnectionSource], cap: usize, connection_active: bool,
  ) -> Vec<PaletteItem> { /* both internal `fixed_actions()` call sites become `fixed_actions(connection_active)` */ }
  ```
  `PaletteAction` grows `BackupDatabase, RestoreDatabase`. The ONE external call site, `main.rs:1666` (`palette::rank_items(query, &tables, &history, &connections, 30)`), becomes `palette::rank_items(query, &tables, &history, &connections, 30, self.active_connection_id.is_some())`. **Read-only "grayed but discoverable" for Restore is SCOPED DOWN** from the design's literal ask: this codebase's `PaletteItem`/action-row rendering (main.rs, the palette's row list) has no existing "disabled row" visual precedent anywhere (unlike the dropdown's icon buttons, where per-item conditional text-color is one line) — implementing a true greyed-out-but-clickable row would mean threading `read_only` all the way through `PaletteItem::Action` and its renderer with no reusable pattern to lean on. This plan instead keeps `RestoreDatabase` listed (discoverable) whenever a connection is active, and enforces read-only at CLICK time with the same inline status-message rejection the ad-hoc SQL editor's own read-only gate already uses (`main.rs:1006-1011`, `"connection is read-only"` style), rather than a disabled visual state. Recorded here as a deliberate, grounded scope reduction, not a silent deviation.
- **Dispatch** (`main.rs::execute_palette_item`, the `PaletteItem::Action` arm, lines 1698-1723): two new match arms —
  ```rust
  PaletteAction::BackupDatabase => {
      if let Some(id) = self.active_connection_id.clone() { self.open_backup_dialog(id, window, cx); }
  }
  PaletteAction::RestoreDatabase => {
      if let Some(id) = self.active_connection_id.clone() { self.open_restore_dialog(id, window, cx); }
  }
  ```
- **`open_backup_dialog`/`open_restore_dialog`/`start_backup`/`start_restore` (new `AppView` methods, main.rs)** follow the exact `cx.spawn(async move |this, cx| { ... this.update(cx, |view, cx| {...}) ... })` event-draining shape `run_query_with`'s own tab-streaming closure already establishes (`main.rs:1058-1089` read above) — for Postgres, the drained stream is `run_external_tool`'s `Receiver<BackupEvent>`; for MSSQL/SQLite, T4's `oneshot::Receiver<Result<(), QueryError>>` is awaited once and mapped straight to `BackupStatus::Succeeded`/`Failed`. The full sequence, Backup (any engine):
  1. `open_backup_dialog(connection_id, window, cx)`: resolves `ConnectionConfig` from `self.config`, builds the suggested filename (`{database}-{yyyyMMdd-HHmmss}.{ext}`), opens the SAVE dialog. On a path: builds a `BackupSession { status: Running, ... }` (backup has no typed-confirm step — design §2 vs §3, only Restore gets the typed-name friction), sets `self.modal = Some(ModalState::BackupRestore(session))`, and calls `start_backup`.
  2. `start_backup(session, spec, target_path, cx)`: dispatches per `cfg.engine` — Postgres: resolve tool path (`resolve_tool_path` — a synchronous, on-UI-thread filesystem check; per design this is acceptable since it's a handful of `is_file`/`where` calls, not a network round-trip, same cost class as `Tunnel`'s own already-on-UI-thread-adjacent calls elsewhere in this codebase's connect flow are explicitly NOT — but tool resolution here specifically is small enough that this plan accepts it inline rather than adding yet another spawn_blocking hop; flagged for the adversarial review to confirm this doesn't introduce a perceptible stall) then `runner.run_external_tool(...)`, draining `BackupEvent`s into `session.log`/`session.status`; MSSQL/SQLite: `runner.run_mssql_backup(...)`/`runner.run_sqlite_backup(...)`, awaited once, mapped to a single terminal status (no intermediate log lines — design's own documented spinner-only fallback for both).
  3. On the terminal event (`Finished`/`Ok(())` → `Succeeded`; `Failed(msg)`/`Err(e)` → `Failed`), T7's `record_history_with_kind` is called (see T7).
  Restore's sequence additionally starts in `BackupStatus::Confirming` (typed-name field shown, "Obnovit" disabled until `confirm_matches(typed, &database) == true`) and only calls `start_restore` once the button is clicked — `start_restore` re-checks `backup::guard_backup_restore_read_only(BackupOp::Restore, cfg.read_only)` one more time before dispatching (belt-and-braces, mirroring `run_write_transaction`'s own "UI already refused, runner refuses again" posture), even though T4's runner methods already refuse internally too — this is the THIRD independent check (menu-item-level, dialog-open-level, runner-level), consistent with this codebase's established "each layer holds on its own" philosophy (`guards.rs`'s own doc comment, quoted in this plan's grounding reading).
- **`checkbox`/`field_row`/`styled_button`** (`connections_ui.rs:1590-1622`, private `fn`s) are reused as-is for the confirm panel's typed-name field and Backup/Restore option checkboxes (Postgres format radio, restore option toggles) — no new styling primitives invented.

- [ ] **Step 1: `palette.rs` changes.**
  - Add `BackupDatabase, RestoreDatabase` to `PaletteAction`.
  - Change `fixed_actions` to `fixed_actions(connection_active: bool)` per the Grounding code above; update BOTH internal call sites inside `rank_items`.
  - Change `rank_items`'s signature to add the trailing `connection_active: bool` parameter.
  - Update existing tests in `palette.rs` that call `fixed_actions()`/`rank_items(...)` to pass an explicit bool (add new tests asserting the two new actions are absent when `connection_active == false` and present, at the tail of the fixed-action list, when `true`).

- [ ] **Step 2: `connections_ui.rs` changes.**
  - Add `ModalState::BackupRestore(crate::backup::BackupSession)`.
  - Add the two icon-button children to `dropdown_item` per the Grounding code above (guard `backup_target`/`restore_target` closures capture `c.id.clone()`).
  - Add a `render_backup_restore_panel(session: &backup::BackupSession, cx: &mut Context<AppView>) -> AnyElement` function and wire it into `render_modal_overlay`'s match (new arm: `ModalState::BackupRestore(session) => render_backup_restore_panel(session, cx),`). Panel shows: title (`"Zálohovat databázi"`/`"Obnovit databázi ze zálohy"`), connection/database name, source/target path, the redacted command line (`session.command_line`), and:
    - `Confirming` (Restore only): the typed-name `field_row`, an amber warning line (`"Tímto se přeruší VŠECHNA ostatní připojení k této databázi."` for MSSQL only), "Obnovit" button (`disabled` — rendered dimmed/non-interactive — until `confirm_matches` passes) and "Zrušit".
    - `Running`: a scrolling `uniform_list`-backed log pane over `session.log.borrow()` (same `uniform_list` pattern `history_panel.rs`/`schema_tree.rs` already use for a growing row list) + an elapsed-time line (`session.started_at.elapsed()`) + "Zrušit" (calls `(session.cancel)()`).
    - `Succeeded`/`Failed`/`Cancelled`: the final log + outcome line + "Zavřít" only.

- [ ] **Step 3: `main.rs` changes.**
  - `use gpui::PathPromptOptions;` (or the already-imported `gpui::*` glob, whichever this file currently uses — check the existing `use gpui::{...}` list and extend it rather than adding a second import line).
  - Add `AppView::open_backup_dialog`, `open_restore_dialog`, `start_backup`, `start_restore`, `confirm_restore` (the "Obnovit" button's click handler once `Confirming`'s typed-name check passes) per the sequence in Grounding above.
  - Extend `execute_palette_item`'s `PaletteAction` match with the two new arms.
  - Update the ONE `palette::rank_items(...)` call site to pass `self.active_connection_id.is_some()`.
  - Extend `on_cancel_query`'s modal-closability match with the `ModalState::BackupRestore` arm per Grounding.
  - Add `mod backup;`'s already-landed line (T2) needs its `#[allow(dead_code)]` REMOVED now that this task is the first real consumer of `backup.rs`'s UI-facing types.

- [ ] **Step 4: Write/extend tests.**
  - `palette.rs`: `backup_restore_actions_hidden_without_active_connection`, `backup_restore_actions_present_and_last_when_connection_active` (assert the two new entries appear, in order, after `RefreshSchema`, only when `connection_active == true`).
  - `connections_ui.rs`/`main.rs`: reuse this plan's T2 `confirm_matches` tests as the ONLY unit-level proof of the typed-name gate's logic (the GPUI button-enablement itself is a render-time concern with no existing precedent for a headless render test anywhere in this crate — consistent with how this repo tests GPUI-adjacent code elsewhere: pure logic gets unit tests, rendering is verified manually/by the reviewer).

- [ ] **Step 5: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
  Expected: all tests pass (T1–T5's plus this task's new palette tests), zero warnings. Bump `crates/dbc-ui/Cargo.toml`'s `version` to `0.11.0` in this same commit (Global Constraints: version tracks the phase number at the `main.rs`-touching tail task).

- [ ] **Step 6: Manual verification** (no automated GPUI render test exists in this repo for modal content — same gap every prior G-phase plan's UI-wiring task has manually verified instead): run the app, open a saved Postgres/SQLite connection, click 🗄 on its dropdown row, confirm the save dialog appears with no `.backup`/`.sql` filter dropdown (GPUI limitation, expected), run a real backup against a scratch database, then click ♻ and confirm the typed-name gate blocks "Obnovit" until the exact name is typed.

- [ ] **Step 7: Commit**
```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/palette.rs crates/dbc-ui/src/main.rs crates/dbc-ui/src/backup.rs crates/dbc-ui/Cargo.toml
git commit -m "feat: backup/restore UI — dropdown actions, palette entries, confirm/progress modal (G11 T6)"
```

---

### Task 7 (T7): History integration — `kind` column + badge

**Files:**
- Modify: `crates/dbc-state/src/history.rs`
- Modify: `crates/dbc-ui/src/history_panel.rs`
- Modify: `crates/dbc-ui/src/main.rs` (two small call sites, land alongside T6)

**Interfaces:**
```rust
// dbc-state/src/history.rs
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub sql: String,
    pub connection: String,
    pub started_at: i64,
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub error: Option<String>,
    pub starred: bool,
    /// New, additive. `"query"` for every pre-existing/ordinary run
    /// (`DEFAULT 'query'` at the schema level covers rows written before
    /// this migration); `"backup"`/`"restore"` for G11 runs.
    pub kind: String,
}

impl HistoryDb {
    /// Existing signature UNCHANGED — every current call site keeps
    /// compiling with zero edits. Thin wrapper: `kind = "query"`.
    pub fn add(&mut self, sql: &str, connection: &str, started_at: i64,
        duration_ms: Option<i64>, row_count: Option<i64>, error: Option<&str>) -> Result<i64, StateError>;

    /// New. The ONLY way a non-"query" `kind` ever reaches the DB.
    pub fn add_with_kind(&mut self, sql: &str, connection: &str, started_at: i64,
        duration_ms: Option<i64>, row_count: Option<i64>, error: Option<&str>, kind: &str) -> Result<i64, StateError>;
}
```

**Grounding:**
- `HistoryDb::open` (`history.rs:46-91`) already runs its `CREATE TABLE IF NOT EXISTS entries (...)` + a SEPARATE `CREATE INDEX IF NOT EXISTS idx_entries_star_time` as two independent, already-idempotent migration statements inside `execute_batch`. This plan adds a THIRD, equally idempotent step: SQLite has no `ADD COLUMN IF NOT EXISTS`, so the idempotency instead comes from probing `PRAGMA table_info(entries)` for a column named `kind` first and only running `ALTER TABLE entries ADD COLUMN kind TEXT NOT NULL DEFAULT 'query'` when it's absent — the exact same "probe, then conditionally migrate" shape `open_creates_the_star_time_index_and_reopen_is_idempotent` (history.rs:328-346) already proves works for the index case, generalized to a column.
- `add`'s existing 6-argument signature is used from MULTIPLE call sites across the codebase already (`history_panel.rs::record_history`, and transitively every `run_query_with` completion arm in `main.rs`) — changing it to 7 arguments would force a mechanical edit at every one of those sites for zero behavioral gain outside this plan's own two new call sites. Keeping `add` as a thin `add_with_kind(..., "query")` wrapper (one line) avoids that churn entirely, matching this plan's "additive, not disruptive" posture for a file no other in-flight phase is editing.
- `format_meta_line`/`collapse_sql` (`history_panel.rs:56-82`) are UNCHANGED — the badge is purely a `line1` prefix, decided once at render time from `entry.kind`, not a new "meta line" variant.

```rust
// history.rs — inside HistoryDb::open, appended after the existing
// execute_batch calls, before the FTS5 probe:
let has_kind_column: bool = conn
    .prepare("PRAGMA table_info(entries)")?
    .query_map([], |row| row.get::<_, String>(1))?
    .filter_map(|r| r.ok())
    .any(|name| name == "kind");
if !has_kind_column {
    conn.execute_batch("ALTER TABLE entries ADD COLUMN kind TEXT NOT NULL DEFAULT 'query';")?;
}
```
```rust
// history.rs — HistoryDb::add becomes a thin wrapper; row_to_entry, the
// dedup-lookup query, and every SELECT in `search` grow the `kind` column.
pub fn add(&mut self, sql: &str, connection: &str, started_at: i64,
    duration_ms: Option<i64>, row_count: Option<i64>, error: Option<&str>) -> Result<i64, StateError> {
    self.add_with_kind(sql, connection, started_at, duration_ms, row_count, error, "query")
}

pub fn add_with_kind(&mut self, sql: &str, connection: &str, started_at: i64,
    duration_ms: Option<i64>, row_count: Option<i64>, error: Option<&str>, kind: &str) -> Result<i64, StateError> {
    // Dedup check UNCHANGED in shape (still keys on sql+connection+time
    // window — a backup/restore's synthetic "sql" description is already
    // unique-per-run via its embedded timestamped filename, so this never
    // spuriously collapses two different runs); the UPDATE/INSERT below
    // additionally write `kind`.
    let last: Option<(i64, String, String, i64)> = self.conn.query_row(
        "SELECT id, sql, connection, started_at FROM entries ORDER BY id DESC LIMIT 1", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional()?;
    if let Some((last_id, last_sql, last_conn, last_started_at)) = last {
        if last_sql == sql && last_conn == connection && (started_at - last_started_at).abs() <= DEDUP_WINDOW_SECS {
            self.conn.execute(
                "UPDATE entries SET started_at = ?1, duration_ms = ?2, row_count = ?3, error = ?4, kind = ?5 WHERE id = ?6",
                params![started_at, duration_ms, row_count, error, kind, last_id],
            )?;
            return Ok(last_id);
        }
    }
    self.conn.execute(
        "INSERT INTO entries (sql, connection, started_at, duration_ms, row_count, error, starred, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
        params![sql, connection, started_at, duration_ms, row_count, error, kind],
    )?;
    Ok(self.conn.last_insert_rowid())
}
```
`row_to_entry` and every `SELECT` in `search` grow one more column (`, kind` appended to the column list; `kind: row.get(8)?` appended to the struct literal — index shifts by one since `starred` moves from index 7 to 7 unchanged and `kind` becomes index 8, added at the END of each `SELECT`'s column list to avoid renumbering every other index).

- [ ] **Step 1: Write the failing tests** (`crates/dbc-state/src/history.rs`, extend the existing `#[cfg(test)] mod tests`):
```rust
#[test]
fn add_defaults_to_query_kind() {
    let (_d, mut h) = db();
    h.add("select 1", "demo", 1000, None, None, None).unwrap();
    assert_eq!(h.search("", 10).unwrap()[0].kind, "query");
}

#[test]
fn add_with_kind_records_backup_and_restore() {
    let (_d, mut h) = db();
    h.add_with_kind("-- BACKUP demo -> x.backup", "demo", 1000, Some(5000), None, None, "backup").unwrap();
    h.add_with_kind("-- RESTORE demo <- x.backup", "demo", 2000, Some(3000), None, None, "restore").unwrap();
    let r = h.search("", 10).unwrap();
    assert_eq!(r[0].kind, "restore"); // newest first
    assert_eq!(r[1].kind, "backup");
}

#[test]
fn old_db_without_kind_column_migrates_on_reopen() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("h.sqlite");
    {
        // Simulate a pre-G11 DB: create the OLD schema directly, bypassing
        // HistoryDb::open (which would already add the column).
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE entries (
                id INTEGER PRIMARY KEY, sql TEXT NOT NULL, connection TEXT NOT NULL,
                started_at INTEGER NOT NULL, duration_ms INTEGER, row_count INTEGER,
                error TEXT, starred INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO entries (sql, connection, started_at, starred) VALUES ('select 1', 'demo', 1000, 0);",
        ).unwrap();
    }
    let h = HistoryDb::open(&p).unwrap();
    let r = h.search("", 10).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].kind, "query", "pre-existing rows must default to 'query' via the column's DEFAULT");
}

#[test]
fn kind_column_migration_is_idempotent_on_reopen() {
    let (d, h) = db();
    drop(h);
    // Reopening an ALREADY-migrated (this session's own fresh) DB a second
    // time must not error on a duplicate ALTER TABLE.
    let h2 = HistoryDb::open(&d.path().join("h.sqlite"));
    assert!(h2.is_ok());
}
```

- [ ] **Step 2: Run to see it fail**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state`
  Expected: compile errors (`HistoryEntry.kind` doesn't exist, `add_with_kind` doesn't exist).

- [ ] **Step 3: Implement** the migration probe, `HistoryEntry.kind`, `add`/`add_with_kind`, and the `kind`-column addition to `row_to_entry` and every `SELECT` in `search`, exactly per the Grounding code above.

- [ ] **Step 4: `history_panel.rs` badge.** In the `uniform_list` row-building closure (`history_panel.rs:159-224`), change:
  ```rust
  let line1 = collapse_sql(&entry.sql, SQL_COLLAPSE_MAX_CHARS);
  ```
  to:
  ```rust
  let raw_line1 = collapse_sql(&entry.sql, SQL_COLLAPSE_MAX_CHARS);
  let line1 = if entry.kind == "query" { raw_line1 } else { format!("🗄 {raw_line1}") };
  ```
  Add `AppView::record_history_with_kind` alongside the existing `record_history` (`history_panel.rs:94-109`) — same body, calling `h.add_with_kind(..., kind)` instead of `h.add(...)`:
  ```rust
  pub(crate) fn record_history_with_kind(
      &mut self, sql: &str, connection: &str, started_at: i64,
      duration_ms: Option<i64>, row_count: Option<i64>, error: Option<&str>, kind: &str,
      cx: &mut Context<Self>,
  ) {
      if let Some(h) = self.history.as_mut() {
          if h.add_with_kind(sql, connection, started_at, duration_ms, row_count, error, kind).is_ok() {
              self.refresh_history_cache(cx);
          }
      }
  }
  ```

- [ ] **Step 5: `main.rs` call sites** (land alongside T6, inside `start_backup`/`start_restore`'s terminal-event handling): on `BackupStatus::Succeeded`/`Failed`, call `self.record_history_with_kind(&description, &connection_name, started_at_unix, Some(elapsed_ms), None, error.as_deref(), "backup" /* or "restore" */, cx)` where `description` is the synthetic secret-free line from this plan's Global Constraints (`display_command_line`'s redacted form is NOT reused here — it still names a real file path, which is fine, but the description format is deliberately its OWN short synthetic string per design §4, not the full command line, to keep the History panel's single-line-collapsed rendering readable).

- [ ] **Step 6: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state -p dbc-ui`
  Expected: all pass, zero warnings.

- [ ] **Step 7: Commit**
```bash
git add crates/dbc-state/src/history.rs crates/dbc-ui/src/history_panel.rs crates/dbc-ui/src/main.rs
git commit -m "feat: history kind column + badge for backup/restore runs (G11 T7)"
```

---

## Self-Review

1. **Deviations from the design doc, all grounded above, restated here for the final reviewer:** (a) no context-menu component exists at this pinned GPUI rev in this codebase — icon buttons on `dropdown_item` replace it; (b) MSSQL backup/restore ships as real, reachable code that fails at `open_spec` exactly like every other MSSQL feature today, not gated dead code, and the design's STATS=10 spike is dropped (unreachable, no live MSSQL session to spike against); (c) the SQLite/MSSQL "drop cached connection handles first" step is dropped — no such registry exists anywhere in `runner.rs`, every run already opens-then-drops its own connection; (d) `pick_highest_version_dir` takes pre-supplied `(path, mtime)` pairs rather than doing its own directory read, to stay 100% pure and hermetically unit-testable — the real `read_dir` call moved one level up into T4's `resolve_tool_path`; (e) Restore's palette-row read-only state is enforced at click-time with a status message rather than a true greyed-out row, since no disabled-row rendering precedent exists in `palette.rs` today.
2. **One external-tool method, not three.** `run_external_tool` serves `pg_dump`, `pg_restore`, and `psql` alike — their mechanics are identical, and the design's own T2/T3/T4 split by ENGINE rather than by MECHANISM would have produced two near-duplicate spawn/stream/redact implementations for no behavioral difference; flagged for the adversarial reviewer to confirm this simplification doesn't lose anything the design actually needed per-tool (it doesn't — every per-tool difference is confined to `args`, already handled by T2's separate builders).
3. **MSSQL identifier quoting uses `dbc_core::quote_ident` (double-quote style), not MSSQL's `[bracket]` style** — technically the wrong T-SQL quoting convention, but, per this plan's Spec section, unreachable by any live connection in this codebase today regardless (MSSQL is entirely unwired). Tracked as a follow-up once BOTH `admin_sql::quote_ident_for` (G10) and the MSSQL driver phase land — same open item G7's plan already recorded for its own MSSQL-adjacent code.
4. **`run_sqlite_restore`'s read-only gate lives in the UI caller (T6), not inside the T4 runner method itself** — the one asymmetry among the four T4 write methods, called out explicitly in T4's Grounding so it isn't mistaken for an oversight in review; the alternative (giving `run_sqlite_restore` a `read_only: bool` parameter just to re-implement the same one-line check `guard_backup_restore_read_only` already provides) was considered and rejected as needless indirection for a method that already has no `ConnectSpec`/secret to justify matching the other three methods' shape.
5. **No automated GPUI render test proves the confirm modal's button-enablement wiring** — consistent with the total absence of headless GPUI render tests anywhere else in this codebase; T6's Step 6 manual-verification checklist is the same class of gap every prior UI-wiring task in this repo's plan history has also closed manually rather than automated.
