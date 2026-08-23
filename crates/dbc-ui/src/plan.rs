//! Execution-plan model, hot-node formulas, per-engine `EXPLAIN` SQL
//! builders, and the SQLite `EXPLAIN QUERY PLAN` parser (G13 T1).
//!
//! See `docs/superpowers/specs/drafts/g13-execution-plans-design.md` and
//! `docs/superpowers/plans/2026-08-23-g13-execution-plans.md` (Task 1) for
//! the binding design this file implements.

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

impl Drop for PlanNode {
    fn drop(&mut self) {
        let mut stack: Vec<PlanNode> = std::mem::take(&mut self.children);
        while let Some(mut node) = stack.pop() {
            stack.extend(std::mem::take(&mut node.children));
            // `node` drops here with empty `children` — no recursion.
        }
    }
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
pub fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|f| f.is_finite())
}

/// Actual-plan self time (design §2 formula): this node's own cumulative
/// time across all loops, minus its DIRECT children's cumulative time,
/// clamped to >= 0.0. Shallow — O(children.len()), never recurses.
pub fn self_time_ms(node: &PlanNode) -> f64 {
    let own = node.actual_time_ms.unwrap_or(0.0) * node.loops.unwrap_or(1) as f64;
    let children_total: f64 = node
        .children
        .iter()
        .map(|c| c.actual_time_ms.unwrap_or(0.0) * c.loops.unwrap_or(1) as f64)
        .sum();
    (own - children_total).max(0.0)
}

/// Estimated-plan self cost, same shape, using `est_cost`.
pub fn self_cost(node: &PlanNode) -> f64 {
    let own = node.est_cost.unwrap_or(0.0);
    let children_total: f64 = node.children.iter().map(|c| c.est_cost.unwrap_or(0.0)).sum();
    (own - children_total).max(0.0)
}

/// `None` only when there is truly nothing to normalize against (SQLite
/// estimated plans have no `est_cost` anywhere); a zero/absent-total
/// denominator otherwise yields `Some(0.0)`, never NaN.
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

/// `EXPLAIN ...` text for the ALWAYS-SAFE estimated path (§5: never
/// executes the statement on any engine — no gating needed, ever).
pub fn explain_sql(engine: dbc_state::Engine, sql: &str) -> String {
    match engine {
        dbc_state::Engine::Postgres => format!("EXPLAIN (FORMAT JSON) {sql}"),
        // T7 (deferred): unreachable until the MSSQL driver phase wires
        // `connect::open_config`'s Engine::Mssql arm.
        dbc_state::Engine::Mssql => format!("SET SHOWPLAN_XML ON; {sql}"),
        dbc_state::Engine::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
    }
}

/// `EXPLAIN ANALYZE ...`-family text, or `None` when the engine has no
/// such mode (SQLite — §1c, the "Analyze" button is hidden entirely).
pub fn explain_analyze_sql(engine: dbc_state::Engine, sql: &str) -> Option<String> {
    match engine {
        dbc_state::Engine::Postgres => Some(format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}")),
        dbc_state::Engine::Mssql => {
            Some(format!("SET STATISTICS XML ON; {sql}; SET STATISTICS XML OFF;"))
        }
        dbc_state::Engine::Sqlite => None,
    }
}

/// Whether the "Analyze" button should render at all for `engine`.
pub fn analyze_button_visible(engine: dbc_state::Engine) -> bool {
    !matches!(engine, dbc_state::Engine::Sqlite)
}

/// SQLite's `EXPLAIN QUERY PLAN` row shape: `(id, parent, detail)` —
/// `notused` (SQLite's docs: always 0, reserved) is dropped by the caller
/// before this function ever sees the row (main.rs, T6).
///
/// Leading-verb allowlist per SQLite's actual `EXPLAIN QUERY PLAN` output
/// vocabulary; anything else falls back to the whole `detail` text as
/// `operation` with `target: None` (fail-open on display text, never
/// panic — design §1c).
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

fn operation_and_target(detail: &str) -> (String, Option<String>) {
    let leading = detail.split_ascii_whitespace().next().unwrap_or(detail).to_string();
    // NOTE (deviation from the plan's grounding code, T1): "USE" is
    // deliberately excluded here even though it's a real SQLite EXPLAIN
    // QUERY PLAN leading verb. Every real "USE" line SQLite emits is "USE
    // TEMP B-TREE FOR ORDER BY"/"... FOR GROUP BY" — it never has a
    // TABLE/INDEX target, so treating it as "known" would collapse the
    // whole descriptive detail down to a bare "USE" and silently discard
    // the only useful information in the line. The plan's own Step-1 test
    // `sqlite_non_matching_detail_is_fail_open` exercises exactly this
    // string and expects the fail-open (full-text) behavior, which
    // contradicts the plan's grounding code listing "USE" as known — the
    // test (verbatim from the plan, TDD-authoritative) wins.
    let known = matches!(leading.as_str(), "SCAN" | "SEARCH" | "CO-ROUTINE" | "COMPOUND" | "EXECUTE");
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

// --- GPUI-flavoured half of this file (G13 T5): `PlanView`, the tree-
// rendering entity, plus the pure `build_index`/`flatten_plan`/`expand_all`/
// `find_by_id` helpers it's built on. Mirrors `schema_tree.rs`'s layout
// convention: pure flatten helpers first (unit-tested with no GPUI
// dependency), the entity second.
//
// PERFORMANCE DEVIATION FROM THE PLAN'S OWN GROUNDING CODE (reality wins
// over the plan — found by adversarial review of the first T5 commit,
// f0e96ef): the plan's grounding code used a path-based `PlanNodeId =
// Vec<usize>` (`[]` = root, `[0, 2]` = ...), rebuilt by cloning that path
// at every node on every `flatten_plan` call, and `flatten_plan` was
// called fresh on every `render` AND again inside the click handler.
// Measured against a synthetic 50,000-deep chain, that shape is O(n^2):
// `HashSet<Vec<usize>>` hashes an O(depth) key per node and every
// `id.clone()` is an O(depth) allocation, so `expand_all`+`flatten_plan`
// took ~73s (vs. ~24ms for a 50k-WIDE, depth-1 tree of the same node
// count). This file instead assigns each node a stable pre-order `usize`
// index exactly ONCE (`build_index`, a single O(n) iterative pass over the
// immutable `Rc<PlanResult>` — ids never change afterward, since the
// result tree never mutates for the life of a `PlanView`), makes
// `PlanNodeId` that `usize`, and caches the flattened VISIBLE-row list on
// `PlanView` itself (`rows_cache`), recomputed only in `new`/
// `toggle_expand` — never inside `render` or the click handler. See
// `plan_view_tests::deep_chain_build_expand_flatten_is_not_quadratic` for
// the measured bound this restores.

use std::collections::HashSet;
use std::rc::Rc;
use gpui::{
    div, prelude::*, px, rgb, rgba, uniform_list, App, ClickEvent, Context, EventEmitter,
    FocusHandle, Focusable, Window,
};

/// Stable pre-order index into `PlanView`'s `index` (`build_index`'s
/// output) — NOT a path (see the perf deviation note above). Stable for
/// the life of a `PlanView` because `Rc<PlanResult>` never mutates after
/// `PlanView::new`.
pub type PlanNodeId = usize;

/// One entry per `PlanNode`, in pre-order — built once by `build_index`.
/// Holds everything `PlanFlatNode`/the node-detail popup need, so neither
/// ever has to walk the original (`Clone`-less) `PlanNode` tree again.
struct PlanIndexEntry {
    depth: usize,
    children: Vec<PlanNodeId>,
    operation: String,
    target: Option<String>,
    est_cost: Option<f64>,
    est_rows: Option<f64>,
    actual_rows: Option<f64>,
    actual_time_total_ms: Option<f64>,
    buffers: Option<BufferStats>,
    hot_fraction: Option<f32>,
    extra: Vec<(String, String)>,
}

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

/// Single O(n) iterative pre-order pass over the ACTUAL `PlanNode` tree
/// (Global Constraints: never recurse) — the only place this file ever
/// walks `PlanNode.children` directly. Every other helper below
/// (`flatten_plan`/`expand_all`/`find_by_id`) works off this index using
/// plain `usize` ids and `Vec`/`HashSet<usize>`, which are O(1) to
/// clone/hash — the fix for the O(n^2) blowup the path-based `PlanNodeId`
/// had.
fn build_index(result: &PlanResult) -> Vec<PlanIndexEntry> {
    let mut out: Vec<PlanIndexEntry> = Vec::new();
    let mut stack: Vec<(&PlanNode, usize, Option<PlanNodeId>)> = vec![(&result.root, 0, None)];
    while let Some((node, depth, parent)) = stack.pop() {
        let hot = hot_fraction(node, &result.root, result.is_analyze, result.total_execution_time_ms)
            .map(|f| f as f32);
        let my_id = out.len();
        out.push(PlanIndexEntry {
            depth,
            children: Vec::new(),
            operation: node.operation.clone(),
            target: node.target.clone(),
            est_cost: node.est_cost,
            est_rows: node.est_rows,
            actual_rows: node.actual_rows,
            actual_time_total_ms: node.actual_time_ms.map(|t| t * node.loops.unwrap_or(1) as f64),
            buffers: node.buffers.clone(),
            hot_fraction: hot,
            extra: node.extra.clone(),
        });
        if let Some(p) = parent {
            out[p].children.push(my_id);
        }
        for child in node.children.iter().rev() {
            stack.push((child, depth + 1, Some(my_id)));
        }
    }
    out
}

/// Iterative pre-order flatten over the ALREADY-BUILT `index` (Global
/// Constraints: never recurse). Visits a node's children only when the
/// node's own id is in `expanded`, so a collapsed subtree costs nothing
/// beyond one O(1) id check — O(visible-node-count) total, with no path
/// allocation per node.
fn flatten_plan(index: &[PlanIndexEntry], expanded: &HashSet<PlanNodeId>) -> Vec<PlanFlatNode> {
    if index.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack: Vec<PlanNodeId> = vec![0];
    while let Some(id) = stack.pop() {
        let entry = &index[id];
        out.push(PlanFlatNode {
            id,
            depth: entry.depth,
            operation: entry.operation.clone(),
            target: entry.target.clone(),
            est_cost: entry.est_cost,
            est_rows: entry.est_rows,
            actual_rows: entry.actual_rows,
            actual_time_total_ms: entry.actual_time_total_ms,
            buffers: entry.buffers.clone(),
            hot_fraction: entry.hot_fraction,
            expandable: !entry.children.is_empty(),
        });
        if entry.children.is_empty() || !expanded.contains(&id) {
            continue;
        }
        for &child in entry.children.iter().rev() {
            stack.push(child);
        }
    }
    out
}

/// Every node id in `index` — used to seed `PlanView`'s default
/// fully-expanded state (a plan is typically tens of nodes, not thousands
/// of schema objects, so — unlike `SchemaTree`'s deliberately-collapsed
/// default — showing the whole shape immediately is the useful default).
/// O(n): `build_index` already visited every node once, so this is just
/// every index in range — no tree walk needed.
fn expand_all(index: &[PlanIndexEntry]) -> HashSet<PlanNodeId> {
    (0..index.len()).collect()
}

/// O(1) lookup by stable pre-order id — never panics, `None` on an
/// out-of-range id (Global Constraints: errors are values).
fn find_by_id(index: &[PlanIndexEntry], id: PlanNodeId) -> Option<&PlanIndexEntry> {
    index.get(id)
}

/// Mirrors `grid.rs`'s `CellDetail`/`render_cell_detail_overlay` idiom
/// (grid.rs:180/1825-1911) EXACTLY — dimmed `.occlude()`d backdrop,
/// centered bordered panel, explicit "Zavřít" close button — but a
/// SEPARATE local instance: `CellDetail` is `ResultGrid`-local state, and
/// `PlanView` is a different entity with no `ResultGrid` to borrow one
/// from (same file-location correction G9's plan already made for its own
/// query-detail popup). `grid.rs`'s idiom has no click-outside-to-dismiss
/// (only the explicit close button), so this doesn't add one either — a
/// deviation from this file's OWN first-commit version, which put
/// `on_mouse_down` straight on the backdrop with no `.occlude()`, letting
/// clicks fall through to the `uniform_list` rows underneath (adversarial
/// review MAJOR 1 on commit f0e96ef).
struct PlanNodeDetail {
    text: String,
    #[allow(dead_code)] // mirrors grid.rs's CellDetail shape; no scroll-wheel wiring in this task
    scroll_lines: usize,
}

pub enum PlanViewEvent {}

pub struct PlanView {
    result: Rc<PlanResult>,
    index: Vec<PlanIndexEntry>,
    expanded: HashSet<PlanNodeId>,
    /// Cached output of `flatten_plan(&self.index, &self.expanded)` —
    /// recomputed ONLY in `new` and `toggle_expand`, never inside `render`
    /// or the node-click handler (see the perf deviation note above this
    /// module section).
    rows_cache: Vec<PlanFlatNode>,
    show_raw: bool,
    node_detail: Option<PlanNodeDetail>,
    focus_handle: FocusHandle,
}

impl PlanView {
    pub fn new(result: Rc<PlanResult>, cx: &mut Context<Self>) -> Self {
        let index = build_index(&result);
        let expanded = expand_all(&index);
        let rows_cache = flatten_plan(&index, &expanded);
        Self {
            result,
            index,
            expanded,
            rows_cache,
            show_raw: false,
            node_detail: None,
            focus_handle: cx.focus_handle(),
        }
    }

    fn toggle_expand(&mut self, id: PlanNodeId) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rows_cache = flatten_plan(&self.index, &self.expanded);
    }

    fn open_node_detail(&mut self, flat_ix: usize) {
        let Some(flat) = self.rows_cache.get(flat_ix) else { return };
        let mut lines = vec![format!("operation: {}", flat.operation)];
        if let Some(t) = &flat.target {
            lines.push(format!("target: {t}"));
        }
        // `extra`/raw buffer fields aren't on `PlanFlatNode` (row-display
        // shape only) — O(1) lookup back into the index by id.
        if let Some(entry) = find_by_id(&self.index, flat.id) {
            for (k, v) in &entry.extra {
                lines.push(format!("{k}: {v}"));
            }
            if let Some(b) = &entry.buffers {
                lines.push(format!(
                    "buffers: hit={:?} read={:?} dirtied={:?} written={:?} temp_read={:?} temp_written={:?}",
                    b.shared_hit, b.shared_read, b.shared_dirtied, b.shared_written, b.temp_read, b.temp_written
                ));
            }
        }
        self.node_detail = Some(PlanNodeDetail { text: lines.join("\n"), scroll_lines: 0 });
    }
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
        let header = div()
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

        // No re-flatten here: `self.rows_cache` is already current (built
        // in `new`, kept current by `toggle_expand`) — the row-count and
        // every per-row read below come straight from the cache.
        let row_count = self.rows_cache.len();
        root = root.child(
            uniform_list(
                "plan-tree-rows",
                row_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let mut items = Vec::with_capacity(range.len());
                    for ix in range {
                        // Copy out the small bits of data this row needs
                        // BEFORE building the `cx.listener` closures below,
                        // so those closures' `&mut this` doesn't conflict
                        // with an outstanding `&this.rows_cache[ix]`
                        // borrow.
                        let (id, depth, expandable, label, est_cost, est_rows, actual_rows, actual_time_total_ms, has_buffers, hot_fraction) = {
                            let flat = &this.rows_cache[ix];
                            let label = match &flat.target {
                                Some(t) => format!("{} ({t})", flat.operation),
                                None => flat.operation.clone(),
                            };
                            (
                                flat.id,
                                flat.depth,
                                flat.expandable,
                                label,
                                flat.est_cost,
                                flat.est_rows,
                                flat.actual_rows,
                                flat.actual_time_total_ms,
                                flat.buffers.is_some(),
                                flat.hot_fraction,
                            )
                        };
                        let is_expanded = this.expanded.contains(&id);
                        let chevron = if expandable { if is_expanded { "▾" } else { "▸" } } else { " " };

                        let mut row = div()
                            .id(("plan-row", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(22.))
                            .pl(px(6. + depth as f32 * 14.))
                            .text_color(rgb(0xcdd6f4))
                            .hover(|s| s.bg(rgb(0x313244)));
                        // Hot-node coloring applies to the ROW background,
                        // not just one column (design §2/§4).
                        match hot_fraction {
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
                                        this.toggle_expand(id);
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
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                        this.open_node_detail(ix);
                                        cx.notify();
                                    })),
                            )
                            .child(div().w(px(70.)).child(fmt_metric(est_cost)))
                            .child(div().w(px(70.)).child(fmt_metric(est_rows)))
                            .child(div().w(px(70.)).child(fmt_metric(actual_rows)))
                            .child(div().w(px(70.)).child(fmt_metric(actual_time_total_ms)))
                            .child(div().w(px(20.)).child(if has_buffers { "▤" } else { "" }));
                        items.push(row);
                    }
                    items
                }),
            )
            .flex_1(),
        );

        if let Some(detail) = &self.node_detail {
            let panel = div()
                .id("plan-node-detail-panel")
                .w(px(560.))
                .max_h(px(420.))
                .bg(rgb(0x1e1e2e))
                .border_1()
                .border_color(rgb(0x45475a))
                .rounded_md()
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("plan-node-detail-body")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .overflow_hidden()
                        .p_2()
                        .text_color(rgb(0xcdd6f4))
                        .child(detail.text.clone()),
                )
                .child(
                    div().flex().flex_row().justify_end().gap_2().p_2().child(
                        div()
                            .id("plan-node-detail-close")
                            .cursor_pointer()
                            .bg(rgb(0x313244))
                            .text_color(rgb(0xcdd6f4))
                            .px_2()
                            .rounded_md()
                            .child("Zavřít")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.node_detail = None;
                                cx.notify();
                            })),
                    ),
                );

            root = root.child(
                div()
                    .id("plan-node-detail")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x00000099))
                    .occlude()
                    .child(panel),
            );
        }
        root
    }
}

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
        let index = build_index(&result);
        let all = expand_all(&index);
        assert_eq!(all.len(), 3); // root + 2 children
        assert!(all.contains(&0)); // root
        assert!(all.contains(&1)); // child_a (orders)
        assert!(all.contains(&2)); // child_b (users)
    }

    #[test]
    fn flatten_plan_fully_expanded_visits_all_in_order() {
        let result = sample_result();
        let index = build_index(&result);
        let expanded = expand_all(&index);
        let rows = flatten_plan(&index, &expanded);
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
        let index = build_index(&result);
        let expanded = HashSet::new(); // nothing expanded
        let rows = flatten_plan(&index, &expanded);
        assert_eq!(rows.len(), 1); // just the root
        assert!(rows[0].expandable);
    }

    #[test]
    fn find_by_id_looks_up_by_stable_index() {
        let result = sample_result();
        let index = build_index(&result);
        assert_eq!(find_by_id(&index, 0).unwrap().operation, "Hash Join");
        assert_eq!(find_by_id(&index, 1).unwrap().target.as_deref(), Some("orders"));
        assert_eq!(find_by_id(&index, 2).unwrap().target.as_deref(), Some("users"));
        assert!(find_by_id(&index, 3).is_none()); // out of range, no panic
        assert!(find_by_id(&index, usize::MAX).is_none()); // pathological out of range, no panic
    }

    #[test]
    fn hot_fraction_carried_through_flatten() {
        let result = sample_result(); // root est_cost 100, child_a 30, child_b 20
        let index = build_index(&result);
        let expanded = expand_all(&index);
        let rows = flatten_plan(&index, &expanded);
        // self_cost(root) = 100 - 30 - 20 = 50 -> 50/100 = 0.5
        assert_eq!(rows[0].hot_fraction, Some(0.5));
    }

    // --- Perf/scale test (adversarial-review MAJOR 2 on commit f0e96ef):
    // the path-based `PlanNodeId` this file replaced was O(n^2) on a deep
    // chain — 73s measured for `expand_all`+`flatten_plan` alone at 50,000
    // deep. `build_index`+`expand_all`+`flatten_plan` on the identical
    // shape must stay comfortably sub-second (measured: see commit
    // message). Asserts both a generous wall-clock bound (avoids CI
    // flakiness while still catching any O(n^2)/O(n log n) regression) and
    // full-coverage correctness — every node visited, depth correct at
    // both ends of the chain.
    #[test]
    fn deep_chain_build_expand_flatten_is_not_quadratic() {
        let depth = 50_000usize;
        // Iterative bottom-up construction (Global Constraints: never
        // build a deep `PlanNode` tree via a self-calling recursive
        // function) — same idiom as
        // `model_tests::deep_sqlite_chain_builds_and_drops_without_overflow`.
        let mut node = leaf(format!("N{depth}"), None);
        for i in (0..depth).rev() {
            let mut parent = leaf(format!("N{i}"), None);
            parent.children = vec![node];
            node = parent;
        }
        let result = PlanResult {
            root: node,
            is_analyze: false,
            engine: dbc_state::Engine::Postgres,
            total_planning_time_ms: None,
            total_execution_time_ms: None,
            top_level_hints: Vec::new(),
            raw_text: "{}".into(),
        };

        let start = std::time::Instant::now();
        let index = build_index(&result);
        let expanded = expand_all(&index);
        let rows = flatten_plan(&index, &expanded);
        let elapsed = start.elapsed();

        assert_eq!(index.len(), depth + 1);
        assert_eq!(expanded.len(), depth + 1);
        assert_eq!(rows.len(), depth + 1);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[depth].depth, depth);
        assert!(
            elapsed.as_secs() < 2,
            "build_index+expand_all+flatten_plan on a {depth}-deep chain took {elapsed:?} \
             — expected well under 1s (O(n)); measured ~79ms during development, and the old \
             path-based PlanNodeId took ~73s here"
        );
    }
}
