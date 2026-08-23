# G9 Server Monitor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Server-monitor dashboard as a special result tab (Postgres now; MSSQL SQL written and gated until its driver lands; SQLite has no monitor tab): four tiles (connections, locks/deadlocks, DB size data-vs-WAL, cache-hit/uptime/TPS), running-queries list with duration colour tiers and kill, blocking-chains tree, per-table sizes with bars — auto-refresh 5 s, pausable, backoff on error, and a confirmed kill flow that is the app's second (narrowly-scoped) write path.

**Architecture:** One new pure module pair with zero GPUI (`dbc-ui/src/monitor.rs` — data model + parsing/aggregation over already-materialized text rows; `dbc-ui/src/monitor_sql.rs` — engine SQL constants + `kill_sql`), a background task in `runner.rs` (`QueryRunner::open_monitor` holding ONE dedicated `Box<dyn Connection>` for the tab's lifetime, command/event channels, sequential per-refresh query drain, and the kill write via `Connection::execute` behind an independent read-only gate), a per-tab GPUI entity (`monitor_view.rs` — `MonitorView`, drains events, owns pause/backoff/generation state, renders tiles/lists/tree), a new `connections_ui::ModalState::KillConfirm` confirm dialog (show-the-exact-SQL, same single-modal infrastructure as every other dialog), and a serialized `main.rs` tail (open action + palette entry with engine gating, `TabContent::Monitor` wiring, the AppView-owned timer loop). The pure core is exhaustively unit-tested standalone; the one REQUIRED guard-level test (read-only kill refusal, design §9.1 CURATION) drives `monitor_loop` directly over a mock `Connection` with no docker dependency; docker `#[ignore]` tests prove the Postgres SQL and the blocking-chain query against a live server.

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no new GPUI features; timer via the existing `cx.background_executor().timer(...)` idiom from `grid.rs:1452`), tokio channels on `QueryRunner`'s existing runtime, `testcontainers-modules` (new **dev**-dependency of `dbc-ui` only, same crate/version the postgres driver's own integration tests already use).

**Spec:** `docs/superpowers/specs/2026-08-22-gui-target-design.md` (G9 phasing row + §3 architecture constraints) and `docs/superpowers/specs/drafts/g9-server-monitor-design.md` (binding design for this phase — implement exactly what it specifies; its CURATION block in §9.1 is non-negotiable, see Global Constraints). The design's §0 constraint amendment (kill as a second confirmed write path through `Connection::execute`) is treated as approved-with-this-plan; T3 and T6 carry the two mandated doc updates.

## Global Constraints

- dbc-core never depends on GPUI; dbc-ui never imports concrete driver crates outside `connect.rs`. (This phase adds NO dbc-core code beyond a doc-comment update and NO driver-crate code at all — every new file lives in `dbc-ui`, and the docker tests connect through `crate::connect`/`open_spec` only, the same sanctioned entry `runner.rs`'s existing sqlite tests use.)
- Errors are values (no panics in production paths). A failed monitor sub-query degrades that ONE tile/section to "n/a"; a dead connection surfaces as a backoff status line; a kill failure stays in the confirm dialog — never a crash. Numeric parsing in `monitor.rs` is fail-soft (`unwrap_or(0)` / `Option`), NOT fail-closed — this is a display feature, not a SQL-safety guard (design §1).
- All write/destructive actions (including `pg_terminate_backend` / kill session) go through the confirm-modal + `run_write_transaction`-style confirmed path; read-only connections must refuse kill with a clear error — and this plan includes the curated REQUIRED test: kill attempted over a read-only connection is refused before reaching the driver (T3's `monitor_kill_refused_on_read_only_before_reaching_driver`, driven over a mock `Connection` whose `execute` call count is asserted to be zero). **CURATION (design §9.1, binding):** that test is REQUIRED, not recommended — the app-level `read_only` flag is the ONLY enforcement (neither engine blocks kill server-side, design §0), so it gets guard-level test discipline, same class as `guards.rs`.
- Kill routes through `Connection::execute()` exclusively — NEVER `query()`. `SELECT pg_terminate_backend(n)` passes `is_read_statement` (leading `SELECT`, no `WRITE_KEYWORDS` token), so the `query()` path would silently bypass the read-only guard entirely (design §0's rejected alternative); T2 pins this fact with a documenting test.
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; no new GPUI features requiring upgrade. Every GPUI primitive this plan uses (`cx.spawn`/`.detach()`, `cx.background_executor().timer`, `uniform_list` + `cx.processor`, `cx.subscribe`/`EventEmitter`, `cx.emit`) already has call sites in this codebase (cited per task).
- Cargo commands always use `-p <crate>` (never bare `cargo test` — the GPUI tree is huge). Cargo is at `%USERPROFILE%\.cargo\bin\cargo.exe`.
- dbc-ui phases touch `main.rs`, so tasks editing `main.rs` must be sequential; this plan's design keeps pure-logic modules (T1/T2's new files) parallelizable in worktrees and serializes the `main.rs`-touching tasks (T4 → T5 → T6) as a single chain ending in the T6 wiring tail. See "Task ordering" at the end.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched. New modules that are compiled before their consumer task lands carry a `#[allow(dead_code)]` on the `mod` declaration with a removal note; T6 removes them (precedent: `tabs.rs`'s own `#[allow(dead_code)]` on the then-unconsumed `Text` variant).
- UI strings are Czech (labels, statuses, error messages) — English only in code/comments/tests. Kill refusal message is the design's literal: `"spojení je pouze pro čtení — zabití procesu odmítnuto"`.
- Tests green before every commit: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (plus `-p dbc-core` for T3's doc-comment commit) must pass with the task's new tests included. `dbc-ui`'s baseline count is in flux on this branch (mid-flight G6 work); each G9 task must leave `dbc-ui` at least as green as it found it, plus its own new tests passing. Docker tests (T7) are `#[ignore]`d and run explicitly via `-- --ignored`.
- Version bump in `crates/dbc-ui/Cargo.toml` at merge per the phasing table's minor-equals-phase-number convention (`G9 → 0.9.0`). The crate is `0.5.0` at the time of writing (G6's `0.6.0` bump not yet landed) — at merge time set it to `0.9.0` regardless of whether G7/G8 shipped (the table maps phase number to minor directly; G9 is explicitly allowed to be pulled forward).

### Task dependency graph (design §8, adapted — see Self-Review note 4 for the T2/T3 merge)

| Task | Depends on | Files | Notes |
|---|---|---|---|
| T1 monitor data model + pure parsers | — | `monitor.rs` (new) | parallel batch |
| T2 SQL constants pg + mssql + `kill_sql` | — | `monitor_sql.rs` (new) | parallel batch |
| T3 `QueryRunner::open_monitor` + `monitor_loop` + REQUIRED read-only kill test | T1, T2 | `runner.rs`, `dbc-core/src/connection.rs` (doc only) | |
| T4 `MonitorView` entity + `TabContent::Monitor` | T3 | `monitor_view.rs` (new), `tabs.rs`, `main.rs` (match arms + helper) | main.rs chain #1 |
| T5 Kill confirm dialog | T4 | `connections_ui.rs` | main.rs chain #2 (shares AppView impl surface) |
| T6 Open action, palette entry, timer loop, subscriptions | T4, T5 | `main.rs`, `palette.rs`, spec doc | main.rs chain #3 (tail) |
| T7 Docker integration tests | T3 | `runner.rs` (test module), `dbc-ui/Cargo.toml` (dev-dep) | parallel with T4–T6 |

T1 and T2 are disjoint new files — one parallel batch (each also adds one `mod` line to `main.rs`; that is a trivially-rebased one-line textual conflict, same as G6's T4/T6 precedent). T4/T5/T6 all touch `main.rs` and/or the `AppView` impl surface in `connections_ui.rs` and are also a real dependency chain — strictly sequential. T7 only touches `runner.rs` (which no task after T3 touches) and the `Cargo.toml` dev-deps — it can run in a worktree parallel to T4–T6.

---

### Task 1 (T1): Monitor data model + pure parsing/aggregation — `monitor.rs`

**Files:**
- Create: `crates/dbc-ui/src/monitor.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `#[allow(dead_code)] // consumed from T3 on; allow removed in T6` + `mod monitor;` to the mod list at the top, alphabetically after `mod history_panel;`)

**Interfaces:**
- Consumes: nothing from other G9 tasks (pure `std` + `dbc_state::Engine` for `monitor_available` — no GPUI, no arrow, no `dbc-buffer`; cell data arrives as already-materialized text rows, the exact shape `runner::fetch_lookup_inner` already produces from a drained `ResultBuffer`, so tests here run against plain vectors — same decoupling precedent as `row_view.rs`'s closure-fed cell access).
- Produces (consumed by T3, T4, T6):
  ```rust
  /// One result row as drained text cells; `None` = SQL NULL (same shape
  /// as `runner::LookupResult`'s rows).
  pub type Row = Vec<Option<String>>;

  #[derive(Debug, Clone, PartialEq)]
  pub struct ConnectionsTile { pub active: i64, pub idle: i64, pub max: Option<i64> } // max None = "neomezeno"
  #[derive(Debug, Clone, PartialEq)]
  pub struct LocksTile { pub waiting: i64, pub deadlocks_since_reset: i64 }
  #[derive(Debug, Clone, PartialEq)]
  pub struct SizeTile { pub data_bytes: Option<i64>, pub wal_or_log_bytes: Option<i64> } // None = that half's query failed ("n/a")
  #[derive(Debug, Clone, PartialEq)]
  pub struct PerfTile {
      pub cache_hit_pct: Option<f64>,
      pub uptime_secs: i64,
      /// Raw cumulative commit+rollback counter — carried so the VIEW can
      /// compute the client-side delta (design §1 "TPS"); the parser never
      /// fills `tps` itself.
      pub xact_total: Option<i64>,
      pub tps: Option<f64>, // None until the 2nd refresh (or after a counter reset)
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct RunningQueryRow {
      pub pid: i64, pub user: Option<String>, pub application: Option<String>,
      pub client: Option<String>, pub state: Option<String>, pub duration_secs: f64,
      pub query: Option<String>,
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct BlockingEdge { pub waiter_pid: i64, pub blocker_pid: i64, pub wait_secs: f64, pub waiter_query: Option<String>, pub blocker_query: Option<String> }
  #[derive(Debug, Clone, PartialEq)]
  pub struct BlockingNode { pub pid: i64, pub query: Option<String>, pub wait_secs: Option<f64>, pub children: Vec<BlockingNode>, pub cycle: bool }
  #[derive(Debug, Clone, PartialEq)]
  pub struct TableSizeRow { pub schema: Option<String>, pub table: String, pub data_bytes: i64, pub index_bytes: i64, pub toast_bytes: i64, pub row_estimate: i64 }

  /// Per-tile/section `Option`: `None` = that query failed this refresh
  /// (render "n/a"), `Some` = parsed (possibly empty) data. This is the
  /// concrete encoding of design §1's "a permission-denied sub-query fails
  /// that ONE query, not the whole refresh" + risk #4.
  #[derive(Debug, Clone, PartialEq)]
  pub struct MonitorSnapshot {
      pub connections: Option<ConnectionsTile>,
      pub locks: Option<LocksTile>,
      pub size: SizeTile,
      pub perf: Option<PerfTile>,
      pub running: Option<Vec<RunningQueryRow>>,
      pub blocking: Option<Vec<BlockingNode>>,
      pub tables: Option<Vec<TableSizeRow>>,
      pub fetched_at: std::time::Instant,
  }

  /// The 8 per-statement outcomes of one refresh, in `monitor_sql::pg`
  /// order (T3 fills this; `Err` is the driver's error message).
  #[derive(Debug, Clone, PartialEq)]
  pub struct RefreshResults {
      pub connections: Result<Vec<Row>, String>,
      pub locks: Result<Vec<Row>, String>,
      pub data_size: Result<Vec<Row>, String>,
      pub wal_size: Result<Vec<Row>, String>,
      pub perf: Result<Vec<Row>, String>,
      pub running: Result<Vec<Row>, String>,
      pub blocking: Result<Vec<Row>, String>,
      pub tables: Result<Vec<Row>, String>,
  }

  pub fn parse_connections(rows: &[Row]) -> ConnectionsTile;
  pub fn parse_locks(rows: &[Row]) -> LocksTile;
  pub fn parse_perf(rows: &[Row]) -> PerfTile;          // tps always None here
  pub fn parse_running(rows: &[Row]) -> Vec<RunningQueryRow>;
  pub fn parse_blocking_edges(rows: &[Row]) -> Vec<BlockingEdge>;
  pub fn parse_tables(rows: &[Row]) -> Vec<TableSizeRow>;

  /// Blocker-as-parent tree; roots = blockers that never appear as a
  /// waiter. Cycle-safe: a wait-for cycle IS a live deadlock-in-progress —
  /// path-tracked, marked `cycle: true` instead of recursing forever; a
  /// pure cycle (no root at all) still renders, rooted at its
  /// first-listed blocker.
  pub fn build_blocking_tree(edges: &[BlockingEdge]) -> Vec<BlockingNode>;

  /// Client-side delta over a CUMULATIVE counter. `None` on: no previous
  /// sample, non-positive elapsed, or a counter that went BACKWARD
  /// (stats reset / server restart).
  pub fn compute_rate(now_total: i64, prev: Option<(i64, std::time::Instant)>, at: std::time::Instant) -> Option<f64>;

  /// 0.0..=1.0 fill fraction for the per-table size bar; `max_in_set <= 0`
  /// or `size <= 0` -> 0.0.
  pub fn bar_fraction(size: i64, max_in_set: i64) -> f32;

  /// `Err(first error message)` iff EVERY query failed (drives the view's
  /// backoff); otherwise `Ok` with per-tile `None`s for the failures.
  pub fn assemble_snapshot(r: RefreshResults, fetched_at: std::time::Instant) -> Result<MonitorSnapshot, String>;

  /// Engine gating (design §7): Postgres only. Sqlite -> false (spec: no
  /// monitor tab); Mssql -> false FOR NOW — flips automatically once
  /// `connect::open_config`'s `Engine::Mssql` arm stops erroring and this
  /// one function is updated; no other monitor-side change needed.
  pub fn monitor_available(engine: dbc_state::Engine) -> bool;

  /// "1.5 GB" / "512 B" — tile + table-size labels.
  pub fn fmt_bytes(bytes: i64) -> String;
  /// "3d 4h 12m" / "4h 12m" / "12m" — uptime label. Negative clamps to 0.
  pub fn fmt_uptime(secs: i64) -> String;
  ```

**Grounding — column order contract:** each parse function reads cells by POSITION, and the positions are pinned to `monitor_sql::pg`'s SELECT lists (T2): `parse_connections` → `[active, idle, max_conn]`; `parse_locks` → `[waiting, deadlocks_since_reset]`; `parse_perf` → `[cache_hit_pct, uptime_secs, xact_total]`; `parse_running` → `[pid, user, application, client, state, duration_secs, query]`; `parse_blocking_edges` → `[waiter_pid, blocker_pid, wait_secs, waiter_query, blocker_query]`; `parse_tables` → `[schema, table, data_bytes, index_bytes, toast_bytes, row_estimate]`. T2's constants carry the mirror comment ("column order is monitor.rs's parse contract — change both together"). Cell readers are row-level private helpers:

```rust
fn col_str(row: &Row, c: usize) -> Option<String> {
    row.get(c).and_then(|v| v.clone())
}
fn col_i64(row: &Row, c: usize) -> Option<i64> {
    let v = row.get(c)?.as_deref()?.trim().to_string();
    // Numeric aggregates (sum() over bigint) come back as arbitrary-precision
    // numeric text on Postgres — accept "123" and "123.0" alike (fail-soft).
    v.parse::<i64>().ok().or_else(|| v.parse::<f64>().ok().map(|f| f as i64))
}
fn col_f64(row: &Row, c: usize) -> Option<f64> {
    row.get(c)?.as_deref()?.trim().parse::<f64>().ok()
}
fn cell_i64(rows: &[Row], r: usize, c: usize) -> Option<i64> { col_i64(rows.get(r)?, c) }
fn cell_f64(rows: &[Row], r: usize, c: usize) -> Option<f64> { col_f64(rows.get(r)?, c) }
```

**Grounding — the tree builder** (the one non-trivial algorithm; this exact shape is what Step 3 implements):

```rust
pub fn build_blocking_tree(edges: &[BlockingEdge]) -> Vec<BlockingNode> {
    use std::collections::HashSet;
    let mut covered: HashSet<i64> = HashSet::new();
    let mut roots = Vec::new();
    // Pass 1: true roots — blockers that never wait on anyone.
    for e in edges {
        let is_root = !edges.iter().any(|x| x.waiter_pid == e.blocker_pid);
        if is_root && covered.insert(e.blocker_pid) {
            roots.push(build_node(e.blocker_pid, None, edges, &mut Vec::new(), &mut covered));
        }
    }
    // Pass 2: pure cycles have NO root (every participant waits on someone)
    // — a live deadlock-in-progress must still render, so start one tree
    // per still-uncovered blocker; the path check below marks the loop
    // closure with `cycle: true` instead of recursing forever.
    for e in edges {
        if covered.insert(e.blocker_pid) {
            roots.push(build_node(e.blocker_pid, None, edges, &mut Vec::new(), &mut covered));
        }
    }
    roots
}

fn build_node(
    pid: i64,
    via: Option<&BlockingEdge>, // the edge this node was reached through (None for a root)
    edges: &[BlockingEdge],
    path: &mut Vec<i64>,
    covered: &mut std::collections::HashSet<i64>,
) -> BlockingNode {
    let query = match via {
        Some(e) => e.waiter_query.clone(),
        None => edges.iter().find(|e| e.blocker_pid == pid).and_then(|e| e.blocker_query.clone()),
    };
    let wait_secs = via.map(|e| e.wait_secs);
    if path.contains(&pid) {
        return BlockingNode { pid, query, wait_secs, children: Vec::new(), cycle: true };
    }
    path.push(pid);
    let children = edges
        .iter()
        .filter(|e| e.blocker_pid == pid)
        .map(|e| {
            covered.insert(e.waiter_pid);
            build_node(e.waiter_pid, Some(e), edges, path, covered)
        })
        .collect();
    path.pop();
    BlockingNode { pid, query, wait_secs, children, cycle: false }
}
```

**Grounding — `assemble_snapshot`** (borrow the 8 results immutably for the all-failed check BEFORE moving them):

```rust
pub fn assemble_snapshot(r: RefreshResults, fetched_at: std::time::Instant) -> Result<MonitorSnapshot, String> {
    let (any_ok, first_err) = {
        let all: [&Result<Vec<Row>, String>; 8] = [
            &r.connections, &r.locks, &r.data_size, &r.wal_size,
            &r.perf, &r.running, &r.blocking, &r.tables,
        ];
        (all.iter().any(|x| x.is_ok()), all.iter().find_map(|x| x.as_ref().err().cloned()))
    };
    if !any_ok {
        return Err(first_err.unwrap_or_else(|| "prázdná odpověď monitoru".to_string()));
    }
    Ok(MonitorSnapshot {
        connections: r.connections.ok().map(|rows| parse_connections(&rows)),
        locks: r.locks.ok().map(|rows| parse_locks(&rows)),
        size: SizeTile {
            data_bytes: r.data_size.ok().and_then(|rows| cell_i64(&rows, 0, 0)),
            wal_or_log_bytes: r.wal_size.ok().and_then(|rows| cell_i64(&rows, 0, 0)),
        },
        perf: r.perf.ok().map(|rows| parse_perf(&rows)),
        running: r.running.ok().map(|rows| parse_running(&rows)),
        blocking: r.blocking.ok().map(|rows| build_blocking_tree(&parse_blocking_edges(&rows))),
        tables: r.tables.ok().map(|rows| parse_tables(&rows)),
        fetched_at,
    })
}
```

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/monitor.rs`, `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn row(cells: &[Option<&str>]) -> Row {
        cells.iter().map(|c| c.map(str::to_string)).collect()
    }

    // --- fail-soft parsing ---

    #[test]
    fn parse_connections_reads_by_position() {
        let rows = vec![row(&[Some("3"), Some("7"), Some("100")])];
        assert_eq!(
            parse_connections(&rows),
            ConnectionsTile { active: 3, idle: 7, max: Some(100) }
        );
    }

    #[test]
    fn parse_connections_garbage_and_null_degrade_not_panic() {
        // Garbage text -> 0 (fail-soft, design §1); NULL max -> None.
        let rows = vec![row(&[Some("oops"), None, None])];
        assert_eq!(parse_connections(&rows), ConnectionsTile { active: 0, idle: 0, max: None });
        // No rows at all (permission-hidden result) — same degrade.
        assert_eq!(parse_connections(&[]), ConnectionsTile { active: 0, idle: 0, max: None });
    }

    #[test]
    fn parse_perf_carries_raw_counter_and_never_fills_tps() {
        let rows = vec![row(&[Some("99.12"), Some("86400"), Some("123456")])];
        let tile = parse_perf(&rows);
        assert_eq!(tile.cache_hit_pct, Some(99.12));
        assert_eq!(tile.uptime_secs, 86_400);
        assert_eq!(tile.xact_total, Some(123_456));
        assert_eq!(tile.tps, None);
    }

    #[test]
    fn numeric_aggregate_decimal_text_still_parses_as_i64() {
        // Postgres sum() over bigint returns numeric — "123456.0" text.
        let rows = vec![row(&[None, Some("5"), Some("123456.0")])];
        assert_eq!(parse_perf(&rows).xact_total, Some(123_456));
    }

    #[test]
    fn parse_running_maps_all_seven_columns_and_defaults_bad_cells() {
        let rows = vec![
            row(&[Some("42"), Some("app_user"), Some("dbc"), Some("10.0.0.5"),
                  Some("active"), Some("12.5"), Some("SELECT 1")]),
            row(&[Some("43"), None, None, None, None, None, None]),
        ];
        let parsed = parse_running(&rows);
        assert_eq!(parsed[0], RunningQueryRow {
            pid: 42,
            user: Some("app_user".into()),
            application: Some("dbc".into()),
            client: Some("10.0.0.5".into()),
            state: Some("active".into()),
            duration_secs: 12.5,
            query: Some("SELECT 1".into()),
        });
        assert_eq!(parsed[1].pid, 43);
        assert_eq!(parsed[1].duration_secs, 0.0); // NULL duration (queued query) -> 0, not a crash
        assert_eq!(parsed[1].query, None);
    }

    // --- compute_rate ---

    #[test]
    fn compute_rate_first_refresh_is_none() {
        assert_eq!(compute_rate(100, None, Instant::now()), None);
    }

    #[test]
    fn compute_rate_delta_over_elapsed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(5);
        let tps = compute_rate(150, Some((100, t0)), t1).unwrap();
        assert!((tps - 10.0).abs() < 1e-9);
    }

    #[test]
    fn compute_rate_zero_elapsed_is_none() {
        let t0 = Instant::now();
        assert_eq!(compute_rate(150, Some((100, t0)), t0), None);
    }

    #[test]
    fn compute_rate_counter_reset_is_none_not_negative() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(compute_rate(50, Some((100, t0)), t1), None);
    }

    // --- build_blocking_tree ---

    fn edge(waiter: i64, blocker: i64) -> BlockingEdge {
        BlockingEdge {
            waiter_pid: waiter,
            blocker_pid: blocker,
            wait_secs: 1.5,
            waiter_query: Some(format!("q{waiter}")),
            blocker_query: Some(format!("q{blocker}")),
        }
    }

    #[test]
    fn linear_chain_nests_waiters_under_blockers() {
        // 30 waits on 20, 20 waits on 10 -> one root (10) with 20 under it
        // and 30 under 20.
        let tree = build_blocking_tree(&[edge(20, 10), edge(30, 20)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, 10);
        assert_eq!(tree[0].wait_secs, None); // a root isn't waiting
        assert!(!tree[0].cycle);
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].pid, 20);
        assert_eq!(tree[0].children[0].wait_secs, Some(1.5));
        assert_eq!(tree[0].children[0].children[0].pid, 30);
    }

    #[test]
    fn two_independent_blockers_are_two_roots() {
        let tree = build_blocking_tree(&[edge(2, 1), edge(4, 3)]);
        let root_pids: Vec<i64> = tree.iter().map(|n| n.pid).collect();
        assert_eq!(root_pids, vec![1, 3]);
    }

    #[test]
    fn cycle_is_marked_not_infinite() {
        // 1 waits on 2, 2 waits on 1 — a live deadlock-in-progress. No true
        // root; pass 2 roots it at the first-listed blocker and the loop
        // closure is a `cycle: true` leaf.
        let tree = build_blocking_tree(&[edge(1, 2), edge(2, 1)]);
        assert_eq!(tree.len(), 1);
        fn find_cycle(n: &BlockingNode) -> bool {
            n.cycle || n.children.iter().any(find_cycle)
        }
        assert!(find_cycle(&tree[0]), "the cycle closure must be marked");
    }

    #[test]
    fn self_referential_edge_defensive_case() {
        // The pg query excludes blocking.pid = blocked.pid, but the builder
        // must survive a self-edge anyway (defensive, design §1).
        let tree = build_blocking_tree(&[edge(5, 5)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, 5);
        assert!(tree[0].children.iter().all(|c| c.cycle));
    }

    #[test]
    fn no_edges_no_tree() {
        assert_eq!(build_blocking_tree(&[]), Vec::new());
    }

    // --- bar_fraction ---

    #[test]
    fn bar_fraction_bounds() {
        assert_eq!(bar_fraction(50, 100), 0.5);
        assert_eq!(bar_fraction(100, 100), 1.0);
        assert_eq!(bar_fraction(0, 100), 0.0);
        assert_eq!(bar_fraction(100, 0), 0.0);  // max_in_set == 0 guard (design §1)
        assert_eq!(bar_fraction(-5, 100), 0.0);
    }

    // --- assemble_snapshot ---

    fn all_ok() -> RefreshResults {
        RefreshResults {
            connections: Ok(vec![row(&[Some("1"), Some("2"), Some("10")])]),
            locks: Ok(vec![row(&[Some("0"), Some("3")])]),
            data_size: Ok(vec![row(&[Some("1024")])]),
            wal_size: Ok(vec![row(&[Some("2048")])]),
            perf: Ok(vec![row(&[Some("95.5"), Some("60"), Some("1000")])]),
            running: Ok(vec![]),
            blocking: Ok(vec![]),
            tables: Ok(vec![]),
        }
    }

    #[test]
    fn assemble_full_success() {
        let snap = assemble_snapshot(all_ok(), Instant::now()).unwrap();
        assert_eq!(snap.connections, Some(ConnectionsTile { active: 1, idle: 2, max: Some(10) }));
        assert_eq!(snap.size, SizeTile { data_bytes: Some(1024), wal_or_log_bytes: Some(2048) });
        assert_eq!(snap.running, Some(vec![]));
        assert_eq!(snap.blocking, Some(vec![]));
    }

    #[test]
    fn assemble_partial_failure_degrades_only_the_failed_tile() {
        // The design's canonical case (§2 caveat): pg_ls_waldir permission-
        // denied fails the WAL half; the data half and everything else
        // survive. Risk #4's explicit partial-failure requirement.
        let mut r = all_ok();
        r.wal_size = Err("permission denied for function pg_ls_waldir".into());
        let snap = assemble_snapshot(r, Instant::now()).unwrap();
        assert_eq!(snap.size, SizeTile { data_bytes: Some(1024), wal_or_log_bytes: None });
        assert!(snap.connections.is_some());
        assert!(snap.running.is_some());
    }

    #[test]
    fn assemble_all_failed_is_err_with_first_message() {
        let dead = || Err::<Vec<Row>, String>("connection closed".to_string());
        let r = RefreshResults {
            connections: dead(), locks: dead(), data_size: dead(), wal_size: dead(),
            perf: dead(), running: dead(), blocking: dead(), tables: dead(),
        };
        assert_eq!(assemble_snapshot(r, Instant::now()), Err("connection closed".to_string()));
    }

    // --- gating + formatting ---

    #[test]
    fn monitor_available_postgres_only() {
        assert!(monitor_available(dbc_state::Engine::Postgres));
        assert!(!monitor_available(dbc_state::Engine::Sqlite));
        assert!(!monitor_available(dbc_state::Engine::Mssql)); // until the driver lands (design §7)
    }

    #[test]
    fn fmt_bytes_scales() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn fmt_uptime_tiers() {
        assert_eq!(fmt_uptime(59), "0m");
        assert_eq!(fmt_uptime(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(fmt_uptime(2 * 86_400 + 3600), "2d 1h 0m");
        assert_eq!(fmt_uptime(-5), "0m"); // clock skew clamps, never panics
    }
}
```

- [ ] **Step 2: Run to see the tests fail (module doesn't exist yet)**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor::`
Expected: compile error (`monitor` module doesn't exist).

- [ ] **Step 3: Implement** everything in the Interfaces block exactly: the structs, `col_str`/`col_i64`/`col_f64`/`cell_i64`/`cell_f64` helpers, `parse_*` (position-based, fail-soft: `col_i64(..).unwrap_or(0)` for counts, `Option` passthrough for nullable text), `build_blocking_tree`/`build_node` per the grounding code, `compute_rate`, `bar_fraction` (`if max_in_set <= 0 || size <= 0 { return 0.0 } ((size as f64 / max_in_set as f64) as f32).clamp(0.0, 1.0)`), `assemble_snapshot` per the grounding code, `monitor_available`, `fmt_bytes` (1024 steps through B/KB/MB/GB/TB, one decimal above B), `fmt_uptime` (`let secs = secs.max(0);` then d/h/m tiers).

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor::`
Expected: all new tests pass, zero warnings (the `#[allow(dead_code)]` on the `mod` declaration covers the not-yet-consumed items).

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/monitor.rs crates/dbc-ui/src/main.rs
git commit -m "feat: monitor data model + pure parsers (G9 T1)"
```

---

### Task 2 (T2): Engine SQL constants + `kill_sql` — `monitor_sql.rs`

**Files:**
- Create: `crates/dbc-ui/src/monitor_sql.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `#[allow(dead_code)] // consumed from T3 on; allow removed in T6` + `mod monitor_sql;` after `mod monitor;`)

**Interfaces:**
- Consumes: `dbc_state::Engine` (for `kill_sql`), `dbc_core::is_read_statement` (tests only).
- Produces (consumed by T3, T4):
  ```rust
  pub mod pg {
      // 8 statements, run sequentially in THIS order per refresh; each
      // SELECT list's column order is monitor.rs's parse contract.
      pub const CONNECTIONS: &str; pub const LOCKS: &str;
      pub const DATA_SIZE: &str;  pub const WAL_SIZE: &str;
      pub const PERF: &str;       pub const RUNNING: &str;
      pub const BLOCKING: &str;   pub const TABLES: &str;
  }
  pub mod mssql { /* design §3's statements, compiled + smoke-tested, no driver to run them */ }

  /// The kill statement (design §6): pid comes ONLY from a just-fetched
  /// `RunningQueryRow`/`BlockingNode` (an i64 out of the driver's own typed
  /// result, never user-typed text) — formatted as a bare integer, no
  /// quoting layer involved. `None` for SQLite (no monitor tab at all).
  pub fn kill_sql(engine: dbc_state::Engine, pid: i64) -> Option<String>;
  ```

**Grounding:** SQL bodies are the design's §2/§3 blocks verbatim, with ONE deliberate change: the pg size query is SPLIT into `DATA_SIZE` + `WAL_SIZE` (two statements instead of the design's combined one), because `pg_ls_waldir()` raises a real permission-denied error for a non-`pg_monitor` role and the design's own §2 caveat requires "the size tile's WAL half must degrade to n/a independently of the data-size half succeeding" — impossible if both live in one statement that errors as a unit. Both are plain autocommit SELECTs, so a failed `WAL_SIZE` poisons nothing for the statements after it (no open transaction to abort). This makes the refresh 8 statements, not 7 (flagged in Self-Review note 1).

- [ ] **Step 1: Write the module with the constants** (`crates/dbc-ui/src/monitor_sql.rs`) — constants first (they ARE the deliverable; the tests in Step 2 then pin their properties):

```rust
//! G9: engine-specific monitor SQL. Every SELECT list is written so every
//! value the client needs is already a number or text (durations as
//! `extract(epoch ...)::float8` / `DATEDIFF(SECOND, ...)`) — never a
//! native timestamp the client would have to parse (design §2 parse
//! strategy). Column ORDER is monitor.rs's parse contract — change both
//! together.

pub mod pg {
    /// `[active, idle, max_conn]`
    pub const CONNECTIONS: &str = "\
SELECT
  count(*) FILTER (WHERE state = 'active') AS active,
  count(*) FILTER (WHERE state = 'idle')   AS idle,
  current_setting('max_connections')::int  AS max_conn
FROM pg_stat_activity WHERE datname = current_database()";

    /// `[waiting, deadlocks_since_reset]`
    pub const LOCKS: &str = "\
SELECT
  (SELECT count(*) FROM pg_locks WHERE NOT granted) AS waiting,
  (SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()) AS deadlocks_since_reset";

    /// `[data_bytes]` — split from WAL_SIZE so a pg_ls_waldir permission
    /// error can't take the data half down with it (design §2 caveat).
    pub const DATA_SIZE: &str = "SELECT pg_database_size(current_database()) AS data_bytes";

    /// `[wal_bytes]` — requires pg_monitor/superuser; failure degrades the
    /// WAL half of the tile to "n/a" (design §2 caveat).
    pub const WAL_SIZE: &str =
        "SELECT coalesce(sum(size), 0)::bigint AS wal_bytes FROM pg_ls_waldir()";

    /// `[cache_hit_pct, uptime_secs, xact_total]` — xact_total is CUMULATIVE;
    /// the client computes the TPS delta (monitor::compute_rate).
    pub const PERF: &str = "\
SELECT
  round(100.0 * sum(blks_hit) / NULLIF(sum(blks_hit) + sum(blks_read), 0), 2) AS cache_hit_pct,
  extract(epoch FROM now() - pg_postmaster_start_time())::bigint AS uptime_secs,
  sum(xact_commit + xact_rollback) AS xact_total
FROM pg_stat_database";

    /// `[pid, user, application, client, state, duration_secs, query]` —
    /// excludes the monitor's own session (design §2).
    pub const RUNNING: &str = "\
SELECT pid, usename AS \"user\", application_name AS application, client_addr::text AS client,
       state, extract(epoch FROM now() - query_start)::float8 AS duration_secs, query
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid()
ORDER BY duration_secs DESC NULLS LAST
LIMIT 200";

    /// `[waiter_pid, blocker_pid, wait_secs, waiter_query, blocker_query]`
    /// — the standard "who blocks whom" pg_locks self-join (design §2).
    pub const BLOCKING: &str = "\
SELECT blocked_activity.pid AS waiter_pid, blocking_activity.pid AS blocker_pid,
       extract(epoch FROM now() - blocked_activity.query_start)::float8 AS wait_secs,
       blocked_activity.query AS waiter_query, blocking_activity.query AS blocker_query
FROM pg_locks blocked
JOIN pg_stat_activity blocked_activity ON blocked_activity.pid = blocked.pid
JOIN pg_locks blocking
  ON blocking.locktype IS NOT DISTINCT FROM blocked.locktype
 AND blocking.database  IS NOT DISTINCT FROM blocked.database
 AND blocking.relation  IS NOT DISTINCT FROM blocked.relation
 AND blocking.page      IS NOT DISTINCT FROM blocked.page
 AND blocking.tuple     IS NOT DISTINCT FROM blocked.tuple
 AND blocking.transactionid IS NOT DISTINCT FROM blocked.transactionid
 AND blocking.classid   IS NOT DISTINCT FROM blocked.classid
 AND blocking.objid     IS NOT DISTINCT FROM blocked.objid
 AND blocking.objsubid  IS NOT DISTINCT FROM blocked.objsubid
 AND blocking.pid <> blocked.pid
JOIN pg_stat_activity blocking_activity ON blocking_activity.pid = blocking.pid
WHERE NOT blocked.granted AND blocking.granted";

    /// `[schema, table, data_bytes, index_bytes, toast_bytes, row_estimate]`
    pub const TABLES: &str = "\
SELECT n.nspname AS schema, c.relname AS \"table\",
       pg_relation_size(c.oid) AS data_bytes,
       pg_indexes_size(c.oid) AS index_bytes,
       CASE WHEN c.reltoastrelid <> 0 THEN pg_relation_size(c.reltoastrelid) ELSE 0 END AS toast_bytes,
       c.reltuples::bigint AS row_estimate
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
ORDER BY pg_relation_size(c.oid) + pg_indexes_size(c.oid) DESC";
}

/// Target shape for when dbc-driver-mssql lands (design §3): compiled and
/// smoke-tested here so a T-SQL typo fails CI today, but NOT runnable — no
/// driver exists (`connect::open_config` hard-errors Engine::Mssql), and
/// `monitor::monitor_available` returns false for Mssql until it does.
/// dead_code allow is PERMANENT for this module (unlike the temporary mod-
/// declaration allows T6 removes) — nothing can call these until the
/// driver lands.
#[allow(dead_code)]
pub mod mssql {
    pub const CONNECTIONS: &str = "\
SELECT
  SUM(CASE WHEN status = 'running'  THEN 1 ELSE 0 END) AS active,
  SUM(CASE WHEN status = 'sleeping' THEN 1 ELSE 0 END) AS idle
FROM sys.dm_exec_sessions WHERE is_user_process = 1";

    /// value_in_use = 0 means "unlimited/dynamic" -> UI max = None.
    pub const CONNECTIONS_MAX: &str =
        "SELECT value_in_use AS max_conn FROM sys.configurations WHERE name = 'user connections'";

    pub const LOCKS_WAITING: &str =
        "SELECT COUNT(*) AS waiting FROM sys.dm_tran_locks WHERE request_status = 'WAIT'";

    /// Misleading name: despite "/sec" this is a CUMULATIVE counter (design §3).
    pub const DEADLOCKS: &str = "\
SELECT cntr_value AS deadlocks_since_reset FROM sys.dm_os_performance_counters
WHERE counter_name = 'Number of Deadlocks/sec' AND instance_name = '_Total'";

    /// Data vs log split — the sp_spaceused equivalent (design §3).
    pub const SIZE: &str = "\
SELECT
  SUM(CASE WHEN type_desc = 'ROWS' THEN size ELSE 0 END) * 8 * 1024 AS data_bytes,
  SUM(CASE WHEN type_desc = 'LOG'  THEN size ELSE 0 END) * 8 * 1024 AS log_bytes
FROM sys.database_files";

    pub const CACHE_HIT: &str = "\
SELECT (a.cntr_value * 1.0 / NULLIF(b.cntr_value, 0)) * 100 AS cache_hit_pct
FROM sys.dm_os_performance_counters a, sys.dm_os_performance_counters b
WHERE a.counter_name = 'Buffer cache hit ratio' AND b.counter_name = 'Buffer cache hit ratio base'";

    pub const UPTIME: &str =
        "SELECT DATEDIFF(SECOND, sqlserver_start_time, GETDATE()) AS uptime_secs FROM sys.dm_os_sys_info";

    /// Cumulative; client-side delta, same as pg's xact_total.
    pub const XACT_TOTAL: &str = "\
SELECT cntr_value AS xact_total FROM sys.dm_os_performance_counters
WHERE counter_name = 'Transactions/sec' AND instance_name = '_Total'";

    /// TOP 200 mirrors the pg LIMIT (design §8 perf caveat).
    pub const RUNNING: &str = "\
SELECT TOP 200 r.session_id AS pid, s.login_name AS [user], s.program_name AS application,
       s.host_name AS client, r.status AS state,
       DATEDIFF(SECOND, r.start_time, GETDATE()) AS duration_secs, t.text AS query
FROM sys.dm_exec_requests r
JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t
WHERE r.session_id <> @@SPID
ORDER BY duration_secs DESC";

    /// blocker_query is NULL when the blocker is idle-in-transaction (no
    /// active request row) — inherent to the DMV, not a bug (design §3).
    pub const BLOCKING: &str = "\
SELECT r.session_id AS waiter_pid, r.blocking_session_id AS blocker_pid,
       r.wait_time / 1000.0 AS wait_secs, tw.text AS waiter_query, tb.text AS blocker_query
FROM sys.dm_exec_requests r
CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) tw
OUTER APPLY (SELECT sql_handle FROM sys.dm_exec_requests br WHERE br.session_id = r.blocking_session_id) bx
OUTER APPLY sys.dm_exec_sql_text(bx.sql_handle) tb
WHERE r.blocking_session_id <> 0";

    /// Set-based sp_spaceused equivalent, no per-table EXEC loop (design §3).
    pub const TABLES: &str = "\
SELECT OBJECT_SCHEMA_NAME(ps.object_id) AS [schema], OBJECT_NAME(ps.object_id) AS [table],
       SUM(CASE WHEN ps.index_id IN (0,1) THEN ps.in_row_data_page_count ELSE 0 END) * 8 * 1024 AS data_bytes,
       SUM(CASE WHEN ps.index_id > 1 THEN ps.used_page_count ELSE 0 END) * 8 * 1024 AS index_bytes,
       SUM(ps.lob_used_page_count + ps.row_overflow_used_page_count) * 8 * 1024 AS toast_bytes,
       MAX(CASE WHEN ps.index_id IN (0,1) THEN ps.row_count ELSE 0 END) AS row_estimate
FROM sys.dm_db_partition_stats ps
JOIN sys.tables t ON t.object_id = ps.object_id
GROUP BY ps.object_id
ORDER BY data_bytes + index_bytes DESC";
}

/// See the Interfaces doc comment. `debug_assert` mirrors sandbox.rs's
/// "must never be constructed wrong" posture for values that only ever
/// originate from our own parsed results (design §6 pid validation).
pub fn kill_sql(engine: dbc_state::Engine, pid: i64) -> Option<String> {
    debug_assert!(pid > 0, "pid must come from a fetched RunningQueryRow/BlockingNode");
    match engine {
        dbc_state::Engine::Postgres => Some(format!("SELECT pg_terminate_backend({pid})")),
        dbc_state::Engine::Mssql => Some(format!("KILL {pid}")),
        dbc_state::Engine::Sqlite => None,
    }
}
```

- [ ] **Step 2: Write the failing/pinning tests** (same file, `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::is_read_statement;

    /// Design §8 T2: regression guard — a future edit that turns any
    /// monitor query into a write must fail CI, not just review.
    #[test]
    fn every_pg_monitor_query_is_read_only_per_guards() {
        for (name, sql) in [
            ("CONNECTIONS", pg::CONNECTIONS),
            ("LOCKS", pg::LOCKS),
            ("DATA_SIZE", pg::DATA_SIZE),
            ("WAL_SIZE", pg::WAL_SIZE),
            ("PERF", pg::PERF),
            ("RUNNING", pg::RUNNING),
            ("BLOCKING", pg::BLOCKING),
            ("TABLES", pg::TABLES),
        ] {
            assert!(is_read_statement(sql), "pg::{name} must pass is_read_statement");
        }
    }

    /// Design §8 T3: leading-keyword smoke only — is_read_statement's
    /// keyword set is pg/sqlite-flavoured and NOT authoritative for T-SQL,
    /// so this asserts the weaker property that holds regardless.
    #[test]
    fn every_mssql_monitor_query_starts_with_select() {
        for (name, sql) in [
            ("CONNECTIONS", mssql::CONNECTIONS),
            ("CONNECTIONS_MAX", mssql::CONNECTIONS_MAX),
            ("LOCKS_WAITING", mssql::LOCKS_WAITING),
            ("DEADLOCKS", mssql::DEADLOCKS),
            ("SIZE", mssql::SIZE),
            ("CACHE_HIT", mssql::CACHE_HIT),
            ("UPTIME", mssql::UPTIME),
            ("XACT_TOTAL", mssql::XACT_TOTAL),
            ("RUNNING", mssql::RUNNING),
            ("BLOCKING", mssql::BLOCKING),
            ("TABLES", mssql::TABLES),
        ] {
            assert!(
                sql.trim_start().to_ascii_uppercase().starts_with("SELECT"),
                "mssql::{name} must lead with SELECT"
            );
        }
    }

    #[test]
    fn kill_sql_per_engine() {
        assert_eq!(
            kill_sql(dbc_state::Engine::Postgres, 1234),
            Some("SELECT pg_terminate_backend(1234)".to_string())
        );
        assert_eq!(kill_sql(dbc_state::Engine::Mssql, 55), Some("KILL 55".to_string()));
        assert_eq!(kill_sql(dbc_state::Engine::Sqlite, 1), None);
    }

    /// Documents design §0's rejected alternative: the pg kill statement
    /// PASSES is_read_statement (leading SELECT, no WRITE_KEYWORDS token),
    /// which is exactly WHY it must never travel through query() — the
    /// read-only guard there would not catch it. If this test ever fails,
    /// the §0 rationale changed and the routing decision must be revisited.
    #[test]
    fn pg_kill_statement_would_evade_the_read_guard_hence_execute_only() {
        let sql = kill_sql(dbc_state::Engine::Postgres, 1).unwrap();
        assert!(is_read_statement(&sql));
    }
}
```

- [ ] **Step 3: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor_sql::`
Expected: all pass, zero warnings. (If Step 1 and 2 are authored together the "see it fail" cycle is the first run with a deliberately-broken constant name in the test — verify the test actually executes by temporarily asserting `!is_read_statement(pg::CONNECTIONS)` and watching it fail, then restore.)

- [ ] **Step 4: Commit**

```bash
git add crates/dbc-ui/src/monitor_sql.rs crates/dbc-ui/src/main.rs
git commit -m "feat: monitor SQL constants pg+mssql, kill_sql (G9 T2)"
```

---

### Task 3 (T3): `QueryRunner::open_monitor` — background task + kill gate — `runner.rs`

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`
- Modify: `crates/dbc-core/src/connection.rs` (doc comment ONLY — design §0's mandated update)

**Interfaces:**
- Consumes: `monitor::{Row, RefreshResults, MonitorSnapshot, assemble_snapshot}` (T1), `monitor_sql::{pg, kill_sql}` (T2), existing `open_spec`/`ConnectSpec`/`CHANNEL_CAPACITY`/`dbc_buffer::ResultBuffer`.
- Produces (consumed by T4, T6):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum MonitorCmd {
      Refresh { generation: u64 },
      Kill { generation: u64, pid: i64 },
  }

  #[derive(Debug)]
  pub enum MonitorEvent {
      Data { generation: u64, snapshot: monitor::MonitorSnapshot },
      Error { generation: u64, message: String },
      KillResult { generation: u64, pid: i64, result: Result<u64, QueryError> },
  }

  /// Design §0/§9.1: the app-level read_only flag is the ONLY kill
  /// enforcement — this exact message is what the background task returns
  /// when it independently refuses a Kill.
  pub const MONITOR_READ_ONLY_KILL_MSG: &str =
      "spojení je pouze pro čtení — zabití procesu odmítnuto";

  impl QueryRunner {
      /// Opens ONE dedicated connection held for the tab's lifetime — no
      /// reconnect per tick (design §4). `read_only` and `engine` are
      /// captured once at open time; the task refuses Kill on its OWN
      /// captured read_only, independent of whatever the UI renders
      /// (belt-and-braces, design §6). Dropping the returned Sender ends
      /// the loop and drops the connection ("drop tears everything down",
      /// same as OpenConnection/Tunnel).
      pub fn open_monitor(
          &self,
          spec: ConnectSpec,
          read_only: bool,
          engine: dbc_state::Engine,
      ) -> (tokio::sync::mpsc::Sender<MonitorCmd>, tokio::sync::mpsc::Receiver<MonitorEvent>);
  }
  ```

**Grounding:** the loop, refresh driver, and drain helper are all free functions so the REQUIRED read-only test can drive `monitor_loop` directly over a mock `Connection` (same factoring rationale as `drive_write_sequence` / `drive_write_sequence_bounded`, `runner.rs:314/383`). The kill write goes through `conn.execute()` — the app's second confirmed write path per design §0 — over the SAME dedicated connection the refreshes use; that satisfies `Connection::execute`'s session-sharing caveat because everything on this connection is strictly sequential inside one loop (a kill never interleaves with an in-flight refresh: the `tokio::select!` below cancels the refresh first). All refresh statements are autocommit SELECTs, so a permission-denied statement poisons nothing for the statements after it (no open transaction to abort — contrast with `drive_write_sequence`'s BEGIN…COMMIT rules).

```rust
/// Bounds a runaway monitor sub-query result (RUNNING/TABLES are already
/// LIMIT/TOP-bounded server-side; this is the defensive client-side cap,
/// same posture as LOOKUP_ROW_CAP above).
const MONITOR_ROW_CAP: usize = 10_000;

pub fn open_monitor(
    &self,
    spec: ConnectSpec,
    read_only: bool,
    engine: dbc_state::Engine,
) -> (tokio::sync::mpsc::Sender<MonitorCmd>, tokio::sync::mpsc::Receiver<MonitorEvent>) {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    let handle = self.handle();
    self.runtime.spawn(async move {
        let opened = match open_spec(spec, handle).await {
            Ok(o) => o,
            Err(e) => {
                // Report against the FIRST dispatched command's generation
                // (MonitorView sends Refresh{1} immediately on open, T4) so
                // the view's generation match doesn't silently drop the
                // connect error. If the tab already closed, just exit.
                if let Some(cmd) = cmd_rx.recv().await {
                    let generation = match cmd {
                        MonitorCmd::Refresh { generation } | MonitorCmd::Kill { generation, .. } => generation,
                    };
                    let _ = event_tx.send(MonitorEvent::Error { generation, message: e.message }).await;
                }
                return;
            }
        };
        // Keep the tunnel (if any) alive for the whole loop lifetime.
        let _tunnel = opened._tunnel;
        monitor_loop(opened.conn, engine, read_only, cmd_rx, event_tx).await;
        // conn + _tunnel drop here — DB session closed (design §4's
        // "no explicit Close command needed").
    });
    (cmd_tx, event_rx)
}

/// Free function (not a method) so tests drive it over a mock Connection
/// directly — the REQUIRED read-only kill refusal test (design §9.1
/// CURATION) depends on this seam existing.
async fn monitor_loop(
    mut conn: Box<dyn Connection>,
    engine: dbc_state::Engine,
    read_only: bool,
    mut cmd_rx: tokio::sync::mpsc::Receiver<MonitorCmd>,
    event_tx: tokio::sync::mpsc::Sender<MonitorEvent>,
) {
    let mut pending: Option<MonitorCmd> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => c,
            None => match cmd_rx.recv().await {
                Some(c) => c,
                None => return, // Sender dropped = tab closed (design §4)
            },
        };
        match cmd {
            MonitorCmd::Refresh { generation } => {
                let cancel = CancelToken::new();
                let refresh = run_monitor_refresh(&mut conn, engine, cancel.clone());
                // Race the refresh against channel activity — same
                // tokio::select! shape connect_and_run's watchdog uses
                // (design §4 "cancellation of an in-flight refresh").
                tokio::select! {
                    results = refresh => {
                        let event = match monitor::assemble_snapshot(results, Instant::now()) {
                            Ok(snapshot) => MonitorEvent::Data { generation, snapshot },
                            Err(message) => MonitorEvent::Error { generation, message },
                        };
                        if event_tx.send(event).await.is_err() {
                            return; // receiver (MonitorView) gone
                        }
                    }
                    next = cmd_rx.recv() => {
                        // A command arrived mid-refresh (only Kill or a
                        // close can — the UI's `awaiting` flag blocks new
                        // Refresh dispatches, design §4 overlap prevention).
                        cancel.cancel(); // protocol-level cancel on pg
                        match next {
                            Some(c) => pending = Some(c),
                            None => return, // tab closed mid-refresh: no stale Data/Error sent
                        }
                    }
                }
            }
            MonitorCmd::Kill { generation, pid } => {
                // BELT-AND-BRACES GATE (design §0/§6/§9.1): this check is
                // one of the TWO independent code paths implementing the
                // ONLY real enforcement — neither engine blocks kill
                // server-side (pg's default_transaction_read_only does NOT
                // stop pg_terminate_backend). It must run BEFORE any
                // conn.execute call. Deliberately a direct check rather
                // than guard_not_read_only() so the design's mandated
                // message text is exact.
                let result = if read_only {
                    Err(QueryError::msg(MONITOR_READ_ONLY_KILL_MSG))
                } else {
                    match crate::monitor_sql::kill_sql(engine, pid) {
                        Some(sql) => conn.execute(&sql, CancelToken::new()).await,
                        None => Err(QueryError::msg("kill není pro tento engine k dispozici")),
                    }
                };
                if event_tx.send(MonitorEvent::KillResult { generation, pid, result }).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// One full refresh: the 8 pg statements, strictly sequential over the ONE
/// dedicated connection (session-sharing caveat), each failure captured
/// per-statement so assemble_snapshot can degrade per-tile (risk #4).
async fn run_monitor_refresh(
    conn: &mut dyn Connection,
    engine: dbc_state::Engine,
    cancel: CancelToken,
) -> monitor::RefreshResults {
    use crate::monitor_sql::pg;
    match engine {
        dbc_state::Engine::Postgres => monitor::RefreshResults {
            connections: drain_rows(conn, pg::CONNECTIONS, &cancel).await,
            locks: drain_rows(conn, pg::LOCKS, &cancel).await,
            data_size: drain_rows(conn, pg::DATA_SIZE, &cancel).await,
            wal_size: drain_rows(conn, pg::WAL_SIZE, &cancel).await,
            perf: drain_rows(conn, pg::PERF, &cancel).await,
            running: drain_rows(conn, pg::RUNNING, &cancel).await,
            blocking: drain_rows(conn, pg::BLOCKING, &cancel).await,
            tables: drain_rows(conn, pg::TABLES, &cancel).await,
        },
        // Unreachable today: monitor_available gates open_monitor to
        // Postgres. When dbc-driver-mssql lands, this arm switches to the
        // monitor_sql::mssql statement set (design §7).
        _ => {
            let err = || Err("monitor není pro tento engine k dispozici".to_string());
            monitor::RefreshResults {
                connections: err(), locks: err(), data_size: err(), wal_size: err(),
                perf: err(), running: err(), blocking: err(), tables: err(),
            }
        }
    }
}

/// One statement -> materialized text rows. Mirrors fetch_lookup_inner's
/// throwaway-ResultBuffer drain (the tested batch-push/cell-read path, not
/// a second arrow-reading code path — design §1 parse strategy), but over
/// an EXISTING connection and returning the error as a plain message
/// String (RefreshResults' per-statement Err type).
async fn drain_rows(
    conn: &mut dyn Connection,
    sql: &str,
    cancel: &CancelToken,
) -> Result<Vec<monitor::Row>, String> {
    let mut stream = conn.query(sql, cancel.clone()).await.map_err(|e| e.message)?;
    let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
    while let Some(item) = stream.batches.recv().await {
        match item {
            Ok(b) => {
                buf.push(b).map_err(|e| e.to_string())?;
                if buf.row_count() >= MONITOR_ROW_CAP {
                    break;
                }
            }
            Err(e) => return Err(e.message),
        }
    }
    let n = buf.row_count().min(MONITOR_ROW_CAP);
    let ncols = buf.column_count();
    let mut rows = Vec::with_capacity(n);
    for r in 0..n {
        let mut row = Vec::with_capacity(ncols);
        for c in 0..ncols {
            row.push(if buf.cell_is_null(r, c) { None } else { Some(buf.cell_text(r, c)) });
        }
        rows.push(row);
    }
    Ok(rows)
}
```

`open_monitor` gets `#[allow(dead_code)] // wired by T6's open_monitor_tab; allow removed there` until T6 (tests exercise `monitor_loop` directly).

- [ ] **Step 1: Write the failing tests** (`runner.rs`, new `#[cfg(test)] mod monitor_tests` below `write_transaction_tests`):

```rust
/// G9 T3: the kill gate + loop-lifecycle tests. The read-only refusal test
/// is the design's §9.1 CURATION-REQUIRED guard-level test — the app-level
/// flag is the ONLY enforcement (no server-side backstop on either
/// engine), so it gets the same test discipline as guards.rs.
#[cfg(test)]
mod monitor_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Mock driver: records every execute() call; query() errors (a test
    /// that never sends Refresh never reaches it).
    struct RecordingConnection {
        execute_calls: Arc<AtomicUsize>,
        executed_sql: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingConnection {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let sqls = Arc::new(Mutex::new(Vec::new()));
            (
                Self { execute_calls: calls.clone(), executed_sql: sqls.clone() },
                calls,
                sqls,
            )
        }
    }

    #[async_trait::async_trait]
    impl Connection for RecordingConnection {
        async fn query(
            &mut self,
            _sql: &str,
            _cancel: CancelToken,
        ) -> Result<dbc_core::QueryStream, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
            Err(QueryError::msg("not exercised by this test"))
        }
        async fn execute(&mut self, sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            self.executed_sql.lock().unwrap().push(sql.to_string());
            Ok(1)
        }
    }

    /// REQUIRED (design §9.1 CURATION): a Kill over a read_only connection
    /// is refused BEFORE reaching the driver — conn.execute is never
    /// called, and the exact Czech refusal message comes back, independent
    /// of whatever the UI renders.
    #[tokio::test]
    async fn monitor_kill_refused_on_read_only_before_reaching_driver() {
        let (conn, calls, _sqls) = RecordingConnection::new();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            Box::new(conn),
            dbc_state::Engine::Postgres,
            /* read_only */ true,
            cmd_rx,
            event_tx,
        ));

        cmd_tx.send(MonitorCmd::Kill { generation: 7, pid: 42 }).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("event within 5s")
            .expect("channel open");
        match ev {
            MonitorEvent::KillResult { generation, pid, result } => {
                assert_eq!(generation, 7);
                assert_eq!(pid, 42);
                assert_eq!(result.unwrap_err().message, MONITOR_READ_ONLY_KILL_MSG);
            }
            other => panic!("expected KillResult, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "read-only kill must never reach Connection::execute"
        );

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .expect("loop exits when the Sender drops")
            .unwrap();
    }

    /// Companion positive case: writable connection -> exactly one
    /// execute() with the exact pid-formatted kill SQL, Ok result echoed.
    #[tokio::test]
    async fn monitor_kill_executes_exact_sql_on_writable_connection() {
        let (conn, calls, sqls) = RecordingConnection::new();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            Box::new(conn),
            dbc_state::Engine::Postgres,
            /* read_only */ false,
            cmd_rx,
            event_tx,
        ));

        cmd_tx.send(MonitorCmd::Kill { generation: 1, pid: 42 }).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            MonitorEvent::KillResult { pid: 42, result: Ok(1), .. } => {}
            other => panic!("expected Ok KillResult, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sqls.lock().unwrap().as_slice(),
            &["SELECT pg_terminate_backend(42)".to_string()]
        );

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.unwrap().unwrap();
    }

    /// End-to-end all-failed path without docker: a real sqlite connection
    /// can't run any pg catalog query, so every drain fails and the loop
    /// must send Error (with the dispatched generation), not Data and not
    /// a panic — proving assemble_snapshot's all-failed contract through
    /// the real drain path.
    #[tokio::test]
    async fn monitor_refresh_all_queries_failing_sends_error_with_generation() {
        let f = tempfile::NamedTempFile::new().expect("temp file");
        let handle = tokio::runtime::Handle::current();
        let conn =
            crate::connect::open(f.path().to_str().expect("utf8 path"), &handle).expect("open sqlite");
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            conn,
            dbc_state::Engine::Postgres,
            false,
            cmd_rx,
            event_tx,
        ));

        cmd_tx.send(MonitorCmd::Refresh { generation: 3 }).await.unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(10), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            MonitorEvent::Error { generation: 3, message } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected Error{{generation: 3}}, got {other:?}"),
        }

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.unwrap().unwrap();
        drop(f);
    }
}
```

- [ ] **Step 2: Run to see the tests fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor_tests::`
Expected: compile error (`MonitorCmd`/`MonitorEvent`/`monitor_loop` don't exist).

- [ ] **Step 3: Implement** `MonitorCmd`, `MonitorEvent`, `MONITOR_READ_ONLY_KILL_MSG`, `MONITOR_ROW_CAP`, `QueryRunner::open_monitor`, `monitor_loop`, `run_monitor_refresh`, `drain_rows` — exactly per the grounding code above. Also remove the two temporary `#[allow(dead_code)]` mod-declaration attributes from T1/T2 IF everything in those modules is now consumed — it is NOT (T4/T6 consume `bar_fraction`, `fmt_*`, `monitor_available`, `MonitorSnapshot`'s render-side reads), so LEAVE them; T6 removes them.

- [ ] **Step 4: Update `Connection::execute`'s doc comment** (`crates/dbc-core/src/connection.rs:19-20`) — design §0's mandated wording change. Replace:

```rust
    /// Executes a non-returning statement, reporting affected rows. This is
    /// the app's write path — ONLY the sandbox Apply flow may call it.
```

with:

```rust
    /// Executes a non-returning statement, reporting affected rows. This is
    /// the app's write path — ONLY the sandbox Apply flow and the
    /// server-monitor's confirmed kill action (G9: `pg_terminate_backend` /
    /// `KILL <spid>`, confirm-dialog-gated, refused on read-only
    /// connections) may call it.
```

- [ ] **Step 5: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor_tests::` then `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-ui`
Expected: all pass (including every pre-existing test), zero warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/runner.rs crates/dbc-core/src/connection.rs
git commit -m "feat: QueryRunner::open_monitor with read-only kill gate (G9 T3)"
```

---

### Task 4 (T4): `MonitorView` entity + `TabContent::Monitor` — `monitor_view.rs`, `tabs.rs`, `main.rs` (match arms)

**Files:**
- Create: `crates/dbc-ui/src/monitor_view.rs`
- Modify: `crates/dbc-ui/src/tabs.rs` (new `TabContent::Monitor` variant)
- Modify: `crates/dbc-ui/src/main.rs` (`#[allow(dead_code)] mod monitor_view;` in the mod list; `Monitor` arms in the three exhaustive `TabContent` matches; the `monitor_view_for_tab` helper)

**Interfaces:**
- Consumes: `runner::{MonitorCmd, MonitorEvent}` (T3), `monitor::{MonitorSnapshot, BlockingNode, compute_rate, bar_fraction, fmt_bytes, fmt_uptime}` (T1), `monitor_sql::kill_sql` (T2), GPUI (`uniform_list` + `cx.processor` per `grid.rs:2531-2534`, `cx.spawn` drain per `main.rs`'s QueryEvent loop, `EventEmitter`/`cx.emit` per `schema_tree.rs`'s `TreeEvent`).
- Produces (consumed by T5, T6):
  ```rust
  // tabs.rs — alongside Grid/Text:
  pub enum TabContent {
      Grid { grid: Entity<ResultGrid>, buffer: Rc<RefCell<ResultBuffer>> },
      #[allow(dead_code)]
      Text { text: String, scroll_lines: usize },
      /// G9: server-monitor dashboard tab (one per connection at a time —
      /// keyed via `preview_key = "monitor:{conn_identity}"`, activated
      /// not re-stacked on reopen; see AppView::open_monitor_tab).
      Monitor { view: Entity<crate::monitor_view::MonitorView> },
  }

  // monitor_view.rs:
  pub struct MonitorView { /* fields below */ }

  #[derive(Debug, Clone)]
  pub enum MonitorViewEvent {
      /// Kill icon clicked on a running-query row (never emitted when
      /// read_only). `label` = "{user} · {application} · běží {n}s" row
      /// facts; `sql` = the exact statement (monitor_sql::kill_sql) the
      /// confirm dialog will display and the background task will run.
      KillRequested { pid: i64, label: String, sql: String },
      /// MonitorEvent::KillResult relayed after the view processed it
      /// (Ok already triggered the out-of-cycle refresh).
      KillFinished { pid: i64, result: Result<(), String> },
  }
  impl gpui::EventEmitter<MonitorViewEvent> for MonitorView {}

  impl MonitorView {
      /// Spawns the event-drain loop and dispatches the initial
      /// Refresh{generation: 1} immediately (first paint must not wait 5s).
      pub fn new(
          cx: &mut Context<Self>,
          cmd_tx: tokio::sync::mpsc::Sender<runner::MonitorCmd>,
          event_rx: tokio::sync::mpsc::Receiver<runner::MonitorEvent>,
          read_only: bool,
          engine: dbc_state::Engine,
      ) -> Self;

      /// Timer-driven (AppView loop, T6): dispatches a Refresh unless
      /// paused or one is already in flight (design §4 overlap prevention —
      /// a skipped tick is skipped, never queued).
      pub fn tick_if_idle(&mut self, cx: &mut Context<Self>);
      /// Toolbar ↻: out-of-cycle refresh regardless of paused/backoff,
      /// still gated by awaiting (design §5).
      pub fn manual_refresh(&mut self, cx: &mut Context<Self>);
      /// Confirm-dialog path (T5): sends MonitorCmd::Kill. Does NOT check
      /// read_only itself — the UI never emits KillRequested when
      /// read_only, and the background task independently refuses (the two
      /// designated gates, design §6); a third check here would mask a
      /// regression in either.
      pub fn dispatch_kill(&mut self, pid: i64, cx: &mut Context<Self>);
      /// Current tick interval (5s, doubling to 60s cap under errors) —
      /// read by AppView's timer loop each lap (T6).
      pub fn interval_secs(&self) -> u64;
  }

  // Duration colour tiers (design §5 — constants live HERE, not monitor.rs):
  pub const DURATION_WARN_SECS: f64 = 1.0;
  pub const DURATION_CRIT_SECS: f64 = 10.0;
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum Tier { Normal, Warn, Crit }
  pub fn duration_tier(secs: f64) -> Tier;
  ```

**Grounding — fields and event handling** (this is the logic Step 3 implements; render described after):

```rust
pub struct MonitorView {
    cmd_tx: tokio::sync::mpsc::Sender<runner::MonitorCmd>,
    pub read_only: bool,
    engine: dbc_state::Engine,
    paused: bool,
    awaiting: bool,
    refresh_generation: u64,
    interval_secs: u64,               // 5 -> 10 -> 20 -> 40 -> 60 backoff
    snapshot: Option<monitor::MonitorSnapshot>,
    /// (xact_total, at) from the previous accepted Data — the client-side
    /// TPS delta state (design §1).
    prev_xact: Option<(i64, std::time::Instant)>,
    last_error: Option<String>,
    last_refresh_at: Option<std::time::Instant>,
    /// Read-only query-text overlay (click a running row's query / a
    /// blocking node) — same local-state idiom as grid.rs's CellDetail
    /// (grid.rs:180-183, render at 1825), not AppView::modal.
    detail: Option<String>,
}

impl MonitorView {
    pub fn new(
        cx: &mut Context<Self>,
        cmd_tx: tokio::sync::mpsc::Sender<runner::MonitorCmd>,
        mut event_rx: tokio::sync::mpsc::Receiver<runner::MonitorEvent>,
        read_only: bool,
        engine: dbc_state::Engine,
    ) -> Self {
        // Event drain — same cx.spawn + channel-recv shape main.rs's
        // QueryEvent loop uses; ends when the runner task drops event_tx
        // OR this entity is released (update() errs).
        cx.spawn(async move |this, cx| {
            while let Some(ev) = event_rx.recv().await {
                if this.update(cx, |view, cx| view.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut view = Self {
            cmd_tx, read_only, engine,
            paused: false, awaiting: false,
            refresh_generation: 0, interval_secs: 5,
            snapshot: None, prev_xact: None,
            last_error: None, last_refresh_at: None, detail: None,
        };
        view.dispatch_refresh(); // initial paint data, generation 1
        view
    }

    fn dispatch_refresh(&mut self) {
        self.refresh_generation += 1;
        // try_send: the loop drains promptly; a full channel just means a
        // dispatch is dropped and the next timer lap retries — never block
        // the UI thread on a channel.
        if self.cmd_tx.try_send(runner::MonitorCmd::Refresh { generation: self.refresh_generation }).is_ok() {
            self.awaiting = true;
        }
    }

    pub fn tick_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.paused || self.awaiting {
            return;
        }
        self.dispatch_refresh();
        cx.notify();
    }

    pub fn manual_refresh(&mut self, cx: &mut Context<Self>) {
        if self.awaiting {
            return;
        }
        self.dispatch_refresh();
        cx.notify();
    }

    pub fn dispatch_kill(&mut self, pid: i64, cx: &mut Context<Self>) {
        let _ = self.cmd_tx.try_send(runner::MonitorCmd::Kill {
            generation: self.refresh_generation,
            pid,
        });
        cx.notify();
    }

    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    fn on_event(&mut self, ev: runner::MonitorEvent, cx: &mut Context<Self>) {
        match ev {
            runner::MonitorEvent::Data { generation, mut snapshot } => {
                // Last-dispatched-wins, same convention as
                // AppView::switch_generation / schema_fetch_generation.
                if generation != self.refresh_generation {
                    return;
                }
                self.awaiting = false;
                self.interval_secs = 5; // any Data resets backoff (design §4)
                self.last_error = None;
                if let Some(perf) = snapshot.perf.as_mut() {
                    if let Some(total) = perf.xact_total {
                        perf.tps = monitor::compute_rate(total, self.prev_xact, snapshot.fetched_at);
                        self.prev_xact = Some((total, snapshot.fetched_at));
                    }
                }
                self.snapshot = Some(snapshot);
                self.last_refresh_at = Some(std::time::Instant::now());
                cx.notify();
            }
            runner::MonitorEvent::Error { generation, message } => {
                if generation != self.refresh_generation {
                    return;
                }
                self.awaiting = false;
                self.interval_secs = (self.interval_secs * 2).min(60); // 5→10→20→40→60
                self.last_error = Some(message);
                cx.notify();
            }
            runner::MonitorEvent::KillResult { pid, result, .. } => {
                // NOT generation-gated: a kill outcome is never superseded
                // by refresh generations (design §4 gates Data/Error only).
                let outcome = result.map(|_affected| ()).map_err(|e| e.message);
                if outcome.is_ok() {
                    // Immediate out-of-cycle refresh so the list reflects
                    // the kill without waiting up to 5s (design §6). Note
                    // pg returns Ok even for "pid already gone" (function
                    // result false, not an error) — the refresh is what
                    // shows the truth either way.
                    self.dispatch_refresh();
                }
                cx.emit(MonitorViewEvent::KillFinished { pid, result: outcome });
                cx.notify();
            }
        }
    }
}

pub fn duration_tier(secs: f64) -> Tier {
    if secs >= DURATION_CRIT_SECS {
        Tier::Crit
    } else if secs >= DURATION_WARN_SECS {
        Tier::Warn
    } else {
        Tier::Normal
    }
}

fn tier_color(t: Tier) -> gpui::Rgba {
    match t {
        Tier::Normal => gpui::rgb(0xcdd6f4),
        Tier::Warn => gpui::rgb(0xf9e2af),  // design §5's literal warn colour
        Tier::Crit => gpui::rgb(0xf38ba8),  // design §5's literal crit colour
    }
}
```

**Grounding — render contract** (`impl Render for MonitorView`, one `render` fn; card tokens `rgb(0x1e1e2e)` bg / `rgb(0x45475a)` border per design §5, matching `connections_ui.rs`'s panels; every interactive element uses `cx.listener` with the 4-arg closure shape grid.rs uses):

1. **Toolbar row** (top-right of the tile row): pause toggle `div().id("mon-pause").cursor_pointer().child(if self.paused { "▶" } else { "⏸" })` flipping `self.paused` on click; manual refresh `↻` calling `self.manual_refresh(cx)`; freshness label `format!("aktualizace před {} s", self.last_refresh_at.map(|t| t.elapsed().as_secs()).unwrap_or(0))` (relative — see Self-Review note 6), or `"načítám…"` before the first Data.
2. **Four tiles**, each a fixed-width card; `None` tile/field renders `"–"`/`"n/a"`:
   - Connections: `format!("{} aktivní · {} idle / max {}", t.active, t.idle, t.max.map(|m| m.to_string()).unwrap_or_else(|| "neomezeno".into()))`.
   - Locks: `format!("{} čeká na zámek · {} deadlocků", t.waiting, t.deadlocks_since_reset)` + sub-label `"od posledního resetu statistik"` (design §5 — NOT "dnes", risk #3).
   - Size: two-segment horizontal bar (two `div().h(px(8.)).w(px(...))` fills over a 160 px track, widths from `bar_fraction` of each half against their sum) + `format!("data {} · WAL {}", size.data_bytes.map(monitor::fmt_bytes).unwrap_or_else(|| "n/a".into()), size.wal_or_log_bytes.map(monitor::fmt_bytes).unwrap_or_else(|| "n/a".into()))`.
   - Perf: `format!("{} % cache hit · uptime {} · TPS {}", pct-or-"–", monitor::fmt_uptime(p.uptime_secs), tps-or-"–")` with `{:.1}` formatting on the two floats.
3. **Error/backoff status line** under the tiles when `last_error` is `Some`: `format!("aktualizace selhala ({msg}) · další pokus za {}s", self.interval_secs)` in the warn colour.
4. **Running queries** — `uniform_list("monitor-running", rows.len(), cx.processor(move |this, range, _window, cx| { ... }))` (exact call shape `grid.rs:2531-2534`), reading `this.snapshot.as_ref().and_then(|s| s.running.as_ref())` inside the processor. Per row: pid · user · application · client · state · `format!("{:.1}s", duration_secs)` in `tier_color(duration_tier(duration_secs))` as TEXT colour (not row bg — composes with alternating row backgrounds) · truncated query (`.overflow_hidden()`, click sets `self.detail = Some(full_query)`) · kill button:
   ```rust
   // kill affordance — the FIRST of the two designated read-only gates
   // (design §6): disabled + tooltip when read_only, never emits.
   let kill = if this.read_only {
       div().id(("mon-kill", pid as usize)).text_color(rgb(0x6c7086))
           .child("✕")
           .tooltip(|window, cx| gpui::Tooltip::simple("spojení je pouze pro čtení", window, cx))
   } else {
       let label = format!(
           "{} · {} · běží {:.0}s",
           row.user.clone().unwrap_or_else(|| "?".into()),
           row.application.clone().unwrap_or_else(|| "?".into()),
           row.duration_secs
       );
       let sql = crate::monitor_sql::kill_sql(this.engine, pid)
           .unwrap_or_default(); // unreachable None: monitor never opens for Sqlite
       div().id(("mon-kill", pid as usize)).cursor_pointer().text_color(rgb(0xf38ba8))
           .child("✕")
           .on_click(cx.listener(move |_this, _, _, cx| {
               cx.emit(MonitorViewEvent::KillRequested {
                   pid,
                   label: label.clone(),
                   sql: sql.clone(),
               });
           }))
   };
   ```
   (If `.tooltip` on a plain `div` fights the pinned rev's API, fall back to the disabled glyph plus a static `"pouze pro čtení"` caption in the row — the tooltip is cosmetic; the DISABLED state is the requirement. Verify against the vendored GPUI checkout, not by upgrading.)
5. **Blocking chains** — flat recursion into a Vec (not `uniform_list` — chain counts are small): `fn push_blocking_rows(node: &monitor::BlockingNode, depth: usize, out: &mut Vec<AnyElement>, ...)` renders `div().pl(px(16. * depth as f32))` + `format!("{} · wait {}s · {}", node.pid, node.wait_secs.map(|w| format!("{w:.1}")).unwrap_or_else(|| "–".into()), truncate(query))`; a `cycle: true` node uses `tier_color(Tier::Crit)` and appends `" (cyklus — možný deadlock)"`; clicking a row sets `self.detail = Some(query)` (reads off the node — no extra round-trip, design §5). `blocking == Some([])` renders `"žádné blokace"`; `None` renders `"n/a"`.
6. **Per-table sizes** — flat list (bounded by schema size, design §5): one row per `TableSizeRow`: `schema.table | fmt_bytes(data) | fmt_bytes(index) | fmt_bytes(toast) | ~{row_estimate} řádků |` + a bar `div().h(px(6.)).w(px(160. * monitor::bar_fraction(total, max_in_set))).bg(rgb(0x89b4fa))` where `total = data+index+toast` and `max_in_set` is the max total across the rendered set.
7. **Detail overlay** when `self.detail` is `Some`: centered `.occlude()` monospace panel mirroring `grid.rs::render_cell_detail_overlay` (grid.rs:1825 — scrollable lines + "Kopírovat" + close on backdrop click clearing `self.detail`). Local to this entity, NOT `AppView::modal` (same reasoning as grid's `cell_detail` field comment, grid.rs:286-289).

**Grounding — `main.rs` additions in THIS task** (the minimal compile-fix set; everything else waits for T6): the new variant breaks three exhaustive `TabContent` matches —
- `render_tab_strip`'s row-count/dirty match (`main.rs:2640-2645`): add `TabContent::Monitor { .. } => (0, false),`
- `render_tab_content`'s dispatch (`main.rs:2727`): add `TabContent::Monitor { view } => view.clone().into_any_element(),`
- the run-loop's active-content match near `main.rs:1217-1220`: add `TabContent::Monitor { .. } => None,`
plus the lookup helper T5/T6 both consume:

```rust
/// The MonitorView entity behind an open Monitor tab, by tab id — used by
/// the kill-confirm dialog (T5) and the per-tab timer loop (T6).
fn monitor_view_for_tab(&self, tab_id: u64) -> Option<Entity<monitor_view::MonitorView>> {
    self.tabs.iter().find(|t| t.id == tab_id).and_then(|t| match &t.content {
        TabContent::Monitor { view } => Some(view.clone()),
        _ => None,
    })
}
```

- [ ] **Step 1: Write the failing tests** (`monitor_view.rs`, `#[cfg(test)] mod tests` — pure logic only; GPUI rendering is not unit-tested anywhere in this repo, per the established split):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_tier_boundaries() {
        assert_eq!(duration_tier(0.0), Tier::Normal);
        assert_eq!(duration_tier(0.999), Tier::Normal);
        assert_eq!(duration_tier(1.0), Tier::Warn);   // >= WARN is Warn
        assert_eq!(duration_tier(9.999), Tier::Warn);
        assert_eq!(duration_tier(10.0), Tier::Crit);  // >= CRIT is Crit
        assert_eq!(duration_tier(120.0), Tier::Crit);
    }

    #[test]
    fn backoff_progression_caps_at_60() {
        // The exact 5→10→20→40→60→60 ladder (design §4), as the pure
        // arithmetic on_event applies.
        let mut interval: u64 = 5;
        let mut seen = Vec::new();
        for _ in 0..6 {
            interval = (interval * 2).min(60);
            seen.push(interval);
        }
        assert_eq!(seen, vec![10, 20, 40, 60, 60, 60]);
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor_view::`
Expected: compile error (module doesn't exist).

- [ ] **Step 3: Implement** `monitor_view.rs` per the grounding blocks (struct + `new`/`dispatch_refresh`/`tick_if_idle`/`manual_refresh`/`dispatch_kill`/`interval_secs`/`on_event`, `MonitorViewEvent` + `EventEmitter`, `Tier`/`duration_tier`/`tier_color`, `impl Render` per the 7-point render contract), the `tabs.rs` `Monitor` variant, and the `main.rs` match arms + `monitor_view_for_tab` + `#[allow(dead_code)] mod monitor_view;`.

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass (tabs.rs's existing tests still green — the new variant must not disturb eviction/identity logic), zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/monitor_view.rs crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/main.rs
git commit -m "feat: MonitorView entity + TabContent::Monitor (G9 T4)"
```

---

### Task 5 (T5): Kill confirm dialog — `connections_ui.rs`

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (new `ModalState::KillConfirm` variant at `connections_ui.rs:892-924`'s enum, a render arm in `render_modal_overlay`'s match at `connections_ui.rs:1044`, the panel render helper, and two `AppView` handlers in this file's existing `impl AppView` block)

**Interfaces:**
- Consumes: `AppView::monitor_view_for_tab` (T4, main.rs), `MonitorView::dispatch_kill` (T4).
- Produces (consumed by T6):
  ```rust
  pub enum ModalState {
      ConnectionDialog(ConnectionDialogUi),
      MasterPasswordPrompt { /* unchanged */ },
      CreateMasterPassword { /* unchanged */ },
      QueryParams { /* unchanged */ },
      /// G9: confirmed-admin-action dialog for kill (design §6). Reuses the
      /// single-modal-at-a-time infrastructure deliberately —
      /// run_query_with already refuses to run while modal.is_some(), and
      /// the dropdown/palette refuse to open a second modal; a kill
      /// confirmation is exactly the blocking dialog that invariant exists
      /// for. `sql` is the LITERAL statement that will run (shown in a
      /// monospace block — same "show the exact generated SQL" principle
      /// as the Apply dialog). `error` is a failed kill's message: the
      /// dialog stays open with it (same "error stays in the modal"
      /// precedent as Apply's rollback-error UX).
      KillConfirm {
          pid: i64,
          label: String,   // "{user} · {application} · běží {n}s"
          sql: String,
          tab_id: u64,
          error: Option<String>,
      },
  }

  impl AppView {
      /// "Zrušit": closes the dialog, nothing was sent.
      pub(crate) fn cancel_kill_confirm(&mut self, cx: &mut Context<Self>);
      /// "Ukončit proces": dispatches MonitorCmd::Kill via the tab's
      /// MonitorView; the dialog STAYS OPEN until KillFinished resolves
      /// (T6's on_monitor_view_event closes it on Ok / fills `error` on Err).
      pub(crate) fn confirm_kill_confirm(&mut self, cx: &mut Context<Self>);
  }
  ```

**Grounding — handlers** (both live in `connections_ui.rs`'s existing `impl AppView` section, alongside `toggle_query_param_null` etc.):

```rust
pub(crate) fn cancel_kill_confirm(&mut self, cx: &mut Context<Self>) {
    if matches!(self.modal, Some(ModalState::KillConfirm { .. })) {
        self.modal = None;
        cx.notify();
    }
}

pub(crate) fn confirm_kill_confirm(&mut self, cx: &mut Context<Self>) {
    let Some(ModalState::KillConfirm { pid, tab_id, .. }) = &self.modal else {
        return;
    };
    let (pid, tab_id) = (*pid, *tab_id);
    let Some(view) = self.monitor_view_for_tab(tab_id) else {
        // Tab closed under the dialog — nothing to kill against.
        self.modal = None;
        self.status = "monitor tab už není otevřený — ukončení zrušeno".into();
        cx.notify();
        return;
    };
    view.update(cx, |m, cx| m.dispatch_kill(pid, cx));
    // Deliberately no self.modal = None here: success/failure arrives as
    // MonitorViewEvent::KillFinished (T6), which closes the dialog on Ok
    // or writes `error` on Err — the failure-stays-in-dialog UX (design §6).
    cx.notify();
}
```

**Grounding — render:** add the match arm at `render_modal_overlay`'s dispatch (alongside the `QueryParams` arm at `connections_ui.rs:1044-1046`):

```rust
ModalState::KillConfirm { pid, label, sql, error, .. } => {
    render_kill_confirm_panel(*pid, label, sql, error, cx)
}
```

and the free panel helper (same section as the other `render_*_panel` helpers; card tokens/backdrop identical to the existing dialogs — the shared `.occlude()`d overlay wrapper at `connections_ui.rs:1048-1062` is untouched):

```rust
fn render_kill_confirm_panel(
    pid: i64,
    label: &str,
    sql: &str,
    error: &Option<String>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .w(px(520.))
        .bg(rgb(0x1e1e2e))
        .border_1()
        .border_color(rgb(0x45475a))
        .rounded_md()
        .text_color(rgb(0xcdd6f4))
        .child(div().font_weight(gpui::FontWeight::BOLD).child("Ukončit proces"))
        .child(format!("Opravdu ukončit proces {pid} ({label})?"))
        // The literal SQL that will run — same "show the exact generated
        // SQL" principle as the Apply dialog (design §6).
        .child(
            div()
                .font_family("Consolas")
                .p_2()
                .bg(rgb(0x181825))
                .rounded_md()
                .child(sql.to_string()),
        );
    if let Some(e) = error {
        panel = panel.child(div().text_color(rgb(0xf38ba8)).child(format!("error: {e}")));
    }
    panel
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap_2()
                .child(
                    div()
                        .id("kill-cancel")
                        .cursor_pointer()
                        .px_3()
                        .py_1()
                        .bg(rgb(0x313244))
                        .rounded_md()
                        .child("Zrušit")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_kill_confirm(cx))),
                )
                .child(
                    div()
                        .id("kill-confirm")
                        .cursor_pointer()
                        .px_3()
                        .py_1()
                        .bg(rgb(0x5d2e2e)) // danger tint — DELETED_ROW_BG family
                        .rounded_md()
                        .child("Ukončit proces")
                        .on_click(cx.listener(|this, _, _, cx| this.confirm_kill_confirm(cx))),
                ),
        )
        .into_any_element()
}
```

- [ ] **Step 1: Implement** the variant, the render arm, `render_kill_confirm_panel`, and the two handlers exactly as above. (No new pure logic exists in this task to TDD in isolation — the variant/render are GPUI glue with the same no-entity-test precedent as every other modal in this file; the gate logic this dialog fronts is already covered by T3's REQUIRED tests, and the open/close/error transitions are covered by T6's wiring plus the manual check below.)

- [ ] **Step 2: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: everything compiles and passes, zero warnings (all pieces are referenced: the arm by `render_modal_overlay`, the handlers by the panel's listeners).

- [ ] **Step 3: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: kill confirm dialog (ModalState::KillConfirm) (G9 T5)"
```

---

### Task 6 (T6): Open action, palette entry, timer loop, subscriptions — `main.rs` + `palette.rs` (serialized tail)

**Files:**
- Modify: `crates/dbc-ui/src/main.rs`
- Modify: `crates/dbc-ui/src/palette.rs`
- Modify: `docs/superpowers/specs/2026-08-22-gui-target-design.md` (§3 wording amendment, design §0)

**Interfaces:**
- Consumes: everything — `monitor::monitor_available` (T1), `runner::open_monitor` (T3), `MonitorView`/`MonitorViewEvent`/`monitor_view_for_tab`/`TabContent::Monitor` (T4), `ModalState::KillConfirm` + `cancel_kill_confirm`/`confirm_kill_confirm` (T5).
- Produces: end-user surface only (leaf task).

**Grounding — palette entry with engine gating** (`palette.rs`): extend the action enum and thread one boolean through the two existing `fixed_actions()` call sites inside `rank_items` (`palette.rs:188/219`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteAction {
    RunQuery,
    ToggleTree,
    ToggleHistory,
    NewConnection,
    RefreshSchema,
    OpenMonitor,
}

/// The fixed action rows, in display order, with their Czech labels.
/// `monitor_available` gates the monitor entry per the ACTIVE connection's
/// engine (design §7): absent entirely — not disabled-but-visible — when
/// the engine has no monitor (the design's rejected alternative: showing
/// it for MSSQL "since the SQL is ready" would just surface the confusing
/// driver-missing connect error).
pub fn fixed_actions(monitor_available: bool) -> Vec<(String, PaletteAction)> {
    let mut actions = vec![
        ("Spustit dotaz".to_string(), PaletteAction::RunQuery),
        ("Přepnout strom".to_string(), PaletteAction::ToggleTree),
        ("Přepnout historii".to_string(), PaletteAction::ToggleHistory),
        ("Nové spojení…".to_string(), PaletteAction::NewConnection),
        ("Obnovit schéma".to_string(), PaletteAction::RefreshSchema),
    ];
    if monitor_available {
        actions.push(("Monitor serveru".to_string(), PaletteAction::OpenMonitor));
    }
    actions
}

pub fn rank_items(
    query: &str,
    tables: &[TableSource],
    history: &[HistorySource],
    connections: &[ConnectionSource],
    monitor_available: bool,
    cap: usize,
) -> Vec<PaletteItem> {
    // body unchanged except both `fixed_actions()` calls become
    // `fixed_actions(monitor_available)`.
}
```

Every existing `rank_items(...)`/`fixed_actions()` call in `palette.rs`'s own tests passes `false` (their assertions count 5 actions and stay valid); add one new test:

```rust
#[test]
fn monitor_entry_present_only_when_available() {
    let items = rank_items("", &[], &[], &[], true, 30);
    assert!(items.iter().any(|i| matches!(
        i,
        PaletteItem::Action { action: PaletteAction::OpenMonitor, .. }
    )));
    let items = rank_items("", &[], &[], &[], false, 30);
    assert!(items.iter().all(|i| !matches!(
        i,
        PaletteItem::Action { action: PaletteAction::OpenMonitor, .. }
    )));
}
```

**Grounding — `main.rs` wiring** (all new code below; `build_palette_items`' final line becomes `palette::rank_items(query, &tables, &history, &connections, self.active_engine().is_some_and(monitor::monitor_available), 30)`, and `execute_palette_item`'s action match gains `PaletteAction::OpenMonitor => self.open_monitor_tab(cx),`):

```rust
/// The ACTIVE connection's engine: saved config's `cfg.engine`, or
/// `engine_from_url` for the CLI-arg back-compat path, `None` with no
/// active connection at all (design §7's three-way gating input).
fn active_engine(&self) -> Option<dbc_state::Engine> {
    if let Some(id) = &self.active_connection_id {
        return self.config.connections.iter().find(|c| &c.id == id).map(|c| c.engine);
    }
    self.conn_url.as_deref().map(engine_from_url)
}

/// Opens (or re-activates) the monitor tab for the active connection.
/// preview_key "monitor:{conn_identity}" gives one-monitor-per-connection:
/// unlike table previews (close_by_preview_key replaces), reopening just
/// ACTIVATES the existing tab (design §5).
fn open_monitor_tab(&mut self, cx: &mut Context<Self>) {
    let Some(engine) = self.active_engine() else {
        self.status = "Bez připojení — vyberte připojení nahoře.".into();
        cx.notify();
        return;
    };
    if !monitor::monitor_available(engine) {
        // The palette already hides the entry (build_palette_items); this
        // is the belt for any other entry point.
        self.status = "monitor serveru není pro tento engine k dispozici".into();
        cx.notify();
        return;
    }
    let key = format!("monitor:{}", self.current_conn_identity());
    if let Some(id) =
        self.tabs.iter().find(|t| t.preview_key.as_deref() == Some(key.as_str())).map(|t| t.id)
    {
        self.tabs.activate(id);
        cx.notify();
        return;
    }
    let Some(spec) = self.active_conn_spec() else {
        self.status = "Bez připojení — vyberte připojení nahoře.".into();
        cx.notify();
        return;
    };
    // Same read-only resolution runner::spec_is_read_only applies: config
    // flag, or always-writable for the CLI-arg URL path.
    let read_only = match &spec {
        ConnectSpec::Config { cfg, .. } => cfg.read_only,
        ConnectSpec::Url(_) => false,
    };
    let (cmd_tx, event_rx) = self.runner.open_monitor(spec, read_only, engine);
    let view = cx.new(|cx| monitor_view::MonitorView::new(cx, cmd_tx, event_rx, read_only, engine));
    let title = collapse_title(&format!("Monitor: {}", self.current_connection_label()));
    let tab_id = self.tabs.open(ResultTab {
        id: 0,
        title,
        pinned: false,
        preview_key: Some(key),
        conn_identity: self.current_conn_identity(),
        content: TabContent::Monitor { view: view.clone() },
    });
    cx.subscribe(&view, move |this, _emitter, event, cx| {
        this.on_monitor_view_event(tab_id, event, cx);
    })
    .detach();
    self.spawn_monitor_timer(tab_id, cx);
    cx.notify();
}

/// One timer loop per open monitor tab (design §4), on the SAME
/// cx.background_executor().timer primitive grid.rs's export chunking uses
/// (grid.rs:1452). Hidden-tab gating is automatic: a tick only reaches
/// tick_if_idle when this tab is the active one; pause/awaiting are
/// checked inside MonitorView. The loop BREAKS (risk #7's explicit fix,
/// not a forever-no-op) when the tab or the AppView is gone.
fn spawn_monitor_timer(&mut self, tab_id: u64, cx: &mut Context<Self>) {
    cx.spawn(async move |this, cx| {
        loop {
            // Re-read the CURRENT interval each lap — backoff can change
            // it between ticks. None = tab closed.
            let interval = match this.update(cx, |view, cx| {
                view.monitor_view_for_tab(tab_id).map(|m| m.read(cx).interval_secs())
            }) {
                Ok(Some(secs)) => secs,
                Ok(None) | Err(_) => break,
            };
            cx.background_executor().timer(std::time::Duration::from_secs(interval)).await;
            let tick = this.update(cx, |view, cx| {
                let visible = view.tabs.active().is_some_and(|t| t.id == tab_id);
                if visible {
                    if let Some(m) = view.monitor_view_for_tab(tab_id) {
                        m.update(cx, |m, cx| m.tick_if_idle(cx));
                    }
                }
            });
            if tick.is_err() {
                break; // AppView released
            }
        }
    })
    .detach();
}

/// MonitorView -> AppView event bridge (subscription wired in
/// open_monitor_tab). KillRequested opens the confirm dialog; KillFinished
/// resolves it (design §6's success/failure UX).
fn on_monitor_view_event(
    &mut self,
    tab_id: u64,
    event: &monitor_view::MonitorViewEvent,
    cx: &mut Context<Self>,
) {
    match event {
        monitor_view::MonitorViewEvent::KillRequested { pid, label, sql } => {
            if self.modal.is_some() {
                return; // single-modal invariant, same as every dialog opener
            }
            self.modal = Some(connections_ui::ModalState::KillConfirm {
                pid: *pid,
                label: label.clone(),
                sql: sql.clone(),
                tab_id,
                error: None,
            });
            cx.notify();
        }
        monitor_view::MonitorViewEvent::KillFinished { pid, result } => {
            match result {
                Ok(()) => {
                    if matches!(&self.modal, Some(connections_ui::ModalState::KillConfirm { .. })) {
                        self.modal = None;
                    }
                    // pg reports Ok even when the pid already exited (the
                    // function returns false, not an error) — the
                    // out-of-cycle refresh MonitorView already dispatched
                    // shows the truth momentarily (design §6).
                    self.status = format!("proces {pid} ukončen");
                }
                Err(msg) => {
                    if let Some(connections_ui::ModalState::KillConfirm { error, .. }) =
                        &mut self.modal
                    {
                        *error = Some(msg.clone()); // dialog stays open
                    } else {
                        self.status = format!("error: {msg}");
                    }
                }
            }
            cx.notify();
        }
    }
}
```

- [ ] **Step 1: Update `palette.rs`** — enum variant, `fixed_actions(monitor_available)`, `rank_items` param, fix every existing test call site with `false`, add the new gating test above.

- [ ] **Step 2: Run to see the new palette test pass and everything else compile-fail forward** 

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui palette::`
Expected: `main.rs` fails to compile (its `rank_items` call and the non-exhaustive `PaletteAction` match) — the wiring below is what fixes it.

- [ ] **Step 3: Implement the `main.rs` wiring** — `active_engine`, `open_monitor_tab`, `spawn_monitor_timer`, `on_monitor_view_event`, the `build_palette_items`/`execute_palette_item` updates, and remove the three temporary `#[allow(dead_code)]` attributes from the `mod monitor;` / `mod monitor_sql;` / `mod monitor_view;` declarations plus the one on `runner::open_monitor` (all their items are now consumed; `monitor_sql::mssql`'s module-level allow stays, permanently, per its doc comment).

- [ ] **Step 4: Amend the target-spec §3 bullet** (`docs/superpowers/specs/2026-08-22-gui-target-design.md:192-193`) with the design §0's recommended replacement wording. Replace:

```
- Sandbox Apply is the ONLY write path in the app; MCP (future) remains
  read-only per the original spec.
```

with:

```
- Sandbox Apply (grid edits) and confirmed admin actions (server-monitor
  kill) are the app's only write paths, both exclusively through
  `Connection::execute()`; MCP (future) remains read-only per the original
  spec. (Amended by G9 per its design §0.)
```

- [ ] **Step 5: Run to green + zero warnings + manual launch checks**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` and `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
Expected: all tests pass, both commands warning-free (the allow-removals in Step 3 are what this verifies).

Manual checks against a docker Postgres (`docker run --rm -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16`), via the `/run` skill or a plain launch:
1. Ctrl+K → "Monitor serveru" appears for a Postgres connection, is ABSENT for a SQLite connection and with no connection.
2. Opening shows tiles within ~1s (initial refresh), then updates every 5s; ⏸ stops updates; switching to another tab stops updates; switching back resumes on the next tick (no burst).
3. Run `SELECT pg_sleep(60)` from a second `psql`; it appears in running queries with a red duration after 10s; ✕ → dialog shows `SELECT pg_terminate_backend(<pid>)`; confirm → status "proces … ukončen" and the row disappears on the immediate refresh.
4. Reopen "Monitor serveru" while the tab exists → activates, doesn't stack.
5. On a read-only saved connection: ✕ is disabled with the tooltip; no dialog opens.
6. Stop the docker container while the tab is open → status line shows the backoff message with the growing interval; restart → next successful tick resets to 5s.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/palette.rs docs/superpowers/specs/2026-08-22-gui-target-design.md
git commit -m "feat: monitor tab wiring — palette entry, timer loop, kill dialog bridge (G9 T6)"
```

---

### Task 7 (T7): Docker integration tests — `runner.rs` `#[ignore]` module

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml` (add dev-dependency `testcontainers-modules = { version = "0.13", features = ["postgres"] }` — the same crate+version `dbc-driver-postgres`'s own `[dev-dependencies]` already pins)
- Modify: `crates/dbc-ui/src/runner.rs` (new `#[cfg(test)] mod monitor_pg_tests` below `monitor_tests`)

**Interfaces:**
- Consumes: `monitor_loop`/`run_monitor_refresh`/`MonitorCmd`/`MonitorEvent` (T3, same-file private access), `crate::connect`/`open_spec` (the ONLY sanctioned driver entry — this file never names `PostgresConnection`).
- Produces: nothing (leaf; CI-optional `--ignored` suite).

**Grounding:** the design's §8 places this in `crates/dbc-ui/tests/monitor_postgres.rs`, but `dbc-ui` is a BINARY crate (only `src/main.rs`, no lib target) — an external `tests/` file cannot `use dbc_ui::…` anything. The test therefore lives as an in-crate `#[cfg(test)]` module in `runner.rs` (full private access to `monitor_loop` et al.), run via `cargo test -p dbc-ui -- --ignored` — Self-Review note 5. Connections are opened through `open_spec(ConnectSpec::Url(...), handle)` — NOT `connect::open` directly, because `open`'s Postgres arm calls `runtime.block_on`, which panics inside a `#[tokio::test]` worker; `open_spec` already wraps it in `spawn_blocking` (its whole reason to exist).

- [ ] **Step 1: Add the dev-dependency**

```toml
# crates/dbc-ui/Cargo.toml [dev-dependencies]:
testcontainers-modules = { version = "0.13", features = ["postgres"] }
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui --tests`
Expected: builds clean.

- [ ] **Step 2: Write the tests** (`runner.rs`):

```rust
/// G9 T7: docker-gated proof of the pg monitor SQL against a live server.
/// Docker required. Run with:
///   %USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored
/// Same testcontainers pattern as dbc-driver-postgres/tests/integration.rs.
#[cfg(test)]
mod monitor_pg_tests {
    use super::*;
    use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

    async fn pg_url(
        node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    ) -> String {
        format!(
            "postgres://postgres:postgres@127.0.0.1:{}/postgres",
            node.get_host_port_ipv4(5432).await.unwrap()
        )
    }

    /// open_spec (NOT connect::open): the pg arm of `open` block_on's the
    /// runtime, which panics on a tokio test worker; open_spec wraps it in
    /// spawn_blocking. Also keeps this file free of driver-crate names.
    async fn open_pg(url: &str) -> Box<dyn Connection> {
        let handle = tokio::runtime::Handle::current();
        open_spec(ConnectSpec::Url(url.to_string()), handle).await.expect("connect").conn
    }

    #[tokio::test]
    #[ignore]
    async fn monitor_refresh_produces_populated_snapshot_on_live_postgres() {
        let node = Postgres::default().start().await.unwrap();
        let url = pg_url(&node).await;

        // Setup session: a table with data so the tables section is non-empty.
        let mut setup = open_pg(&url).await;
        setup
            .execute("CREATE TABLE mon_t(id INT PRIMARY KEY, v TEXT)", CancelToken::new())
            .await
            .unwrap();
        setup
            .execute(
                "INSERT INTO mon_t SELECT g, 'v' || g FROM generate_series(1, 1000) g",
                CancelToken::new(),
            )
            .await
            .unwrap();

        let mut conn = open_pg(&url).await;
        let results = run_monitor_refresh(
            &mut *conn,
            dbc_state::Engine::Postgres,
            CancelToken::new(),
        )
        .await;
        let snap = monitor::assemble_snapshot(results, Instant::now()).expect("snapshot");

        let connections = snap.connections.expect("connections tile");
        assert!(connections.max.unwrap_or(0) > 0, "max_connections should parse");
        assert!(snap.locks.is_some());
        assert!(snap.size.data_bytes.unwrap_or(0) > 0, "database has a size");
        // Container's postgres superuser may read pg_ls_waldir:
        assert!(snap.size.wal_or_log_bytes.is_some(), "WAL size readable as superuser");
        let perf = snap.perf.expect("perf tile");
        assert!(perf.uptime_secs >= 0);
        assert!(perf.xact_total.is_some());
        assert_eq!(perf.tps, None, "tps is a client-side delta, never parsed");
        assert!(snap.running.is_some());
        let tables = snap.tables.expect("tables section");
        assert!(
            tables.iter().any(|t| t.table == "mon_t"),
            "created table must appear in per-table sizes"
        );
    }

    /// Design §8 T7: a deliberate lock wait — session A holds a row lock,
    /// session B blocks on it; the blocking-chain query must return the
    /// waiter/blocker pair and build_blocking_tree must nest them.
    #[tokio::test]
    #[ignore]
    async fn blocking_chain_query_sees_a_real_lock_wait() {
        let node = Postgres::default().start().await.unwrap();
        let url = pg_url(&node).await;

        let mut setup = open_pg(&url).await;
        setup
            .execute("CREATE TABLE lock_t(id INT PRIMARY KEY, v INT)", CancelToken::new())
            .await
            .unwrap();
        setup
            .execute("INSERT INTO lock_t VALUES (1, 0)", CancelToken::new())
            .await
            .unwrap();

        // Session A: open transaction holding the row lock.
        let mut a = open_pg(&url).await;
        a.execute("BEGIN", CancelToken::new()).await.unwrap();
        a.execute("UPDATE lock_t SET v = 1 WHERE id = 1", CancelToken::new()).await.unwrap();

        // Session B: blocks on the same row, in a background task.
        let mut b = open_pg(&url).await;
        let b_task = tokio::spawn(async move {
            let _ = b.execute("UPDATE lock_t SET v = 2 WHERE id = 1", CancelToken::new()).await;
            b
        });
        tokio::time::sleep(Duration::from_secs(2)).await; // let B reach the lock queue

        let mut mon = open_pg(&url).await;
        let results =
            run_monitor_refresh(&mut *mon, dbc_state::Engine::Postgres, CancelToken::new()).await;
        let snap = monitor::assemble_snapshot(results, Instant::now()).expect("snapshot");

        let tree = snap.blocking.expect("blocking section");
        assert_eq!(tree.len(), 1, "exactly one blocking chain expected, got {tree:?}");
        assert_eq!(tree[0].children.len(), 1, "one waiter under the blocker");
        assert!(!tree[0].cycle && !tree[0].children[0].cycle);
        assert!(
            tree[0].children[0].query.as_deref().unwrap_or("").contains("UPDATE lock_t"),
            "waiter query text should surface"
        );
        let waiting = snap.locks.expect("locks tile").waiting;
        assert!(waiting >= 1, "waiting-locks counter must see the queued lock");

        // Release: A rolls back, B completes.
        a.execute("ROLLBACK", CancelToken::new()).await.unwrap();
        let _b = tokio::time::timeout(Duration::from_secs(10), b_task)
            .await
            .expect("B unblocks once A rolls back")
            .unwrap();
    }

    /// End-to-end kill through the REAL loop: find the pg_sleep session's
    /// pid via a refresh, Kill it, assert KillResult Ok — the full
    /// execute()-path counterpart of T3's mock-level gate tests.
    #[tokio::test]
    #[ignore]
    async fn kill_terminates_a_live_session_via_monitor_loop() {
        let node = Postgres::default().start().await.unwrap();
        let url = pg_url(&node).await;

        // Victim session: a long sleep, driven in a background task.
        let mut victim = open_pg(&url).await;
        let victim_task = tokio::spawn(async move {
            let cancel = CancelToken::new();
            match victim.query("SELECT pg_sleep(600)", cancel).await {
                Ok(mut s) => {
                    while let Some(item) = s.batches.recv().await {
                        if item.is_err() {
                            return true; // stream errored = terminated
                        }
                    }
                    false
                }
                Err(_) => true,
            }
        });
        tokio::time::sleep(Duration::from_secs(2)).await;

        let mon = open_pg(&url).await;
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
        let loop_task = tokio::spawn(monitor_loop(
            mon,
            dbc_state::Engine::Postgres,
            /* read_only */ false,
            cmd_rx,
            event_tx,
        ));

        // Refresh to learn the victim's pid.
        cmd_tx.send(MonitorCmd::Refresh { generation: 1 }).await.unwrap();
        let pid = loop {
            match tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
                .await
                .expect("event")
                .expect("channel open")
            {
                MonitorEvent::Data { snapshot, .. } => {
                    let running = snapshot.running.expect("running section");
                    let found = running
                        .iter()
                        .find(|r| r.query.as_deref().unwrap_or("").contains("pg_sleep(600)"))
                        .map(|r| r.pid);
                    break found.expect("victim session visible in running queries");
                }
                MonitorEvent::Error { message, .. } => panic!("refresh failed: {message}"),
                MonitorEvent::KillResult { .. } => unreachable!("no kill dispatched yet"),
            }
        };

        cmd_tx.send(MonitorCmd::Kill { generation: 1, pid }).await.unwrap();
        match tokio::time::timeout(Duration::from_secs(30), event_rx.recv())
            .await
            .expect("event")
            .expect("channel open")
        {
            MonitorEvent::KillResult { pid: killed, result, .. } => {
                assert_eq!(killed, pid);
                assert!(result.is_ok(), "kill failed: {result:?}");
            }
            other => panic!("expected KillResult, got {other:?}"),
        }

        let terminated = tokio::time::timeout(Duration::from_secs(30), victim_task)
            .await
            .expect("victim query ends after termination")
            .unwrap();
        assert!(terminated, "victim's stream must surface the termination error");

        drop(cmd_tx);
        tokio::time::timeout(Duration::from_secs(5), loop_task).await.unwrap().unwrap();
    }
}
```

- [ ] **Step 3: Verify the suite compiles without docker, then run it with docker**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui monitor_pg_tests::`
Expected: "3 ignored", zero failures, zero warnings (nothing runs without `--ignored`).
Run (docker up): `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored monitor_pg_tests::`
Expected: all 3 pass. If the container image needs pulling, first run is slow — that's the container, not the code.

- [ ] **Step 4: Commit**

```bash
git add crates/dbc-ui/Cargo.toml crates/dbc-ui/src/runner.rs
git commit -m "test: docker monitor integration — live refresh, lock wait, kill (G9 T7)"
```

---

## Task ordering

**Parallel batch 1 (worktrees):** T1 (`monitor.rs`) and T2 (`monitor_sql.rs`) — disjoint new files, no dependency between them. Each adds one `mod` line to `main.rs`; whichever merges second rebases that single line (trivial, no logic conflict).

**Serialized after batch 1:** T3 (`runner.rs` + the dbc-core doc comment) — needs T1's `RefreshResults`/`assemble_snapshot` and T2's constants.

**After T3, two independent branches:**
- **The main.rs chain (strictly sequential, same worker or rebase-in-order):** T4 (`monitor_view.rs` + `tabs.rs` variant + `main.rs` match arms) → T5 (`connections_ui.rs` dialog — depends on T4's `monitor_view_for_tab`/`dispatch_kill`) → T6 (`main.rs`/`palette.rs` wiring tail + spec amendment + allow-removals + manual verification). T4 and T5 nominally touch different primary files, but T5 compiles against T4's types and T6 compiles against both, and all three touch the `AppView` surface — do not parallelize any pair of them.
- **T7 (docker tests)** — touches only `runner.rs`'s test modules and `Cargo.toml` dev-deps, which no post-T3 task touches: safe in a parallel worktree alongside T4–T6, merged whenever green.

**Version bump (`0.9.0`) and the merge checklist** happen at branch finish, per Global Constraints — not inside any task's commit.

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §0 constraint amendment: kill via `execute()` only → T3 (`monitor_loop`'s Kill arm + the `connection.rs` doc update); the spec §3 wording change → T6 Step 4; the "query() would evade the guard" rationale → pinned by T2's documenting test. The two-independent-gates requirement → T4 (UI disabled state) + T3 (task-side refusal), each labeled as such in code comments.
- §1 data model + parse strategy → T1 (structs, fail-soft parsing, `compute_rate`, `build_blocking_tree` cycle-safety, `bar_fraction`); drain-through-`ResultBuffer` → T3's `drain_rows` (the `fetch_lookup_inner` shape).
- §2 pg SQL + permission caveats → T2 constants (+ the size-query split, note 1 below); per-tile degrade → T1's `assemble_snapshot` + its partial-failure test; proven live → T7.
- §3 MSSQL SQL → T2's `mssql` module (compiled + smoke-tested, permanently `#[allow(dead_code)]` until the driver lands).
- §4 refresh lifecycle → T3 (`open_monitor`, one held connection, select!-raced refresh, drop-teardown) + T4 (`awaiting`/generation/backoff/pause) + T6 (timer loop, hidden-tab gating, risk #7's explicit `break`).
- §5 UI layout → T4's render contract (tiles wording, duration tiers with the design's literal colours, blocking tree with the cycle suffix, per-table bars, toolbar) + T6 (palette entry, one-tab-per-connection activation).
- §6 kill flow → T4 (kill button + label + sql in `KillRequested`), T5 (confirm dialog showing the literal SQL, failure-stays-in-dialog via `error`), T6 (bridge, success status + the already-exited nuance), T3 (execute + gate), T7 (live end-to-end).
- §7 engine gating → T1's `monitor_available` + T6's palette hiding + `open_monitor_tab`'s belt check.
- §8 task table → this plan's T1–T7 (renumbered; see notes 4–5). §9 risks: #1 → the REQUIRED T3 test; #2 → T2's smoke-only posture, explicitly flagged un-runnable; #3 → the "od posledního resetu statistik" label (T4); #4 → T1's partial-failure test + T7's superuser-WAL assertion; #5 → noted in `mssql::RUNNING`'s doc comment, unresolvable without a driver; #6 → per-tab `MonitorView`+task pairs share nothing by construction (T4/T6), still only single-connection-tested (T7) as the design accepts; #7 → T6's timer loop breaks on `None`/`Err` instead of no-op'ing forever.

**Placeholder scan:** every step shows real code, a real diff target with line references, or a concrete cargo/docker command. T4's render is specified as a 7-point contract with real code for the two behavior-bearing pieces (the kill affordance and the tier colours) — same precedent as G6 T3/T7 and G5's dialog renders; all logic the render calls is fully coded and tested. No TBDs.

**Type-name consistency across tasks:** `monitor::{Row, RefreshResults, MonitorSnapshot, SizeTile, ...}` (T1) match T3's `run_monitor_refresh`/`assemble_snapshot` call sites and T4's snapshot reads. `monitor_sql::pg::*`/`kill_sql` (T2) match T3's drain calls and T4's `KillRequested` sql. `runner::{MonitorCmd, MonitorEvent, MONITOR_READ_ONLY_KILL_MSG, open_monitor}` (T3) match T4's `dispatch_refresh`/`on_event` and T6's `open_monitor_tab`. `MonitorView::{new, tick_if_idle, manual_refresh, dispatch_kill, interval_secs}` + `MonitorViewEvent` (T4) match T5's `confirm_kill_confirm` and T6's subscription/timer. `ModalState::KillConfirm{pid, label, sql, tab_id, error}` (T5) matches T6's open/resolve arms. `fixed_actions(bool)`/`rank_items(..., bool, cap)`/`PaletteAction::OpenMonitor` (T6) are self-consistent with `build_palette_items`/`execute_palette_item`.

**Deviations from the design draft (each with reason; none touch the CURATION requirement):**
1. **The pg size query is split into `DATA_SIZE` + `WAL_SIZE`** (8 refresh statements, not the design's 7): §2's own caveat requires the WAL half to degrade independently of the data half, which one combined statement that hard-errors on `pg_ls_waldir` permission cannot deliver. `SizeTile` fields become `Option<i64>` accordingly.
2. **Per-tile/section `Option`s on `MonitorSnapshot`** (design §1's struct sketch had plain fields): the concrete encoding of §1's per-query degrade + risk #4's partial-failure requirement; `assemble_snapshot` centralizes it and is directly tested.
3. **`PerfTile` gains `xact_total: Option<i64>`**: the raw cumulative counter must travel from the parser to the view for the client-side TPS delta the design itself specifies; `tps` stays exactly as designed (`None` until the 2nd refresh).
4. **Design tasks T2+T3 (pg SQL / mssql SQL) merged into plan T2**: both are pure constants in the same new file with one shared test cycle; keeping them separate would force two workers into one file for no parallelism gain.
5. **Docker tests live in `runner.rs`'s `#[cfg(test)] mod monitor_pg_tests`, not `crates/dbc-ui/tests/monitor_postgres.rs`**: `dbc-ui` is a binary crate with no lib target — an external integration-test file cannot import `monitor_loop` or anything else. In-crate placement also gives the tests `open_spec` (needed because `connect::open`'s pg arm `block_on`s, which panics on a tokio test worker).
6. **"poslední aktualizace HH:MM:SS" rendered as relative `"aktualizace před {n} s"`**: `std` has no local-timezone wall-clock formatting; an HH:MM:SS label would need a new `chrono` dependency in dbc-ui for one cosmetic string. Relative freshness carries the same information (how stale is this data). Flagged for the controller — trivially revisited if chrono ever lands for another reason.
7. **The query-detail popup is a MonitorView-local overlay mirroring `grid.rs`'s `CellDetail` idiom** (design §5 says "the SAME read-only cell-detail popup `row_view.rs` already provides"): that popup actually lives in `grid.rs` (`CellDetail`, grid.rs:180/1825) as grid-local state tied to a `ResultGrid` — it cannot be invoked without one. Same visual/interaction idiom, separate instance; `row_view.rs` contains no popup at all (file-location correction, same class as G6's ModalState note).
8. **`ModalState::KillConfirm` gains `error: Option<String>`** over the design's field sketch: the mandated failure-stays-in-dialog UX needs somewhere to render the message (same pattern as `QueryParams.error`).
9. **`open_monitor` takes an explicit `engine` parameter** (design's sketch had `(spec, read_only)`): the loop needs the engine for statement-set dispatch and `kill_sql` without re-deriving it from `ConnectSpec` (which would duplicate `engine_from_url` inside runner.rs).
10. **`MonitorCmd` carries no `Close`; connect-failure errors are reported against the first dispatched command's generation**: the design specifies drop-teardown (kept) but not how a connect failure survives the view's generation filter; since `MonitorView` always dispatches `Refresh{1}` on open, echoing that generation is the smallest correct answer.
11. **Refresh/pause/backoff state lives on the per-tab `MonitorView`, with AppView owning only the timer loop keyed by `tab_id`** (design §4's sketch had `view.monitor` on AppView): multiple simultaneous monitor tabs (risk #6) work by construction this way, and the design's own §5/§8 already put `MonitorView` in charge of that state — the sketch's placement was illustrative.
