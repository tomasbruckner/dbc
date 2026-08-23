# G13 Execution Plans Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers for every task, a **sonnet** adversarial code review per task before it's considered done, and a **default-model** final review once all tasks land (same staffing convention as the G9 plan).

**Goal:** "Explain"/"Analyze" actions in the status bar that run a query's estimated or actual (`ANALYZE`) execution plan and render it as an indented, metric-columned tree in a new result-tab kind — per-node operation/cost/rows/timing/buffers, hot-node highlighting, engine-provided hints (MSSQL missing-index), and a raw-text toggle. Postgres is fully wired end-to-end in this phase; SQLite gets the (cheap, always-safe) estimated-only path; MSSQL's parser lands unit-tested against hand-authored fixtures but stays dead code until the (separate, unscheduled) MSSQL driver phase wires `dbc-ui`'s connect path.

**Architecture:** One new pure-then-GPUI module, `crates/dbc-ui/src/plan.rs`, following `schema_tree.rs`'s colocation convention: unified `PlanResult`/`PlanNode` model + per-engine parsers (`parse_pg_json` via `serde_json`, `parse_mssql_xml` via `quick-xml`, `parse_sqlite_rows` with no new dependency) + the hot-node formulas + the `analyze_gate` write-safety dispatcher, all pure and unit-tested, in the first half of the file; the `PlanView` GPUI entity (indented `uniform_list` tree, same pattern as `SchemaTree`) in the second half. No `dbc-core` change, no driver change — plans ride the existing `Connection::query`/`execute` paths. The one write-adjacent piece is a new **runner-owned, sanctioned** method, `QueryRunner::run_analyze_write`, implementing the "ANALYZE-on-a-write" sequence (dedicated connection, `BEGIN` → the `EXPLAIN ANALYZE` query → `ROLLBACK`, always) — it is the app's third confirmed write path, alongside G5's Apply flow and (once merged) G9's kill flow, sharing the SAME `guard_not_read_only`/`spec_is_read_only` gate already in `runner.rs`. `TabContent` grows a `Plan { view: Entity<PlanView> }` variant; `main.rs` grows two status-bar buttons ("Vysvětlit"/"Analyzovat"), a new `ModalState::AnalyzeWriteConfirm` confirm dialog, and the dispatch glue — all as a serialized tail task after G6/G9/G12's `runner.rs`/`main.rs` work merges (single-writer files across parallel phases in this repo).

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no new GPUI primitive beyond `uniform_list`/`cx.processor`/`cx.spawn`, all already demonstrated in `schema_tree.rs`/`main.rs`), `serde_json` (new **direct** dependency of `dbc-ui`; already a workspace dependency used transitively via `dbc-state`, pinned `"1"` workspace-wide, resolves to `1.0.151`), `quick-xml = "0.41"` (new dependency of `dbc-ui`; already resolves to `0.41.0` in `Cargo.lock` transitively, so this pin introduces no version skew), `testcontainers-modules = { version = "0.13", features = ["postgres"] }` (new **dev**-dependency of `dbc-ui`, same crate+version `dbc-driver-postgres`'s own integration tests already pin).

**Spec:** `docs/superpowers/specs/2026-08-22-gui-target-design.md` (G13 phasing row) and `docs/superpowers/specs/drafts/g13-execution-plans-design.md` (binding design for this phase — implement exactly what it specifies; its CURATION block is non-negotiable, see Global Constraints). Every API claim below is grounded against the actual code on this branch as of the design's own read: `crates/dbc-core/src/connection.rs`, `crates/dbc-core/src/guards.rs`, `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/src/tabs.rs`, `crates/dbc-ui/src/schema_tree.rs`, `crates/dbc-ui/src/main.rs`, `crates/dbc-state/src/config.rs`, `crates/dbc-driver-mssql` (odbc-api 29.0.0, confirmed against the vendored source: `Cursor::more_results(self) -> Result<Option<Self>, Error>` at `odbc-api-29.0.0/src/cursor.rs:102/382`, consuming `self` and yielding the next result set's cursor or `None`).

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- Errors are values; no panics on parser or DB paths. A malformed/unexpected server payload (missing JSON key, unrecognized XML attribute, a SQLite `detail` string that doesn't match the known verb set) degrades to `None`/a fallback string — never a crash. `serde_json::from_str`/`quick-xml`'s `Reader` are used via their normal (recursion-limited) safe API — `Deserializer::disable_recursion_limit()` is NEVER called.
- **Non-finite floats are rejected at the parser boundary, unconditionally.** Every `f64` field the parsers populate (`est_cost`, `est_rows`, `actual_rows`, `actual_time_ms`, `rows_removed_by_filter`, planning/execution time) is passed through a `finite()` guard (`Option<f64>::filter(|f| f.is_finite())`) before it reaches `PlanNode`/`PlanResult` — a hostile or buggy payload containing `Infinity`/`NaN` (JSON itself has no such literal, so this matters most for the MSSQL XML attribute path, where a malformed `EstimatedTotalSubtreeCost="inf"` string could otherwise parse via a permissive float parser) degrades to `None`, never propagates into a cost/time display or the hot-fraction formulas.
- **Deep recursive tree hazard (binding, applies to every task in this plan that touches `PlanNode`):** plan trees can nest arbitrarily (a wide `UNION`/CTE chain, or a hostile payload). This repo has already hit real stack overflows from naive recursive tree code — confirmed again while drafting this plan: a `struct Naive { children: Vec<Naive> }` with NO custom `Drop` survives a synthetic 2,000-deep single-child chain but **overflows the stack at 5,000 deep** (Windows default thread stack, both debug and release). Consequently:
  - `PlanNode` does **NOT** derive `Clone` (only `Debug, PartialEq`) — nothing in this plan ever needs to clone a whole plan tree; shared ownership uses `Rc<PlanResult>`/`Entity<PlanView>`, the same convention `TabContent::Grid`'s `Rc<RefCell<ResultBuffer>>` already establishes for "heavy state one tab owns, rendered by one entity."
  - `PlanNode` gets a **custom, iterative `Drop`** (T1) — verified during drafting to survive a 50,000-deep synthetic chain in both debug and release builds where the naive derive fails at 5,000.
  - Every function that **builds** a `PlanNode` tree from external input (`convert_pg_tree` in T2, `parse_mssql_xml`'s `<RelOp>` walk in T3, `parse_sqlite_rows`'s id/parent assembly in T1) is **iterative** (an explicit `Vec`-based frame stack), not a self-calling recursive function — each is grounded below with the exact stack-machine shape, verified during drafting against real captured Postgres output and hand-authored XML.
  - Every function that **walks** an already-built tree for rendering (`flatten_plan` in T5) is likewise iterative.
  - `PgPlanJson`'s `serde_json` deserialization is the one place actual recursion happens (serde's derive-generated visitor calls itself once per nesting level) — this is safe because `serde_json`'s default `Deserializer` has `disable_recursion_limit: false` and a hard-coded `remaining_depth: 128` (confirmed against `serde_json-1.0.151/src/de.rs:63`): nesting beyond 128 levels returns a clean `Err("recursion limit exceeded ...")`, not a panic — confirmed during drafting with a 1,000-level synthetic payload. 128 is far below the ~2,000–5,000-level danger zone measured above, so the JSON parse step itself never reaches the hazard zone; `convert_pg_tree`'s OWN walk of the (≤128-deep) result is still written iteratively regardless, for uniformity with T3/T5 and because a future relaxation of that assumption (e.g. someone calling `disable_recursion_limit`) must not silently reintroduce the hazard.
  - MSSQL's XML walk additionally carries an explicit `MAX_XML_DEPTH` cap (T3) since `quick-xml`'s `Reader` has no recursion-limit-equivalent of its own to lean on.
  - `#[derive(PartialEq)]` on `PlanNode` is itself structurally recursive (each `assert_eq!` on two subtrees walks both) — fine for the shallow fixtures every test in this plan uses, but any stress/deep-chain test MUST NOT `assert_eq!` two deep trees; use a targeted iterative helper (`count_nodes`/depth-walk) instead, exactly as T1's own stress test does.
- **Write invariant (§3-novela, binding project-wide, restated per this plan's CURATION item 2):** every write reaches `Connection::execute` only through (a) a confirm modal showing the exact SQL, (b) a runner-owned method with explicit transaction discipline, and (c) the SHARED read-only guard at the runner choke point (`runner::guard_not_read_only`/`spec_is_read_only`, already present in `runner.rs` today — G5's Apply flow already uses them; no new logic invented here). This plan's ANALYZE-on-a-write sequence is case (b)+(c): a new `QueryRunner::run_analyze_write` method (T6) that (1) lives in `runner.rs`, (2) calls `guard_not_read_only` itself (belt-and-braces — the UI-side `analyze_gate` already refused before ever dispatching, but the runner method refuses independently too, same two-independent-gates posture the G9 kill flow uses), (3) is dispatched ONLY from the `ModalState::AnalyzeWriteConfirm` confirm modal's "Analyzovat" button. `execute()`'s doc comment (`connection.rs`) is updated once (T6) to add this method to its sanctioned-caller list.
- **Read-only enforcement — REQUIRED tests (binding):** `analyze_gate` unit tests cover the three-case matrix (read → `Run`; write + read-only → `Blocked`; write + writable → `NeedsConfirm`) PLUS the CTE/comment/`EXPLAIN ANALYZE`-bypass edge cases `dbc_core::guards`'s own test suite already covers for `is_read_statement` (T4). `run_analyze_write`'s belt-and-braces refusal is unit tested directly against a read-only `ConnectSpec` with NO live connection ever attempted (T6, same "refuses before `open_spec` is ever called" shape as `run_write_transaction_refuses_read_only_connection_without_connecting`).
- No credentials/result data in history or logs. This phase does not record Explain/Analyze runs to the query-history DB at all (design is silent on history for G13; the existing `record_history` call sites are all inside `run_query_with`'s dispatch, which the Explain/Analyze buttons deliberately bypass per §4 of the design — same precedent as G9's monitor tiles, which also never touch history). The confirm-modal SQL and the plan's `raw_text` never leave the UI process.
- MSSQL: `odbc-api` 29.0.0's `Cursor::more_results(self) -> Result<Option<Self>, Error>` (confirmed above) is the mechanism a future MSSQL driver phase would use to skip `STATISTICS XML`'s leading statement result set and read the trailing plan-XML result set. This plan's T3 parser is written and unit-tested against that eventual shape but ships as unreachable/dead code — `dbc-ui/src/connect.rs::open_config`'s `Engine::Mssql` arm still hard-errors (`"MSSQL driver zatím není k dispozici"`) as of this branch, so no UI path can dispatch MSSQL Explain/Analyze SQL yet regardless of what T3 parses.
- Task-ordering (single-writer files across parallel phases): `crates/dbc-ui/src/runner.rs` and `crates/dbc-ui/src/main.rs` are being edited concurrently by G6/G9/G12 on separate branches in this repo. T6 (the only task in this plan touching either file) is a **serialized tail task, dispatched only after G6+G9+G12's `runner.rs`/`main.rs` work has merged to `main`** and this branch has rebased onto that merge — re-locate every line reference below by symbol, not line number, after that rebase. T1–T5 are pure-new-file/pure-additions-to-one-new-file tasks (`plan.rs` doesn't exist on any other in-flight branch) and can run in parallel worktrees today.
- Version bump to `0.13.0` in `crates/dbc-ui/Cargo.toml` at merge (phasing-table convention: version tracks the phase number, not landing order).

### Task dependency graph

| Task | Depends on | Files | Notes |
|---|---|---|---|
| T1 | — | `plan.rs` (new) | solo, first — every other task needs its model types |
| T2 | T1 | `plan.rs`, `crates/dbc-ui/Cargo.toml`, `crates/dbc-ui/tests/fixtures/pg_explain_*.json` (new) | parallel batch |
| T3 | T1 | `plan.rs`, `crates/dbc-ui/Cargo.toml`, `crates/dbc-ui/tests/fixtures/mssql_showplan_*.xml` (new) | parallel batch |
| T4 | T1 | `plan.rs` | parallel batch |
| T5 | T1 | `plan.rs` | parallel batch |
| T6 | T1, T2, T4, T5 | `runner.rs`, `main.rs`, `connections_ui.rs`, `tabs.rs`, `connection.rs` (doc only) | serialized tail, after G6+G9+G12 merge |
| T7 | T3 (parser), T6 (wiring shape) | — | **deferred**, tracked only — no code in this plan's mergeable scope |

Suggested batches: **{T1}** solo → **{T2, T3, T4, T5}** in parallel worktrees, each depending only on T1's merged model types → **{T6}** solo, after the rebase note above → **T7** whenever the MSSQL driver phase happens (orthogonal, unscheduled).

---

### Task 1 (T1): Model + hot-node formulas + SQL builders + SQLite parser — `plan.rs`

**Files:**
- Create: `crates/dbc-ui/src/plan.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `#[allow(dead_code)] // consumed from T6 on; allow removed in T6` + `mod plan;` to the mod list, alphabetically after `mod palette;`)

**Interfaces:**
- Consumes: `dbc_state::Engine`, `dbc_core::is_read_statement` (for T4's `analyze_gate`, added in this same file by T4 — listed here since it's part of "the pure half").
- Produces (consumed by T2, T3, T4, T5, T6):

```rust
#[derive(Debug, PartialEq)]
pub struct PlanResult {
    pub root: PlanNode,
    pub is_analyze: bool,
    pub engine: dbc_state::Engine,
    pub total_planning_time_ms: Option<f64>,
    pub total_execution_time_ms: Option<f64>,
    pub top_level_hints: Vec<PlanHint>,
    pub raw_text: String,
}

/// NOT `Clone` — see the Global Constraints "Deep recursive tree hazard"
/// note. Shared ownership uses `Rc<PlanResult>` (T5/T6), never a deep copy.
#[derive(Debug, PartialEq)]
pub struct PlanNode {
    pub operation: String,
    pub target: Option<String>,
    pub est_cost: Option<f64>,
    pub est_rows: Option<f64>,
    pub actual_rows: Option<f64>,
    pub actual_time_ms: Option<f64>,
    pub loops: Option<u64>,
    pub rows_removed_by_filter: Option<f64>,
    pub buffers: Option<BufferStats>,
    pub extra: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BufferStats {
    pub shared_hit: Option<u64>,
    pub shared_read: Option<u64>,
    pub shared_dirtied: Option<u64>,
    pub shared_written: Option<u64>,
    pub temp_read: Option<u64>,
    pub temp_written: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanHint {
    pub message: String,
    pub detail: Option<String>,
}

/// Rejects `Infinity`/`NaN` at the parser boundary (Global Constraints).
pub fn finite(v: Option<f64>) -> Option<f64>;

/// Actual-plan self time (design §2 formula): this node's own cumulative
/// time across all loops, minus its DIRECT children's cumulative time,
/// clamped to >= 0.0. Shallow — O(children.len()), never recurses.
pub fn self_time_ms(node: &PlanNode) -> f64;
/// Estimated-plan self cost, same shape, using `est_cost`.
pub fn self_cost(node: &PlanNode) -> f64;
/// `None` only when there is truly nothing to normalize against (SQLite
/// estimated plans have no `est_cost` anywhere); a zero/absent-total
/// denominator otherwise yields `Some(0.0)`, never NaN.
pub fn hot_fraction(
    node: &PlanNode,
    root: &PlanNode,
    is_analyze: bool,
    total_execution_time_ms: Option<f64>,
) -> Option<f64>;

/// `EXPLAIN ...` text for the ALWAYS-SAFE estimated path (§5: never
/// executes the statement on any engine — no gating needed, ever).
pub fn explain_sql(engine: dbc_state::Engine, sql: &str) -> String;
/// `EXPLAIN ANALYZE ...`-family text, or `None` when the engine has no
/// such mode (SQLite — §1c, the "Analyze" button is hidden entirely).
pub fn explain_analyze_sql(engine: dbc_state::Engine, sql: &str) -> Option<String>;
/// Whether the "Analyze" button should render at all for `engine`.
pub fn analyze_button_visible(engine: dbc_state::Engine) -> bool;

/// SQLite's `EXPLAIN QUERY PLAN` row shape: `(id, parent, detail)` —
/// `notused` (SQLite's docs: always 0, reserved) is dropped by the caller
/// before this function ever sees the row (main.rs, T6).
pub fn parse_sqlite_rows(rows: &[(i64, i64, String)]) -> PlanNode;
```

**Grounding — `PlanNode`'s iterative `Drop`** (verified during drafting: survives a 50,000-deep synthetic chain in debug AND release; the equivalent type with NO custom `Drop` overflows the stack at 5,000 deep):

```rust
impl Drop for PlanNode {
    fn drop(&mut self) {
        let mut stack: Vec<PlanNode> = std::mem::take(&mut self.children);
        while let Some(mut node) = stack.pop() {
            stack.extend(std::mem::take(&mut node.children));
            // `node` drops here with empty `children` — no recursion.
        }
    }
}
```

**Grounding — hot-fraction formulas** (design §2; only look at DIRECT children, so no recursion/depth concern here regardless of tree depth):

```rust
pub fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|f| f.is_finite())
}

pub fn self_time_ms(node: &PlanNode) -> f64 {
    let own = node.actual_time_ms.unwrap_or(0.0) * node.loops.unwrap_or(1) as f64;
    let children_total: f64 = node
        .children
        .iter()
        .map(|c| c.actual_time_ms.unwrap_or(0.0) * c.loops.unwrap_or(1) as f64)
        .sum();
    (own - children_total).max(0.0)
}

pub fn self_cost(node: &PlanNode) -> f64 {
    let own = node.est_cost.unwrap_or(0.0);
    let children_total: f64 = node.children.iter().map(|c| c.est_cost.unwrap_or(0.0)).sum();
    (own - children_total).max(0.0)
}

pub fn hot_fraction(
    node: &PlanNode,
    root: &PlanNode,
    is_analyze: bool,
    total_execution_time_ms: Option<f64>,
) -> Option<f64> {
    if is_analyze {
        let denom = total_execution_time_ms
            .unwrap_or_else(|| root.actual_time_ms.unwrap_or(0.0) * root.loops.unwrap_or(1) as f64);
        if denom <= 0.0 {
            return Some(0.0);
        }
        Some((self_time_ms(node) / denom).clamp(0.0, 1.0))
    } else {
        match root.est_cost {
            None => None, // SQLite estimated plans: nothing to normalize against.
            Some(total) if total <= 0.0 => Some(0.0),
            Some(total) => Some((self_cost(node) / total).clamp(0.0, 1.0)),
        }
    }
}
```

**Grounding — SQL builders** (design §1a/§1b/§1c/§5):

```rust
pub fn explain_sql(engine: dbc_state::Engine, sql: &str) -> String {
    match engine {
        dbc_state::Engine::Postgres => format!("EXPLAIN (FORMAT JSON) {sql}"),
        // T7 (deferred): unreachable until the MSSQL driver phase wires
        // `connect::open_config`'s Engine::Mssql arm.
        dbc_state::Engine::Mssql => format!("SET SHOWPLAN_XML ON; {sql}"),
        dbc_state::Engine::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
    }
}

pub fn explain_analyze_sql(engine: dbc_state::Engine, sql: &str) -> Option<String> {
    match engine {
        dbc_state::Engine::Postgres => Some(format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}")),
        dbc_state::Engine::Mssql => {
            Some(format!("SET STATISTICS XML ON; {sql}; SET STATISTICS XML OFF;"))
        }
        dbc_state::Engine::Sqlite => None,
    }
}

pub fn analyze_button_visible(engine: dbc_state::Engine) -> bool {
    !matches!(engine, dbc_state::Engine::Sqlite)
}
```

**Grounding — `parse_sqlite_rows`, iterative build, cycle-safe** (design §1c; no new dependency — `regex` is NOT a workspace/`dbc-ui` dependency today and none of the other 40+ crates it would pull in are worth adding for one small fixed-shape scan, so target extraction is a manual token walk instead of `r"(?:TABLE|INDEX) (\w+)"`; verified during drafting against SQLite's real `SCAN`/`SEARCH`/`USE`/`COMPOUND` verb shapes, including a 20,000-deep synthetic parent chain built and dropped without overflow):

```rust
fn extract_target(detail: &str) -> Option<String> {
    let words: Vec<&str> = detail.split_ascii_whitespace().collect();
    for i in 0..words.len().saturating_sub(1) {
        let w = words[i].to_ascii_uppercase();
        if w == "TABLE" || w == "INDEX" {
            let raw = words[i + 1];
            let name: String =
                raw.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Leading-verb allowlist per SQLite's actual `EXPLAIN QUERY PLAN` output
/// vocabulary; anything else falls back to the whole `detail` text as
/// `operation` with `target: None` (fail-open on display text, never
/// panic — design §1c).
fn operation_and_target(detail: &str) -> (String, Option<String>) {
    let leading = detail.split_ascii_whitespace().next().unwrap_or(detail).to_string();
    let known = matches!(
        leading.as_str(),
        "SCAN" | "SEARCH" | "USE" | "CO-ROUTINE" | "COMPOUND" | "EXECUTE"
    );
    if known {
        (leading, extract_target(detail))
    } else {
        (detail.to_string(), None)
    }
}

fn leaf(operation: String, target: Option<String>) -> PlanNode {
    PlanNode {
        operation,
        target,
        est_cost: None,
        est_rows: None,
        actual_rows: None,
        actual_time_ms: None,
        loops: None,
        rows_removed_by_filter: None,
        buffers: None,
        extra: Vec::new(),
        children: Vec::new(),
    }
}

/// Iterative build from SQLite's flat `(id, parent, detail)` rows —
/// `parent == 0` are roots (SQLite's "no parent" sentinel). Cycle-safe: a
/// per-root `visited` set stops a malformed self/mutual-parent chain from
/// looping forever (SQLite itself shouldn't emit this, but the parser must
/// never trust server output blindly — same defensive posture as G9's
/// `build_blocking_tree`). Multiple root rows (a compound statement, e.g.
/// `UNION`) get a synthesized `"QUERY PLAN"` wrapper root, per design §1c.
pub fn parse_sqlite_rows(rows: &[(i64, i64, String)]) -> PlanNode {
    use std::collections::{HashMap, HashSet};

    let mut children_of: HashMap<i64, Vec<usize>> = HashMap::new();
    for (ix, (_, parent, _)) in rows.iter().enumerate() {
        children_of.entry(*parent).or_default().push(ix);
    }

    struct Frame {
        row_ix: usize,
        pending_children: Vec<usize>,
        done: Vec<PlanNode>,
    }

    fn build_one(root_ix: usize, rows: &[(i64, i64, String)], children_of: &HashMap<i64, Vec<usize>>) -> PlanNode {
        let mut visited: HashSet<i64> = HashSet::new();
        visited.insert(rows[root_ix].0);
        let mut first_pending = children_of.get(&rows[root_ix].0).cloned().unwrap_or_default();
        first_pending.reverse(); // pop() takes from the end -> preserve original row order
        let mut stack = vec![Frame { row_ix: root_ix, pending_children: first_pending, done: Vec::new() }];
        loop {
            let top = stack.last_mut().expect("stack never empty inside loop");
            if let Some(child_ix) = top.pending_children.pop() {
                let child_id = rows[child_ix].0;
                if !visited.insert(child_id) {
                    // Cycle/duplicate id: attach as a leaf, don't descend again.
                    let (operation, target) = operation_and_target(&rows[child_ix].2);
                    top.done.push(leaf(operation, target));
                    continue;
                }
                let mut pending = children_of.get(&child_id).cloned().unwrap_or_default();
                pending.reverse();
                stack.push(Frame { row_ix: child_ix, pending_children: pending, done: Vec::new() });
                continue;
            }
            let frame = stack.pop().expect("just checked non-empty");
            let (operation, target) = operation_and_target(&rows[frame.row_ix].2);
            let mut node = leaf(operation, target);
            node.children = frame.done;
            match stack.last_mut() {
                Some(parent) => parent.done.push(node),
                None => return node,
            }
        }
    }

    let root_ixs = children_of.get(&0).cloned().unwrap_or_default();
    if rows.is_empty() {
        return leaf("QUERY PLAN".to_string(), None);
    }
    let mut roots: Vec<PlanNode> = root_ixs.iter().map(|&ix| build_one(ix, rows, &children_of)).collect();
    if roots.len() == 1 {
        roots.pop().expect("checked len == 1")
    } else {
        let mut wrapper = leaf("QUERY PLAN".to_string(), None);
        wrapper.children = roots;
        wrapper
    }
}
```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/plan.rs`, `#[cfg(test)] mod model_tests`):

```rust
#[cfg(test)]
mod model_tests {
    use super::*;

    fn node(operation: &str) -> PlanNode {
        PlanNode {
            operation: operation.to_string(),
            target: None,
            est_cost: None,
            est_rows: None,
            actual_rows: None,
            actual_time_ms: None,
            loops: None,
            rows_removed_by_filter: None,
            buffers: None,
            extra: Vec::new(),
            children: Vec::new(),
        }
    }

    #[test]
    fn finite_rejects_non_finite() {
        assert_eq!(finite(Some(f64::INFINITY)), None);
        assert_eq!(finite(Some(f64::NAN)), None);
        assert_eq!(finite(Some(f64::NEG_INFINITY)), None);
        assert_eq!(finite(Some(1.5)), Some(1.5));
        assert_eq!(finite(None), None);
    }

    #[test]
    fn self_time_subtracts_direct_children_times_loops() {
        let mut child_a = node("A");
        child_a.actual_time_ms = Some(2.0);
        child_a.loops = Some(3); // 6.0 total
        let mut child_b = node("B");
        child_b.actual_time_ms = Some(1.0);
        child_b.loops = Some(1); // 1.0 total
        let mut root = node("Root");
        root.actual_time_ms = Some(10.0);
        root.loops = Some(1);
        root.children = vec![child_a, child_b];
        assert_eq!(self_time_ms(&root), 10.0 - 6.0 - 1.0);
    }

    #[test]
    fn self_time_clamps_negative_noise_to_zero() {
        let mut child = node("C");
        child.actual_time_ms = Some(100.0);
        child.loops = Some(1);
        let mut root = node("Root");
        root.actual_time_ms = Some(1.0); // less than its child — floating noise case
        root.loops = Some(1);
        root.children = vec![child];
        assert_eq!(self_time_ms(&root), 0.0);
    }

    #[test]
    fn hot_fraction_actual_zero_denominator_is_zero_not_nan() {
        let root = node("Root");
        assert_eq!(hot_fraction(&root, &root, true, None), Some(0.0));
        assert_eq!(hot_fraction(&root, &root, true, Some(0.0)), Some(0.0));
    }

    #[test]
    fn hot_fraction_estimated_none_when_no_cost_anywhere() {
        let root = node("Root"); // est_cost: None (SQLite estimated plan)
        assert_eq!(hot_fraction(&root, &root, false, None), None);
    }

    #[test]
    fn hot_fraction_estimated_normalizes_against_root_cost() {
        let mut root = node("Root");
        root.est_cost = Some(100.0);
        let mut child = node("Child");
        child.est_cost = Some(30.0);
        root.children = vec![child];
        // self_cost(root) = 100 - 30 = 70; 70/100 = 0.7
        assert_eq!(hot_fraction(&root, &root, false, None), Some(0.7));
    }

    #[test]
    fn explain_sql_per_engine() {
        assert_eq!(
            explain_sql(dbc_state::Engine::Postgres, "SELECT 1"),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
        assert_eq!(
            explain_sql(dbc_state::Engine::Sqlite, "SELECT 1"),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
        assert!(explain_sql(dbc_state::Engine::Mssql, "SELECT 1").starts_with("SET SHOWPLAN_XML ON"));
    }

    #[test]
    fn explain_analyze_sql_sqlite_is_none() {
        assert_eq!(explain_analyze_sql(dbc_state::Engine::Sqlite, "SELECT 1"), None);
        assert!(explain_analyze_sql(dbc_state::Engine::Postgres, "SELECT 1").unwrap().contains("ANALYZE, BUFFERS"));
        assert!(explain_analyze_sql(dbc_state::Engine::Mssql, "SELECT 1").unwrap().contains("STATISTICS XML ON"));
    }

    #[test]
    fn analyze_button_visible_hides_only_for_sqlite() {
        assert!(analyze_button_visible(dbc_state::Engine::Postgres));
        assert!(analyze_button_visible(dbc_state::Engine::Mssql));
        assert!(!analyze_button_visible(dbc_state::Engine::Sqlite));
    }

    // --- parse_sqlite_rows ---

    fn row(id: i64, parent: i64, detail: &str) -> (i64, i64, String) {
        (id, parent, detail.to_string())
    }

    #[test]
    fn sqlite_single_scan() {
        let root = parse_sqlite_rows(&[row(1, 0, "SCAN TABLE t")]);
        assert_eq!(root.operation, "SCAN");
        assert_eq!(root.target.as_deref(), Some("t"));
        assert!(root.children.is_empty());
    }

    #[test]
    fn sqlite_search_with_index() {
        let root = parse_sqlite_rows(&[row(1, 0, "SEARCH TABLE t USING INDEX idx (col=?)")]);
        assert_eq!(root.operation, "SEARCH");
        assert_eq!(root.target.as_deref(), Some("t"));
    }

    #[test]
    fn sqlite_multi_root_gets_synthetic_wrapper() {
        let root = parse_sqlite_rows(&[row(1, 0, "SCAN TABLE a"), row(2, 0, "SCAN TABLE b")]);
        assert_eq!(root.operation, "QUERY PLAN");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].target.as_deref(), Some("a"));
        assert_eq!(root.children[1].target.as_deref(), Some("b"));
    }

    #[test]
    fn sqlite_nested_preserves_row_order() {
        let root = parse_sqlite_rows(&[
            row(1, 0, "COMPOUND QUERY"),
            row(2, 1, "SCAN TABLE a"),
            row(3, 1, "SCAN TABLE b"),
        ]);
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].target.as_deref(), Some("a"));
        assert_eq!(root.children[1].target.as_deref(), Some("b"));
    }

    #[test]
    fn sqlite_non_matching_detail_is_fail_open() {
        let root = parse_sqlite_rows(&[row(1, 0, "USE TEMP B-TREE FOR ORDER BY")]);
        assert_eq!(root.operation, "USE TEMP B-TREE FOR ORDER BY");
        assert_eq!(root.target, None);
    }

    #[test]
    fn sqlite_empty_rows_never_panics() {
        let root = parse_sqlite_rows(&[]);
        assert_eq!(root.operation, "QUERY PLAN");
    }

    #[test]
    fn sqlite_self_referential_row_is_defensive_leaf_not_infinite_loop() {
        // A row that (malformed) claims itself as parent — must terminate.
        let root = parse_sqlite_rows(&[row(1, 1, "SCAN TABLE t")]);
        // id=1 never appears under parent=0, so there are no roots at all;
        // the empty-roots case degrades to the synthetic wrapper with zero
        // children rather than hanging.
        assert_eq!(root.operation, "QUERY PLAN");
        assert!(root.children.is_empty());
    }

    // --- deep-chain stress test: iterative Drop must survive far past the
    // ~2,000–5,000-deep danger zone measured for naive recursive drop glue.
    // Per the Global Constraints note: NEVER `assert_eq!` two deep trees
    // (PartialEq is itself recursive) — use an iterative depth probe.
    #[test]
    fn deep_sqlite_chain_builds_and_drops_without_overflow() {
        let depth = 20_000i64;
        let rows: Vec<(i64, i64, String)> =
            (1..=depth).map(|i| (i, i - 1, format!("SCAN TABLE t{i}"))).collect();
        let root = parse_sqlite_rows(&rows);

        fn iterative_depth(root: &PlanNode) -> usize {
            let mut d = 0usize;
            let mut cur = root;
            loop {
                d += 1;
                match cur.children.first() {
                    Some(c) => cur = c,
                    None => return d,
                }
            }
        }
        assert_eq!(iterative_depth(&root), depth as usize);
        drop(root); // must not overflow — proves PlanNode's custom Drop works end to end.
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui plan::`
Expected: compile error (`plan` module doesn't exist).

- [ ] **Step 3: Implement** everything in the Interfaces block: the structs, `PlanNode`'s custom `Drop`, `finite`/`self_time_ms`/`self_cost`/`hot_fraction`, `explain_sql`/`explain_analyze_sql`/`analyze_button_visible`, `extract_target`/`operation_and_target`/`leaf`/`parse_sqlite_rows` — exactly per the grounding code above.

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui plan::`
Expected: all new tests pass, zero warnings (the `#[allow(dead_code)]` on the `mod plan;` line covers the not-yet-consumed public items).

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/plan.rs crates/dbc-ui/src/main.rs
git commit -m "feat: execution-plan model, hot-node formulas, SQLite parser (G13 T1)"
```

---

### Task 2 (T2): Postgres parser — `parse_pg_json` + docker-validated fixtures

**Files:**
- Modify: `crates/dbc-ui/src/plan.rs` (add `parse_pg_json`, `PgPlanJson`, `PgExplainRoot`, `convert_pg_tree`, tests)
- Modify: `crates/dbc-ui/Cargo.toml` (add `serde_json.workspace = true` to `[dependencies]`; add `testcontainers-modules = { version = "0.13", features = ["postgres"] }` to `[dev-dependencies]`)
- Create: `crates/dbc-ui/tests/fixtures/pg_explain_seq_scan.json`, `pg_explain_estimated_hash_join.json`, `pg_explain_index_scan_analyze.json`, `pg_explain_hash_join_analyze.json`, `pg_explain_parallel_analyze.json`

**Interfaces:**
- Consumes: `PlanNode`/`PlanResult`/`BufferStats`/`finite` (T1).
- Produces (consumed by T6, and by the dispatcher `parse_plan` this task also adds):

```rust
pub fn parse_pg_json(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String>;

/// Dispatches by engine; SQLite is NOT routed through here — its parser
/// needs typed rows, not raw text (T1's `parse_sqlite_rows`, called
/// directly by T6's tab-construction code). MSSQL routes here once T3
/// lands (dead code until the driver phase).
pub fn parse_plan(engine: dbc_state::Engine, is_analyze: bool, raw_text: &str) -> Result<PlanResult, String>;
```

**Grounding — real captures.** These fixtures are VERBATIM `EXPLAIN (FORMAT JSON)`/`EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)` output captured against a live `postgres:16.13` docker container while drafting this plan (schema: `users(id, name, age)` 20,000 rows, `orders(id, user_id, amount)` 50,000 rows referencing `users`, one index `idx_orders_user` on `orders(user_id)`) — this satisfies the design's CURATION item 5 requirement that fixture capture happen against real docker-pg before this task closes, including a parallel-worker plan.

`crates/dbc-ui/tests/fixtures/pg_explain_seq_scan.json` (estimated, `Filter`, no `Plans` array — a leaf node):
```json
[
  {
    "Plan": {
      "Node Type": "Seq Scan",
      "Parallel Aware": false,
      "Async Capable": false,
      "Relation Name": "users",
      "Alias": "users",
      "Startup Cost": 0.00,
      "Total Cost": 377.00,
      "Plan Rows": 250,
      "Plan Width": 17,
      "Filter": "(age = 42)"
    }
  }
]
```

`crates/dbc-ui/tests/fixtures/pg_explain_estimated_hash_join.json` (estimated — proves `"Planning Time"`/`"Execution Time"` are ABSENT for a plain estimated plan, per design §1a's version note):
```json
[
  {
    "Plan": {
      "Node Type": "Hash Join",
      "Join Type": "Inner",
      "Startup Cost": 577.00,
      "Total Cost": 1499.25,
      "Plan Rows": 9999,
      "Plan Width": 13,
      "Hash Cond": "(o.user_id = u.id)",
      "Plans": [
        {
          "Node Type": "Seq Scan",
          "Parent Relationship": "Outer",
          "Relation Name": "orders",
          "Alias": "o",
          "Startup Cost": 0.00,
          "Total Cost": 896.00,
          "Plan Rows": 9999,
          "Plan Width": 8,
          "Filter": "(amount > '400'::numeric)"
        },
        {
          "Node Type": "Hash",
          "Parent Relationship": "Inner",
          "Startup Cost": 327.00,
          "Total Cost": 327.00,
          "Plan Rows": 20000,
          "Plan Width": 13,
          "Plans": [
            {
              "Node Type": "Seq Scan",
              "Parent Relationship": "Outer",
              "Relation Name": "users",
              "Alias": "u",
              "Startup Cost": 0.00,
              "Total Cost": 327.00,
              "Plan Rows": 20000,
              "Plan Width": 13
            }
          ]
        }
      ]
    }
  }
]
```

`crates/dbc-ui/tests/fixtures/pg_explain_index_scan_analyze.json` (ANALYZE+BUFFERS — bitmap index scan, proves buffer stats + the `"Planning"` sibling key that is NOT parsed into any typed field):
```json
[
  {
    "Plan": {
      "Node Type": "Bitmap Heap Scan",
      "Relation Name": "orders",
      "Alias": "orders",
      "Startup Cost": 4.31,
      "Total Cost": 15.40,
      "Plan Rows": 3,
      "Plan Width": 12,
      "Actual Startup Time": 0.024,
      "Actual Total Time": 0.034,
      "Actual Rows": 3,
      "Actual Loops": 1,
      "Recheck Cond": "(user_id = 5)",
      "Shared Hit Blocks": 3,
      "Shared Read Blocks": 2,
      "Shared Dirtied Blocks": 0,
      "Shared Written Blocks": 0,
      "Temp Read Blocks": 0,
      "Temp Written Blocks": 0,
      "Plans": [
        {
          "Node Type": "Bitmap Index Scan",
          "Parent Relationship": "Outer",
          "Index Name": "idx_orders_user",
          "Startup Cost": 0.00,
          "Total Cost": 4.31,
          "Plan Rows": 3,
          "Plan Width": 0,
          "Actual Startup Time": 0.018,
          "Actual Total Time": 0.019,
          "Actual Rows": 3,
          "Actual Loops": 1,
          "Index Cond": "(user_id = 5)",
          "Shared Hit Blocks": 0,
          "Shared Read Blocks": 2,
          "Shared Dirtied Blocks": 0,
          "Shared Written Blocks": 0,
          "Temp Read Blocks": 0,
          "Temp Written Blocks": 0
        }
      ]
    },
    "Planning": {
      "Shared Hit Blocks": 79,
      "Shared Read Blocks": 1
    },
    "Planning Time": 0.281,
    "Triggers": [],
    "Execution Time": 0.068
  }
]
```

`crates/dbc-ui/tests/fixtures/pg_explain_hash_join_analyze.json` (ANALYZE+BUFFERS — proves multi-child ordering AND `"Rows Removed by Filter"`):
```json
[
  {
    "Plan": {
      "Node Type": "Hash Join",
      "Join Type": "Inner",
      "Startup Cost": 577.00,
      "Total Cost": 1499.25,
      "Plan Rows": 9999,
      "Plan Width": 13,
      "Actual Startup Time": 5.140,
      "Actual Total Time": 12.108,
      "Actual Rows": 9900,
      "Actual Loops": 1,
      "Hash Cond": "(o.user_id = u.id)",
      "Shared Hit Blocks": 398,
      "Shared Read Blocks": 0,
      "Shared Dirtied Blocks": 0,
      "Shared Written Blocks": 0,
      "Temp Read Blocks": 0,
      "Temp Written Blocks": 0,
      "Plans": [
        {
          "Node Type": "Seq Scan",
          "Parent Relationship": "Outer",
          "Relation Name": "orders",
          "Alias": "o",
          "Startup Cost": 0.00,
          "Total Cost": 896.00,
          "Plan Rows": 9999,
          "Plan Width": 8,
          "Actual Startup Time": 0.047,
          "Actual Total Time": 5.247,
          "Actual Rows": 9900,
          "Actual Loops": 1,
          "Filter": "(amount > '400'::numeric)",
          "Rows Removed by Filter": 40100,
          "Shared Hit Blocks": 271,
          "Shared Read Blocks": 0,
          "Shared Dirtied Blocks": 0,
          "Shared Written Blocks": 0,
          "Temp Read Blocks": 0,
          "Temp Written Blocks": 0
        },
        {
          "Node Type": "Hash",
          "Parent Relationship": "Inner",
          "Startup Cost": 327.00,
          "Total Cost": 327.00,
          "Plan Rows": 20000,
          "Plan Width": 13,
          "Actual Startup Time": 4.997,
          "Actual Total Time": 4.998,
          "Actual Rows": 20000,
          "Actual Loops": 1,
          "Hash Buckets": 32768,
          "Peak Memory Usage": 1190,
          "Shared Hit Blocks": 127,
          "Shared Read Blocks": 0,
          "Shared Dirtied Blocks": 0,
          "Shared Written Blocks": 0,
          "Temp Read Blocks": 0,
          "Temp Written Blocks": 0,
          "Plans": [
            {
              "Node Type": "Seq Scan",
              "Parent Relationship": "Outer",
              "Relation Name": "users",
              "Alias": "u",
              "Startup Cost": 0.00,
              "Total Cost": 327.00,
              "Plan Rows": 20000,
              "Plan Width": 13,
              "Actual Startup Time": 0.003,
              "Actual Total Time": 1.706,
              "Actual Rows": 20000,
              "Actual Loops": 1,
              "Shared Hit Blocks": 127,
              "Shared Read Blocks": 0,
              "Shared Dirtied Blocks": 0,
              "Shared Written Blocks": 0,
              "Temp Read Blocks": 0,
              "Temp Written Blocks": 0
            }
          ]
        }
      ]
    },
    "Planning": { "Shared Hit Blocks": 219, "Shared Read Blocks": 1 },
    "Planning Time": 0.488,
    "Triggers": [],
    "Execution Time": 12.396
  }
]
```

`crates/dbc-ui/tests/fixtures/pg_explain_parallel_analyze.json` (ANALYZE+BUFFERS with forced parallel workers — proves `"Workers Launched"` (on the `Gather` node) folds into `extra`, and `"Actual Loops": 5` on its child confirms the per-loop-averaging story `self_time_ms` relies on):
```json
[
  {
    "Plan": {
      "Node Type": "Aggregate",
      "Strategy": "Plain",
      "Partial Mode": "Finalize",
      "Startup Cost": 452.20,
      "Total Cost": 452.21,
      "Plan Rows": 1,
      "Plan Width": 8,
      "Actual Startup Time": 4.548,
      "Actual Total Time": 6.181,
      "Actual Rows": 1,
      "Actual Loops": 1,
      "Shared Hit Blocks": 271,
      "Shared Read Blocks": 0,
      "Shared Dirtied Blocks": 0,
      "Shared Written Blocks": 0,
      "Temp Read Blocks": 0,
      "Temp Written Blocks": 0,
      "Plans": [
        {
          "Node Type": "Gather",
          "Parent Relationship": "Outer",
          "Startup Cost": 452.19,
          "Total Cost": 452.19,
          "Plan Rows": 4,
          "Plan Width": 8,
          "Actual Startup Time": 4.497,
          "Actual Total Time": 6.175,
          "Actual Rows": 5,
          "Actual Loops": 1,
          "Workers Planned": 4,
          "Workers Launched": 4,
          "Single Copy": false,
          "Shared Hit Blocks": 271,
          "Shared Read Blocks": 0,
          "Shared Dirtied Blocks": 0,
          "Shared Written Blocks": 0,
          "Temp Read Blocks": 0,
          "Temp Written Blocks": 0,
          "Plans": [
            {
              "Node Type": "Aggregate",
              "Strategy": "Plain",
              "Partial Mode": "Partial",
              "Parent Relationship": "Outer",
              "Startup Cost": 452.19,
              "Total Cost": 452.19,
              "Plan Rows": 1,
              "Plan Width": 8,
              "Actual Startup Time": 1.839,
              "Actual Total Time": 1.840,
              "Actual Rows": 1,
              "Actual Loops": 5,
              "Shared Hit Blocks": 271,
              "Shared Read Blocks": 0,
              "Shared Dirtied Blocks": 0,
              "Shared Written Blocks": 0,
              "Temp Read Blocks": 0,
              "Temp Written Blocks": 0,
              "Plans": [
                {
                  "Node Type": "Seq Scan",
                  "Parent Relationship": "Outer",
                  "Parallel Aware": true,
                  "Relation Name": "orders",
                  "Alias": "orders",
                  "Startup Cost": 0.00,
                  "Total Cost": 427.25,
                  "Plan Rows": 9974,
                  "Plan Width": 0,
                  "Actual Startup Time": 0.011,
                  "Actual Total Time": 1.438,
                  "Actual Rows": 7980,
                  "Actual Loops": 5,
                  "Filter": "(amount > '100'::numeric)",
                  "Rows Removed by Filter": 2020,
                  "Shared Hit Blocks": 271,
                  "Shared Read Blocks": 0,
                  "Shared Dirtied Blocks": 0,
                  "Shared Written Blocks": 0,
                  "Temp Read Blocks": 0,
                  "Temp Written Blocks": 0
                }
              ]
            }
          ]
        }
      ]
    },
    "Planning": { "Shared Hit Blocks": 76, "Shared Read Blocks": 2 },
    "Planning Time": 0.322,
    "Triggers": [],
    "Execution Time": 6.222
  }
]
```

**Grounding — the intermediate deserialize target and iterative conversion** (verified during drafting: compiles, parses the real fixtures above correctly including multi-child ordering, and the 128-level `serde_json` recursion cap was confirmed to reject a synthetic 1,000-level payload with a clean `Err`, never a panic):

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PgPlanJson {
    #[serde(rename = "Node Type")]
    node_type: String,
    #[serde(rename = "Relation Name")]
    relation_name: Option<String>,
    #[serde(rename = "Index Name")]
    index_name: Option<String>,
    #[serde(rename = "Alias")]
    alias: Option<String>,
    #[serde(rename = "Total Cost")]
    total_cost: Option<f64>,
    #[serde(rename = "Plan Rows")]
    plan_rows: Option<f64>,
    #[serde(rename = "Actual Total Time")]
    actual_total_time: Option<f64>,
    #[serde(rename = "Actual Rows")]
    actual_rows: Option<f64>,
    #[serde(rename = "Actual Loops")]
    actual_loops: Option<u64>,
    #[serde(rename = "Rows Removed by Filter")]
    rows_removed_by_filter: Option<f64>,
    #[serde(rename = "Rows Removed by Join Filter")]
    rows_removed_by_join_filter: Option<f64>,
    #[serde(rename = "Shared Hit Blocks")]
    shared_hit_blocks: Option<u64>,
    #[serde(rename = "Shared Read Blocks")]
    shared_read_blocks: Option<u64>,
    #[serde(rename = "Shared Dirtied Blocks")]
    shared_dirtied_blocks: Option<u64>,
    #[serde(rename = "Shared Written Blocks")]
    shared_written_blocks: Option<u64>,
    #[serde(rename = "Temp Read Blocks")]
    temp_read_blocks: Option<u64>,
    #[serde(rename = "Temp Written Blocks")]
    temp_written_blocks: Option<u64>,
    #[serde(rename = "Plans")]
    plans: Option<Vec<PgPlanJson>>,
    /// Catches everything not named above (`"Startup Cost"`, `"Plan
    /// Width"`, `"Filter"`, `"Join Type"`, `"Workers Launched"`, ...) —
    /// this is what keeps `PlanNode.extra` complete without hand-listing
    /// pg's ~40 keys (design §3).
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct PgExplainRoot {
    #[serde(rename = "Plan")]
    plan: PgPlanJson,
    #[serde(rename = "Planning Time")]
    planning_time: Option<f64>,
    #[serde(rename = "Execution Time")]
    execution_time: Option<f64>,
    // `"Planning"` (buffer stats DURING planning, distinct from `"Planning
    // Time"`) and `"Triggers"` are present in real ANALYZE output but are
    // NOT parsed into any typed field (design §1a: Triggers folds into
    // `raw_text` only) — serde ignores unrecognized fields by default, so
    // no explicit handling is needed here; `raw_text` (the original,
    // untouched JSON string) is what preserves them for the UI's raw
    // toggle.
}

fn extra_from_map(map: serde_json::Map<String, serde_json::Value>) -> Vec<(String, String)> {
    map.into_iter().map(|(k, v)| (k, v.to_string())).collect()
}

/// Iterative conversion (Global Constraints "Deep recursive tree hazard"):
/// an explicit frame stack, never a self-calling function. Each `Frame`
/// owns one node's still-pending `PgPlanJson` children (popped from the
/// back, having been `reverse()`d first so pop-order matches the original
/// `"Plans"` array order) and the `PlanNode` children already converted.
fn convert_pg_tree(root: PgPlanJson) -> PlanNode {
    struct Frame {
        node_type: String,
        target: Option<String>,
        est_cost: Option<f64>,
        est_rows: Option<f64>,
        actual_rows: Option<f64>,
        actual_time_ms: Option<f64>,
        loops: Option<u64>,
        rows_removed_by_filter: Option<f64>,
        buffers: Option<BufferStats>,
        extra: Vec<(String, String)>,
        pending: Vec<PgPlanJson>,
        done: Vec<PlanNode>,
    }

    fn buffers_of(j: &PgPlanJson) -> Option<BufferStats> {
        let any = j.shared_hit_blocks.is_some()
            || j.shared_read_blocks.is_some()
            || j.shared_dirtied_blocks.is_some()
            || j.shared_written_blocks.is_some()
            || j.temp_read_blocks.is_some()
            || j.temp_written_blocks.is_some();
        any.then(|| BufferStats {
            shared_hit: j.shared_hit_blocks,
            shared_read: j.shared_read_blocks,
            shared_dirtied: j.shared_dirtied_blocks,
            shared_written: j.shared_written_blocks,
            temp_read: j.temp_read_blocks,
            temp_written: j.temp_written_blocks,
        })
    }

    fn make_frame(mut j: PgPlanJson) -> Frame {
        let target = j.relation_name.clone().or_else(|| j.index_name.clone()).or_else(|| j.alias.clone());
        let rrf = match (j.rows_removed_by_filter, j.rows_removed_by_join_filter) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        let buffers = buffers_of(&j);
        let mut pending = j.plans.take().unwrap_or_default();
        pending.reverse(); // pop() takes from the end -> preserves original order
        Frame {
            node_type: std::mem::take(&mut j.node_type),
            target,
            est_cost: finite(j.total_cost),
            est_rows: finite(j.plan_rows),
            actual_rows: finite(j.actual_rows),
            actual_time_ms: finite(j.actual_total_time),
            loops: j.actual_loops,
            rows_removed_by_filter: finite(rrf),
            buffers,
            extra: extra_from_map(std::mem::take(&mut j.extra)),
            pending,
            done: Vec::new(),
        }
    }

    let mut stack: Vec<Frame> = vec![make_frame(root)];
    loop {
        let top = stack.last_mut().expect("stack never empty inside loop");
        if let Some(child_json) = top.pending.pop() {
            stack.push(make_frame(child_json));
            continue;
        }
        let frame = stack.pop().expect("just checked non-empty");
        let node = PlanNode {
            operation: frame.node_type,
            target: frame.target,
            est_cost: frame.est_cost,
            est_rows: frame.est_rows,
            actual_rows: frame.actual_rows,
            actual_time_ms: frame.actual_time_ms,
            loops: frame.loops,
            rows_removed_by_filter: frame.rows_removed_by_filter,
            buffers: frame.buffers,
            extra: frame.extra,
            children: frame.done,
        };
        match stack.last_mut() {
            Some(parent) => parent.done.push(node),
            None => return node,
        }
    }
}

pub fn parse_pg_json(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    let roots: Vec<PgExplainRoot> =
        serde_json::from_str(raw_text).map_err(|e| format!("neplatný JSON plánu: {e}"))?;
    let root_json = roots.into_iter().next().ok_or_else(|| "prázdné pole v odpovědi EXPLAIN".to_string())?;
    let planning_time = finite(root_json.planning_time);
    let execution_time = finite(root_json.execution_time);
    Ok(PlanResult {
        root: convert_pg_tree(root_json.plan),
        is_analyze,
        engine: dbc_state::Engine::Postgres,
        total_planning_time_ms: planning_time,
        total_execution_time_ms: execution_time,
        top_level_hints: Vec::new(), // pg has no engine-provided hints, design §1a
        raw_text: raw_text.to_string(),
    })
}

pub fn parse_plan(engine: dbc_state::Engine, is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    match engine {
        dbc_state::Engine::Postgres => parse_pg_json(is_analyze, raw_text),
        dbc_state::Engine::Mssql => parse_mssql_xml(is_analyze, raw_text), // T3
        dbc_state::Engine::Sqlite => Err(
            "parse_plan: SQLite plans are row-shaped — call parse_sqlite_rows directly (see plan.rs's parser entry point doc)".to_string(),
        ),
    }
}
```

- [ ] **Step 1: Add the dependencies**

```toml
# crates/dbc-ui/Cargo.toml [dependencies]:
serde_json.workspace = true
# [dev-dependencies]:
testcontainers-modules = { version = "0.13", features = ["postgres"] }
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui --tests`
Expected: builds clean.

- [ ] **Step 2: Create the fixture files** exactly as captured above, under `crates/dbc-ui/tests/fixtures/`.

- [ ] **Step 3: Write the failing tests** (`crates/dbc-ui/src/plan.rs`, `#[cfg(test)] mod pg_parser_tests`):

```rust
#[cfg(test)]
mod pg_parser_tests {
    use super::*;

    #[test]
    fn seq_scan_estimated_leaf() {
        let raw = include_str!("../tests/fixtures/pg_explain_seq_scan.json");
        let result = parse_pg_json(false, raw).expect("parse");
        assert_eq!(result.root.operation, "Seq Scan");
        assert_eq!(result.root.target.as_deref(), Some("users"));
        assert_eq!(result.root.est_cost, Some(377.0));
        assert_eq!(result.root.est_rows, Some(250.0));
        assert_eq!(result.total_planning_time_ms, None);
        assert_eq!(result.total_execution_time_ms, None);
        assert!(result.root.buffers.is_none());
        assert!(result.root.extra.iter().any(|(k, _)| k == "Filter"));
        assert!(result.root.children.is_empty());
        assert_eq!(result.raw_text, raw);
    }

    #[test]
    fn estimated_plan_has_no_planning_or_execution_time() {
        let raw = include_str!("../tests/fixtures/pg_explain_estimated_hash_join.json");
        let result = parse_pg_json(false, raw).expect("parse");
        assert_eq!(result.total_planning_time_ms, None);
        assert_eq!(result.total_execution_time_ms, None);
        assert_eq!(result.root.actual_time_ms, None);
    }

    #[test]
    fn index_scan_analyze_has_buffers_and_planning_execution_times() {
        let raw = include_str!("../tests/fixtures/pg_explain_index_scan_analyze.json");
        let result = parse_pg_json(true, raw).expect("parse");
        assert_eq!(result.root.operation, "Bitmap Heap Scan");
        assert_eq!(result.total_planning_time_ms, Some(0.281));
        assert_eq!(result.total_execution_time_ms, Some(0.068));
        let buffers = result.root.buffers.as_ref().expect("buffers present under ANALYZE, BUFFERS");
        assert_eq!(buffers.shared_hit, Some(3));
        assert_eq!(buffers.shared_read, Some(2));
        assert_eq!(result.root.children.len(), 1);
        assert_eq!(result.root.children[0].operation, "Bitmap Index Scan");
        assert_eq!(result.root.children[0].target.as_deref(), Some("idx_orders_user"));
    }

    #[test]
    fn hash_join_analyze_preserves_child_order_and_rows_removed() {
        let raw = include_str!("../tests/fixtures/pg_explain_hash_join_analyze.json");
        let result = parse_pg_json(true, raw).expect("parse");
        assert_eq!(result.root.operation, "Hash Join");
        assert_eq!(result.root.children.len(), 2);
        assert_eq!(result.root.children[0].operation, "Seq Scan");
        assert_eq!(result.root.children[0].target.as_deref(), Some("orders"));
        assert_eq!(result.root.children[0].rows_removed_by_filter, Some(40100.0));
        assert_eq!(result.root.children[1].operation, "Hash");
        assert_eq!(result.root.children[1].children.len(), 1);
        assert_eq!(result.root.children[1].children[0].target.as_deref(), Some("users"));
        assert!(result.total_execution_time_ms.unwrap() > 0.0);
    }

    /// CURATION item 5: parallel-worker plan, `Workers Launched > 0`.
    #[test]
    fn parallel_workers_launched_folds_into_extra_on_gather_node() {
        let raw = include_str!("../tests/fixtures/pg_explain_parallel_analyze.json");
        let result = parse_pg_json(true, raw).expect("parse");
        assert_eq!(result.root.operation, "Aggregate");
        let gather = &result.root.children[0];
        assert_eq!(gather.operation, "Gather");
        let workers_launched = gather.extra.iter().find(|(k, _)| k == "Workers Launched");
        assert_eq!(workers_launched, Some(&("Workers Launched".to_string(), "4".to_string())));
        // Per-loop-averaging: the per-worker Aggregate node ran 5 times
        // (4 workers + leader) — self_time_ms must multiply by loops.
        let per_worker_agg = &gather.children[0];
        assert_eq!(per_worker_agg.loops, Some(5));
        assert!(self_time_ms(per_worker_agg) >= 0.0);
    }

    #[test]
    fn malformed_json_is_err_not_panic() {
        assert!(parse_pg_json(false, "{ not json").is_err());
        assert!(parse_pg_json(false, "[]").is_err()); // empty array
        assert!(parse_pg_json(false, "[{}]").is_err()); // missing "Plan" key
    }

    /// Deep-nesting guard: serde_json's own recursion limit (128, confirmed
    /// against the pinned 1.0.151 source) must reject pathological input
    /// with a clean Err, never a panic.
    #[test]
    fn pathologically_deep_json_fails_closed() {
        let mut deep = String::new();
        for _ in 0..1000 {
            deep.push_str(r#"{"Plan":{"Node Type":"X","Plans":["#);
        }
        deep.push_str(r#"{"Node Type":"Leaf"}"#);
        for _ in 0..1000 {
            deep.push_str("]}}");
        }
        let wrapped = format!("[{deep}]");
        let err = parse_pg_json(false, &wrapped).unwrap_err();
        assert!(err.contains("neplatný JSON"), "expected a parse error, got: {err}");
    }

    #[test]
    fn parse_plan_dispatches_postgres_and_refuses_sqlite() {
        let raw = include_str!("../tests/fixtures/pg_explain_seq_scan.json");
        assert!(parse_plan(dbc_state::Engine::Postgres, false, raw).is_ok());
        assert!(parse_plan(dbc_state::Engine::Sqlite, false, raw).is_err());
    }
}
```

- [ ] **Step 4: Run to see it fail, then implement, then green**

Run (before implementing): `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui pg_parser_tests::` — expect compile errors.
Implement `PgPlanJson`/`PgExplainRoot`/`extra_from_map`/`convert_pg_tree`/`parse_pg_json`/`parse_plan` exactly per the grounding code (the `parse_mssql_xml` branch inside `parse_plan` will not compile until T3 lands — if T2 merges before T3, stub it: `dbc_state::Engine::Mssql => Err("MSSQL parser not yet available (T3)".to_string()),` and T3 replaces that arm).
Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui pg_parser_tests::`
Expected: all pass, zero warnings.

- [ ] **Step 5: Docker-gated live validation** (`crates/dbc-ui/src/plan.rs`, new `#[cfg(test)] mod pg_docker_tests` — `dbc-ui` is a binary crate with no lib target, so this lives in-crate, not under `tests/`, mirroring the G9 plan's T7 precedent; connections go through `runner::open_spec`/`ConnectSpec::Url`, NOT `connect::open` directly, since `open`'s Postgres arm calls `runtime.block_on`, which panics on a `#[tokio::test]` worker):

```rust
/// G13 T2: docker-gated proof that a LIVE server's real EXPLAIN output
/// round-trips through `runner::connect_and_run` -> a drained
/// `dbc_buffer::ResultBuffer` -> `parse_pg_json`, end to end — not just the
/// static JSON shape the fixture-based tests above already cover. Docker
/// required. Run with:
///   %USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored pg_docker_tests::
#[cfg(test)]
mod pg_docker_tests {
    use super::*;
    use crate::runner::{ConnectSpec, QueryRunner};
    use dbc_core::CancelToken;
    use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

    async fn pg_url(node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>) -> String {
        format!("postgres://postgres:postgres@127.0.0.1:{}/postgres", node.get_host_port_ipv4(5432).await.unwrap())
    }

    /// Drains one `QueryEvent` stream fully into a single text cell (the
    /// shape `runner.rs` will use for the real Explain/Analyze dispatch in
    /// T6) — kept local to this test module so T2 doesn't depend on T6's
    /// not-yet-written UI dispatch code.
    async fn run_and_capture_single_cell(runner: &QueryRunner, spec: ConnectSpec, sql: String) -> String {
        let mut rx = runner.connect_and_run(spec, sql, CancelToken::new(), None);
        let mut buffer: Option<dbc_buffer::ResultBuffer> = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                crate::runner::QueryEvent::Started { columns } => {
                    buffer = Some(dbc_buffer::ResultBuffer::new(columns));
                }
                crate::runner::QueryEvent::Batch(b) => {
                    buffer.as_mut().expect("Started before Batch").push(b).expect("push");
                }
                crate::runner::QueryEvent::Finished { .. } => break,
                crate::runner::QueryEvent::Failed(e) => panic!("query failed: {e}"),
            }
        }
        let buf = buffer.expect("Started must have fired");
        assert_eq!(buf.row_count(), 1, "EXPLAIN (FORMAT JSON) always returns exactly one row");
        buf.cell_text(0, 0)
    }

    #[tokio::test]
    #[ignore]
    async fn live_parallel_explain_analyze_round_trips_through_parser() {
        let node = Postgres::default().start().await.unwrap();
        let url = pg_url(&node).await;
        let runner = QueryRunner::new();

        let setup_sql = "\
CREATE TABLE orders(id INT PRIMARY KEY, amount NUMERIC);
INSERT INTO orders SELECT g, (g % 500) FROM generate_series(1, 50000) g;
SET max_parallel_workers_per_gather = 4;
SET parallel_setup_cost = 0;
SET parallel_tuple_cost = 0;
SET min_parallel_table_scan_size = 0;"
            .to_string();
        // Multi-statement setup — connect_and_run runs one statement; drive
        // each piece separately over the SAME url (autocommit DDL/DML is
        // fine to split across connections here, unlike a transaction).
        for stmt in setup_sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            run_and_capture_single_cell_ignore_shape(&runner, ConnectSpec::Url(url.clone()), stmt.to_string()).await;
        }

        let explain = crate::plan::explain_analyze_sql(dbc_state::Engine::Postgres, "SELECT count(*) FROM orders WHERE amount > 100").unwrap();
        let raw = run_and_capture_single_cell(&runner, ConnectSpec::Url(url.clone()), explain).await;
        let result = parse_pg_json(true, &raw).expect("live plan must parse");
        assert!(result.total_execution_time_ms.unwrap() >= 0.0);
        // Assert SOMEWHERE in the tree a node reports Workers Launched > 0
        // in extra — proves the live round trip, not just the static fixture.
        fn any_worker_launch(n: &PlanNode) -> bool {
            n.extra.iter().any(|(k, v)| k == "Workers Launched" && v.parse::<i64>().unwrap_or(0) > 0)
                || n.children.iter().any(any_worker_launch)
        }
        assert!(any_worker_launch(&result.root), "expected a Gather node with Workers Launched > 0: {result:?}");
    }

    /// Helper for setup statements that return no rows (DDL/SET) — doesn't
    /// assert row_count, just drains to completion or panics on failure.
    async fn run_and_capture_single_cell_ignore_shape(runner: &QueryRunner, spec: ConnectSpec, sql: String) {
        let mut rx = runner.connect_and_run(spec, sql, CancelToken::new(), None);
        while let Some(ev) = rx.recv().await {
            if let crate::runner::QueryEvent::Failed(e) = ev {
                panic!("setup statement failed: {e}");
            }
        }
    }
}
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui pg_docker_tests::`
Expected: "1 ignored", zero failures (nothing runs without `--ignored`).
Run (docker up): `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored pg_docker_tests::`
Expected: passes. First run is slow if the postgres image needs pulling — that's the container, not the code.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/plan.rs crates/dbc-ui/Cargo.toml crates/dbc-ui/tests/fixtures/pg_explain_*.json
git commit -m "feat: Postgres EXPLAIN JSON parser, docker-validated fixtures (G13 T2)"
```

---

### Task 3 (T3): MSSQL parser — `parse_mssql_xml` (dead code until the driver phase)

**Files:**
- Modify: `crates/dbc-ui/src/plan.rs` (add `parse_mssql_xml` + helpers, `#[allow(dead_code)]` module-level note, tests)
- Modify: `crates/dbc-ui/Cargo.toml` (add `quick-xml = "0.41"` to `[dependencies]` — already resolves to `0.41.0` in `Cargo.lock` transitively, so this pin introduces no new version)
- Create: `crates/dbc-ui/tests/fixtures/mssql_showplan_estimated.xml`, `mssql_showplan_analyze.xml`, `mssql_showplan_missing_index.xml`

**Interfaces:**
- Consumes: `PlanNode`/`PlanResult`/`BufferStats`/`PlanHint`/`finite` (T1).
- Produces (consumed by T2's `parse_plan` dispatcher, and eventually T7):

```rust
/// **needs-verification** (design §1b/§6/§7): every attribute name and the
/// result-set delivery mechanics below are best-effort from Microsoft's
/// published Showplan XML documentation — no live MSSQL server or driver
/// exists to capture real output against yet (dbc-ui's `connect::open_config`
/// hard-errors `Engine::Mssql` today). Correct against real captures once
/// the MSSQL driver phase lands (T7).
pub fn parse_mssql_xml(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String>;
```

**Grounding — iterative `<RelOp>` walk** (design §1b: "the parser walks ANY descendant `<RelOp>` element... stopping recursion at the next `<RelOp>` boundary" — implemented here as an explicit frame stack over `quick-xml`'s own already-iterative `Reader`/`Event` token stream, verified during drafting to compile against `quick-xml` 0.41's real API — `Attribute::normalized_value(XmlVersion)`, not the deprecated `decode_and_unescape_value`/`unescape_value` — and to correctly parse a hand-authored nested-`<RelOp>` XML sample, including a depth-guard rejection of a synthetic 5,010-level-deep payload):

```rust
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use quick_xml::XmlVersion;

/// Defensive cap, independent of anything `quick-xml` enforces itself
/// (unlike `serde_json`, it has no built-in recursion-limit equivalent) —
/// converts a pathological/adversarial payload into a clean `Err`, not
/// unbounded memory growth or a stack issue elsewhere in the pipeline.
const MAX_XML_DEPTH: usize = 5000;

fn attr_string(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name.as_bytes())
        .and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok().map(|c| c.into_owned()))
}

fn leaf_node(operation: String, target: Option<String>, est_cost: Option<f64>, est_rows: Option<f64>) -> PlanNode {
    PlanNode {
        operation,
        target,
        est_cost,
        est_rows,
        actual_rows: None,
        actual_time_ms: None,
        loops: None,
        rows_removed_by_filter: None,
        buffers: None,
        extra: Vec::new(),
        children: Vec::new(),
    }
}

struct RelOpFrame {
    node: PlanNode,
    // Set once, from the FIRST `<RunTimeCountersPerThread>` seen while this
    // frame is open — see the aggregation note below.
    actual_rows_sum: f64,
    actual_ms_max: f64,
    has_runtime_counters: bool,
}

/// Walks the whole document once, iteratively, tracking three independent
/// pieces of state as events arrive: (a) the `<RelOp>` frame stack (builds
/// the tree), (b) whether we're inside `<Object .../>` (sets the current
/// frame's `target`), (c) `<MissingIndexGroup>` accumulation (top-level
/// hints, unrelated to any one `RelOp`, per design §1b — attached to
/// `PlanResult.top_level_hints`, not any node).
fn parse_mssql_xml(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    let mut reader = Reader::from_str(raw_text);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<RelOpFrame> = Vec::new();
    let mut root: Option<PlanNode> = None;
    let mut depth: usize = 0;

    let mut hints: Vec<PlanHint> = Vec::new();
    let mut cur_hint: Option<(f64, String, String, String, Vec<String>)> = None; // (impact, db, schema, table, columns)

    loop {
        match reader.read_event().map_err(|e| format!("chyba XML plánu: {e}"))? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let is_empty = matches!(reader.read_event(), Ok(Event::Eof)); // placeholder never used; see below
                let _ = is_empty;
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(format!("XML plán překročil maximální hloubku {MAX_XML_DEPTH}"));
                }
                match e.name().as_ref() {
                    b"RelOp" => {
                        let operation = attr_string(&e, "PhysicalOp")
                            .or_else(|| attr_string(&e, "LogicalOp"))
                            .unwrap_or_else(|| "?".to_string());
                        let est_cost = finite(attr_string(&e, "EstimatedTotalSubtreeCost").and_then(|s| s.parse().ok()));
                        let est_rows = finite(attr_string(&e, "EstimateRows").and_then(|s| s.parse().ok()));
                        stack.push(RelOpFrame {
                            node: leaf_node(operation, None, est_cost, est_rows),
                            actual_rows_sum: 0.0,
                            actual_ms_max: 0.0,
                            has_runtime_counters: false,
                        });
                    }
                    b"Object" => {
                        if let Some(top) = stack.last_mut() {
                            if top.node.target.is_none() {
                                top.node.target = attr_string(&e, "Table").or_else(|| attr_string(&e, "Index"));
                            }
                        }
                    }
                    b"RunTimeCountersPerThread" if is_analyze => {
                        if let Some(top) = stack.last_mut() {
                            let rows = attr_string(&e, "ActualRows").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                            let ms = attr_string(&e, "ActualElapsedms").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                            top.actual_rows_sum += rows;
                            top.actual_ms_max = top.actual_ms_max.max(ms);
                            top.has_runtime_counters = true;
                        }
                    }
                    b"MissingIndexGroup" => {
                        let impact = attr_string(&e, "Impact").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        cur_hint = Some((impact, String::new(), String::new(), String::new(), Vec::new()));
                    }
                    b"MissingIndex" => {
                        if let Some((_, db, schema, table, _)) = cur_hint.as_mut() {
                            *db = attr_string(&e, "Database").unwrap_or_default();
                            *schema = attr_string(&e, "Schema").unwrap_or_default();
                            *table = attr_string(&e, "Table").unwrap_or_default();
                        }
                    }
                    b"Column" => {
                        if let Some((_, _, _, _, cols)) = cur_hint.as_mut() {
                            if let Some(name) = attr_string(&e, "Name") {
                                cols.push(name);
                            }
                        }
                    }
                    _ => {}
                }
                // `Event::Empty` (self-closing) never gets a matching
                // `Event::End` — close whatever this element opened
                // immediately for the two element kinds that can be
                // self-closing in practice (`<RelOp .../>` with no
                // children, `<Column .../>`); `depth` is corrected to
                // match. `Object`/`MissingIndex`/`MissingIndexGroup`/
                // `RunTimeCountersPerThread` are always self-closing or
                // handled entirely on Start, so no separate End-side logic
                // is needed for them beyond RelOp's own closure below.
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                if e.name().as_ref() == b"RelOp" {
                    let mut frame = stack.pop().ok_or_else(|| "nepárový </RelOp>".to_string())?;
                    if is_analyze && frame.has_runtime_counters {
                        frame.node.actual_rows = finite(Some(frame.actual_rows_sum));
                        frame.node.actual_time_ms = finite(Some(frame.actual_ms_max));
                        frame.node.loops = Some(1); // per-thread sums/max are already whole-node totals
                    }
                    match stack.last_mut() {
                        Some(parent) => parent.node.children.push(frame.node),
                        None => root = Some(frame.node),
                    }
                } else if e.name().as_ref() == b"MissingIndexGroup" {
                    if let Some((impact, db, schema, table, cols)) = cur_hint.take() {
                        let col_list = cols.join(", ");
                        hints.push(PlanHint {
                            message: format!("Chybějící index: dopad {impact:.1}%"),
                            detail: Some(format!(
                                "-- návrh, ověřte před spuštěním:\nCREATE INDEX ix_suggested ON {db}.{schema}.{table} ({col_list});"
                            )),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    if !stack.is_empty() {
        return Err("neuzavřené <RelOp> elementy (neplatné XML)".to_string());
    }
    let root = root.ok_or_else(|| "v XML plánu nebyl nalezen žádný <RelOp>".to_string())?;
    Ok(PlanResult {
        root,
        is_analyze,
        engine: dbc_state::Engine::Mssql,
        total_planning_time_ms: None, // MSSQL Showplan XML carries no separate planning-time figure
        total_execution_time_ms: None, // no single top-level total in STATISTICS XML — per-node only (design §2 fallback)
        top_level_hints: hints,
        raw_text: raw_text.to_string(),
    })
}
```

*(Implementer note, not a placeholder — a real fix required at Step 3: the `Event::Start(e) | Event::Empty(e)` combined match arm above cannot distinguish "self-closing, no matching End" from "opened, awaiting End" using one shared branch — the dead `is_empty` line is there to flag this explicitly. Split into two separate match arms — `Event::Start(e) => { ...push RelOpFrame only... }` and `Event::Empty(e) => { ...push then IMMEDIATELY pop-and-attach, exactly like a Start followed instantly by an End... }` — mirroring the two-branch shape already proven during drafting (a working version of exactly this split, for the `RelOp`-only case, compiled and passed against a hand-authored nested-`<RelOp>` fixture, including a self-closing `<RelOp .../>` leaf). Extend that proven two-branch shape to also cover `Object`/`MissingIndex`/`Column`'s self-closing form the same way. This is real, required Step-3 work, not optional polish — the combined-arm sketch above must not ship as-is.)*

`crates/dbc-ui/tests/fixtures/mssql_showplan_estimated.xml` (hand-authored from Microsoft's published Showplan XML schema samples; no live server available — flagged needs-verification):
```xml
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan">
  <BatchSequence><Batch><Statements><StmtSimple>
    <QueryPlan>
      <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="3" EstimatedTotalSubtreeCost="0.123">
        <NestedLoops>
          <RelOp NodeId="1" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.01">
            <IndexScan><Object Table="[dbo].[users]" Index="[PK_users]" Schema="[dbo]" /></IndexScan>
          </RelOp>
          <RelOp NodeId="2" PhysicalOp="Clustered Index Seek" LogicalOp="Clustered Index Seek" EstimateRows="3" EstimatedTotalSubtreeCost="0.02">
            <IndexScan><Object Table="[dbo].[orders]" Index="[PK_orders]" Schema="[dbo]" /></IndexScan>
          </RelOp>
        </NestedLoops>
      </RelOp>
    </QueryPlan>
  </StmtSimple></Statements></Batch></BatchSequence>
</ShowPlanXML>
```

`crates/dbc-ui/tests/fixtures/mssql_showplan_analyze.xml` (adds `<RunTimeInformation>`/`<RunTimeCountersPerThread>` per design §1b's `STATISTICS XML` shape):
```xml
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan">
  <BatchSequence><Batch><Statements><StmtSimple>
    <QueryPlan>
      <RelOp NodeId="0" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="1000" EstimatedTotalSubtreeCost="4.5">
        <IndexScan><Object Table="[dbo].[orders]" Index="[PK_orders]" Schema="[dbo]" /></IndexScan>
        <RunTimeInformation>
          <RunTimeCountersPerThread Thread="0" ActualRows="600" ActualElapsedms="12" ActualExecutions="1" />
          <RunTimeCountersPerThread Thread="1" ActualRows="400" ActualElapsedms="9" ActualExecutions="1" />
        </RunTimeInformation>
      </RelOp>
    </QueryPlan>
  </StmtSimple></Statements></Batch></BatchSequence>
</ShowPlanXML>
```

`crates/dbc-ui/tests/fixtures/mssql_showplan_missing_index.xml` (top-level `<MissingIndexes>`, sibling of `<QueryPlan>`, per design §1b):
```xml
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan">
  <BatchSequence><Batch><Statements><StmtSimple>
    <QueryPlan>
      <MissingIndexes>
        <MissingIndexGroup Impact="87.3">
          <MissingIndex Database="[app]" Schema="[dbo]" Table="[orders]">
            <ColumnGroup Usage="EQUALITY">
              <Column Name="[user_id]" ColumnId="2" />
            </ColumnGroup>
            <ColumnGroup Usage="INCLUDE">
              <Column Name="[amount]" ColumnId="3" />
            </ColumnGroup>
          </MissingIndex>
        </MissingIndexGroup>
      </MissingIndexes>
      <RelOp NodeId="0" PhysicalOp="Table Scan" LogicalOp="Table Scan" EstimateRows="50000" EstimatedTotalSubtreeCost="120.0">
        <TableScan><Object Table="[dbo].[orders]" Schema="[dbo]" /></TableScan>
      </RelOp>
    </QueryPlan>
  </StmtSimple></Statements></Batch></BatchSequence>
</ShowPlanXML>
```

- [ ] **Step 1: Add the dependency**

```toml
# crates/dbc-ui/Cargo.toml [dependencies]:
quick-xml = "0.41"
```

- [ ] **Step 2: Create the fixture files** exactly as above.

- [ ] **Step 3: Write the failing tests, then implement** the corrected (two-branch, per the implementer note above) `parse_mssql_xml` and its helpers:

```rust
#[cfg(test)]
mod mssql_parser_tests {
    use super::*;

    #[test]
    fn estimated_nested_loops_tree_shape_and_targets() {
        let raw = include_str!("../tests/fixtures/mssql_showplan_estimated.xml");
        let result = parse_mssql_xml(false, raw).expect("parse");
        assert_eq!(result.root.operation, "Nested Loops");
        assert_eq!(result.root.est_cost, Some(0.123));
        assert_eq!(result.root.children.len(), 2);
        assert_eq!(result.root.children[0].operation, "Index Seek");
        assert_eq!(result.root.children[0].target.as_deref(), Some("[dbo].[users]"));
        assert_eq!(result.root.children[1].target.as_deref(), Some("[dbo].[orders]"));
        assert!(result.top_level_hints.is_empty());
    }

    #[test]
    fn analyze_aggregates_runtime_counters_per_thread() {
        let raw = include_str!("../tests/fixtures/mssql_showplan_analyze.xml");
        let result = parse_mssql_xml(true, raw).expect("parse");
        // design §1b v1 aggregation: sum ActualRows, max ActualElapsedms.
        assert_eq!(result.root.actual_rows, Some(1000.0));
        assert_eq!(result.root.actual_time_ms, Some(12.0));
    }

    #[test]
    fn estimated_mode_never_reads_runtime_counters_even_if_present() {
        let raw = include_str!("../tests/fixtures/mssql_showplan_analyze.xml");
        let result = parse_mssql_xml(false, raw).expect("parse");
        assert_eq!(result.root.actual_rows, None);
        assert_eq!(result.root.actual_time_ms, None);
    }

    #[test]
    fn missing_index_hint_flattens_to_top_level_with_create_index_suggestion() {
        let raw = include_str!("../tests/fixtures/mssql_showplan_missing_index.xml");
        let result = parse_mssql_xml(false, raw).expect("parse");
        assert_eq!(result.top_level_hints.len(), 1);
        let hint = &result.top_level_hints[0];
        assert!(hint.message.contains("87.3"));
        let detail = hint.detail.as_ref().expect("detail present");
        assert!(detail.contains("CREATE INDEX"));
        assert!(detail.contains("[orders]"));
        assert!(detail.contains("[user_id]"));
        assert!(detail.contains("[amount]"));
        assert_eq!(result.root.operation, "Table Scan"); // the RelOp tree still parses alongside the hint
    }

    #[test]
    fn malformed_xml_is_err_not_panic() {
        assert!(parse_mssql_xml(false, "not xml at all").is_err());
        assert!(parse_mssql_xml(false, "<ShowPlanXML></ShowPlanXML>").is_err()); // no RelOp
        assert!(parse_mssql_xml(false, "<RelOp NodeId=\"0\">").is_err()); // unclosed
    }

    #[test]
    fn pathologically_deep_xml_fails_closed_at_max_depth() {
        let mut deep = String::from("<a>");
        for _ in 0..(MAX_XML_DEPTH + 10) {
            deep.push_str("<a>");
        }
        for _ in 0..(MAX_XML_DEPTH + 11) {
            deep.push_str("</a>");
        }
        let err = parse_mssql_xml(false, &deep).unwrap_err();
        assert!(err.contains("hloubku"), "expected the depth-guard message, got: {err}");
    }
}
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui mssql_parser_tests::`
Expected: all pass, zero warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/dbc-ui/src/plan.rs crates/dbc-ui/Cargo.toml crates/dbc-ui/tests/fixtures/mssql_showplan_*.xml
git commit -m "feat: MSSQL Showplan XML parser, hand-authored fixtures, needs-verification (G13 T3)"
```

---

### Task 4 (T4): Write-safety gating — `analyze_gate`

**Files:**
- Modify: `crates/dbc-ui/src/plan.rs` (add `AnalyzeGate`, `analyze_gate`, tests)

**Interfaces:**
- Consumes: `dbc_core::is_read_statement` (existing, `guards.rs`, exported from `dbc_core::lib.rs`).
- Produces (consumed by T6):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeGate {
    /// A read (`SELECT`/`WITH`/etc.) — run immediately, no confirmation,
    /// on any connection including a read-only one (design §5 case 1).
    Run,
    /// A write on a read-only connection — refused outright, status-bar
    /// only, no modal (design §5 case 2).
    Blocked,
    /// A write on a writable connection — show the confirm modal
    /// (design §5 case 3).
    NeedsConfirm,
}

/// §5's three-case dispatch, decided purely from the RAW (pre-`EXPLAIN`-
/// wrap) editor SQL — the exact same `dbc_core::is_read_statement` call
/// `run_query_with`'s Guard 1 already makes on the pre-wrap text; "Explain"
/// (estimated) never calls this at all (§5: always safe, unconditionally,
/// on every engine).
pub fn analyze_gate(sql: &str, read_only: bool) -> AnalyzeGate {
    if dbc_core::is_read_statement(sql) {
        AnalyzeGate::Run
    } else if read_only {
        AnalyzeGate::Blocked
    } else {
        AnalyzeGate::NeedsConfirm
    }
}
```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/plan.rs`, `#[cfg(test)] mod analyze_gate_tests`):

```rust
#[cfg(test)]
mod analyze_gate_tests {
    use super::*;

    #[test]
    fn read_statement_always_runs_regardless_of_read_only() {
        assert_eq!(analyze_gate("SELECT 1", false), AnalyzeGate::Run);
        assert_eq!(analyze_gate("SELECT 1", true), AnalyzeGate::Run);
        assert_eq!(analyze_gate("WITH x AS (SELECT 1) SELECT * FROM x", true), AnalyzeGate::Run);
    }

    #[test]
    fn write_statement_on_read_only_is_blocked() {
        assert_eq!(analyze_gate("UPDATE t SET a = 1", true), AnalyzeGate::Blocked);
        assert_eq!(analyze_gate("DELETE FROM t", true), AnalyzeGate::Blocked);
        assert_eq!(analyze_gate("INSERT INTO t VALUES (1)", true), AnalyzeGate::Blocked);
    }

    #[test]
    fn write_statement_on_writable_needs_confirm() {
        assert_eq!(analyze_gate("UPDATE t SET a = 1", false), AnalyzeGate::NeedsConfirm);
        assert_eq!(analyze_gate("INSERT INTO t VALUES (1)", false), AnalyzeGate::NeedsConfirm);
    }

    /// REQUIRED per CURATION item 3: the same bypass edges `guards.rs`
    /// already proves for `is_read_statement` must gate the same way here
    /// — this function is a thin wrapper, not a parallel implementation.
    #[test]
    fn cte_and_comment_bypass_edges_fail_closed_to_needs_confirm_or_blocked() {
        // Data-modifying CTE: lexically starts with WITH/SELECT-shaped but
        // contains an UPDATE token -> is_read_statement is false -> a write.
        let cte_write = "WITH x AS (UPDATE t SET a=1 RETURNING *) SELECT * FROM x";
        assert_eq!(analyze_gate(cte_write, false), AnalyzeGate::NeedsConfirm);
        assert_eq!(analyze_gate(cte_write, true), AnalyzeGate::Blocked);

        // Nested-block-comment bypass: real leading statement is the UPDATE.
        let nested_comment = "/* /* */ SELECT 1 */ UPDATE t SET a=1";
        assert_eq!(analyze_gate(nested_comment, false), AnalyzeGate::NeedsConfirm);

        // EXPLAIN ANALYZE UPDATE ... wrapped by the USER themselves (not by
        // this feature) still correctly classifies as a write on the
        // UNWRAPPED text this function receives.
        assert_eq!(analyze_gate("EXPLAIN ANALYZE UPDATE t SET a=1", false), AnalyzeGate::NeedsConfirm);

        // SELECT ... INTO (legacy CREATE TABLE AS spelling) is a write.
        assert_eq!(analyze_gate("SELECT * INTO new_tbl FROM t", true), AnalyzeGate::Blocked);

        // Unterminated comment/string fails closed -> not a read -> a write.
        assert_eq!(analyze_gate("SELECT 1 /* unterminated", true), AnalyzeGate::Blocked);
    }

    #[test]
    fn multi_statement_batch_any_write_anywhere_is_a_write() {
        assert_eq!(analyze_gate("SELECT 1; DROP TABLE t", false), AnalyzeGate::NeedsConfirm);
        assert_eq!(analyze_gate("SELECT 1; SELECT 2", true), AnalyzeGate::Run);
    }
}
```

- [ ] **Step 2: Run to see it fail, implement, run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui analyze_gate_tests::` (expect compile error) → implement `AnalyzeGate`/`analyze_gate` per the Interfaces block → run again, expect all pass, zero warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/dbc-ui/src/plan.rs
git commit -m "feat: analyze_gate write-safety dispatch (G13 T4)"
```

---

### Task 5 (T5): `PlanView` — GPUI tree-rendering entity

**Files:**
- Modify: `crates/dbc-ui/src/plan.rs` (add the GPUI-flavoured second half of the file: `PlanNodeId`, `PlanFlatNode`, `flatten_plan`, `expand_all`, `PlanNodeDetail`, `PlanView`, `PlanViewEvent`, render code, tests)

**Interfaces:**
- Consumes: `PlanResult`/`PlanNode`/`BufferStats`/`hot_fraction` (T1); GPUI (`uniform_list`, `div`, `Entity`, `Context`, `EventEmitter`, same primitives `schema_tree.rs` already uses).
- Produces (consumed by T6):

```rust
/// Path-based, stable across expand/collapse (mirrors `schema_tree.rs`'s
/// `NodeId` path-based-not-index-based rationale — `[]` = root, `[0]` =
/// first child, `[0, 2]` = root's first child's third child).
pub type PlanNodeId = Vec<usize>;

pub struct PlanFlatNode {
    pub id: PlanNodeId,
    pub depth: usize,
    pub operation: String,
    pub target: Option<String>,
    pub est_cost: Option<f64>,
    pub est_rows: Option<f64>,
    pub actual_rows: Option<f64>,
    /// Total across all loops (`actual_time_ms * loops`), for display — NOT
    /// `self_time_ms` (that's used only for `hot_fraction`, not shown as
    /// its own column).
    pub actual_time_total_ms: Option<f64>,
    pub buffers: Option<BufferStats>,
    pub hot_fraction: Option<f32>,
    pub expandable: bool,
}

/// Iterative pre-order flatten (Global Constraints: never recurse over a
/// `PlanNode` tree). Only visits nodes whose ancestor chain is fully
/// `expanded` (or the root, always visited).
pub fn flatten_plan(result: &PlanResult, expanded: &std::collections::HashSet<PlanNodeId>) -> Vec<PlanFlatNode>;

/// Every node id in `result`'s tree — used to seed `PlanView`'s default
/// fully-expanded state (a plan is typically tens of nodes, not thousands
/// of schema objects, so — unlike `SchemaTree`'s deliberately-collapsed
/// default — showing the whole shape immediately is the useful default).
/// Iterative, same stack idiom.
pub fn expand_all(result: &PlanResult) -> std::collections::HashSet<PlanNodeId>;

pub enum PlanViewEvent {
    /// Emitted when the header's connection-name-less close affordance
    /// isn't relevant here — PlanView emits nothing tab-management-related
    /// (the tab strip owns close/pin, same as every other `TabContent`);
    /// this event exists only for forward-compatibility symmetry with
    /// `TreeEvent`/`GridEvent` and currently has no variants a consumer
    /// needs to act on beyond `cx.notify()`, which `PlanView` triggers
    /// directly on its own entity — see Self-Review note on this design
    /// simplification.
}

pub struct PlanView {
    pub fn new(result: std::rc::Rc<PlanResult>, cx: &mut Context<Self>) -> Self;
}
```

**Grounding — iterative flatten + default-expand-all** (mirrors `convert_pg_tree`/`parse_sqlite_rows`'s frame-stack idiom exactly — pushed in reverse child order so popping yields left-to-right):

```rust
pub fn flatten_plan(result: &PlanResult, expanded: &HashSet<PlanNodeId>) -> Vec<PlanFlatNode> {
    let mut out = Vec::new();
    let mut stack: Vec<(&PlanNode, usize, PlanNodeId)> = vec![(&result.root, 0, Vec::new())];
    while let Some((node, depth, id)) = stack.pop() {
        let hot = hot_fraction(node, &result.root, result.is_analyze, result.total_execution_time_ms)
            .map(|f| f as f32);
        out.push(PlanFlatNode {
            id: id.clone(),
            depth,
            operation: node.operation.clone(),
            target: node.target.clone(),
            est_cost: node.est_cost,
            est_rows: node.est_rows,
            actual_rows: node.actual_rows,
            actual_time_total_ms: node.actual_time_ms.map(|t| t * node.loops.unwrap_or(1) as f64),
            buffers: node.buffers.clone(),
            hot_fraction: hot,
            expandable: !node.children.is_empty(),
        });
        if node.children.is_empty() || !expanded.contains(&id) {
            continue;
        }
        for (ix, child) in node.children.iter().enumerate().rev() {
            let mut child_id = id.clone();
            child_id.push(ix);
            stack.push((child, depth + 1, child_id));
        }
    }
    out
}

pub fn expand_all(result: &PlanResult) -> HashSet<PlanNodeId> {
    let mut out = HashSet::new();
    let mut stack: Vec<(&PlanNode, PlanNodeId)> = vec![(&result.root, Vec::new())];
    while let Some((node, id)) = stack.pop() {
        for (ix, child) in node.children.iter().enumerate() {
            let mut child_id = id.clone();
            child_id.push(ix);
            stack.push((child, child_id));
        }
        out.insert(id);
    }
    out
}
```

**Grounding — `PlanView` entity + render**, following `schema_tree.rs`'s exact row-rendering machinery (chevron/indent/click-to-expand at `schema_tree.rs:1002-1040`) and its disabled/error/hot-node colour precedents (`0xcdd6f4` default text, `0x45475a` selected-row background, `0xf38ba8` red / `0xf9e2af` amber — Catppuccin Mocha, per the design's palette-naming CURATION fix):

```rust
use std::collections::HashSet;
use std::rc::Rc;
use gpui::{div, prelude::*, px, rgb, uniform_list, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, Window};

/// Mirrors `grid.rs`'s `CellDetail`/`render_cell_detail_overlay` idiom
/// (grid.rs:180/1825) — same centered-overlay shape and interaction, but a
/// SEPARATE local instance: `CellDetail` is `ResultGrid`-local state, and
/// `PlanView` is a different entity with no `ResultGrid` to borrow one
/// from (same file-location correction G9's plan already made for its own
/// query-detail popup — see this plan's Self-Review deviations).
struct PlanNodeDetail {
    text: String,
    scroll_lines: usize,
}

pub enum PlanViewEvent {}

pub struct PlanView {
    result: Rc<PlanResult>,
    expanded: HashSet<PlanNodeId>,
    show_raw: bool,
    node_detail: Option<PlanNodeDetail>,
    focus_handle: FocusHandle,
}

impl PlanView {
    pub fn new(result: Rc<PlanResult>, cx: &mut Context<Self>) -> Self {
        let expanded = expand_all(&result);
        Self { result, expanded, show_raw: false, node_detail: None, focus_handle: cx.focus_handle() }
    }

    fn toggle_expand(&mut self, id: &PlanNodeId) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.clone());
        }
    }

    fn open_node_detail(&mut self, flat: &PlanFlatNode) {
        let mut lines = vec![format!("operation: {}", flat.operation)];
        if let Some(t) = &flat.target {
            lines.push(format!("target: {t}"));
        }
        // Full `extra` key/values for the clicked node — re-walk to find it
        // by id (small tree, cheap; avoids storing a parallel index).
        if let Some(node) = find_by_id(&self.result.root, &flat.id) {
            for (k, v) in &node.extra {
                lines.push(format!("{k}: {v}"));
            }
            if let Some(b) = &node.buffers {
                lines.push(format!(
                    "buffers: hit={:?} read={:?} dirtied={:?} written={:?} temp_read={:?} temp_written={:?}",
                    b.shared_hit, b.shared_read, b.shared_dirtied, b.shared_written, b.temp_read, b.temp_written
                ));
            }
        }
        self.node_detail = Some(PlanNodeDetail { text: lines.join("\n"), scroll_lines: 0 });
    }
}

/// Iterative id-path lookup — same stack idiom as `flatten_plan`, never
/// recurses.
fn find_by_id<'a>(root: &'a PlanNode, id: &[usize]) -> Option<&'a PlanNode> {
    let mut cur = root;
    for &ix in id {
        cur = cur.children.get(ix)?;
    }
    Some(cur)
}

impl EventEmitter<PlanViewEvent> for PlanView {}

impl Focusable for PlanView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn fmt_metric(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{n:.1}"),
        None => "—".to_string(),
    }
}

impl Render for PlanView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let badge = if self.result.is_analyze { "Skutečný plán" } else { "Odhadovaný plán" };
        let mut header = div()
            .h(px(28.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(rgb(0x181825))
            .text_color(rgb(0xcdd6f4))
            .child(format!(
                "{badge}{}",
                match (self.result.total_planning_time_ms, self.result.total_execution_time_ms) {
                    (Some(p), Some(e)) => format!(" · plánování {p:.2} ms · běh {e:.2} ms"),
                    (None, Some(e)) => format!(" · běh {e:.2} ms"),
                    _ => String::new(),
                }
            ))
            .child(
                div()
                    .id("plan-raw-toggle")
                    .cursor_pointer()
                    .px_1()
                    .child(if self.show_raw { "Strom" } else { "Raw" })
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.show_raw = !this.show_raw;
                        cx.notify();
                    })),
            );
        let mut root = div()
            .id("plan-view")
            .key_context("PlanView")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .child(header);

        if !self.result.top_level_hints.is_empty() {
            let mut banner = div().flex().flex_col().bg(rgb(0x2a2a1e)).text_color(rgb(0xf9e2af)).px_2().py_1();
            for hint in &self.result.top_level_hints {
                banner = banner.child(div().child(format!("⚠ {}", hint.message)));
            }
            root = root.child(banner);
        }

        if self.show_raw {
            root = root.child(
                div().flex_1().overflow_hidden().p_2().text_color(rgb(0xcdd6f4)).child(self.result.raw_text.clone()),
            );
            return root;
        }

        let rows = flatten_plan(&self.result, &self.expanded);
        root = root.child(
            uniform_list(
                "plan-tree-rows",
                rows.len(),
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let mut items = Vec::with_capacity(range.len());
                    for ix in range {
                        let flat = &rows[ix];
                        let is_expanded = this.expanded.contains(&flat.id);
                        let chevron = if flat.expandable { if is_expanded { "▾" } else { "▸" } } else { " " };
                        let chevron_id = flat.id.clone();
                        let click_id = flat.id.clone();
                        let label = match &flat.target {
                            Some(t) => format!("{} ({t})", flat.operation),
                            None => flat.operation.clone(),
                        };

                        let mut row = div()
                            .id(("plan-row", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(22.))
                            .pl(px(6. + flat.depth as f32 * 14.))
                            .text_color(rgb(0xcdd6f4))
                            .hover(|s| s.bg(rgb(0x313244)));
                        // Hot-node coloring applies to the ROW background,
                        // not just one column (design §2/§4).
                        match flat.hot_fraction {
                            Some(f) if f >= 0.30 => row = row.bg(rgb(0xf38ba8)),
                            Some(f) if f >= 0.10 => row = row.bg(rgb(0xf9e2af)),
                            _ => {}
                        }
                        row = row
                            .child(
                                div()
                                    .id(("plan-chevron", ix))
                                    .w(px(14.))
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .child(chevron)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                        cx.stop_propagation();
                                        this.toggle_expand(&chevron_id);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("plan-op", ix))
                                    .flex_1()
                                    .overflow_hidden()
                                    .cursor_pointer()
                                    .child(label)
                                    .on_click(cx.listener({
                                        let flat_ix = ix;
                                        move |this, _: &ClickEvent, _window, cx| {
                                            let rows2 = flatten_plan(&this.result, &this.expanded);
                                            if let Some(f) = rows2.get(flat_ix) {
                                                this.open_node_detail(f);
                                            }
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(div().w(px(70.)).child(fmt_metric(flat.est_cost)))
                            .child(div().w(px(70.)).child(fmt_metric(flat.est_rows)))
                            .child(div().w(px(70.)).child(fmt_metric(flat.actual_rows)))
                            .child(div().w(px(70.)).child(fmt_metric(flat.actual_time_total_ms)))
                            .child(div().w(px(20.)).child(if flat.buffers.is_some() { "▤" } else { "" }));
                        let _ = click_id;
                        items.push(row);
                    }
                    items
                }),
            )
            .flex_1(),
        );

        if let Some(detail) = &self.node_detail {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(rgb(0x11111b))
                    .p_4()
                    .id("plan-node-detail")
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _window, cx| {
                        this.node_detail = None;
                        cx.notify();
                    }))
                    .child(div().text_color(rgb(0xcdd6f4)).child(detail.text.clone())),
            );
        }
        root
    }
}
```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/plan.rs`, `#[cfg(test)] mod plan_view_tests` — pure `flatten_plan`/`expand_all`/`find_by_id` tests only; a full `Render` smoke test needs a GPUI test window, which is out of scope for this task's unit tests, same precedent `schema_tree.rs`'s own test module follows for its `flatten` function):

```rust
#[cfg(test)]
mod plan_view_tests {
    use super::*;
    use std::collections::HashSet;

    fn sample_result() -> PlanResult {
        let mut child_a = PlanNode {
            operation: "Seq Scan".into(), target: Some("orders".into()), est_cost: Some(30.0),
            est_rows: Some(100.0), actual_rows: None, actual_time_ms: None, loops: None,
            rows_removed_by_filter: None, buffers: None, extra: Vec::new(), children: Vec::new(),
        };
        let child_b = PlanNode {
            operation: "Seq Scan".into(), target: Some("users".into()), est_cost: Some(20.0),
            est_rows: Some(50.0), actual_rows: None, actual_time_ms: None, loops: None,
            rows_removed_by_filter: None, buffers: None, extra: Vec::new(), children: Vec::new(),
        };
        child_a.children = vec![]; // leaf
        let root = PlanNode {
            operation: "Hash Join".into(), target: None, est_cost: Some(100.0), est_rows: Some(10.0),
            actual_rows: None, actual_time_ms: None, loops: None, rows_removed_by_filter: None,
            buffers: None, extra: Vec::new(), children: vec![child_a, child_b],
        };
        PlanResult {
            root, is_analyze: false, engine: dbc_state::Engine::Postgres,
            total_planning_time_ms: None, total_execution_time_ms: None,
            top_level_hints: Vec::new(), raw_text: "{}".into(),
        }
    }

    #[test]
    fn expand_all_covers_every_node() {
        let result = sample_result();
        let all = expand_all(&result);
        assert_eq!(all.len(), 3); // root + 2 children
        assert!(all.contains(&Vec::<usize>::new()));
        assert!(all.contains(&vec![0]));
        assert!(all.contains(&vec![1]));
    }

    #[test]
    fn flatten_plan_fully_expanded_visits_all_in_order() {
        let result = sample_result();
        let expanded = expand_all(&result);
        let rows = flatten_plan(&result, &expanded);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].operation, "Hash Join");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].target.as_deref(), Some("orders"));
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].target.as_deref(), Some("users"));
    }

    #[test]
    fn flatten_plan_collapsed_root_hides_children() {
        let result = sample_result();
        let expanded = HashSet::new(); // nothing expanded
        let rows = flatten_plan(&result, &expanded);
        assert_eq!(rows.len(), 1); // just the root
        assert!(rows[0].expandable);
    }

    #[test]
    fn find_by_id_walks_path() {
        let result = sample_result();
        assert_eq!(find_by_id(&result.root, &[]).unwrap().operation, "Hash Join");
        assert_eq!(find_by_id(&result.root, &[0]).unwrap().target.as_deref(), Some("orders"));
        assert_eq!(find_by_id(&result.root, &[1]).unwrap().target.as_deref(), Some("users"));
        assert!(find_by_id(&result.root, &[5]).is_none());
        assert!(find_by_id(&result.root, &[0, 0]).is_none()); // leaf has no children
    }

    #[test]
    fn hot_fraction_carried_through_flatten() {
        let result = sample_result(); // root est_cost 100, child_a 30, child_b 20
        let expanded = expand_all(&result);
        let rows = flatten_plan(&result, &expanded);
        // self_cost(root) = 100 - 30 - 20 = 50 -> 50/100 = 0.5
        assert_eq!(rows[0].hot_fraction, Some(0.5));
    }
}
```

- [ ] **Step 2: Run to see it fail, implement, run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui plan_view_tests::` (expect compile errors) → implement `PlanNodeId`/`PlanFlatNode`/`flatten_plan`/`expand_all`/`find_by_id`/`PlanNodeDetail`/`PlanView`/`PlanViewEvent`/`fmt_metric`/`Render for PlanView` per the grounding code → run again, expect all pass, zero warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/dbc-ui/src/plan.rs
git commit -m "feat: PlanView tree-rendering entity (G13 T5)"
```

---

### Task 6 (T6): Tab + trigger-button wiring, runner-owned analyze-write sequence

**PRECONDITION — do not start until:** G6, G9, and G12's `runner.rs`/`main.rs` work has merged to `main`, and this branch has rebased onto that merge. Every line/symbol reference below was correct against the pre-G9/G12 state of `runner.rs`/`main.rs` at the time this plan was written (confirmed by reading the actual files on this branch) — re-locate by symbol name after the rebase, since G9 adds `open_monitor`/`monitor_loop` and G12 adds `run_script`/`connect_and_run_many` to the SAME two files this task edits.

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs` (`QueryRunner::run_analyze_write` + `run_analyze_write_inner` + `drive_analyze_write` + `drive_analyze_write_bounded` + `drain_single_text_cell`, tests)
- Modify: `crates/dbc-ui/src/tabs.rs` (`TabContent::Plan { view: Entity<PlanView> }` variant)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::AnalyzeWriteConfirm { sql: String, engine: dbc_state::Engine }` variant + its render panel)
- Modify: `crates/dbc-ui/src/main.rs` (status-bar "Vysvětlit"/"Analyzovat" buttons, `AppView::run_explain`, `AppView::run_analyze_write_confirmed`, `render_modal_overlay` match arm, `mod plan;` allow-removal)
- Modify: `crates/dbc-core/src/connection.rs` (doc comment ONLY — add `run_analyze_write` to `execute()`'s sanctioned-caller list)

**Interfaces:**
- Consumes: `plan::{PlanResult, PlanView, parse_plan, parse_sqlite_rows, explain_sql, explain_analyze_sql, analyze_button_visible, analyze_gate, AnalyzeGate}` (T1/T2/T4/T5), `runner::{guard_not_read_only, spec_is_read_only, open_spec, ConnectSpec, CancelToken, CHANNEL_CAPACITY, ROLLBACK_GRACE_SECS}` (existing), `tabs::{collapse_title, ResultTab, TabContent, Tabs}` (existing).
- Produces:

```rust
// runner.rs
impl QueryRunner {
    /// G13 CURATION item 2: the app's THIRD sanctioned write path (after
    /// G5's Apply flow and G9's kill flow) — a dedicated one-shot
    /// connection, BEGIN -> the EXPLAIN ANALYZE query -> ROLLBACK, ALWAYS
    /// (never COMMIT — the whole point is to measure real execution
    /// without keeping the effects). Belt-and-braces: refuses on
    /// `spec_is_read_only(&spec)` itself, independent of whatever gate the
    /// caller already applied (`plan::analyze_gate`).
    pub fn run_analyze_write(
        &self,
        spec: ConnectSpec,
        explain_analyze_sql: String,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::oneshot::Receiver<Result<String, QueryError>>;
}
```

**Grounding — the runner-owned method**, mirroring `run_write_transaction`/`drive_write_sequence`/`drive_write_sequence_bounded`'s exact dedicated-connection, timeout-bounded, tolerate-rollback-failure shape (runner.rs's existing G5 Task 4 code, read in full while drafting this plan) — the ONE structural difference is that this sequence uses `query()` (not `execute()`) for the middle step and ALWAYS rolls back, regardless of whether that `query()` step itself succeeded:

```rust
pub fn run_analyze_write(
    &self,
    spec: ConnectSpec,
    explain_analyze_sql: String,
    timeout_secs: Option<u64>,
) -> tokio::sync::oneshot::Receiver<Result<String, QueryError>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = self.handle();
    self.runtime.spawn(async move {
        let result = run_analyze_write_inner(spec, explain_analyze_sql, timeout_secs, handle).await;
        let _ = tx.send(result);
    });
    rx
}

/// Drains a single-row, single-column TEXT result (pg's `EXPLAIN (ANALYZE,
/// BUFFERS, FORMAT JSON)` output shape) via the same `dbc_buffer::
/// ResultBuffer` drain `fetch_lookup_inner` already uses.
async fn drain_single_text_cell(
    conn: &mut dyn Connection,
    sql: &str,
    cancel: CancelToken,
) -> Result<String, QueryError> {
    let mut stream = conn.query(sql, cancel).await?;
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        buf.push(item?).map_err(|e| QueryError::msg(e.to_string()))?;
    }
    if buf.row_count() == 0 || buf.cell_is_null(0, 0) {
        return Err(QueryError::msg("EXPLAIN ANALYZE nevrátil žádný řádek"));
    }
    Ok(buf.cell_text(0, 0))
}

/// BEGIN -> query -> ROLLBACK, ALWAYS (never COMMIT — see this function's
/// module-level doc above). Stops nothing early on the query step's own
/// error; the ROLLBACK still runs either way, same "tolerate ROLLBACK
/// itself failing" posture `drive_write_sequence` already documents.
async fn drive_analyze_write(
    conn: &mut dyn Connection,
    explain_analyze_sql: &str,
    cancel: CancelToken,
) -> Result<String, QueryError> {
    if let Err(e) = conn.execute("BEGIN", cancel.clone()).await {
        let _ = conn.execute("ROLLBACK", cancel.clone()).await;
        return Err(e);
    }
    let plan_result = drain_single_text_cell(conn, explain_analyze_sql, cancel.clone()).await;
    let _ = conn.execute("ROLLBACK", cancel.clone()).await; // ALWAYS — see doc comment.
    plan_result
}

/// Same timeout/cancel/bounded-rollback-grace shape as
/// `drive_write_sequence_bounded` (runner.rs, G5 Task 4) — reuses the SAME
/// `ROLLBACK_GRACE_SECS` constant so a hung ROLLBACK can never wedge this
/// path any differently than it can already wedge the Apply flow.
async fn drive_analyze_write_bounded(
    conn: &mut dyn Connection,
    explain_analyze_sql: &str,
    cancel: CancelToken,
    timeout_secs: Option<u64>,
) -> Result<String, QueryError> {
    match timeout_secs {
        Some(t) => {
            let sequence = drive_analyze_write(conn, explain_analyze_sql, cancel.clone());
            match tokio::time::timeout(Duration::from_secs(t), sequence).await {
                Ok(result) => result,
                Err(_elapsed) => {
                    cancel.cancel();
                    let rollback = conn.execute("ROLLBACK", CancelToken::new());
                    let _ = tokio::time::timeout(Duration::from_secs(ROLLBACK_GRACE_SECS), rollback).await;
                    Err(QueryError::msg(format!("[timeout] analýza překročila {t}s")))
                }
            }
        }
        None => drive_analyze_write(conn, explain_analyze_sql, cancel).await,
    }
}

async fn run_analyze_write_inner(
    spec: ConnectSpec,
    explain_analyze_sql: String,
    timeout_secs: Option<u64>,
    handle: tokio::runtime::Handle,
) -> Result<String, QueryError> {
    guard_not_read_only(spec_is_read_only(&spec))?; // belt-and-braces — see doc comment.
    let mut opened = open_spec(spec, handle).await?;
    let cancel = CancelToken::new();
    drive_analyze_write_bounded(&mut *opened.conn, &explain_analyze_sql, cancel, timeout_secs).await
    // `opened` drops here unconditionally — the ultimate backstop, same as run_write_transaction_inner.
}
```

**Grounding — REQUIRED read-only tests** (runner.rs, `#[cfg(test)] mod analyze_write_tests`, mirroring `write_transaction_tests`'s exact fixtures — `open_sqlite_test_conn`/`read_one` helpers reused or duplicated per that module's own precedent):

```rust
#[cfg(test)]
mod analyze_write_tests {
    use super::*;
    use super::write_transaction_tests::{open_sqlite_test_conn, read_one}; // or duplicate, per that module's own precedent

    /// REQUIRED (Global Constraints): refuses BEFORE `open_spec` is ever
    /// called — no connection attempted, no driver reached.
    #[tokio::test]
    async fn run_analyze_write_refuses_read_only_connection_without_connecting() {
        let cfg = dbc_state::ConnectionConfig {
            id: "x".into(), name: "x".into(), folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            database: "\0invalid".into(), // never actually opened — see comment below
            host: String::new(), port: None, user: String::new(),
            read_only: true, timeout_secs: None, auto_limit: None, ssh: None, favourite: false,
        };
        let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
        let handle = tokio::runtime::Handle::current();
        let err = run_analyze_write_inner(spec, "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT 1".to_string(), None, handle)
            .await
            .unwrap_err();
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn drive_analyze_write_always_rolls_back_on_success() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();

        let out = drive_analyze_write(&mut *conn, "SELECT 'plan-text'", CancelToken::new()).await.unwrap();
        assert_eq!(out, "plan-text");
        // Sanity: this connection is still usable afterward (ROLLBACK, not
        // a leaked open transaction) — a fresh statement succeeds.
        conn.execute("INSERT INTO t VALUES (2, 'b')", CancelToken::new()).await.unwrap();
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 2").await, Some("b".to_string()));
    }

    #[tokio::test]
    async fn drive_analyze_write_rolls_back_writes_even_though_it_never_commits() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
        // Simulate the sharp edge: the "EXPLAIN ANALYZE" text itself is
        // just SQL from this test's point of view — sqlite has no EXPLAIN
        // ANALYZE, so drive a real INSERT through the same code path to
        // prove the ROLLBACK really undoes it (the design's actual
        // pg/mssql sequences execute the real statement as a SIDE EFFECT
        // of measuring it — this test proves the undo half of that, engine
        // details aside).
        drive_analyze_write(&mut *conn, "INSERT INTO t VALUES (99, 'ghost')", CancelToken::new()).await.unwrap();
        assert_eq!(read_one(&mut *conn, "SELECT n FROM t WHERE id = 99").await, None);
    }

    #[tokio::test]
    async fn drive_analyze_write_still_rolls_back_when_the_query_step_errors() {
        let (_f, mut conn) = open_sqlite_test_conn().await;
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", CancelToken::new()).await.unwrap();
        let err = drive_analyze_write(&mut *conn, "SELECT * FROM no_such_table", CancelToken::new()).await.unwrap_err();
        assert!(!err.message.is_empty());
        // Connection must still be usable — ROLLBACK ran despite the error.
        conn.execute("INSERT INTO t VALUES (1)", CancelToken::new()).await.unwrap();
    }
}
```

- [ ] **Step 1: `runner.rs` — write the failing tests above, implement, run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui analyze_write_tests::` (expect compile errors) → implement `run_analyze_write`/`run_analyze_write_inner`/`drive_analyze_write`/`drive_analyze_write_bounded`/`drain_single_text_cell` per the grounding code → run again, expect all pass, zero warnings.

- [ ] **Step 2: `connection.rs` doc update**

In `crates/dbc-core/src/connection.rs`'s `execute()` doc comment, extend the sanctioned-caller sentence:

```rust
    /// Executes a non-returning statement, reporting affected rows. This is
    /// the app's write path — ONLY the sandbox Apply flow, the server-
    /// monitor kill flow, and the ANALYZE-on-a-write sequence (dedicated
    /// connection, BEGIN … ROLLBACK — never COMMIT) may call it.
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core`
Expected: passes (doc-only change).

- [ ] **Step 3: `tabs.rs` — `TabContent::Plan` variant**

```rust
// tabs.rs, inside `pub enum TabContent`:
    Plan { view: gpui::Entity<crate::plan::PlanView> },
```

Add `mod plan;` is already present from T1 with `#[allow(dead_code)]` — remove that allow here (T6 is the first real consumer).

- [ ] **Step 4: `connections_ui.rs` — `ModalState::AnalyzeWriteConfirm`**

```rust
// connections_ui.rs, inside `pub enum ModalState` (derives Clone already —
// both new fields are Clone: String and dbc_state::Engine):
    AnalyzeWriteConfirm {
        sql: String,
        engine: dbc_state::Engine,
    },
```

Render panel, added to `render_modal_overlay`'s match (same `.occlude()`/centered-panel shape every other modal here uses):

```rust
ModalState::AnalyzeWriteConfirm { sql, engine } => {
    let engine = *engine;
    div()
        .id("analyze-write-confirm")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .occlude()
        .child(
            div()
                .w(px(520.))
                .bg(rgb(0x1e1e2e))
                .border_1()
                .border_color(rgb(0x45475a))
                .rounded_md()
                .p_4()
                .flex()
                .flex_col()
                .gap_2()
                .text_color(rgb(0xcdd6f4))
                .child(div().text_color(rgb(0xf9e2af)).child(
                    "Toto SQL bude SKUTEČNĚ PROVEDENO, aby bylo možné změřit skutečný plán, a poté vráceno zpět (ROLLBACK). Vedlejší efekty MIMO transakci (např. hodnoty sekvencí/IDENTITY, volání externích funkcí) NEBUDOU vráceny zpět.",
                ))
                .child(div().p_2().bg(rgb(0x11111b)).rounded_md().child(sql.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .justify_end()
                        .child(
                            div()
                                .id("analyze-write-cancel")
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .child("Zrušit")
                                .on_click(cx.listener(|view, _: &ClickEvent, _window, cx| {
                                    view.modal = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("analyze-write-confirm-btn")
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .text_color(rgb(0xf38ba8))
                                .child("Analyzovat")
                                .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                                    view.on_confirm_analyze_write(engine, window, cx);
                                })),
                        ),
                ),
        )
        .into_any_element()
}
```

- [ ] **Step 5: `main.rs` — status-bar buttons, dispatch, tab wiring**

Restructure the status bar (currently a single `div().h(px(28.))...child(self.status.clone())`) into a flex row with the two buttons left of the status text:

```rust
root = root.child(
    div()
        .h(px(28.))
        .px_2()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .bg(rgb(0x313244))
        .text_color(rgb(0xa6adc8))
        .child({
            let enabled = self.cancel.is_none() && !self.sql.read(cx).text().trim().is_empty();
            let color = if enabled { rgb(0xcdd6f4) } else { rgb(0x45475a) };
            div()
                .id("btn-explain")
                .cursor_pointer()
                .text_color(color)
                .child("Vysvětlit")
                .on_click(cx.listener(move |view, _: &ClickEvent, _window, cx| {
                    if enabled {
                        view.run_explain(false, cx);
                    }
                }))
        })
        .child({
            let engine = self.active_engine(); // small new helper, see below
            let visible = engine.map(plan::analyze_button_visible).unwrap_or(true);
            let enabled = visible && self.cancel.is_none() && !self.sql.read(cx).text().trim().is_empty();
            if !visible {
                div() // empty placeholder — hidden entirely per design §1c/§4
            } else {
                let color = if enabled { rgb(0xcdd6f4) } else { rgb(0x45475a) };
                div()
                    .id("btn-analyze")
                    .cursor_pointer()
                    .text_color(color)
                    .child("Analyzovat")
                    .on_click(cx.listener(move |view, _: &ClickEvent, _window, cx| {
                        if enabled {
                            view.run_explain(true, cx);
                        }
                    }))
            }
        })
        .child(div().flex_1().child(self.status.clone())),
);
```

New `AppView` helper (small, pure read of existing state — mirrors `current_connection_label`'s own `active_connection_id` lookup):

```rust
/// `None` when there's no active connection at all (both buttons render
/// but stay disabled via the `enabled` checks above, same as `run_query`'s
/// existing "no connection" guard).
fn active_engine(&self) -> Option<dbc_state::Engine> {
    if let Some(id) = &self.active_connection_id {
        self.config.connections.iter().find(|c| &c.id == id).map(|c| c.engine)
    } else if self.conn_url.is_some() {
        Some(engine_from_url(self.conn_url.as_ref().unwrap()))
    } else {
        None
    }
}
```

`AppView::run_explain` — the dispatch (mirrors `run_query_with`'s spec-resolution block verbatim for the read-only/engine/auto-limit-irrelevant parts, then diverges per §5's three-case gate):

```rust
fn run_explain(&mut self, is_analyze: bool, cx: &mut Context<Self>) {
    if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
        return;
    }
    if self.cancel.is_some() {
        return;
    }
    let sql = self.sql.read(cx).text().to_string();
    if sql.trim().is_empty() {
        return;
    }

    let Some((read_only, timeout_secs, engine, spec)) = self.resolve_spec_for_explain(cx) else {
        return; // resolve_spec_for_explain already set self.status on failure
    };

    if !is_analyze {
        // §5: Explain is ALWAYS safe — no gate, dispatch immediately.
        self.dispatch_plan_query(spec, plan::explain_sql(engine, &sql), engine, false, timeout_secs, cx);
        return;
    }

    match plan::analyze_gate(&sql, read_only) {
        plan::AnalyzeGate::Run => {
            let Some(explain_sql) = plan::explain_analyze_sql(engine, &sql) else { return }; // SQLite: button hidden, unreachable
            self.dispatch_plan_query(spec, explain_sql, engine, true, timeout_secs, cx);
        }
        plan::AnalyzeGate::Blocked => {
            self.status = "error: připojení je jen pro čtení".to_string();
            cx.notify();
        }
        plan::AnalyzeGate::NeedsConfirm => {
            self.modal = Some(connections_ui::ModalState::AnalyzeWriteConfirm { sql, engine });
            cx.notify();
        }
    }
}

/// Shared spec-resolution slice of `run_query_with`'s own block, factored
/// out for `run_explain` to reuse without duplicating the connection-
/// lookup/CLI-url branching. Returns `None` (status already set) on no
/// active connection.
fn resolve_spec_for_explain(&mut self, cx: &mut Context<Self>) -> Option<(bool, Option<u64>, dbc_state::Engine, ConnectSpec)> {
    if let Some(id) = self.active_connection_id.clone() {
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else {
            self.status = "connection no longer exists".into();
            cx.notify();
            return None;
        };
        let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
        let (read_only, timeout_secs, engine) = (cfg.read_only, cfg.timeout_secs, cfg.engine);
        Some((read_only, timeout_secs, engine, ConnectSpec::Config { cfg: Box::new(cfg), secret }))
    } else if let Some(url) = self.conn_url.clone() {
        Some((false, None, engine_from_url(&url), ConnectSpec::Url(url)))
    } else {
        self.status = "Bez připojení — vyberte připojení nahoře.".into();
        cx.notify();
        None
    }
}
```

`AppView::dispatch_plan_query` — the estimated-path AND the read-case-of-Analyze path both go through the NORMAL `connect_and_run` (no write gating needed for either), draining exactly like an ad-hoc tab but capturing text/rows instead of opening a live grid:

```rust
fn dispatch_plan_query(
    &mut self,
    spec: ConnectSpec,
    wrapped_sql: String,
    engine: dbc_state::Engine,
    is_analyze: bool,
    timeout_secs: Option<u64>,
    cx: &mut Context<Self>,
) {
    let cancel = CancelToken::new();
    self.cancel = Some(cancel.clone());
    self.run_generation += 1;
    let my_generation = self.run_generation;
    self.status = if is_analyze { "analyzuji plán…".to_string() } else { "vysvětluji plán…".to_string() };
    cx.notify();

    let sql_title = format!("Plán: {}", collapse_title(&wrapped_sql));
    let conn_identity = self.current_conn_identity();
    let mut rx = self.runner.connect_and_run(spec, wrapped_sql, cancel, timeout_secs);
    cx.spawn(async move |this, cx| {
        let mut buffer: Option<dbc_buffer::ResultBuffer> = None;
        let mut failed: Option<QueryError> = None;
        while let Some(ev) = rx.recv().await {
            let stop = this
                .update(cx, |view, _cx| match ev {
                    QueryEvent::Started { columns } => {
                        buffer = Some(dbc_buffer::ResultBuffer::new(columns));
                        false
                    }
                    QueryEvent::Batch(b) => {
                        if let Some(buf) = buffer.as_mut() {
                            if let Err(e) = buf.push(b) {
                                failed = Some(QueryError::msg(e.to_string()));
                            }
                        }
                        false
                    }
                    QueryEvent::Finished { .. } => true,
                    QueryEvent::Failed(e) => {
                        failed = Some(e);
                        true
                    }
                })
                .unwrap_or(true);
            if stop {
                break;
            }
        }

        let _ = this.update(cx, move |view, cx| {
            if view.run_generation != my_generation {
                return; // a newer run superseded this one — don't clobber its state
            }
            view.cancel = None;
            if let Some(e) = failed {
                view.status = format!("error: {e}");
                cx.notify();
                return;
            }
            let Some(buf) = buffer else {
                view.status = "prázdná odpověď EXPLAIN".to_string();
                cx.notify();
                return;
            };

            let parsed = if engine == dbc_state::Engine::Sqlite {
                let mut rows: Vec<(i64, i64, String)> = Vec::new();
                let mut raw_lines: Vec<String> = Vec::new();
                for r in 0..buf.row_count() {
                    let id = buf.cell_text(r, 0).parse().unwrap_or(0);
                    let parent = buf.cell_text(r, 1).parse().unwrap_or(0);
                    let detail = buf.cell_text(r, 3); // columns: id, parent, notused, detail
                    raw_lines.push(format!("{id}\t{parent}\t{detail}"));
                    rows.push((id, parent, detail));
                }
                Ok(plan::PlanResult {
                    root: plan::parse_sqlite_rows(&rows),
                    is_analyze,
                    engine,
                    total_planning_time_ms: None,
                    total_execution_time_ms: None,
                    top_level_hints: Vec::new(),
                    raw_text: raw_lines.join("\n"),
                })
            } else {
                let raw_text = if buf.row_count() == 0 || buf.cell_is_null(0, 0) {
                    Err("EXPLAIN nevrátil žádný řádek".to_string())
                } else {
                    Ok(buf.cell_text(0, 0))
                };
                raw_text.and_then(|t| plan::parse_plan(engine, is_analyze, &t))
            };

            match parsed {
                Ok(result) => {
                    let result = std::rc::Rc::new(result);
                    let view_entity = cx.new(|cx| plan::PlanView::new(result, cx));
                    let tab = ResultTab {
                        id: 0,
                        title: sql_title,
                        pinned: false,
                        preview_key: None,
                        conn_identity,
                        content: TabContent::Plan { view: view_entity },
                    };
                    view.tabs.open(tab);
                    view.status = "hotovo".to_string();
                }
                Err(e) => {
                    view.status = format!("error parsování plánu: {e}");
                }
            }
            cx.notify();
        });
    })
    .detach();
}
```

`AppView::on_confirm_analyze_write` — dispatches T6's runner method, called from the confirm modal's "Analyzovat" button (Step 4):

```rust
fn on_confirm_analyze_write(&mut self, engine: dbc_state::Engine, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(connections_ui::ModalState::AnalyzeWriteConfirm { sql, .. }) = self.modal.take() else { return };
    let Some((_, timeout_secs, _, spec)) = self.resolve_spec_for_explain(cx) else { return };
    let Some(explain_sql) = plan::explain_analyze_sql(engine, &sql) else { return };

    self.cancel = Some(CancelToken::new()); // blocks a second run while this awaits; cleared below regardless of outcome
    self.run_generation += 1;
    let my_generation = self.run_generation;
    self.status = "analyzuji plán (BEGIN…ROLLBACK)…".to_string();
    cx.notify();

    let sql_title = format!("Plán: {}", collapse_title(&sql));
    let conn_identity = self.current_conn_identity();
    let rx = self.runner.run_analyze_write(spec, explain_sql, timeout_secs);
    cx.spawn(async move |this, cx| {
        let result = rx.await;
        let _ = this.update(cx, move |view, cx| {
            if view.run_generation != my_generation {
                return;
            }
            view.cancel = None;
            match result {
                Ok(Ok(raw_text)) => match plan::parse_plan(engine, true, &raw_text) {
                    Ok(parsed) => {
                        let parsed = std::rc::Rc::new(parsed);
                        let view_entity = cx.new(|cx| plan::PlanView::new(parsed, cx));
                        view.tabs.open(ResultTab {
                            id: 0,
                            title: sql_title,
                            pinned: false,
                            preview_key: None,
                            conn_identity,
                            content: TabContent::Plan { view: view_entity },
                        });
                        view.status = "hotovo (změny vráceny zpět)".to_string();
                    }
                    Err(e) => view.status = format!("error parsování plánu: {e}"),
                },
                Ok(Err(e)) => view.status = format!("error: {e}"),
                Err(_canceled) => view.status = "error: analýza zrušena".to_string(),
            }
            cx.notify();
        });
    })
    .detach();
}
```

`render_modal_overlay`'s match (Step 4's panel already written) gets one new arm added to the existing `match modal { ... }`; `render_tab_content`'s existing match on `TabContent` gets one new arm:

```rust
TabContent::Plan { view } => view.clone().into_any_element(),
```

- [ ] **Step 6: Build, run the full suite, manual smoke test**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
Expected: zero warnings (remove the `#[allow(dead_code)]` on `mod plan;` now that T6 is the real consumer).
Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-ui`
Expected: all pass.
Manual (per `/run` skill or a plain launch against the SQLite fixture): "Vysvětlit" on `SELECT * FROM t` opens a Plan tab with a tree; "Analyzovat" is NOT rendered at all for a SQLite connection; against a docker-pg connection, "Analyzovat" on a `SELECT` runs immediately (no modal), "Analyzovat" on an `UPDATE` over a writable connection shows the confirm modal with the literal SQL, confirming shows a plan tab and the row is provably unchanged afterward; "Analyzovat" on an `UPDATE` over a read-only pg connection shows the status-bar refusal with no modal at all.

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/src/runner.rs crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/main.rs crates/dbc-core/src/connection.rs
git commit -m "feat: Explain/Analyze tab wiring, runner-owned ANALYZE-write sequence (G13 T6)"
```

---

### Task 7 (T7): MSSQL end-to-end wiring — DEFERRED, not part of this plan's mergeable scope

No files, no code. Tracked here only so the deferral is explicit and discoverable: T3's `parse_mssql_xml` lands, unit-tested, in this plan — but the actual `SET SHOWPLAN_XML`/`STATISTICS XML` SQL has nothing to run against, because `dbc-ui/src/connect.rs::open_config`'s `Engine::Mssql` arm still hard-errors (`"MSSQL driver zatím není k dispozici"`) as of this branch, independent of anything this plan does. When the (separate, unscheduled) MSSQL driver phase wires that arm:
1. Correct T3's fixtures/attribute-name assumptions against real captures from a live SQL Server (every needs-verification flag in T3's doc comments points at exactly what to re-check).
2. Verify `odbc-api` 29.0.0's `Cursor::more_results()` (confirmed to exist, `cursor.rs:102/382`) can skip `STATISTICS XML`'s leading statement result set and select the trailing one-row-one-column result set whose column name is historically `"Microsoft SQL Server 2005 XML Showplan"` — this needs a driver-side change in `dbc-driver-mssql`, not `dbc-ui`, since `Connection::query()`'s one-`QueryStream`-per-call shape currently has no way to say "skip result set 1, return result set 2."
3. Add an `Engine::Mssql` branch to T6's `dispatch_plan_query`/`on_confirm_analyze_write` (today they already build the right SQL text via `plan::explain_sql`/`explain_analyze_sql`'s existing MSSQL arms — the missing piece is purely "can `connect_and_run`/`run_analyze_write` reach a real MSSQL connection at all," which is entirely the driver phase's concern).

---

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §0 architecture decision (plans ride `query()`, parser lives in `dbc-ui`) → the whole plan's shape; no `dbc-core`/driver code anywhere except T6's one doc-comment line.
- §1a Postgres acquisition + field mapping → T2 (`PgPlanJson`, `convert_pg_tree`, fixtures captured live against docker-pg 16.13).
- §1b MSSQL acquisition + XML shape → T3 (parser + fixtures, all needs-verification flags preserved and pointed at from T7).
- §1c SQLite acquisition → T1 (`parse_sqlite_rows`); the "Analyze button hidden entirely" decision → T1's `analyze_button_visible` + T6's render gate.
- §2 unified model + hot-node formula → T1 (structs, `self_time_ms`/`self_cost`/`hot_fraction`); the exact 0.30/0.10 thresholds and Catppuccin Mocha hex values → T5's render.
- §3 parsers + dependency justification → T2 (`serde_json`)/T3 (`quick-xml`, version corrected from the design's illustrative "0.31" to the actually-resolving `0.41`); `parse_plan` entry point → T2.
- §4 UI (tab-not-modal, trigger buttons, layout, hints banner, raw toggle, node detail, Czech text) → T5 (`PlanView`'s render) + T6 (status-bar buttons, tab wiring).
- §5 the sharp edge (three-case gate, confirm-modal copy, BEGIN…ROLLBACK sequence, defense-in-depth posture) → T4 (`analyze_gate`) + T6 (`run_analyze_write` + the confirm modal + `run_explain`'s dispatch).
- §6 task decomposition → this plan's T1–T7 (renumbered/merged where noted below); the suggested parallel batching → this plan's Task dependency graph section, and Global Constraints' task-ordering note about G6/G9/G12.
- §7 risks → MSSQL risks carried as T3 doc comments + T7's deferral; the pg per-loop-averaging formula → T2's parallel-worker fixture test (`Actual Loops: 5` case); the `BEGIN`/`ROLLBACK`-doesn't-affect-timing claim → T6 Step 6's manual smoke test.

**Placeholder scan:** every step shows real, complete code or a concrete cargo/docker command — no TBDs. The one deliberate exception is flagged explicitly as such, not silently: T3's combined `Event::Start(e) | Event::Empty(e)` match-arm sketch is marked with an inline "Implementer note, not a placeholder" callout requiring it be split into two real arms before Step 3 closes, because a single shared arm structurally cannot distinguish self-closing elements from open-with-a-later-End elements — this is real, described, required work with a concrete correct shape to follow (proven during drafting for the `RelOp`-only case), not an unspecified gap.

**Type-name consistency across tasks:** `plan::{PlanResult, PlanNode, BufferStats, PlanHint, finite, self_time_ms, self_cost, hot_fraction}` (T1) match T2/T3's constructors and T5's `flatten_plan`/`hot_fraction` call sites. `plan::{explain_sql, explain_analyze_sql, analyze_button_visible}` (T1) match T6's `run_explain`/status-bar gating. `plan::{parse_pg_json, parse_plan}` (T2) match T6's `dispatch_plan_query` non-SQLite branch. `plan::parse_mssql_xml` (T3) matches T2's `parse_plan` dispatcher (stubbed until T3 merges, per T2 Step 4's note) and T7's stated follow-up. `plan::{AnalyzeGate, analyze_gate}` (T4) match T6's `run_explain`. `plan::{PlanNodeId, PlanFlatNode, flatten_plan, expand_all, PlanView, PlanViewEvent}` (T5) match T6's `TabContent::Plan { view: Entity<PlanView> }` and `PlanView::new`'s call site. `runner::run_analyze_write` (T6) matches `connection.rs`'s updated doc list (T6 Step 2) and `main.rs::on_confirm_analyze_write`.

**Deviations from the design draft (each with reason; none touch the CURATION requirements):**
1. **`PlanNode`/`PlanResult` do NOT derive `Clone`** (design's struct sketch had `derive(Debug, Clone, PartialEq)`) — the Global Constraints "deep recursive tree hazard" note, added per this plan's explicit brief, requires it: an unbounded-depth tree with a derived `Clone` is exactly as recursion-hazardous as a derived `Drop`. `TabContent::Plan` and every cross-task handoff use `Rc<PlanResult>`/`Entity<PlanView>` instead, the same "wrap heavy non-`Clone` state" convention `TabContent::Grid`'s `Rc<RefCell<ResultBuffer>>` already establishes.
2. **`PlanNode` gets a custom iterative `Drop`, and every tree-building/tree-walking function (`convert_pg_tree`, `parse_mssql_xml`'s `<RelOp>` walk, `parse_sqlite_rows`, `flatten_plan`, `expand_all`, `find_by_id`) is written iteratively** — not specified at this level of detail in the design, which predates the hazard-class briefing this plan was written under. Verified during drafting: a derive-only equivalent type overflows the stack at 5,000 deep; the iterative versions here survive 20,000–50,000 deep in both debug and release.
3. **`regex` is NOT added as a dependency** for SQLite's `target` extraction (design §1c suggested `r"(?:TABLE|INDEX) (\w+)"`) — it isn't a `dbc-ui` dependency today and pulling it in for one small fixed-shape scan isn't worth a new crate; a manual token walk (`extract_target`) does the same job.
4. **`TabContent::Plan { view: Entity<PlanView> }`**, not the design's `TabContent::Plan { result: PlanResult }` — follows `TabContent::Grid`'s existing `Entity<T>`-holds-the-state convention, and is required anyway once `PlanResult` isn't `Clone` (see deviation 1).
5. **The node-detail popup is a `PlanView`-local `PlanNodeDetail`/overlay, not a shared component** (design §4 says "reuses that popup component") — `grid.rs`'s `CellDetail`/`render_cell_detail_overlay` (grid.rs:180/1825) is `ResultGrid`-local state; `PlanView` is a different entity with no `ResultGrid` to borrow one from. Same visual/interaction idiom, a separate instance — identical file-location correction the G9 plan already made for its own query-detail popup (that plan's Self-Review note 7).
6. **Postgres's `"Planning"` buffer-stats key (a top-level sibling of `"Plan"`, distinct from `"Planning Time"`) is confirmed present in real captures but deliberately left unparsed into any typed field** — not mentioned in the design at all; `raw_text` preserves it verbatim for the raw-text toggle, consistent with the design's own treatment of `"Triggers"`.
7. **MSSQL's per-`<RelOp>` runtime-counter aggregation sets `loops: Some(1)`** after summing `ActualRows`/maxing `ActualElapsedms` across `<RunTimeCountersPerThread>` entries — not stated in the design, which only specifies the sum/max aggregation itself; setting `loops` to 1 here prevents `self_time_ms`'s `* loops` multiplication from double-counting an already-aggregated total. Flagged needs-verification alongside every other T3 MSSQL claim.
8. **`total_planning_time_ms`/`total_execution_time_ms` are both `None` for MSSQL** (design §1b doesn't specify a top-level total for `STATISTICS XML`, only per-node counters) — `hot_fraction`'s existing fallback (root's own `actual_time_ms * loops`) already covers this per design §2's own fallback clause, so no MSSQL-specific formula branch was needed.
9. **No query-history recording for Explain/Analyze runs** — the design is silent on history for G13; `run_explain`/`on_confirm_analyze_write` deliberately bypass `run_query_with` (and therefore `record_history`) entirely, per §4's own framing that the buttons "have their own dispatch... precisely so `bypass_auto_limit`/history-recording/preview-tab logic doesn't leak into this flow." Same precedent as G9's monitor tiles.
10. **`quick-xml` pinned at `"0.41"`**, not the design's illustrative `"0.31"` — `0.41.0` already resolves in `Cargo.lock` transitively (introduces no new version into the dependency graph) and is what was actually compiled against while grounding T3's code.
