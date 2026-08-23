# G5 Sandbox Editing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sandbox grid editing on preview tabs — staged cell edits / row deletes / row inserts with diff colouring, an Apply dialog showing exact generated SQL, execution in a single transaction with PK-based optimistic checks. The app's first and only write path.

**Architecture:** A pure `sandbox.rs` edit model (staging + SQL generation, fully unit-tested) sits beside the grid; the grid renders diff tints and hosts the cell editor; `Connection` gains `execute()` (affected rows) implemented by both drivers; a new runner method drives BEGIN/…/COMMIT with rollback on any error or affected-rows mismatch. Guards: preview-only, PK required, read-only connections excluded.

**Tech Stack:** Rust, GPUI (pinned 907ed09), Arrow (schema for numeric-typed columns), rusqlite, tokio-postgres.

**Spec:** docs/superpowers/specs/2026-08-22-gui-target-design.md §1 "Grid editing" + "G5 design pass" block (authoritative for rules), §3 constraints.

## Global Constraints

- Sandbox Apply is the ONLY write path; nothing else may call `Connection::execute`.
- Errors are values; a failed Apply rolls back, keeps staged edits, surfaces the error in the dialog — never panics, never half-applies.
- SQL generation is pure + exhaustively tested (quoting, NULL vs empty string, numeric emit, PK WHERE with original values).
- Editing requires: preview tab + PK detected + connection not read-only + engine sqlite/pg. Everything else renders exactly as today.
- Czech labels (Aplikovat, Zahodit, Smazat řádek, + řádek, NULL, "{n} změn").
- Suites stay green (dbc-ui 145, dbc-core 13, dbc-state 20, dbc-buffer 7, dbc-driver-sqlite 9, pg docker 9); explicit `-p` only.
- Version bump to 0.5.0 at merge.

---

### Task 1: `Connection::execute` (dbc-core + both drivers)

**Files:**
- Modify: `crates/dbc-core/src/connection.rs`, `crates/dbc-driver-sqlite/src/lib.rs`, `crates/dbc-driver-postgres/src/lib.rs` (+ its `tests/integration.rs`)

**Interfaces:**
```rust
// trait Connection gains:
/// Executes a non-returning statement, reporting affected rows. This is the
/// app's write path — ONLY the sandbox Apply flow may call it.
async fn execute(&mut self, sql: &str, cancel: CancelToken) -> Result<u64, QueryError>;
```
- sqlite: `spawn_blocking` + `conn.execute(sql, [])` (affected via `rusqlite::Connection::changes()` — note `execute` returns usize changed rows directly; BEGIN/COMMIT return 0); cancel checked before dispatch (no mid-statement interrupt needed for v1 — statements are tiny).
- postgres: `client.execute(sql, &[])` (returns u64); wrap with the same cancel-token pre-check.
- IMPORTANT: the connection used for Apply must be the SAME connection across BEGIN…COMMIT (transactions are per-connection). The existing per-run open pattern already yields one `Box<dyn Connection>` per run — Task 4's runner method opens ONE connection and drives all statements over it.

- [ ] **Step 1: sqlite unit tests** (in the driver): `execute_reports_affected_rows` (CREATE + 2 INSERTs via execute → each 1; UPDATE hitting 2 rows → 2; DELETE WHERE miss → 0), `execute_in_transaction_rolls_back` (BEGIN, INSERT, ROLLBACK → row absent). **Step 2: implement both drivers; extend pg docker test with the same shape (`#[ignore]`).** **Step 3:** `cargo test -p dbc-core -p dbc-driver-sqlite` green; `cargo build -p dbc-driver-postgres` clean; run docker suite only if `docker info` succeeds. **Step 4: Commit** — `git commit -m "feat: Connection::execute write path with affected rows"`

---

### Task 2: Sandbox edit model + SQL generation (dbc-ui, pure)

**Files:**
- Create: `crates/dbc-ui/src/sandbox.rs`

**Interfaces:**
```rust
/// Staged, not-yet-applied edits for one editable preview tab. Pure model.
#[derive(Default)]
pub struct EditState {
    /// (source_row, source_col) -> staged value (None = SQL NULL).
    pub cells: HashMap<(usize, usize), Option<String>>,
    pub deleted_rows: HashSet<usize>,
    /// Each entry: per visible source column an optional staged value;
    /// column set fixed at insert time (headers.len()).
    pub inserted_rows: Vec<Vec<Option<Option<String>>>>, // outer Option = "left untouched → table default"
}
impl EditState {
    pub fn is_dirty(&self) -> bool;
    pub fn change_count(&self) -> usize; // edited rows + deletes + inserts (row-granular)
    pub fn stage_cell(&mut self, row: usize, col: usize, v: Option<String>);
    pub fn toggle_delete(&mut self, row: usize);
    pub fn add_insert_row(&mut self, cols: usize) -> usize;
    pub fn stage_insert_cell(&mut self, ins_ix: usize, col: usize, v: Option<String>);
    pub fn clear(&mut self);
}

pub struct TableMeta<'a> {
    pub schema: Option<&'a str>,
    pub table: &'a str,
    pub headers: &'a [String],          // source columns of the preview result
    pub pk_cols: &'a [usize],           // source ixs, non-empty
    pub numeric_cols: &'a [bool],       // per source col, from Arrow schema
}

/// Generates the exact statements the Apply dialog shows, in order:
/// UPDATEs (by ascending row), DELETEs, INSERTs. `original(row, col) ->
/// Option<String>` supplies pre-edit values (None = SQL NULL) for SET-
/// comparison is NOT done (all staged cells emit) and for PK WHERE values.
/// Deleted rows' staged cell edits are ignored (delete wins).
/// Every returned statement pairs with its expected affected-row count
/// (1 for UPDATE/DELETE; inserts don't carry an expectation — the driver
/// reports 1 but server triggers may differ, so INSERT expectation is None).
pub fn generate_statements(
    meta: &TableMeta, edits: &EditState,
    original: &mut dyn FnMut(usize, usize) -> Option<String>,
) -> Vec<(String, Option<u64>)>;

/// Value emitter: staged None -> "NULL"; Some(s) with numeric col AND
/// s parses as f64/i128 strictly -> bare s (trimmed); otherwise '...'
/// with '' doubling.
pub fn sql_value(v: Option<&str>, numeric: bool) -> String;
```
- Quoting: identifiers via `dbc_core::ddl::{quote_ident, quote_qualified}`.
- UPDATE emits ONLY staged columns of that row; WHERE = every pk col `"pk" = {sql_value(original)}` AND-joined; original NULL pk value → `"pk" IS NULL` (edge; test it).
- INSERT emits only columns whose outer Option is Some (user touched them); zero touched columns → `INSERT INTO t DEFAULT VALUES` (sqlite+pg both support; test string form).

- [ ] **Step 1: Tests** (exhaustive; the dialog shows these strings verbatim):
  update single cell (quoted string, PK where), NULL staging vs empty string, numeric unquoted + numeric-parse-failure quoted, multi-cell one row = one UPDATE, delete row (edits on it ignored), insert partial columns, insert untouched → DEFAULT VALUES, pk-null → IS NULL, we"ird idents quoted, o'reilly values escaped, statement ordering + expectations (1/1/None), change_count. **Step 2: implement → all green (`cargo test -p dbc-ui sandbox`).** **Step 3: Commit** — `git commit -m "feat: sandbox edit model and SQL generation"`

---

### Task 3: Grid edit mode (dbc-ui)

**Files:**
- Modify: `crates/dbc-ui/src/grid.rs`, `crates/dbc-ui/src/main.rs`

**Contract:**
1. Editability flows in at tab creation: main.rs computes `Editable{pk_cols, numeric_cols}` for preview tabs (PK from snapshot TableInfo.columns is_pk mapped to result headers; numeric from the result's Arrow schema DataType::is_numeric — buffer schema) when connection not read-only and engine != Mssql; grid gains `editable: Option<Editable>` + `edit_state: EditState`.
2. Double-click on a cell: editable tab → open cell editor (small overlay anchored to the cell or centered modal — centered modal is acceptable v1: shows column name, current value, TextField, buttons Uložit/NULL/Zrušit + "Detail" showing full text); non-editable → existing cell-detail popup unchanged.
3. Staged rendering: cell with staged edit → bg 0x6b5d2e (yellow-ish) and shows the STAGED value; deleted row → all cells bg 0x5d2e2e and strikethrough not required; inserted rows render after real rows (green bg 0x2e5d3a), each cell double-clickable to stage values. Row affordances: a narrow leftmost gutter column (~24 px) with "✕" per row (toggle delete) and a "+ řádek" button in the toolbar (only on editable tabs).
4. PK-less table or read-only or ad-hoc: no gutter, no editor, status notice on preview open of a PK-less table: "tabulka nemá primární klíč — jen pro čtení".
5. Sort/filter interplay: staged cells keyed by SOURCE row — sorting/filtering doesn't lose edits; inserted rows always render at the end regardless of sort; find/export ignore inserted rows (document).
6. Dirty indicator: when edit_state.is_dirty, tab title gains " •" and the apply bar (Task 4) shows.
- Pure helpers tested: pk/numeric mapping from headers+snapshot (extract into sandbox.rs or main.rs helper with tests); staged-value display resolution (staged → display text "(NULL)" for staged NULL).

- [ ] **Step 1: helpers + tests → implement UI.** **Step 2:** suites green, zero warnings, both sanity launches (+ a PK fixture launch). **Step 3: Commit** — `git commit -m "feat: grid edit mode with staged diff rendering"`

---

### Task 4: Apply flow (dbc-ui)

**Files:**
- Modify: `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/src/grid.rs` (apply-bar render seam)

**Contract:**
1. Apply bar above the status bar when the ACTIVE tab is dirty: "{n} změn · Aplikovat · Zahodit". Zahodit → edit_state.clear() + notify. Aplikovat → modal listing the generated statements (monospace, scrollable — reuse Text-tab body pattern) + "Potvrdit a spustit" / "Zrušit".
2. Confirm → `QueryRunner::run_write_transaction(spec, statements: Vec<(String, Option<u64>)>) -> oneshot::Receiver<Result<u64 /*total affected*/, QueryError>>`: opens ONE connection via open_spec, then over the SAME connection: execute("BEGIN"), each statement (checking driver-affected == expectation when Some; mismatch → execute("ROLLBACK") + Err "řádek se mezitím změnil — aplikace zrušena"), execute("COMMIT"); any execute error → attempt ROLLBACK + return the original error. Timeout: cfg.timeout_secs bounds the whole transaction (tokio::time::timeout around the sequence; on timeout attempt ROLLBACK).
3. While applying: modal shows "aplikuji…", buttons disabled; success → close modal, clear edit_state, status "aplikováno ({n} příkazů)", re-run the preview (existing pipeline, preserves joins via from_join_change=false machinery), record ONE history entry (sql = the statements joined by newline, connection name, affected rows as row_count).
4. Failure → modal stays open showing the error; edits stay staged.
5. Secret handling identical to run_query_with (vault lookup); read-only cfg can never reach here (grid has no editable state), but run_write_transaction ALSO hard-refuses when cfg.read_only (belt-and-braces, tested at the pure level by a guard fn).
- Pure helpers tested: the affected-mismatch decision (expectation vs reported), the guard (read_only → refuse).

- [ ] **Step 1: helpers + tests → runner method → UI wiring.** **Step 2:** full sweep green incl. drivers; zero warnings; sanity launches; if Docker runs, a manual-ish integration check via the sqlite driver path is possible headlessly: not required, note it. **Step 3: Commit** — `git commit -m "feat: apply dialog and single-transaction write flow"`

---

## Self-Review Notes

- Spec coverage (G5 design pass block): staging model + diff colours → T3; Apply dialog with exact SQL + single transaction + rollback + affected-check → T2 (generation+expectations) + T4 (execution); PK detection/read-only/preview-only gating → T3; Connection::execute → T1; history record → T4; NULL vs empty string → T2 editor affordance T3.
- Type consistency: TableMeta consumed by T4's dialog via T2's generate_statements; Editable{pk_cols,numeric_cols} produced in T3 main.rs matches TableMeta inputs; run_write_transaction signature matches T2's Vec<(String, Option<u64>)>.
- Order: T1 ∥ T2 → T3 → T4.
- Known risks: transaction-over-one-connection is the crux (T1 note + T4 contract pin it); sort/filter vs staged-source-row mapping (T3 contract; reviewer must probe); the re-run-after-apply resets the grid — staged-clear-before-rerun ordering (T4 reviewer trace).
