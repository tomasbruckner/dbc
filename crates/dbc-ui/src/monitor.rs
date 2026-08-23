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
    v.parse::<i64>().ok().or_else(|| v.parse::<f64>().ok().map(|f| f as i64))
}

fn col_f64(row: &Row, c: usize) -> Option<f64> {
    row.get(c)?.as_deref()?.trim().parse::<f64>().ok()
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

/// Blocker-as-parent tree; roots = blockers that never appear as a
/// waiter. Cycle-safe: a wait-for cycle IS a live deadlock-in-progress —
/// path-tracked, marked `cycle: true` instead of recursing forever; a
/// pure cycle (no root at all) still renders, rooted at its
/// first-listed blocker.
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

/// Client-side delta over a CUMULATIVE counter. `None` on: no previous
/// sample, non-positive elapsed, or a counter that went BACKWARD
/// (stats reset / server restart).
pub fn compute_rate(
    now_total: i64,
    prev: Option<(i64, std::time::Instant)>,
    at: std::time::Instant,
) -> Option<f64> {
    let (prev_total, prev_at) = prev?;
    if now_total < prev_total {
        return None;
    }
    let elapsed = at.checked_duration_since(prev_at)?.as_secs_f64();
    if elapsed <= 0.0 {
        return None;
    }
    Some((now_total - prev_total) as f64 / elapsed)
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

/// Engine gating (design §7): Postgres only. Sqlite -> false (spec: no
/// monitor tab); Mssql -> false FOR NOW — flips automatically once
/// `connect::open_config`'s `Engine::Mssql` arm stops erroring and this
/// one function is updated; no other monitor-side change needed.
pub fn monitor_available(engine: dbc_state::Engine) -> bool {
    matches!(engine, dbc_state::Engine::Postgres)
}

/// "1.5 GB" / "512 B" — tile + table-size labels.
pub fn fmt_bytes(bytes: i64) -> String {
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
