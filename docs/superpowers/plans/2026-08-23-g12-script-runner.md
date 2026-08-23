# G12 Script Runner Implementation Plan (remainder: T2, T5, T3, T7, T4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run external `.sql` files/folders streamed statement-by-statement against a chosen connection (transaction scope + error policy, live progress tab), unlock multi-statement SQL in the editor (per-statement dispatch, one tab per row-producing statement), and CSV import into a table (column mapping, batched INSERTs in one transaction, read-only respected) — all built on the already-landed T1 statement splitter and T6 CSV pure model.

**Architecture:** Two new runner-owned execution engines in `crates/dbc-ui/src/runner.rs` (`run_script` and `run_csv_import`, streaming `mpsc` events; `connect_and_run_many` for the editor unlock) that reuse `QueryRunner`'s existing `open_spec`/`CancelToken`/channel conventions and the ALREADY-EXISTING shared read-only guard `runner::guard_not_read_only` (runner.rs:256-262) at every write choke point; pure, unit-testable decision helpers (`dispatch_statement`, `failure_action`) encode the design's §2 matrix; UI is a new `ModalState::ScriptRun`/`ModalState::CsvImport` confirm modal pair plus a third `TabContent::ScriptRun` tab kind whose `ScriptRunState` is plain data in `tabs.rs`. History reuses `HistoryEntry`'s fixed field set via synthetic `sql` strings — no schema migration.

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`), tokio (existing), `csv = "1"` (new dep, `dbc-ui` only, T7), `dbc_core::split` (T1, frozen), `dbc-ui/src/csv_import.rs` (T6, frozen).

**Spec:** `docs/superpowers/specs/2026-08-22-gui-target-design.md` (G12 phasing row) and `docs/superpowers/specs/drafts/g12-script-runner-design.md` (binding design — the CURATION block in it is non-negotiable, see Global Constraints). Frozen dependencies on branch `feature/g12-script-runner`: `crates/dbc-core/src/split.rs` (T1 — `Dialect`, `SplitError`, `StatementSplitter::{new,push,finish}`, `UnterminatedKind`, `split_sql`, exported from `lib.rs:19`; 35 tests, reviewed PASS) and `crates/dbc-ui/src/csv_import.rs` (T6 — `CSV_IMPORT_BATCH_SIZE = 500`, `is_numeric_type_name`, `TargetColumn { name, numeric }`, `ColumnMapping { targets: Vec<Option<usize>> }` + `mapped_pairs()`, `CsvRow = Vec<Option<String>>`, `generate_insert_batches(schema, table, columns, mapping, rows) -> Result<Vec<String>, String>` with duplicate-target `Err`; 15 tests). Do NOT re-plan or modify either beyond removing T6's `#![allow(dead_code)]` once T7 wires it in.

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- Errors are values; no panics on DB or user-data paths. An unreadable file, an unterminated SQL construct, a malformed CSV row, or a failed statement surfaces as an event/status/dialog error string — never a crash.
- `dbc-core` never sees GPUI (this phase adds nothing to `dbc-core` except T4's doc-comment edit in `connection.rs`). `dbc-ui` imports no concrete driver crate outside `connect.rs` (all new execution goes through `runner::open_spec` → `connect::open_config`/`connect::open`).
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; the vendored checkout at `C:\Users\tomas\.cargo\git\checkouts\zed-a70e2ad075855582\907ed09\` is API ground truth for anything not directly verifiable by building this repo.
- UI strings are Czech (dialog labels, error messages, status notices, log lines) — English only in code/comments/tests. (Exception, deliberate: the per-statement timeout error text is the design's literal English `"[timeout] statement exceeded {t}s"`, matching `connect_and_run`'s existing `"[timeout] query exceeded {t}s"` precedent at runner.rs:118.)
- Tests green before every commit: `cargo test -p dbc-core -p dbc-state -p dbc-ui` must pass with the task's new tests included. Baselines are in flux across the G6-merge rebase (see Task ordering) — each task must leave every crate at least as green as it found it, plus its own new tests passing.
- **CURATION item 1 (§3-novela, binding):** the app-wide write invariant is the PATTERN, not one function — every write reaches `Connection::execute` only through (a) a confirm modal showing the exact SQL, (b) a runner-owned method with explicit transaction discipline, and (c) the SHARED read-only guard at the runner choke point. The shared guard is the EXISTING `runner::guard_not_read_only(read_only: bool) -> Result<(), QueryError>` (runner.rs:256-262, message `"připojení je jen pro čtení"`) fed by `runner::spec_is_read_only(&ConnectSpec)` (runner.rs:267-272). Every new write method in this plan calls it — `run_script`'s per-statement rejection (T2), `connect_and_run_many`'s per-statement rejection (T5), and `run_csv_import`'s up-front refusal (T7). No fresh read-only logic anywhere. T4 updates `execute()`'s doc comment ONCE to state the pattern + all four sanctioned callers.
- **CURATION item 3 (interception order, binding):** `run_query`/`run_query_with`'s order is fixed: param scan/substitution (G6, on the full editor text — already live at main.rs:722-735) → `split_sql` → per-statement guards/auto-limit → dispatch. T5 adds the mandated test: two statements each carrying `:p`.
- **CURATION item 4 (REQUIRED read-only tests, binding):** (a) T2 — a script containing a write statement over a `read_only` connection: that statement rejected client-side before the driver, error-policy matrix honored; (b) T7 — CSV import entry point hidden/disabled on read-only AND the runtime guard refuses if reached anyway; (c) T5 — editor `SELECT 1; UPDATE …` on read-only runs the SELECT, stops at the UPDATE.
- **CURATION item 2 (out of scope):** `Dialect::Mssql`/`GO` pre-pass and the `Engine::Duckdb → Dialect::Postgres` mapping are NOT in this phase (`dbc_state::Engine` on this branch is `{ Postgres, Mssql, Sqlite }`, config.rs:23 — no Duckdb variant exists to map). `Engine::Mssql` falls back to today's single-statement path and refuses script runs with a status note.
- Version bump to `0.12.0` in `crates/dbc-ui/Cargo.toml` at merge (phasing-table convention: the version tracks the PHASE number — G6 → 0.6.0, G12 → 0.12.0 — not the landing order; `dbc-ui` shows `0.5.0` pre-G6-merge, expect `0.6.0` after the rebase described below).

### Task dependency graph (design §6, remainder)

| Task | Design id | Depends on | Files |
|---|---|---|---|
| Task 1 | T2 `run_script` engine | T1 (frozen) | `runner.rs` |
| Task 2 | T5 editor multi-statement | T1 (frozen); textually after Task 1 (shares `runner.rs`) | `runner.rs`, `main.rs` |
| Task 3 | T3 script runner UI | Task 1 (event shape); textually after Task 2 (shares `main.rs`) | `main.rs`, `tabs.rs`, `connections_ui.rs`, `palette.rs` |
| Task 4 | T7 CSV import UI | T6 (frozen), Task 3 (shared tab kind), Task 1 (runner conventions) | `main.rs`, `connections_ui.rs`, `runner.rs`, `grid.rs`, `schema_tree.rs`, `Cargo.toml` |
| Task 5 | T4 integration sweep | Tasks 1–4 | `dbc-core/src/connection.rs`, `dbc-ui/Cargo.toml` |

**Ordering — everything below serializes AFTER a rebase.** G6 is merged on `feature/g6-editor-pro`; T1/T6 are merged on `feature/g12-script-runner`. Before Task 1 starts: merge G6 to `main`, then rebase `feature/g12-script-runner` onto `main` so the branch carries BOTH the frozen T1/T6 modules AND G6's `main.rs` (`run_query` params interception at main.rs:722-735, `build_param_sql` at main.rs:83, `ModalState::QueryParams` at connections_ui.rs:916-923). All line references in this plan to `main.rs`/`connections_ui.rs` are against the post-G6-merge state (verified on `feature/g6-editor-pro` while writing this plan) and may drift a few lines after rebase — re-locate by symbol, not line number.

**Parallelization: none in practice.** T2 and T5 are logically independent execution paths sharing only the splitter, but both edit `runner.rs` — serialize them (same author) rather than split the file. T3 and T7 both edit `main.rs` and `connections_ui.rs` — sequential. Net: Task 1 → Task 2 → Task 3 → Task 4 → Task 5, one chain.

---

### Task 1 (design T2): `run_script` execution engine — `dbc-ui/src/runner.rs`

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs` (new types + `QueryRunner::run_script` + `drive_script` + pure helpers + tests)

**Interfaces:**
- Consumes: `dbc_core::{split_sql is NOT used here — StatementSplitter, Dialect, SplitError, UnterminatedKind}` (T1, frozen), `dbc_core::is_read_statement` (guards.rs:295), `runner::{guard_not_read_only, spec_is_read_only, open_spec, ConnectSpec}` (existing), `dbc_core::{CancelToken, Connection, QueryError, CHANNEL_CAPACITY}`.
- Produces (consumed by Task 3, and — event-shape-wise — mirrored by Task 4):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxScope { None, PerFile, WholeRun }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPolicy { Stop, Continue }

pub struct ScriptRunOptions {
    pub tx_scope: TxScope,
    pub error_policy: ErrorPolicy,
    pub dialect: dbc_core::Dialect,
    /// From the connection's existing `cfg.timeout_secs` — bounds EACH
    /// statement individually (design §2: a whole-run timeout would be
    /// hostile), via a per-statement child CancelToken + tokio timeout.
    pub statement_timeout_secs: Option<u64>,
}

/// Streaming progress events, same mpsc/CHANNEL_CAPACITY convention as
/// `QueryEvent`. NOTE (deviation from design §2, flagged in Self-Review):
/// `StatementStarted` carries no `stmt_total_in_file` — the runner streams
/// statements as the splitter completes them and cannot know a file's total
/// mid-file; the UI already has exact per-file totals from its own pre-scan
/// (Task 3) and renders totals from there.
pub enum ScriptEvent {
    FileStarted { path: std::path::PathBuf, index: usize, total_files: usize },
    StatementStarted { stmt_index: usize, sql_preview: String },
    StatementFinished { stmt_index: usize, affected: Option<u64>, elapsed: Duration },
    StatementFailed { stmt_index: usize, error: QueryError },
    FileFinished { path: std::path::PathBuf, statements_run: usize, statements_failed: usize, elapsed: Duration },
    RunFinished { files_run: usize, statements_run: usize, statements_failed: usize, elapsed: Duration, aborted: bool },
}

impl QueryRunner {
    /// One dedicated connection for the WHOLE run (satisfies
    /// `Connection::execute`'s transaction-per-connection invariant across
    /// every tx scope), dropped when the future completes. Read-only
    /// connections are NOT refused up front — a read-only script over a
    /// read-only connection is legitimate; write statements are rejected
    /// per-statement via the shared guard (CURATION item 1(c)).
    pub fn run_script(
        &self,
        spec: ConnectSpec,
        files: Vec<std::path::PathBuf>,
        opts: ScriptRunOptions,
        cancel: CancelToken,
    ) -> tokio::sync::mpsc::Receiver<ScriptEvent>;
}

// Pure decision helpers (both unit-tested directly):
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StmtDispatch { RunAsRead, RunAsWrite, RejectReadOnly }
pub fn dispatch_statement(sql: &str, read_only: bool) -> StmtDispatch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction { AbortRun, NextStatement, NextFile }
pub fn failure_action(policy: ErrorPolicy, scope: TxScope) -> FailureAction;

/// Single-line-collapsed, char-safe 200-cap preview for log lines (same
/// collapse idiom as `tabs::collapse_title`, different cap — deliberate
/// small copy, same precedent as `history_panel::collapse_sql`).
pub const SQL_PREVIEW_CAP: usize = 200;
pub fn sql_preview(sql: &str) -> String;
```

**Grounding:**
- **Sibling, not reuse, of `run_write_transaction`** (runner.rs:228-241 / `drive_write_sequence` 314-344 / `drive_write_sequence_bounded` 383-412): that path is oneshot, always-stop, always-one-transaction. `run_script` streams per-statement events with configurable scope/policy — design §2's explicit "sibling" decision. If BEGIN/ROLLBACK plumbing turns out mechanically identical, unifying is a refactor at implementation time, not a constraint.
- **Connection conventions:** `open_spec(spec, handle)` (runner.rs:480-497) is the ONLY way to open; `guard_not_read_only`/`spec_is_read_only` (runner.rs:256-272) are the shared guard pair; `CHANNEL_CAPACITY` mpsc shape per `connect_and_run` (runner.rs:73-128); drain-and-count for read statements mirrors `fetch_lookup_inner`'s drain loop (runner.rs:442-474) minus the buffer — count rows off each `RecordBatch::num_rows()`, never materialize.
- **Per-statement timeout + cancellation:** `CancelToken` IS `tokio_util::sync::CancellationToken` (dbc-core/src/cancel.rs:3) — it has `child_token()`. Each statement gets `run_cancel.child_token()`: a per-statement `tokio::time::timeout` expiry cancels the CHILD only (protocol-level cancel of the in-flight statement, per `drive_write_sequence`'s cancel-threading doc runner.rs:300-306) and reports an ordinary `StatementFailed` subject to the matrix; a UI Esc cancels the RUN token, which propagates to the child automatically. Run-token checked before every statement dispatch and between files (two-tier discipline per `connect_and_run`'s doc, runner.rs:58-66); cancellation with an open tx takes the same ROLLBACK path as a statement error and ends with `RunFinished { aborted: true }`.
- **Engine tx divergence:** `Connection::execute`'s doc (connection.rs:28-42) — stop-and-rollback on first error inside a tx, tolerate ROLLBACK failing (`let _ =`, same as drive_write_sequence). BEGIN/COMMIT/ROLLBACK go through `execute()` directly as transaction control (not user writes — they don't pass `dispatch_statement`); T4's doc novela sanctions this explicitly.
- **`query()` inside an open tx:** connection.rs:38-42's session-sharing caveat targets UNRELATED interleaving; a script's own read statements run sequentially, fully drained, on the same session inside its own tx — intended and safe. T4's doc rewrite clarifies the wording.

**Execution model (the code Step 3 implements):**

```rust
/// Read-chunk size for streaming .sql files into the splitter (design §2).
const SCRIPT_READ_CHUNK: usize = 64 * 1024;

pub fn dispatch_statement(sql: &str, read_only: bool) -> StmtDispatch {
    if dbc_core::is_read_statement(sql) {
        StmtDispatch::RunAsRead
    } else if read_only {
        StmtDispatch::RejectReadOnly
    } else {
        StmtDispatch::RunAsWrite
    }
}

pub fn failure_action(policy: ErrorPolicy, scope: TxScope) -> FailureAction {
    match (policy, scope) {
        (ErrorPolicy::Stop, _) => FailureAction::AbortRun,
        (ErrorPolicy::Continue, TxScope::None) => FailureAction::NextStatement,
        (ErrorPolicy::Continue, TxScope::PerFile) => FailureAction::NextFile,
        // UI forbids this combination (design §2 matrix); if it arrives
        // anyway, fail safe: abort — never continue inside one open tx.
        (ErrorPolicy::Continue, TxScope::WholeRun) => FailureAction::AbortRun,
    }
}

/// One statement: dispatch per the read-only matrix, per-statement child
/// cancel + timeout. `Ok(affected)` — `Some(rows)` for a drained read,
/// `Some(n)` for a write's affected count.
async fn run_script_statement(
    conn: &mut dyn Connection,
    sql: &str,
    read_only: bool,
    timeout_secs: Option<u64>,
    run_cancel: &CancelToken,
) -> Result<Option<u64>, QueryError> {
    let action = dispatch_statement(sql, read_only);
    if action == StmtDispatch::RejectReadOnly {
        // CURATION item 1(c): the SHARED guard produces the rejection —
        // no fresh read-only logic here.
        return Err(guard_not_read_only(true).unwrap_err());
    }
    let stmt_cancel = run_cancel.child_token();
    let fut = async {
        match action {
            StmtDispatch::RunAsRead => {
                let mut stream = conn.query(sql, stmt_cancel.clone()).await?;
                let mut rows: u64 = 0;
                while let Some(item) = stream.batches.recv().await {
                    rows += item?.num_rows() as u64;
                }
                Ok(Some(rows))
            }
            StmtDispatch::RunAsWrite => conn.execute(sql, stmt_cancel.clone()).await.map(Some),
            StmtDispatch::RejectReadOnly => unreachable!("handled above"),
        }
    };
    match timeout_secs {
        Some(t) => match tokio::time::timeout(Duration::from_secs(t), fut).await {
            Ok(r) => r,
            Err(_elapsed) => {
                stmt_cancel.cancel(); // protocol-level cancel of the in-flight statement only
                Err(QueryError::msg(format!("[timeout] statement exceeded {t}s")))
            }
        },
        None => fut.await,
    }
}
```

`drive_script(conn: &mut dyn Connection, read_only: bool, files: &[PathBuf], opts: &ScriptRunOptions, cancel: CancelToken, tx: &mpsc::Sender<ScriptEvent>)` — testable over a temp-file sqlite connection exactly like `write_transaction_tests` (runner.rs:551-816). Skeleton the implementer fills in (all branches specified here, none left open):

1. `run_started = Instant::now()`; run-level counters; `aborted = false`.
2. `TxScope::WholeRun` → `conn.execute("BEGIN", cancel.child_token())`; a BEGIN failure aborts the run immediately (`RunFinished { aborted: true }` after a best-effort nothing — no tx opened).
3. Per file `(index, path)`: send `FileStarted`; `TxScope::PerFile` → `BEGIN`. Open `tokio::fs::File::open(path)`; loop `read(&mut [0u8; SCRIPT_READ_CHUNK])` feeding `StatementSplitter::push`; every completed statement → step 4. After EOF, `splitter.finish()`: `Ok(Some(text))` → step 4 for the final un-terminated-`;` statement; `Ok(None)` → nothing; `Err(e)` → synthesize a statement failure (`QueryError::msg(format!("[skript] neúplný SQL konstrukt na konci souboru: {e:?}"))`) and treat per step 5's FILE-LEVEL rule. A file-open/read IO error or `SplitError::InvalidUtf8` likewise synthesizes `QueryError::msg(format!("[soubor] {path}: {err}"))` — file-level.
4. Per statement: run-cancel check first (`cancel.is_cancelled()` → rollback any open tx, `aborted = true`, break out to `RunFinished`); send `StatementStarted { stmt_index, sql_preview: sql_preview(&stmt) }`; `run_script_statement(...)`; `Ok(affected)` → `StatementFinished { affected, elapsed }`, bump counters; `Err(e)` → `StatementFailed`, bump failed counter, then step 5.
5. On statement failure: `failure_action(opts.error_policy, opts.tx_scope)`:
   - `AbortRun` → ROLLBACK whichever tx is open (`let _ = conn.execute("ROLLBACK", ...)`), send `FileFinished` for the current file, `aborted = true`, stop everything.
   - `NextStatement` → keep consuming this file's statements (scope is `None`; nothing to roll back).
   - `NextFile` → ROLLBACK the per-file tx, stop reading this file (remaining statements skipped), send `FileFinished`, advance to the next file.
   - FILE-LEVEL errors (open/read/split/utf8) can't meaningfully "continue to the next statement" of a broken file: they map `Stop → AbortRun`, `Continue → NextFile` regardless of scope (deviation, flagged in Self-Review).
6. Clean end of file: `TxScope::PerFile` → `COMMIT` (a COMMIT failure is a file-level failure: ROLLBACK best-effort + step 5); send `FileFinished { statements_run, statements_failed, elapsed }`.
7. After all files: `TxScope::WholeRun` → `COMMIT` (failure → ROLLBACK + `aborted = true`). Send `RunFinished { files_run, statements_run, statements_failed, elapsed, aborted }`.

`QueryRunner::run_script` itself is the thin spawn wrapper (mirror `connect_and_run`'s shape runner.rs:73-128): channel of `CHANNEL_CAPACITY`, pre-connect cancel check, `open_spec`, compute `read_only = spec_is_read_only(&spec)` BEFORE `spec` moves, then `drive_script`, connection drops at the end (ultimate rollback backstop, same note as run_write_transaction_inner runner.rs:428-431).

- [ ] **Step 1: Write the failing tests** (`#[cfg(test)] mod script_run_tests` in runner.rs; reuse `write_transaction_tests`' helpers `open_sqlite_test_conn`/`read_one` by extracting them into a `#[cfg(test)] mod test_support` both modules use, or duplicate the ~25 lines — implementer's choice, zero-warning either way). Pure tests first:

```rust
#[test]
fn dispatch_statement_matrix() {
    assert_eq!(dispatch_statement("SELECT 1", false), StmtDispatch::RunAsRead);
    assert_eq!(dispatch_statement("SELECT 1", true), StmtDispatch::RunAsRead);
    assert_eq!(dispatch_statement("UPDATE t SET x = 1", false), StmtDispatch::RunAsWrite);
    assert_eq!(dispatch_statement("UPDATE t SET x = 1", true), StmtDispatch::RejectReadOnly);
    // fail-closed inputs are writes, not reads (guards.rs contract):
    assert_eq!(dispatch_statement("SELECT 1 /* unterminated", true), StmtDispatch::RejectReadOnly);
}

#[test]
fn failure_action_full_matrix() {
    use ErrorPolicy::*;
    use TxScope::*;
    assert_eq!(failure_action(Stop, None), FailureAction::AbortRun);
    assert_eq!(failure_action(Stop, PerFile), FailureAction::AbortRun);
    assert_eq!(failure_action(Stop, WholeRun), FailureAction::AbortRun);
    assert_eq!(failure_action(Continue, None), FailureAction::NextStatement);
    assert_eq!(failure_action(Continue, PerFile), FailureAction::NextFile);
    // UI forbids the combination; runner fails safe if it arrives anyway:
    assert_eq!(failure_action(Continue, WholeRun), FailureAction::AbortRun);
}

#[test]
fn sql_preview_collapses_and_caps() {
    assert_eq!(sql_preview("SELECT\n  1"), "SELECT 1");
    let long = "x".repeat(300);
    let p = sql_preview(&long);
    assert_eq!(p.chars().count(), SQL_PREVIEW_CAP + 1);
    assert!(p.ends_with('…'));
}
```

Integration tests over a sqlite temp connection + `tempfile::tempdir()` `.sql` files (write files with `std::fs::write`; a helper `collect_events(rx)`/direct `drive_script` call + `Vec<ScriptEvent>` drain via a locally-constructed `mpsc::channel` and `tokio::join!`):

```rust
/// CURATION item 4(a): write statement over a read_only connection is
/// rejected CLIENT-SIDE (before the driver — proven by the table staying
/// unchanged even though the underlying test connection is writable), with
/// the SHARED guard's exact message, and Continue policy keeps running the
/// script's read statements.
#[tokio::test]
async fn script_write_statement_rejected_on_read_only_policy_matrix_honored() {
    let (_f, mut conn) = open_sqlite_test_conn().await;
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let f1 = dir.path().join("01.sql");
    std::fs::write(&f1, "SELECT * FROM t;\nUPDATE t SET n = 'hacked' WHERE id = 1;\nSELECT 1;").unwrap();

    let opts = ScriptRunOptions {
        tx_scope: TxScope::None,
        error_policy: ErrorPolicy::Continue,
        dialect: dbc_core::Dialect::Sqlite,
        statement_timeout_secs: None,
    };
    let events = drive_collect(&mut *conn, /* read_only */ true, &[f1], &opts).await;

    let guard_msg = guard_not_read_only(true).unwrap_err().message;
    assert!(events.iter().any(|e| matches!(e,
        ScriptEvent::StatementFailed { stmt_index: 1, error } if error.message == guard_msg)));
    // Continue: both SELECTs still ran.
    let finished: Vec<_> = events.iter().filter(|e| matches!(e, ScriptEvent::StatementFinished { .. })).collect();
    assert_eq!(finished.len(), 2);
    assert!(matches!(events.last(), Some(ScriptEvent::RunFinished { statements_run: 2, statements_failed: 1, aborted: false, .. })));
    // Client-side proof: the write never reached the (writable) driver.
    assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 1").await, Some("a".to_string()));
}

#[tokio::test]
async fn per_file_scope_stop_policy_rolls_back_file_and_aborts_run() {
    // file 1: INSERT ok; then invalid SQL -> file 1 ROLLED BACK (its INSERT
    // gone), run aborted, file 2 never started (no FileStarted for it).
}

#[tokio::test]
async fn per_file_scope_continue_policy_skips_failed_file_commits_next() {
    // file 1 fails mid-file -> rolled back; file 2 runs fully and commits.
    // Assert FileFinished for both, RunFinished { aborted: false }.
}

#[tokio::test]
async fn whole_run_scope_rolls_back_everything_on_late_failure() {
    // file 1 commits nothing on its own; failure in file 2 -> NOTHING from
    // file 1 is visible afterwards.
}

#[tokio::test]
async fn no_tx_continue_skips_only_failing_statement() {
    // stmt 1 INSERT ok (autocommitted), stmt 2 invalid, stmt 3 INSERT ok ->
    // rows 1 and 3 present, statements_failed == 1.
}

#[tokio::test]
async fn final_statement_without_trailing_semicolon_runs() {
    // "INSERT ...;\nSELECT * FROM t" -> 2 statements run.
}

#[tokio::test]
async fn unterminated_construct_surfaces_as_statement_failure() {
    // file ends inside a string -> StatementFailed with "[skript]" text;
    // Stop policy -> aborted run.
}

#[tokio::test]
async fn precancelled_token_aborts_before_any_statement() {
    // cancel.cancel() first -> RunFinished { aborted: true, statements_run: 0 }.
}

#[tokio::test]
async fn read_statements_report_drained_row_counts() {
    // "SELECT * FROM t;" over 3 rows -> StatementFinished { affected: Some(3) }.
}
```

(`drive_collect` is a ~15-line test helper: builds an `mpsc::channel(CHANNEL_CAPACITY)`, runs `drive_script` and a receiver-drain concurrently via `tokio::join!`, returns the `Vec<ScriptEvent>`.)

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui script_run_tests::`
Expected: compile error (types don't exist).

- [ ] **Step 3: Implement** `TxScope`/`ErrorPolicy`/`ScriptRunOptions`/`ScriptEvent`, `dispatch_statement`, `failure_action`, `sql_preview`, `run_script_statement`, `drive_script` (per the numbered execution model above), and `QueryRunner::run_script`.

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass (pre-existing + new), zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/runner.rs
git commit -m "feat: run_script engine with tx-scope/error-policy matrix"
```

---

### Task 2 (design T5): editor multi-statement unlock — `runner.rs` + `main.rs`

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs` (`MultiQueryEvent`, `connect_and_run_many` + `connect_and_run_many_inner`, tests)
- Modify: `crates/dbc-ui/src/main.rs` (`run_query_with` split interception, `dialect_for_engine` + `auto_limit_each` pure helpers + tests, multi-run event loop + `open_adhoc_result_tab` helper)

**Interfaces:**
- Consumes: `dbc_core::{split_sql, Dialect, SplitError}` (T1), `dispatch_statement`/`guard_not_read_only`/`spec_is_read_only` (Task 1 + existing), `dbc_core::apply_auto_limit` (guards.rs:323), `tabs::collapse_title`, `record_history` (history_panel.rs:94-109).
- Produces:

```rust
pub enum MultiQueryEvent {
    /// `columns: Some` = a row-producing statement (Batches follow);
    /// `None` = a non-row statement (write) — no tab opens for it.
    StatementStarted { index: usize, total: usize, columns: Option<SchemaRef> },
    Batch(RecordBatch),
    /// `affected: Some(n)` for a write; `None` for a read (its rows went to
    /// the tab, not a count).
    StatementFinished { index: usize, affected: Option<u64>, elapsed: Duration },
    StatementFailed { index: usize, error: QueryError },
    RunFinished,
}

impl QueryRunner {
    /// One connection, N statements, STOP on first error (design §4 —
    /// error-policy choice is a script-runner-only concept). Per-statement
    /// read-only rejection via the SHARED guard = CURATION item 1(c).
    /// Per-statement timeout via child tokens, same shape as run_script.
    pub fn connect_and_run_many(
        &self,
        spec: ConnectSpec,
        statements: Vec<String>,
        cancel: CancelToken,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::mpsc::Receiver<MultiQueryEvent>;
}
```

**Grounding:**
- **Interception point:** `run_query_with` (main.rs:870-1366). G6's params interception is UPSTREAM in `run_query` (main.rs:722-735) — so CURATION item 3's order (params → split → per-statement guards → dispatch) falls out automatically by splitting inside `run_query_with`, which receives already-substituted SQL. The split happens AFTER the modal/single-flight/empty guards (main.rs:882-890) and after spec resolution (main.rs:892-919, which yields `conn_meta: Option<(bool, Engine)>`), and BEFORE guard 1 (read-only, main.rs:926) and guard 2 (auto-limit, main.rs:934-944).
- **Per-statement guards vs. the whole-blob gate (resolved design conflict, flagged in Self-Review):** design §4 says both "guards apply per statement" AND "is_read_statement still called on the ORIGINAL full text for the read-only gate" — but a whole-text pre-gate would reject `SELECT 1; UPDATE …` on read-only outright, making CURATION item 4(c) ("runs the SELECT, stops at the UPDATE") impossible. CURATION wins: when the split yields >1 statement, the read-only gate is PER-STATEMENT inside `connect_and_run_many` (shared guard); the single-statement path (split yields 0–1) keeps guard 1 exactly as today.
- **Auto-limit per statement:** `apply_auto_limit` fires only when the whole string starts with SELECT (guards.rs:330) — today a multi-statement blob never got limited. Post-split, apply it per statement.
- **Tab-per-row-producing-statement:** mirror the `Started` arm's AD-HOC subset (main.rs:1014-1133: `ResultBuffer::new(columns)`, `fk_info_for_adhoc`, grid entity + `cx.subscribe(&grid, AppView::on_grid_event)` main.rs:1110, `tabs.open(ResultTab { title: collapse_title(stmt), preview_key: None, conn_identity, content: TabContent::Grid { .. } })` main.rs:1123-1131). Extract that subset as a NEW helper `open_adhoc_result_tab(&mut self, columns: SchemaRef, title_sql: &str, conn_identity: &str, cx) -> (u64, Rc<RefCell<ResultBuffer>>)` used by the multi path ONLY — do NOT refactor the existing single-run `Started` arm to use it (leave working code untouched; the duplication is deliberate and documented, same precedent as `collapse_sql`).
- **Single-flight/generation:** the multi loop sets `self.cancel`, bumps `run_generation`, and generation-guards the tail cleanup exactly like the existing loop (main.rs:946-953, 1348-1363).
- **History:** ONE entry per run, `sql` = the original full (post-params) editor text — same recording shape as today (main.rs:1238-1257 for the fields), `row_count` = returned rows + affected sum (design silent; flagged in Self-Review).
- **Read-only spec test fixture:** `ConnectSpec::Config` with a sqlite `ConnectionConfig { read_only: true, database: <temp file>, .. }` — `connect::open_config` opens it `SQLITE_OPEN_READ_ONLY`, and the guard fires before the driver anyway. Test via an extracted `connect_and_run_many_inner(spec, statements, cancel, timeout, handle, tx)` driven under `#[tokio::test]` with `Handle::current()` (same pattern as `run_write_transaction_refuses_read_only_connection_without_connecting`, runner.rs:702-734).

**`main.rs` interception (real code):**

```rust
/// Engine → splitter dialect. `Mssql` (and any future engine without a
/// dialect) returns None -> today's single-statement path, unchanged
/// (CURATION item 2: the GO pre-pass is an explicit non-goal; when DuckDB
/// wiring lands, map Duckdb → Dialect::Postgres + one test — not now).
fn dialect_for_engine(engine: dbc_state::Engine) -> Option<dbc_core::Dialect> {
    match engine {
        dbc_state::Engine::Postgres => Some(dbc_core::Dialect::Postgres),
        dbc_state::Engine::Sqlite => Some(dbc_core::Dialect::Sqlite),
        dbc_state::Engine::Mssql => None,
    }
}

/// Per-statement auto-limit (design §4): only bare SELECTs in the split
/// list get a LIMIT appended. Returns the rewritten list + whether any
/// statement changed (drives the " · auto-LIMIT {n}" status suffix).
fn auto_limit_each(statements: Vec<String>, limit: Option<u64>, bypass: bool) -> (Vec<String>, bool) {
    let Some(n) = limit.filter(|_| !bypass) else { return (statements, false) };
    let mut changed_any = false;
    let out = statements
        .into_iter()
        .map(|s| {
            let (rewritten, changed) = dbc_core::apply_auto_limit(&s, n);
            changed_any |= changed;
            rewritten
        })
        .collect();
    (out, changed_any)
}
```

In `run_query_with`, immediately after the `(read_only, auto_limit, timeout_secs, conn_meta, spec)` destructure (main.rs:919) and BEFORE guard 1:

```rust
// G12 T5: multi-statement unlock. Params were already substituted upstream
// (run_query, G6) — CURATION-fixed order: params → split → per-statement
// guards/auto-limit → dispatch.
if preview.is_none() {
    if let Some(dialect) = conn_meta.map(|(_, e)| e).and_then(dialect_for_engine) {
        match dbc_core::split_sql(&sql, dialect) {
            Err(e) => {
                self.status = format!("error: SQL nelze rozdělit na příkazy: {e:?}");
                cx.notify();
                return;
            }
            Ok(stmts) if stmts.len() > 1 => {
                let (stmts, limited) = auto_limit_each(stmts, auto_limit, bypass_auto_limit);
                self.run_many(spec, sql, stmts, limited, timeout_secs, cx);
                return;
            }
            // 0 or 1 statements — fall through to the existing
            // single-statement pipeline below, byte-for-byte unchanged
            // (guard 1 read-only on the full text, guard 2 auto-limit).
            Ok(_) => {}
        }
    }
}
```

`run_many` (new method) mirrors the existing spawn loop: single-flight guards already ran; sets `cancel`/`run_generation`/`started_at`/status; captures `history_conn_name`/`history_started_at`/`conn_identity` identically (main.rs:961-976); then consumes `connect_and_run_many`:
- `StatementStarted { columns: Some(cols), .. }` → `open_adhoc_result_tab(cols, &statements[index], ...)` becomes the CURRENT (tab, buffer); status `"příkaz {index+1}/{total}…"`.
- `StatementStarted { columns: None, .. }` → no tab; status only.
- `Batch(b)` → push into current buffer (on push error: latch, cancel, status — simplified version of the existing spill handling; the terminal history entry carries the latched error).
- `StatementFinished { affected: Some(n), .. }` → accumulate `total_affected += n`; `None` → accumulate `rows_returned` from the current buffer's `row_count()`.
- `StatementFailed { index, error }` → status `"selhalo na příkazu #{index+1}: {error}"`; record ONE failed history entry (sql = full text); clear cancel (generation-guarded); stop.
- `RunFinished` → status `"{total} příkazů, {with_rows} s výsledky, {writes} zápisů ({total_affected} řádků) — hotovo{limit_suffix}"`; record ONE success history entry; clear cancel (generation-guarded).

- [ ] **Step 1: Write the failing tests.**

Pure (main.rs, `#[cfg(test)] mod multi_statement_tests` — alongside the file's existing per-feature test modules):

```rust
#[cfg(test)]
mod multi_statement_tests {
    use super::*;

    #[test]
    fn dialect_for_engine_maps_pg_sqlite_and_refuses_mssql() {
        assert_eq!(dialect_for_engine(dbc_state::Engine::Postgres), Some(dbc_core::Dialect::Postgres));
        assert_eq!(dialect_for_engine(dbc_state::Engine::Sqlite), Some(dbc_core::Dialect::Sqlite));
        assert_eq!(dialect_for_engine(dbc_state::Engine::Mssql), None);
    }

    #[test]
    fn auto_limit_each_limits_only_bare_selects() {
        let stmts = vec![
            "SELECT * FROM a".to_string(),
            "UPDATE t SET x = 1".to_string(),
            "SELECT * FROM b LIMIT 5".to_string(),
        ];
        let (out, changed) = auto_limit_each(stmts, Some(100), false);
        assert!(changed);
        assert_eq!(out[0], "SELECT * FROM a LIMIT 100");
        assert_eq!(out[1], "UPDATE t SET x = 1");
        assert_eq!(out[2], "SELECT * FROM b LIMIT 5");
    }

    #[test]
    fn auto_limit_each_bypass_and_none_are_noops() {
        let stmts = vec!["SELECT 1".to_string()];
        assert_eq!(auto_limit_each(stmts.clone(), Some(100), true), (stmts.clone(), false));
        assert_eq!(auto_limit_each(stmts.clone(), None, false), (stmts, false));
    }

    /// CURATION item 3's mandated test: two statements each carrying `:p` —
    /// params resolve BEFORE splitting, so a substituted literal containing
    /// `;` inside quotes is handled by the splitter's normal string rules.
    #[test]
    fn params_resolve_before_split_two_statements() {
        let names = vec!["p".to_string()];
        let out = build_param_sql(
            "SELECT :p; UPDATE t SET x = :p;",
            &names,
            &[("a;b".to_string(), false)],
        )
        .unwrap();
        assert_eq!(out, "SELECT 'a;b'; UPDATE t SET x = 'a;b';");
        let stmts = dbc_core::split_sql(&out, dbc_core::Dialect::Sqlite).unwrap();
        assert_eq!(stmts, vec![
            "SELECT 'a;b'".to_string(),
            "UPDATE t SET x = 'a;b'".to_string(),
        ]);
    }
}
```

Runner integration (runner.rs, `#[cfg(test)] mod run_many_tests`):

```rust
/// CURATION item 4(c): `SELECT 1; UPDATE …` on a READ-ONLY connection runs
/// the SELECT (Started with columns + Finished), then stops at the UPDATE
/// with the SHARED guard's message; nothing after it runs.
#[tokio::test]
async fn read_only_multi_run_runs_select_then_stops_at_update() {
    // sqlite temp file with table t prepared via a WRITABLE open first,
    // then a ConnectSpec::Config { read_only: true, .. } pointing at it.
    // statements: ["SELECT 1", "UPDATE t SET n = 'x'", "SELECT 2"].
    // Assert events: StatementStarted{index:0, columns: Some}, >=0 Batch,
    // StatementFinished{index:0}, StatementFailed{index:1, error.message ==
    // guard_not_read_only(true).unwrap_err().message} — and NO event for
    // index 2, no RunFinished. Reopen writable: table unchanged.
}

#[tokio::test]
async fn multi_run_mixed_reads_and_writes_over_writable_connection() {
    // ["CREATE TABLE ...", "INSERT ...", "SELECT * FROM t"] -> Started{None}
    // + Finished{Some(0)}, Started{None} + Finished{Some(1)},
    // Started{Some} + Batch + Finished{None}, RunFinished.
}

#[tokio::test]
async fn multi_run_stops_on_first_error() {
    // stmt 2 invalid -> StatementFailed{index:1}, stmt 3 never dispatched
    // (no third StatementStarted).
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui multi_statement_tests:: run_many_tests::`
Expected: compile errors (helpers/method don't exist).

- [ ] **Step 3: Implement** `MultiQueryEvent`, `connect_and_run_many` (+ `_inner` for testability), `dialect_for_engine`, `auto_limit_each`, the `run_query_with` interception block, `open_adhoc_result_tab`, and `run_many`'s event loop.

- [ ] **Step 4: Run to green + zero warnings + a sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Manually launch against the SQLite fixture: `SELECT 1; SELECT 2;` opens TWO tabs; `CREATE TABLE x(a); INSERT INTO x VALUES (1); SELECT * FROM x;` opens ONE tab (the SELECT) and the status line reports the writes; a single-statement query behaves byte-for-byte as before; a read-only connection running `SELECT 1; UPDATE …` shows the SELECT tab then the read-only error. Also the design-§7 regression pass: re-run any starred/history entries containing multi-statement text and confirm they now execute per-statement instead of failing at the driver.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/runner.rs crates/dbc-ui/src/main.rs
git commit -m "feat: editor multi-statement unlock (connect_and_run_many)"
```

---

### Task 3 (design T3): script runner UI — pickers, pre-scan, confirm modal, progress tab

**Files:**
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::{RunSqlFile, RunSqlFolder}` + `fixed_actions` rows)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::ScriptRun` variant)
- Modify: `crates/dbc-ui/src/tabs.rs` (`TabContent::ScriptRun` + `ScriptRunState` plain-data types)
- Modify: `crates/dbc-ui/src/main.rs` (pickers, pre-scan, confirm/cancel handlers, run loop, renders, history, pure helpers + tests)

**Interfaces:**
- Consumes: `runner::{run_script, ScriptRunOptions, ScriptEvent, TxScope, ErrorPolicy, sql_preview}` (Task 1), `dbc_core::{StatementSplitter, Dialect}` (T1), `dialect_for_engine` (Task 2), `record_history` (history_panel.rs:94).
- Produces (consumed by Task 4 — the shared tab kind):

```rust
// tabs.rs — plain data, GPUI-free like the rest of the file:
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptRunOutcome { Running, Done, Failed, Cancelled }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptFileStatus { Pending, Running, Done, Failed, Skipped }

pub struct ScriptFileRow {
    pub name: String,
    pub status: ScriptFileStatus,
    pub statements_run: usize,
    pub statements_failed: usize,
}

/// Cap on retained log lines (fixed constant, same posture as TAB_CAP).
pub const SCRIPT_LOG_CAP: usize = 1000;

pub struct ScriptRunState {
    pub files: Vec<ScriptFileRow>,
    /// From the UI pre-scan (scripts) or the row pre-count (CSV, Task 4).
    pub total_statements: usize,
    pub statements_run: usize,
    pub statements_failed: usize,
    pub total_affected: u64,
    /// CSV import only (Task 4): (rows done, rows total) — drives an honest
    /// progress bar. None for script runs.
    pub progress_rows: Option<(u64, u64)>,
    pub log: std::collections::VecDeque<String>,
    pub outcome: ScriptRunOutcome,
    pub started_at: std::time::Instant,
    pub elapsed: Option<std::time::Duration>,
}
impl ScriptRunState {
    pub fn push_log(&mut self, line: String); // pops front past SCRIPT_LOG_CAP
}

// TabContent grows its third variant (design §3):
pub enum TabContent {
    Grid { .. },
    Text { .. },
    ScriptRun { state: Rc<RefCell<ScriptRunState>> },
}
```

```rust
// connections_ui.rs — ModalState grows (all field types are Clone;
// TxScope/ErrorPolicy derive Copy in Task 1):
ScriptRun {
    /// (path, pre-scanned statement count) per file, run order.
    files: Vec<(std::path::PathBuf, usize)>,
    tx_scope: crate::runner::TxScope,
    error_policy: crate::runner::ErrorPolicy,
    /// "{filename}" or "{foldername}/ ({n} souborů)" — drives the modal
    /// heading AND the progress tab title.
    source_label: String,
},
```

**Grounding — the GPUI picker spike (design §7's NEEDS-VERIFICATION, RESOLVED here by reading the pinned checkout; the implementer builds against these verified facts, no spike step needed):**
1. `App::prompt_for_paths(options: PathPromptOptions) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>>` exists at the pinned rev — `crates/gpui/src/app.rs:1564-1569` in the vendored checkout. Callable as `cx.prompt_for_paths(...)` from a `Context<AppView>` (deref to `App`), exactly like `cx.prompt_for_new_path` in `grid.rs::start_export` (grid.rs:1331).
2. `PathPromptOptions { files: bool, directories: bool, multiple: bool, prompt: Option<SharedString> }` — `crates/gpui/src/platform.rs:2139-2148`. The design's assumed `directories: true` folder mode EXISTS.
3. Windows implementation verified: `crates/gpui_windows/src/platform.rs:604-617` + `file_open_dialog` at 1279-1319 — `directories: true` sets `FOS_PICKFOLDERS`; user cancel yields `Ok(Ok(None))`; files and folders are mutually exclusive on Windows (`can_select_mixed_files_and_dirs` returns `false`, platform.rs:637-640) — irrelevant here since the two entry points pick one or the other.
4. **No extension-filter API exists** (`PathPromptOptions` has no filter field; the Windows impl never calls `SetFileTypes`) — the design's "filtered to `*.sql`" is IMPOSSIBLE at this rev. Deviation (flagged in Self-Review): validate client-side after selection — a picked file not ending in `.sql` (case-insensitive) aborts with status `"error: vyberte soubor .sql"`.
5. Await shape is the same triple-layer match `start_export` already handles (grid.rs:1338-1377): `Ok(Ok(Some(paths)))` / `Ok(Ok(None))` cancelled / `Ok(Err(e))` platform error / `Err(_)` dropped channel. Per design §3: NO Downloads-style fallback for an OPEN dialog — every non-`Some` arm just sets a status note (`"výběr zrušen"` / `format!("error: dialog selhal: {e}")` / `"error: dialog není dostupný"`) and returns.

**Grounding — other:**
- Palette: `PaletteAction` (palette.rs:97-105, `Copy` derive — keep new variants field-less) + `fixed_actions` (palette.rs:135-143, Czech labels); dispatch arms next to `PaletteAction::RunQuery` (main.rs:1619). Toolbar affordance: two small buttons `„SQL soubor…“`/`„SQL složka…“` in the editor toolbar row next to the existing run affordance (render-by-contract, same precedent as G6 T3's dialog render).
- Modal render: same `.occlude()` overlay shape as the other four variants; Esc-closable BEFORE the run starts (the modal CLOSES on „Spustit“, so the design's "not Esc-closable once running" is structurally moot — flagged in Self-Review); the `on_cancel_query` modal-close match (main.rs:1396-1400) gains `ModalState::ScriptRun { .. } => true`.
- Single-flight: `confirm_script_run` refuses when `self.cancel.is_some()` (one run at a time, same guard as run_query_with main.rs:885-887); it then sets `self.cancel = Some(token)` + bumps `run_generation` so Esc (`on_cancel_query` → `cancel.cancel()`) cancels the script run, and the tail clears generation-guarded (main.rs:1359-1361 idiom).
- Tab: opened via `self.tabs.open(ResultTab { title, pinned: false, preview_key: None, conn_identity, content: TabContent::ScriptRun { state } })` — subject to TAB_CAP eviction/pinning like every tab (tabs.rs:102-122). Title `"Skript: {source_label}"`.
- Render arm: `render_tab_content` (main.rs:2723+) gains a `TabContent::ScriptRun` arm — summary bar (files done/total, `{statements_run}/{total_statements}` příkazů, elapsed, „Zrušit“ button while `outcome == Running` wired to `on_cancel_query`'s token, else „Hotovo“/„Selhalo“/„Zrušeno“), per-file rows with glyphs `▶ ✓ ✗ ⊘`, and the log tail as monospace lines (reuse the `TabContent::Text` arm's wrapped-monospace idiom, main.rs:2740+; auto-scroll = render the tail that fits).
- History: ONE entry per run on `RunFinished`, via `record_history(&script_history_sql(...), &history_conn_name, started_at, Some(elapsed_ms), Some(total_affected as i64), error_opt, cx)` — `error_opt = Some("běh přerušen")` when aborted. Fields per `HistoryEntry` (history.rs:22-31); `sql` is the synthetic description, NEVER file contents.

**Pure helpers (main.rs, with tests):**

```rust
/// Streams `path` through its own StatementSplitter in SCRIPT_READ_CHUNK
/// chunks (std::fs — runs inside cx.background_spawn, never the UI thread)
/// solely to count statements. An IO/split error yields Err(text) — shown
/// in the status line, the run is not offered.
fn count_statements_in_file(path: &std::path::Path, dialect: dbc_core::Dialect) -> Result<usize, String>;

/// Non-recursive `*.sql` listing (case-insensitive extension), ordered by
/// file_name() string comparison — NOT full path (design §3).
fn list_sql_files(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, String>;

/// The §2 matrix's UI rule: whole-run scope is only selectable under Stop.
fn script_options_valid(scope: runner::TxScope, policy: runner::ErrorPolicy) -> bool {
    !(scope == runner::TxScope::WholeRun && policy == runner::ErrorPolicy::Continue)
}

/// History `sql` synthesis (design §3): single file
/// "[skript] {path} — {m} příkazů, {ok} OK, {fail} chyb"; multi
/// "[skript] {path} — {n} souborů, {m} příkazů, {ok} OK, {fail} chyb".
fn script_history_sql(files: &[(std::path::PathBuf, usize)], statements_run: usize, statements_failed: usize) -> String;
```

**Flow (`start_script_pick(folder: bool)` → modal → run):**
1. Guards: `self.modal.is_some() || self.cancel.is_some()` → return (same gating `on_open_palette` main.rs:1475 applies). Resolve dialect: active connection's engine via the same `conn_meta` lookup `run_query_with` does (or `engine_from_url` main.rs:165 for the CLI path); `dialect_for_engine(..) == None` → status `"error: skripty nejsou podporovány pro tento engine"` and return.
2. `let dialog = cx.prompt_for_paths(PathPromptOptions { files: !folder, directories: folder, multiple: false, prompt: Some("Spustit".into()) });` then `cx.spawn` awaits it (grid.rs:1332's idiom). Non-`Some` arms per the spike note above.
3. In `cx.background_spawn`: folder → `list_sql_files` (empty list → status `"složka neobsahuje žádné .sql soubory"`); file → `.sql` extension check. Then `count_statements_in_file` per file (this is the accepted second sequential read, design §3; the count label says „odhad“).
4. Back on the UI thread: open `ModalState::ScriptRun { files, tx_scope: TxScope::PerFile, error_policy: ErrorPolicy::Stop, source_label }`. Render contract: heading `"Spustit skript: {source_label}"`; file list rows `"{name} — {count} příkazů"` + total; target connection name + `„jen pro čtení“` badge when read-only; radio „Transakce“: `„žádná transakce“ / „transakce na soubor“ (default) / „jedna transakce na celý běh“`; radio „Při chybě“: `„zastavit“ (default) / „pokračovat“`; clicking a combination that violates `script_options_valid` is a no-op with the offending option rendered dimmed; timeout display `"timeout na příkaz: {t}s"` / `"bez timeoutu"` (read from cfg, not editable); buttons „Spustit“ / „Zrušit“.
5. `confirm_script_run`: build `ScriptRunOptions { tx_scope, error_policy, dialect, statement_timeout_secs: cfg.timeout_secs }`; create `ScriptRunState` (files → `Pending` rows, `total_statements` = sum of pre-scan counts, `outcome: Running`); open the `TabContent::ScriptRun` tab; close the modal; set `self.cancel`/`run_generation`; `self.runner.run_script(spec, paths, opts, cancel)`; `cx.spawn` loop mapping events onto the state (`FileStarted` → row `Running`; `StatementStarted` → nothing rendered-critical (its preview seeds the upcoming log line); `StatementFinished` → `push_log(format!("✓ #{i} {preview} ({affected} řádků, {ms} ms)"))` + counters + `total_affected`; `StatementFailed` → `push_log(format!("✗ #{i} {preview} — chyba: {e}"))`; `FileFinished` → row `Done`/`Failed` (+ later rows `Skipped` on abort); `RunFinished` → outcome `Done`/`Failed`/`Cancelled` (cancelled when aborted && token cancelled), `elapsed`, history entry, generation-guarded `self.cancel = None`), `cx.notify()` per event.

- [ ] **Step 1: Write the failing tests** — `tabs.rs` (`push_log` cap + a `ScriptRun`-variant eviction sanity test alongside the existing `text_tab` tests) and `main.rs` `#[cfg(test)] mod script_ui_tests`:

```rust
// tabs.rs tests:
#[test]
fn script_log_caps_at_limit() {
    let mut s = ScriptRunState { /* fresh, log: VecDeque::new(), .. */ };
    for i in 0..(SCRIPT_LOG_CAP + 10) {
        s.push_log(format!("line {i}"));
    }
    assert_eq!(s.log.len(), SCRIPT_LOG_CAP);
    assert_eq!(s.log.front().map(String::as_str), Some("line 10"));
}

// main.rs script_ui_tests:
#[test]
fn list_sql_files_filters_and_orders_by_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.sql"), "select 1;").unwrap();
    std::fs::write(dir.path().join("A.SQL"), "select 1;").unwrap();
    std::fs::write(dir.path().join("c.txt"), "nope").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("d.sql"), "select 1;").unwrap(); // non-recursive: ignored
    let files = list_sql_files(dir.path()).unwrap();
    let names: Vec<_> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(names, vec!["A.SQL".to_string(), "b.sql".to_string()]);
}

#[test]
fn count_statements_streams_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.sql");
    std::fs::write(&p, "SELECT 1;\n-- c ; c\nSELECT ';';\nSELECT 3").unwrap();
    assert_eq!(count_statements_in_file(&p, dbc_core::Dialect::Sqlite), Ok(3));
}

#[test]
fn count_statements_surfaces_unterminated_as_err() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bad.sql");
    std::fs::write(&p, "SELECT 'oops").unwrap();
    assert!(count_statements_in_file(&p, dbc_core::Dialect::Sqlite).is_err());
}

#[test]
fn whole_run_plus_continue_is_invalid() {
    use crate::runner::{ErrorPolicy::*, TxScope::*};
    assert!(script_options_valid(WholeRun, Stop));
    assert!(!script_options_valid(WholeRun, Continue));
    assert!(script_options_valid(PerFile, Continue));
}

#[test]
fn script_history_sql_single_and_multi_file_wording() {
    let one = vec![(std::path::PathBuf::from("C:/s/a.sql"), 5)];
    assert_eq!(script_history_sql(&one, 5, 0), "[skript] C:/s/a.sql — 5 příkazů, 5 OK, 0 chyb");
    let two = vec![(std::path::PathBuf::from("C:/s"), 5), (std::path::PathBuf::from("C:/s/b.sql"), 2)];
    let s = script_history_sql(&two, 6, 1);
    assert!(s.starts_with("[skript] ") && s.contains("2 souborů") && s.contains("7 příkazů") && s.contains("6 OK") && s.contains("1 chyb"));
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui script_ui_tests::`
Expected: compile errors.

- [ ] **Step 3: Implement** the `tabs.rs` types + `TabContent::ScriptRun`, `ModalState::ScriptRun` (+ the `on_cancel_query` Esc-close arm), palette actions + toolbar buttons, `start_script_pick`/`confirm_script_run`/`cancel_script_run` (= `close_modal`), the pure helpers, the event loop, both renders (modal + tab arm). Czech strings per the flow contract above.

- [ ] **Step 4: Run to green + zero warnings + a sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Manually: palette → „Spustit SQL soubor…“ against a multi-statement `.sql` over the SQLite fixture — confirm pre-scan counts in the modal, live log in the tab, Esc cancels mid-run (tab shows „Zrušeno“), history shows the `[skript]` entry; folder run over a 2-file directory in name order; per-file scope + `„pokračovat“` skips a broken file and commits the next.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/palette.rs crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/main.rs
git commit -m "feat: script runner UI (pickers, confirm modal, progress tab)"
```

---

### Task 4 (design T7): CSV import UI + batched-execute runner method

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml` (add `csv = "1"`)
- Modify: `crates/dbc-ui/src/runner.rs` (`CsvImportEvent`, `CsvImportJob`, `run_csv_import` + inner, tests)
- Modify: `crates/dbc-ui/src/csv_import.rs` (remove `#![allow(dead_code)]` — now wired)
- Modify: `crates/dbc-ui/src/schema_tree.rs` (`TreeEvent::ImportCsv` + per-table-row affordance + `set_read_only`)
- Modify: `crates/dbc-ui/src/grid.rs` (preview-toolbar „Import CSV“ button + `csv_import_enabled` flag + `GridEvent` variant)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::CsvImport`)
- Modify: `crates/dbc-ui/src/main.rs` (entry-point handlers, peek/pre-count, mapping modal handlers + render, run loop reusing the `ScriptRun` tab kind, pure helpers + tests)

**Interfaces:**
- Consumes: `csv_import::{CSV_IMPORT_BATCH_SIZE, is_numeric_type_name, TargetColumn, ColumnMapping, CsvRow, generate_insert_batches}` (T6, frozen), `guard_not_read_only`/`spec_is_read_only`/`open_spec` (existing), `TabContent::ScriptRun`/`ScriptRunState` (Task 3), `SchemaSnapshot`/`TableInfo.columns` (same source `detect_editable_pk` reads), `record_history`.
- Produces (runner.rs):

```rust
pub enum CsvImportEvent {
    BatchStarted { batch_index: usize, rows_in_batch: usize },
    /// "committed" in the design's naming; actually "executed inside the
    /// still-open transaction" — nothing is durable until Finished.
    BatchFinished { batch_index: usize, rows_committed_so_far: u64 },
    Failed { error: QueryError },
    Finished { rows_imported: u64, elapsed: Duration },
}

pub struct CsvImportJob {
    pub path: std::path::PathBuf,
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<crate::csv_import::TargetColumn>,
    pub mapping: crate::csv_import::ColumnMapping,
}

impl QueryRunner {
    /// ONE transaction for the whole import (design §5 — not configurable):
    /// BEGIN, one execute() per generated 500-row batch, COMMIT; ANY
    /// failure → ROLLBACK, zero rows imported. FIRST action: the SHARED
    /// guard `guard_not_read_only(spec_is_read_only(&spec))` — CURATION
    /// items 1(c) + 4(b)'s runtime refusal, before any file/DB touch.
    /// Cancellation checked BETWEEN batches only (bounded by the 500-row
    /// cap); `timeout_secs` bounds each batch statement via the same
    /// child-token shape as run_script.
    pub fn run_csv_import(
        &self,
        spec: ConnectSpec,
        job: CsvImportJob,
        cancel: CancelToken,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::mpsc::Receiver<CsvImportEvent>;
}
```

**Grounding:**
- **Streaming parse:** the `csv` crate's `Reader` is already incremental over a `BufReader` (design §5 — no bespoke state machine). Inside `run_csv_import`: a `tokio::task::spawn_blocking` producer reads `csv::Reader::from_path(&job.path)` (`has_headers(true)` — v1 requires a header row), maps each `StringRecord` to a `CsvRow` via `csv_field_to_value` per field, and sends `Vec<CsvRow>` chunks of `CSV_IMPORT_BATCH_SIZE` over a small `tokio::sync::mpsc::channel(4)`; the async driver consumes chunks, calls `generate_insert_batches(schema, table, columns, mapping, &chunk)` (each ≤500-row chunk yields exactly one statement), executes it, emits events. A producer-side parse error is forwarded as a chunk-level `Err(String)` → `Failed` + ROLLBACK.
- **NULL handling — deviation from design §5, flagged in Self-Review:** the `csv` crate's `StringRecord`/`ByteRecord` UNESCAPE fields and retain no was-quoted metadata, so `a,,c` and `a,"",c` are indistinguishable post-parse — the design's quoted-empty-vs-unquoted-empty rule is unimplementable without hand-writing an RFC-4180 scanner (which §5 explicitly decided against). v1 rule: `fn csv_field_to_value(field: &str) -> Option<String>` maps ANY empty field → `None` (SQL NULL), non-empty → `Some`. Documented in the mapping modal's helper text (`„prázdné pole → NULL; hlavičkový řádek je povinný“`). Verify-first step below in case a newer csv 1.x added such an API.
- **Entry points (both design §5 surfaces, both gated on read-only — CURATION item 4(b) entry-gate half):** (1) schema tree: table rows gain a small `⇪` affordance (same per-row-button precedent as G3's ★ toggle) emitting `TreeEvent::ImportCsv { schema: Option<String>, table: String }` (`TreeEvent` at schema_tree.rs:78-87); `SchemaTree` gains `set_read_only(bool)` and renders NO `⇪` when read-only — `main.rs` calls it wherever the tree's snapshot/favourites are pushed (connection switch + snapshot refresh). (2) preview tab: `grid.rs` toolbar (next to „Export ▾“, grid.rs:1589) renders „Import CSV“ only when a new `csv_import_enabled: bool` is set; `main.rs`'s `Started` arm sets it via `grid.update` for preview tabs when the connection is NOT read-only (it has `conn_meta`/`preview` in scope, main.rs:1030-1104); the click emits a new `GridEvent` variant `ImportCsvRequested` routed through the existing `cx.subscribe(&grid, AppView::on_grid_event)` (main.rs:1110). BOTH handlers in `main.rs` re-check read-only and refuse with status `"error: připojení je jen pro čtení"` (belt and braces above the runner's own guard).
- **Peek + pre-count:** on entry, `cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Import".into()) })` (same verified API as Task 3; `.csv` extension validated client-side — no dialog filter exists, same deviation). Then ONE `cx.background_spawn` pass: `csv::Reader::from_path` → `headers()` (Vec<String>), then stream `records()` counting rows AND retaining only the FIRST `CSV_IMPORT_BATCH_SIZE` rows (as `CsvRow`s) for the sample SQL — never the whole file in memory. Target columns from the schema snapshot's `TableInfo.columns`: `TargetColumn { name: c.name.clone(), numeric: is_numeric_type_name(&c.data_type) }`.
- **Modal:** `ModalState::CsvImport { path, schema, table, headers: Vec<String>, columns: Vec<TargetColumn>, targets: Vec<Option<usize>>, row_count: usize, first_rows: Vec<CsvRow>, sample_sql: Option<String>, error: Option<String> }`. Render contract: file path, target table, per-header mapping row (header label + a cycle-button through `(přeskočit)` → each target column — same lightweight idiom as grid's Export ▾ menu rather than a real dropdown), exact row count, `"dávka: 500 řádků"`, and the REAL first batch's `INSERT` verbatim in a scrollable monospace box (recomputed by `recompute_csv_sample` on every mapping change: `generate_insert_batches(schema, table, columns, &mapping, &first_rows)` → first statement; its `Err` (duplicate target) fills `error` and disables „Spustit import“). Buttons „Spustit import“ / „Zrušit“; `on_cancel_query`'s close-match gains `CsvImport { .. } => true`.
- **Run:** `confirm_csv_import` closes the modal, opens a `TabContent::ScriptRun` tab titled `"CSV import: {filename}"` with `progress_rows: Some((0, row_count as u64))` and `total_statements` = batch count; sets `self.cancel`; spawns the `run_csv_import` loop: `BatchStarted` → log `"▶ dávka #{i} ({rows} řádků)"`; `BatchFinished` → progress + log `"✓ dávka #{i} — celkem {so_far} řádků"`; `Failed` → outcome `Failed`, log `"✗ chyba: {e} — import zrušen, žádná data nezapsána"`, history entry (`row_count: Some(0)`, `error: Some(..)`); `Finished` → outcome `Done`, history `sql = format!("[CSV import] {path} → {table} ({n} řádků, dávka {CSV_IMPORT_BATCH_SIZE})")`, `row_count: Some(n)`.

**Pure helpers (main.rs):**

```rust
/// Auto-map by case-insensitive name equality; unmatched headers start as
/// skip (None).
fn default_csv_mapping(headers: &[String], columns: &[crate::csv_import::TargetColumn]) -> crate::csv_import::ColumnMapping;

fn csv_field_to_value(field: &str) -> Option<String> {
    if field.is_empty() { None } else { Some(field.to_string()) }
}
```

- [ ] **Step 1: VERIFY the csv-crate quoting claim** (10 minutes, before writing code): add the `csv = "1"` dep, then check the resolved crate's docs/source (`~/.cargo/registry/src/**/csv-1.*/src/string_record.rs`) for any per-field "was quoted" API. Expected finding: none exists → proceed with the empty→NULL rule and keep the Self-Review deviation. If one DOES exist, implement the design's original quoted-empty-vs-unquoted distinction instead and drop the deviation note.

- [ ] **Step 2: Write the failing tests.**

main.rs `#[cfg(test)] mod csv_ui_tests`:

```rust
#[test]
fn default_csv_mapping_matches_names_case_insensitively() {
    let headers = vec!["ID".to_string(), "Name".to_string(), "extra".to_string()];
    let cols = vec![
        crate::csv_import::TargetColumn { name: "id".into(), numeric: true },
        crate::csv_import::TargetColumn { name: "name".into(), numeric: false },
    ];
    let m = default_csv_mapping(&headers, &cols);
    assert_eq!(m.targets, vec![Some(0), Some(1), None]);
}

#[test]
fn csv_field_to_value_empty_is_null() {
    assert_eq!(csv_field_to_value(""), None);
    assert_eq!(csv_field_to_value("0"), Some("0".to_string()));
    assert_eq!(csv_field_to_value(" "), Some(" ".to_string()));
}
```

runner.rs `#[cfg(test)] mod csv_import_tests`:

```rust
/// CURATION item 4(b), runtime half: a read-only spec is refused by the
/// SHARED guard before any file or DB is touched (nonsense path proves it,
/// same pattern as run_write_transaction_refuses_read_only...).
#[tokio::test]
async fn run_csv_import_refuses_read_only_spec_without_touching_anything() {
    // ConnectSpec::Config { read_only: true, database: "\0invalid", .. },
    // job.path = "Z:/does/not/exist.csv" -> Failed with
    // guard_not_read_only(true)'s message, and nothing panicked.
}

#[tokio::test]
async fn csv_import_commits_all_rows_in_one_transaction() {
    // sqlite temp conn: CREATE TABLE t(id INTEGER, name TEXT, note TEXT);
    // temp CSV: header "id,name,note", rows incl. an empty note (-> NULL),
    // a quoted comma value ("a,b"), an '' -escape case. Drive the inner fn,
    // assert Finished { rows_imported == N }, rows present, NULL where the
    // field was empty, numeric id stored bare.
}

#[tokio::test]
async fn csv_import_rolls_back_everything_on_batch_failure() {
    // Table with a NOT NULL column; CSV whose LAST row violates it ->
    // Failed event, and ZERO rows present afterwards (never partial).
}

#[tokio::test]
async fn csv_import_batches_by_500() {
    // 1100-row CSV -> events show 3 BatchStarted (500/500/100) and
    // rows_committed_so_far == 1100 at the end.
}
```

- [ ] **Step 3: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui csv_ui_tests:: csv_import_tests::`
Expected: compile errors.

- [ ] **Step 4: Implement** the runner method (guard → open_spec → BEGIN → spawn_blocking producer + chunk channel → per-batch generate/execute/events → COMMIT/ROLLBACK), the two entry points (+ read-only gates on both AND in both handlers), the peek/pre-count pass, `ModalState::CsvImport` + handlers (`cycle_csv_target`, `recompute_csv_sample`, `confirm_csv_import`, cancel) + render, the run loop on the shared tab kind, and remove `csv_import.rs`'s `#![allow(dead_code)]`.

- [ ] **Step 5: Run to green + zero warnings + a sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Manually: import a small CSV into a SQLite fixture table via the tree `⇪` AND via a preview tab's „Import CSV“; confirm the sample INSERT shown is the real first batch; confirm a duplicate mapping is refused in-modal; flip the connection to read-only and confirm BOTH entry points disappear; confirm a mid-file constraint violation leaves zero rows.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/Cargo.toml crates/dbc-ui/src/runner.rs crates/dbc-ui/src/csv_import.rs crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/grid.rs crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/main.rs
git commit -m "feat: CSV import UI with mapping modal and batched runner"
```

---

### Task 5 (design T4): final integration sweep

**Files:**
- Modify: `crates/dbc-core/src/connection.rs` (`execute()` doc comment — the CURATION item 1 novela)
- Modify: `crates/dbc-ui/Cargo.toml` (version `0.12.0`)

**Grounding:** `execute()`'s doc (connection.rs:19-43) still reads "ONLY the sandbox Apply flow may call it" — stale for the third and fourth time (design §7). Replace ONLY the first paragraph (lines 19-20); keep the transaction-per-connection and engine-divergence paragraphs verbatim; amend the session-sharing caveat's last sentence.

- [ ] **Step 1: Rewrite the doc comment.** New first paragraph + amended caveat (exact text):

```rust
    /// Executes a non-returning statement, reporting affected rows. This is
    /// the app's write path, governed by a PATTERN, not a single caller:
    /// every write reaches `execute` only through (a) a confirm modal
    /// showing the exact SQL that will run, (b) a runner-owned method with
    /// explicit transaction discipline, and (c) the shared read-only guard
    /// at the runner choke point (`dbc-ui`'s `runner::guard_not_read_only`).
    /// Sanctioned runner callers as of G12: `run_write_transaction` (sandbox
    /// Apply), `run_script` (script-runner write statements plus its
    /// BEGIN/COMMIT/ROLLBACK transaction control), `run_csv_import` (batched
    /// CSV INSERTs plus transaction control), and `connect_and_run_many`
    /// (editor multi-statement — its per-statement read-only rejection is
    /// guard (c)). No other code may call this method.
```

and, appended to the session-sharing caveat paragraph (after "…dropped immediately after."):

```rust
    /// The script runner's own read statements are the sanctioned exception:
    /// they run sequentially, fully drained, over this same dedicated
    /// connection inside the script's own transaction — the caveat forbids
    /// UNRELATED interleaving, not a script's own ordered statements.
```

- [ ] **Step 2: Version bump.** `crates/dbc-ui/Cargo.toml`: `version = "0.12.0"` (phase-numbered convention, G12 → 0.12.0; the field reads `0.6.0` post-G6-merge — this is not a skip, versions track phase ids).

- [ ] **Step 3: Entry-point sweep.** Confirm all three features are reachable and gated: palette rows „Spustit SQL soubor…“/„Spustit SQL složku…“ + editor-toolbar buttons (Task 3); CSV `⇪` tree affordance + preview-toolbar button, both absent under read-only (Task 4); multi-statement Ctrl+Enter needs no new entry point (Task 2). Confirm `on_cancel_query`'s modal-close match covers `ScriptRun` and `CsvImport`.

- [ ] **Step 4: Full sweep to green, zero warnings**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-core -p dbc-state -p dbc-ui`
Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui`
Expected: everything passes, zero warnings in every crate touched this phase.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-core/src/connection.rs crates/dbc-ui/Cargo.toml
git commit -m "feat: G12 integration sweep - execute() novela, v0.12.0"
```

---

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §2 Execution model: sibling decision, tx-scope/error-policy matrix, per-statement dispatch via `is_read_statement`, streaming 64 KiB chunked reads feeding per-file splitters, `ScriptEvent` channel, per-statement timeout, cancellation two-tier discipline → Task 1. Matrix encoded as the PURE `failure_action`/`dispatch_statement` with exhaustive tests.
- §3 UI: palette/toolbar entry points, verified `prompt_for_paths` picker (spike RESOLVED — findings with file:line in Task 3's grounding, stronger than the design's requested spike step), non-recursive name-ordered folder semantics, pre-scan "estimate", `ModalState::ScriptRun` with the three controls + matrix-driven graying, `TabContent::ScriptRun` progress tab with TAB_CAP/pinning, history field-reuse wording → Task 3.
- §4 Editor unlock: split-before-guards in `run_query_with`, per-statement auto-limit, stop-on-first-error, `connect_and_run_many`/`MultiQueryEvent`, one-tab-per-row-producing-statement, one history entry, CURATION-order params test → Task 2.
- §5 UI half (pure model is frozen T6): header peek, mapping modal with real-first-batch sample SQL, row pre-count, one-transaction batched runner method with the shared guard, progress reuse of the ScriptRun tab kind, read-only entry gating + runtime guard, history entry → Task 4.
- §6 decomposition honored for the remaining tasks; §7 risks: `guards.rs` dollar-quote gap untouched (out of scope, unchanged); `execute()` doc staleness → Task 5; pre-scan double-IO accepted + labeled „odhad“; fixed 500 batch/log caps kept; cancellation granularity unchanged (between statements/batches, protocol-level for in-flight via child tokens); editor-unlock behavior change → explicit regression pass in Task 2 Step 4.
- CURATION items: 1 → shared `guard_not_read_only` named at every choke point + Task 5 novela listing all four callers; 2 → `dialect_for_engine` returns `None` for Mssql, no Duckdb variant exists, mapping deferred to wiring time; 3 → order enforced structurally (params upstream in `run_query`) + `params_resolve_before_split_two_statements`; 4(a) → `script_write_statement_rejected_on_read_only_policy_matrix_honored`; 4(b) → entry gating in two surfaces + `run_csv_import_refuses_read_only_spec_without_touching_anything`; 4(c) → `read_only_multi_run_runs_select_then_stops_at_update`.

**Placeholder scan:** every step shows real code (types, helpers, full or contract-complete test bodies) or a concrete cargo/git command. The four `// ...` integration-test bodies in Tasks 1/2/4 specify exact fixtures, exact expected events, and exact assertions in their comments — contract-complete per the G5/G6 precedent for GPUI-adjacent glue; renders are described by contract (exact fields, exact Czech labels) matching G6 T3/T7's precedent.

**Name consistency across tasks:** `TxScope { None, PerFile, WholeRun }` / `ErrorPolicy { Stop, Continue }` / `ScriptRunOptions { tx_scope, error_policy, dialect, statement_timeout_secs }` (Task 1) match Task 3's modal fields and `confirm_script_run`. `ScriptEvent`'s six variants and field names match Task 3's event loop arms. `MultiQueryEvent` (Task 2 runner) matches Task 2's `run_many` arms. `ScriptRunState`/`ScriptFileRow`/`ScriptRunOutcome`/`SCRIPT_LOG_CAP` (Task 3 tabs.rs) match Task 4's CSV loop (`progress_rows`, `push_log`). `CsvImportJob { path, schema, table, columns, mapping }` matches T6's `generate_insert_batches(schema, table, columns, mapping, rows)` parameter order and types. The shared guard is `guard_not_read_only` in all four write paths.

**Resolved design ambiguities / deviations (flagged for controller review, not vetoed unilaterally):**
1. **`ScriptEvent::StatementStarted` drops `stmt_total_in_file`** — a streaming splitter cannot know a file's total mid-file; the UI's pre-scan already has exact per-file counts and renders totals from `ModalState::ScriptRun.files`/`ScriptRunState.total_statements`.
2. **No whole-blob `is_read_statement` pre-gate on the multi-statement editor path** — design §4's two paragraphs conflict; CURATION item 4(c) (SELECT runs, THEN the UPDATE is rejected) is binding and wins. Single-statement path keeps guard 1 unchanged.
3. **No `*.sql`/`*.csv` dialog filter** — verified impossible at the pinned GPUI rev (`PathPromptOptions` platform.rs:2139-2148 has no filter field; the Windows `file_open_dialog` gpui_windows/platform.rs:1279 never calls `SetFileTypes`). Client-side extension validation after selection instead.
4. **CSV quoted-empty vs. unquoted-empty is unimplementable with the `csv` crate** (records are unescaped; quoting metadata is not retained) — v1 maps EVERY empty field to NULL, stated in the modal helper text; Task 4 Step 1 re-verifies against the resolved crate version before locking this in. This is the one deviation from a design-§5 "binding constraint" phrasing — the alternative (hand-written RFC-4180 scanner) contradicts §5's own parser decision.
5. **`failure_action(Continue, WholeRun)` = `AbortRun` defensively** in the runner even though the UI grays the combination out — never continue inside one open transaction.
6. **File-level errors (open/read/UTF-8/unterminated-at-EOF) under `Continue` map to `NextFile` regardless of scope** — a broken file has no "next statement" to continue to.
7. **The design's "modal not Esc-closable once running" is structurally moot** — the modal closes when „Spustit“ is confirmed; the tab is the only ongoing surface. Esc during the run cancels via the single-flight token, same as a query.
8. **Multi-statement history `row_count`** = returned rows + affected sum (design silent on the aggregate; a single number is what `HistoryEntry` has room for).
9. **Timeout error text stays English** (`"[timeout] statement exceeded {t}s"`) per the design's literal and `connect_and_run`'s existing precedent; guard/UI strings are Czech.
