//! G9: server-monitor data model + pure parsing/aggregation over
//! already-materialized text rows. Zero GPUI, zero driver crates — cell
//! data arrives in the same shape `runner::fetch_lookup_inner` already
//! produces from a drained `ResultBuffer` (see `Row`).

/// One result row as drained text cells; `None` = SQL NULL (same shape
/// as `runner::LookupResult`'s rows).
pub type Row = Vec<Option<String>>;

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionsTile {
    pub active: i64,
    pub idle: i64,
    pub max: Option<i64>, // None = "neomezeno"
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocksTile {
    pub waiting: i64,
    pub deadlocks_since_reset: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizeTile {
    pub data_bytes: Option<i64>,
    pub wal_or_log_bytes: Option<i64>, // None = that half's query failed ("n/a")
}

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
    pub pid: i64,
    pub user: Option<String>,
    pub application: Option<String>,
    pub client: Option<String>,
    pub state: Option<String>,
    pub duration_secs: f64,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockingEdge {
    pub waiter_pid: i64,
    pub blocker_pid: i64,
    pub wait_secs: f64,
    pub waiter_query: Option<String>,
    pub blocker_query: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockingNode {
    pub pid: i64,
    pub query: Option<String>,
    pub wait_secs: Option<f64>,
    pub children: Vec<BlockingNode>,
    pub cycle: bool,
}

/// Not in the plan's grounding code — found while fixing BLOCKER 1
/// (fmt_bytes/compute_rate review): `BlockingNode`'s owned `Vec<Self>`
/// children make it a recursive structure, so the *default* generated
/// `Drop` glue recurses one stack frame per tree depth when it goes out of
/// scope — the exact same class of stack overflow the iterative rewrite of
/// `build_blocking_tree` fixed for construction, just on the way down
/// instead of the way up (confirmed empirically: the 10k-depth regression
/// test built fine but then overflowed the stack when the test function's
/// local `tree` value dropped at end of scope). Drains the tree
/// iteratively with an explicit stack instead: each popped node has its
/// own `children` moved out (`Vec::append` leaves the source empty)
/// *before* it is allowed to drop, so the automatic per-field drop that
/// still runs after this method body sees an already-empty `Vec` and
/// recurses at most one level — never proportional to tree depth.
impl Drop for BlockingNode {
    fn drop(&mut self) {
        let mut pending: Vec<BlockingNode> = std::mem::take(&mut self.children);
        while let Some(mut node) = pending.pop() {
            pending.append(&mut node.children);
            // `node` drops here with `children` already empty.
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableSizeRow {
    pub schema: Option<String>,
    pub table: String,
    pub data_bytes: i64,
    pub index_bytes: i64,
    pub toast_bytes: i64,
    pub row_estimate: i64,
}

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
    /// Review-mandated (BLOCKER 2): `true` when `build_blocking_tree` hit
    /// `MONITOR_TREE_NODE_CAP` and stopped materializing further nodes —
    /// the rendered tree is a PREFIX of the real one, not the whole thing.
    /// `false` whenever `blocking` is `None` (that query failed — a
    /// different degrade, not a truncation).
    pub blocking_truncated: bool,
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

fn col_str(row: &Row, c: usize) -> Option<String> {
    row.get(c).and_then(|v| v.clone())
}

fn col_i64(row: &Row, c: usize) -> Option<i64> {
    let v = row.get(c)?.as_deref()?.trim().to_string();
    // Numeric aggregates (sum() over bigint) come back as arbitrary-precision
    // numeric text on Postgres — accept "123" and "123.0" alike (fail-soft).
    // Deviation from the plan's grounding code (review finding): the f64
    // fallback must reject non-finite text ("NaN"/"inf"/"-inf") — `f as i64`
    // is a saturating cast that would otherwise silently produce
    // i64::MAX/MIN/0, which is exactly the kind of extreme value that made
    // `compute_rate`'s subtraction overflow in production (see its doc
    // comment). Rejecting here means that path can no longer feed it.
    v.parse::<i64>()
        .ok()
        .or_else(|| v.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f as i64))
}

fn col_f64(row: &Row, c: usize) -> Option<f64> {
    // Deviation from the plan's grounding code: reject non-finite text
    // ("NaN"/"inf"/"-inf") rather than passing it through — see col_i64's
    // comment for why (review finding, advisory (a)).
    let v = row.get(c)?.as_deref()?.trim().parse::<f64>().ok()?;
    v.is_finite().then_some(v)
}

fn cell_i64(rows: &[Row], r: usize, c: usize) -> Option<i64> {
    col_i64(rows.get(r)?, c)
}

fn cell_f64(rows: &[Row], r: usize, c: usize) -> Option<f64> {
    col_f64(rows.get(r)?, c)
}

/// `[active, idle, max_conn]`
pub fn parse_connections(rows: &[Row]) -> ConnectionsTile {
    ConnectionsTile {
        active: cell_i64(rows, 0, 0).unwrap_or(0),
        idle: cell_i64(rows, 0, 1).unwrap_or(0),
        max: cell_i64(rows, 0, 2),
    }
}

/// `[waiting, deadlocks_since_reset]`
pub fn parse_locks(rows: &[Row]) -> LocksTile {
    LocksTile {
        waiting: cell_i64(rows, 0, 0).unwrap_or(0),
        deadlocks_since_reset: cell_i64(rows, 0, 1).unwrap_or(0),
    }
}

/// `[cache_hit_pct, uptime_secs, xact_total]` — tps always None here.
pub fn parse_perf(rows: &[Row]) -> PerfTile {
    PerfTile {
        cache_hit_pct: cell_f64(rows, 0, 0),
        uptime_secs: cell_i64(rows, 0, 1).unwrap_or(0),
        xact_total: cell_i64(rows, 0, 2),
        tps: None,
    }
}

/// `[pid, user, application, client, state, duration_secs, query]`
pub fn parse_running(rows: &[Row]) -> Vec<RunningQueryRow> {
    rows.iter()
        .map(|row| RunningQueryRow {
            pid: col_i64(row, 0).unwrap_or(0),
            user: col_str(row, 1),
            application: col_str(row, 2),
            client: col_str(row, 3),
            state: col_str(row, 4),
            duration_secs: col_f64(row, 5).unwrap_or(0.0),
            query: col_str(row, 6),
        })
        .collect()
}

/// `[waiter_pid, blocker_pid, wait_secs, waiter_query, blocker_query]`
pub fn parse_blocking_edges(rows: &[Row]) -> Vec<BlockingEdge> {
    rows.iter()
        .map(|row| BlockingEdge {
            waiter_pid: col_i64(row, 0).unwrap_or(0),
            blocker_pid: col_i64(row, 1).unwrap_or(0),
            wait_secs: col_f64(row, 2).unwrap_or(0.0),
            waiter_query: col_str(row, 3),
            blocker_query: col_str(row, 4),
        })
        .collect()
}

/// `[schema, table, data_bytes, index_bytes, toast_bytes, row_estimate]`
pub fn parse_tables(rows: &[Row]) -> Vec<TableSizeRow> {
    rows.iter()
        .map(|row| TableSizeRow {
            schema: col_str(row, 0),
            table: col_str(row, 1).unwrap_or_default(),
            data_bytes: col_i64(row, 2).unwrap_or(0),
            index_bytes: col_i64(row, 3).unwrap_or(0),
            toast_bytes: col_i64(row, 4).unwrap_or(0),
            row_estimate: col_i64(row, 5).unwrap_or(0),
        })
        .collect()
}

/// Hard cap on MATERIALIZED tree nodes — independent of, and NOT covered
/// by, `MONITOR_ROW_CAP` (runner.rs), which only bounds the number of
/// EDGES/rows the driver returns. Review-mandated fix (BLOCKER 2):
/// `build_blocking_tree`'s blocker-as-parent shape duplicates a waiter's
/// entire subtree once per DISTINCT blocker it's reachable from (no
/// sharing) — an ordinary fan-in "diamond ladder" (a handful of pids per
/// layer, each layer blocked by the whole previous layer) blows this up
/// combinatorially even though the edge count stays tiny and well within
/// `MONITOR_ROW_CAP` (empirically: 30 pids / 56 edges arranged as a
/// 15-layer diamond ladder materializes 65,534 nodes). Enforced inside
/// `build_tree_iterative` — when hit, expansion stops (fail-soft, the
/// caller gets a truncated-but-valid forest) and `TreeBudget::truncated`
/// is set so `MonitorSnapshot::blocking_truncated` can surface it.
pub const MONITOR_TREE_NODE_CAP: usize = 20_000;

/// Shared expansion budget threaded through every root's build (BLOCKER 2)
/// — a single cap across the WHOLE forest, not per-root, since an
/// adversarial/pathological set of edges could otherwise spend the cap
/// `roots.len()` times over.
struct TreeBudget {
    remaining: usize,
    truncated: bool,
}

/// Blocker-as-parent tree; roots = blockers that never appear as a
/// waiter. Cycle-safe: a wait-for cycle IS a live deadlock-in-progress —
/// path-tracked, marked `cycle: true` instead of recursing forever; a
/// pure cycle (no root at all) still renders, rooted at its
/// first-listed blocker. Returns `(forest, truncated)` — `truncated` is
/// `true` iff `MONITOR_TREE_NODE_CAP` was hit and some subtree(s) were cut
/// short (BLOCKER 2, review-mandated; see `MONITOR_TREE_NODE_CAP`'s doc
/// comment).
///
/// Deviation from the plan's grounding code (review finding, BLOCKER 1):
/// the plan's `build_node` was a plain recursive DFS — one stack frame per
/// tree depth. A long linear blocking chain (thousands of sessions each
/// waiting on the previous one — plausible given `MONITOR_ROW_CAP =
/// 10_000` in runner.rs) overflows the thread stack; on tokio worker
/// threads (smaller stacks than the test-thread default) this is reachable
/// in production, not just a pathological test. Rewritten as an explicit-
/// stack iterative traversal — no recursion, so depth is bounded only by
/// heap, not by stack. Cycle detection still needs "is this pid already on
/// the current root-to-node path", so `path` is carried alongside a
/// `HashSet` mirror for O(1) membership (a `Vec::contains` scan would be
/// O(depth) per node, i.e. O(n^2) on a linear chain of n edges).
pub fn build_blocking_tree(edges: &[BlockingEdge]) -> (Vec<BlockingNode>, bool) {
    use std::collections::{HashMap, HashSet};
    // Precomputed once: blocker_pid -> its outgoing edges (its waiters).
    // Avoids an O(n) scan of `edges` per node (the plan's grounding code
    // did `edges.iter().filter(...)` inside every `build_node` call).
    let mut children_of: HashMap<i64, Vec<&BlockingEdge>> = HashMap::new();
    let mut waiter_pids: HashSet<i64> = HashSet::new();
    for e in edges {
        children_of.entry(e.blocker_pid).or_default().push(e);
        waiter_pids.insert(e.waiter_pid);
    }

    let mut covered: HashSet<i64> = HashSet::new();
    let mut roots = Vec::new();
    let mut budget = TreeBudget { remaining: MONITOR_TREE_NODE_CAP, truncated: false };
    // Pass 1: true roots — blockers that never wait on anyone.
    for e in edges {
        let is_root = !waiter_pids.contains(&e.blocker_pid);
        if is_root && !covered.contains(&e.blocker_pid) {
            if budget.remaining == 0 {
                budget.truncated = true;
                break;
            }
            covered.insert(e.blocker_pid);
            roots.push(build_tree_iterative(e.blocker_pid, &children_of, &mut covered, &mut budget));
        }
    }
    // Pass 2: pure cycles have NO root (every participant waits on someone)
    // — a live deadlock-in-progress must still render, so start one tree
    // per still-uncovered blocker; the path check below marks the loop
    // closure with `cycle: true` instead of recursing forever.
    for e in edges {
        if !covered.contains(&e.blocker_pid) {
            if budget.remaining == 0 {
                budget.truncated = true;
                break;
            }
            covered.insert(e.blocker_pid);
            roots.push(build_tree_iterative(e.blocker_pid, &children_of, &mut covered, &mut budget));
        }
    }
    (roots, budget.truncated)
}

/// One stack frame of the iterative post-order tree build: the node under
/// construction plus an index into `children_of[pid]` for the next
/// not-yet-visited child edge. Deliberately holds no borrowed iterator (and
/// so no lifetime parameter) — re-looking-up `children_of.get(&pid)` by
/// value each step is an O(1) hash lookup, simpler than threading borrow
/// lifetimes through the explicit stack.
struct BuildFrame {
    pid: i64,
    query: Option<String>,
    wait_secs: Option<f64>,
    next_idx: usize,
    children: Vec<BlockingNode>,
}

/// Iterative replacement for the plan's recursive `build_node` (see
/// `build_blocking_tree`'s doc comment). Same output shape and cycle
/// semantics: a pid already on the current path becomes a `cycle: true`
/// leaf instead of being descended into again. `budget` (BLOCKER 2):
/// consumed one unit per MATERIALIZED node (the root here included); once
/// exhausted, every further edge in this build — root's own children AND
/// any still-open ancestor frame's remaining children, since the check
/// runs on every loop iteration regardless of which frame is on top — is
/// treated as absent, so the walk unwinds and closes out whatever was
/// already built rather than expanding further.
fn build_tree_iterative(
    root_pid: i64,
    children_of: &std::collections::HashMap<i64, Vec<&BlockingEdge>>,
    covered: &mut std::collections::HashSet<i64>,
    budget: &mut TreeBudget,
) -> BlockingNode {
    use std::collections::HashSet;
    let root_query = children_of.get(&root_pid).and_then(|v| v.first()).and_then(|e| e.blocker_query.clone());

    if budget.remaining == 0 {
        budget.truncated = true;
        return BlockingNode { pid: root_pid, query: root_query, wait_secs: None, children: Vec::new(), cycle: false };
    }
    budget.remaining -= 1; // the root node itself counts against the cap.

    // `path_set` alone is enough: membership answers the cycle check, and
    // popping a frame already knows its own pid to remove — no separate
    // ordered path vector needed.
    let mut path_set: HashSet<i64> = HashSet::from([root_pid]);
    let mut stack: Vec<BuildFrame> =
        vec![BuildFrame { pid: root_pid, query: root_query, wait_secs: None, next_idx: 0, children: Vec::new() }];

    loop {
        let pid = stack.last().unwrap().pid;
        let idx = stack.last().unwrap().next_idx;
        let real_edge = children_of.get(&pid).and_then(|v| v.get(idx)).copied();
        let edge = if budget.remaining == 0 { None } else { real_edge };
        if real_edge.is_some() && edge.is_none() {
            // There WAS a next edge to expand but the cap forced an early
            // stop — a genuine truncation, not just "ran out of children".
            budget.truncated = true;
        }
        match edge {
            Some(e) => {
                stack.last_mut().unwrap().next_idx += 1;
                budget.remaining -= 1;
                covered.insert(e.waiter_pid);
                if path_set.contains(&e.waiter_pid) {
                    stack.last_mut().unwrap().children.push(BlockingNode {
                        pid: e.waiter_pid,
                        query: e.waiter_query.clone(),
                        wait_secs: Some(e.wait_secs),
                        children: Vec::new(),
                        cycle: true,
                    });
                } else {
                    path_set.insert(e.waiter_pid);
                    stack.push(BuildFrame {
                        pid: e.waiter_pid,
                        query: e.waiter_query.clone(),
                        wait_secs: Some(e.wait_secs),
                        next_idx: 0,
                        children: Vec::new(),
                    });
                }
            }
            None => {
                let frame = stack.pop().unwrap();
                path_set.remove(&frame.pid);
                let node = BlockingNode {
                    pid: frame.pid,
                    query: frame.query,
                    wait_secs: frame.wait_secs,
                    children: frame.children,
                    cycle: false,
                };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => return node,
                }
            }
        }
    }
}

/// Client-side delta over a CUMULATIVE counter. `None` on: no previous
/// sample, non-positive elapsed, or a counter that went BACKWARD
/// (stats reset / server restart).
pub fn compute_rate(
    now_total: i64,
    prev: Option<(i64, std::time::Instant)>,
    at: std::time::Instant,
) -> Option<f64> {
    let (prev_total, prev_at) = prev?;
    // Deviation from the plan's grounding code (review finding, BLOCKER 2):
    // the plan computed `now_total < prev_total` then `(now_total -
    // prev_total) as f64`, which still panics with "attempt to subtract
    // with overflow" in debug builds when the subtraction itself overflows
    // i64's range (e.g. now=i64::MAX, prev=i64::MIN — both individually
    // valid i64 values, `now >= prev` holds, but the difference doesn't fit
    // i64). `checked_sub` catches that case; the ordinary counter-reset
    // case (now < prev, no overflow) is still caught by the `delta < 0`
    // check below, so both existing semantics are preserved.
    let delta = now_total.checked_sub(prev_total)?;
    if delta < 0 {
        return None;
    }
    let elapsed = at.checked_duration_since(prev_at)?.as_secs_f64();
    if elapsed <= 0.0 {
        return None;
    }
    Some(delta as f64 / elapsed)
}

/// 0.0..=1.0 fill fraction for the per-table size bar; `max_in_set <= 0`
/// or `size <= 0` -> 0.0.
pub fn bar_fraction(size: i64, max_in_set: i64) -> f32 {
    if max_in_set <= 0 || size <= 0 {
        return 0.0;
    }
    ((size as f64 / max_in_set as f64) as f32).clamp(0.0, 1.0)
}

/// `Err(first error message)` iff EVERY query failed (drives the view's
/// backoff); otherwise `Ok` with per-tile `None`s for the failures.
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
    // BLOCKER 2 fix: `build_blocking_tree` now returns `(forest, truncated)`
    // — split out here so `MonitorSnapshot::blocking_truncated` can carry
    // the flag (`false` when the query itself failed; that's a different,
    // already-represented degrade via `blocking: None`).
    let (blocking, blocking_truncated) = match r.blocking {
        Ok(rows) => {
            let (tree, truncated) = build_blocking_tree(&parse_blocking_edges(&rows));
            (Some(tree), truncated)
        }
        Err(_) => (None, false),
    };
    Ok(MonitorSnapshot {
        connections: r.connections.ok().map(|rows| parse_connections(&rows)),
        locks: r.locks.ok().map(|rows| parse_locks(&rows)),
        size: SizeTile {
            data_bytes: r.data_size.ok().and_then(|rows| cell_i64(&rows, 0, 0)),
            wal_or_log_bytes: r.wal_size.ok().and_then(|rows| cell_i64(&rows, 0, 0)),
        },
        perf: r.perf.ok().map(|rows| parse_perf(&rows)),
        running: r.running.ok().map(|rows| parse_running(&rows)),
        blocking,
        blocking_truncated,
        tables: r.tables.ok().map(|rows| parse_tables(&rows)),
        fetched_at,
    })
}

/// Engine gating (design §7): Postgres only. Sqlite -> false (spec: no
/// monitor tab); Mssql -> false FOR NOW — flips automatically once
/// `connect::open_config`'s `Engine::Mssql` arm stops erroring and this
/// one function is updated; no other monitor-side change needed.
pub fn monitor_available(engine: dbc_state::Engine) -> bool {
    matches!(engine, dbc_state::Engine::Postgres)
}

/// "1.5 GB" / "512 B" — tile + table-size labels.
pub fn fmt_bytes(bytes: i64) -> String {
    // Deviation from the plan's grounding code (review advisory (b)): clamp
    // negative to 0, same posture as fmt_uptime — a negative byte count is
    // a nonsensical display value (e.g. a driver quirk), not something to
    // render as "-5 B".
    let bytes = bytes.max(0);
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    format!("{value:.1} {}", UNITS[unit_idx])
}

/// "3d 4h 12m" / "4h 12m" / "12m" — uptime label. Negative clamps to 0.
pub fn fmt_uptime(secs: i64) -> String {
    let secs = secs.max(0);
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let minutes = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

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

    /// Regression test for review advisory (a): "NaN"/"inf"/"-inf" text
    /// must degrade to None, not to a saturating-cast extreme (i64::MAX /
    /// i64::MIN / 0 for col_i64's f64 fallback) — that extreme is exactly
    /// the kind of value that fed BLOCKER 2's subtraction overflow.
    #[test]
    fn non_finite_text_degrades_to_none_not_a_saturated_extreme() {
        let rows = vec![row(&[Some("NaN"), Some("inf"), Some("-inf")])];
        // cache_hit_pct (col_f64), uptime_secs (col_i64, unwrap_or(0)), xact_total (col_i64)
        let tile = parse_perf(&rows);
        assert_eq!(tile.cache_hit_pct, None);
        assert_eq!(tile.uptime_secs, 0); // fail-soft default, not a saturated cast
        assert_eq!(tile.xact_total, None);
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

    /// Regression test for review BLOCKER 2: the plan's grounding code did
    /// `now_total < prev_total` then `(now_total - prev_total) as f64`,
    /// which panics with "attempt to subtract with overflow" in debug
    /// builds when now=i64::MAX and prev=i64::MIN — both are individually
    /// valid i64 values and `now >= prev` holds, so the `<` guard doesn't
    /// catch it, but the subtraction itself doesn't fit i64's range.
    /// `checked_sub` must catch this and return None, same as an ordinary
    /// counter reset.
    #[test]
    fn compute_rate_subtraction_overflow_is_none_not_a_panic() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(compute_rate(i64::MAX, Some((i64::MIN, t0)), t1), None);
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
        let (tree, truncated) = build_blocking_tree(&[edge(20, 10), edge(30, 20)]);
        assert!(!truncated);
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
        let (tree, truncated) = build_blocking_tree(&[edge(2, 1), edge(4, 3)]);
        assert!(!truncated);
        let root_pids: Vec<i64> = tree.iter().map(|n| n.pid).collect();
        assert_eq!(root_pids, vec![1, 3]);
    }

    #[test]
    fn cycle_is_marked_not_infinite() {
        // 1 waits on 2, 2 waits on 1 — a live deadlock-in-progress. No true
        // root; pass 2 roots it at the first-listed blocker and the loop
        // closure is a `cycle: true` leaf.
        let (tree, truncated) = build_blocking_tree(&[edge(1, 2), edge(2, 1)]);
        assert!(!truncated);
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
        let (tree, truncated) = build_blocking_tree(&[edge(5, 5)]);
        assert!(!truncated);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, 5);
        assert!(tree[0].children.iter().all(|c| c.cycle));
    }

    #[test]
    fn no_edges_no_tree() {
        assert_eq!(build_blocking_tree(&[]), (Vec::new(), false));
    }

    /// Regression test for review BLOCKER 1: the plan's grounding code used
    /// plain recursion for `build_node`, one stack frame per tree depth — a
    /// long linear blocking chain overflowed the thread stack
    /// (`STATUS_STACK_OVERFLOW`), and production runs on tokio worker
    /// threads with smaller stacks than the test-thread default, so this
    /// was reachable, not just a pathological test. `build_tree_iterative`
    /// replaced the recursion with an explicit heap-allocated stack; this
    /// must build a 10,000-edge linear chain (waiter i+1 blocked by i) to
    /// depth 10,000 without aborting the process.
    #[test]
    fn ten_thousand_edge_linear_chain_builds_without_stack_overflow() {
        let edges: Vec<BlockingEdge> = (0..10_000).map(|i| edge(i + 1, i)).collect();
        let (tree, truncated) = build_blocking_tree(&edges);
        // 10,001 nodes total — under MONITOR_TREE_NODE_CAP (20_000), so this
        // must NOT be reported as truncated.
        assert!(!truncated);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, 0);
        // Walk the chain down to depth 10,000, confirming no truncation and
        // no cycle mis-marking anywhere along the way.
        let mut node = &tree[0];
        for depth in 0..10_000 {
            assert!(!node.cycle, "node at depth {depth} incorrectly marked cycle");
            assert_eq!(node.children.len(), 1, "node at depth {depth} should have exactly one child");
            node = &node.children[0];
        }
        assert_eq!(node.pid, 10_000);
    }

    /// Iterative node counter for test assertions — never recurses over a
    /// `BlockingNode` tree (same stack-safety posture as
    /// `build_tree_iterative`/`flatten_blocking_tree`), even though these
    /// diamond-shaped test trees are shallow (bounded by `LAYERS`) so a
    /// naive recursive counter would also be safe here.
    fn count_nodes_iterative(roots: &[BlockingNode]) -> usize {
        let mut count = 0usize;
        let mut stack: Vec<&BlockingNode> = roots.iter().collect();
        while let Some(node) = stack.pop() {
            count += 1;
            stack.extend(node.children.iter());
        }
        count
    }

    /// Regression test for review BLOCKER 2: the blocker-as-parent tree
    /// shape has NO subtree sharing — a waiter reachable via two different
    /// blockers gets its whole subtree materialized once PER blocker. An
    /// ordinary fan-in "diamond ladder" (each layer's pids are blocked by
    /// EVERY pid in the layer below) blows this up combinatorially even
    /// though the edge/pid count stays tiny: 15 layers x 2 pids/layer = 30
    /// pids, 56 edges, but ~65,534 materialized nodes with no cap —
    /// `MONITOR_TREE_NODE_CAP` must hold this to <= 20,000 and report
    /// `truncated`.
    #[test]
    fn diamond_ladder_fan_in_stays_within_the_node_cap_and_reports_truncated() {
        const LAYERS: i64 = 15;
        const PER_LAYER: i64 = 2;
        let mut edges = Vec::new();
        for layer in 1..LAYERS {
            for w in 0..PER_LAYER {
                let waiter = layer * PER_LAYER + w;
                for b in 0..PER_LAYER {
                    let blocker = (layer - 1) * PER_LAYER + b;
                    edges.push(edge(waiter, blocker));
                }
            }
        }
        assert_eq!(edges.len(), 56, "sanity: 14 non-root layers x 2 waiters x 2 blockers");

        let (tree, truncated) = build_blocking_tree(&edges);
        assert!(truncated, "the diamond ladder must exceed MONITOR_TREE_NODE_CAP and report truncation");

        let total = count_nodes_iterative(&tree);
        assert!(total <= MONITOR_TREE_NODE_CAP, "materialized {total} nodes, cap is {MONITOR_TREE_NODE_CAP}");
    }

    /// BLOCKER 2 companion case: an ordinary small tree (far under the cap)
    /// must build EXACTLY as before — untruncated, identical shape to the
    /// pre-cap behaviour.
    #[test]
    fn normal_small_graph_is_unaffected_by_the_node_cap() {
        let (tree, truncated) = build_blocking_tree(&[edge(20, 10), edge(30, 20)]);
        assert!(!truncated);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, 10);
        assert_eq!(tree[0].children[0].pid, 20);
        assert_eq!(tree[0].children[0].children[0].pid, 30);
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
    fn fmt_bytes_clamps_negative() {
        // Review advisory (b): same clamp posture as fmt_uptime — a
        // negative byte count is nonsensical, never rendered as "-5 B".
        assert_eq!(fmt_bytes(-5), "0 B");
    }

    #[test]
    fn fmt_uptime_tiers() {
        assert_eq!(fmt_uptime(59), "0m");
        assert_eq!(fmt_uptime(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(fmt_uptime(2 * 86_400 + 3600), "2d 1h 0m");
        assert_eq!(fmt_uptime(-5), "0m"); // clock skew clamps, never panics
    }
}
