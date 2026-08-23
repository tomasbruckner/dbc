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

// --- G13 T2: Postgres `EXPLAIN (FORMAT JSON)` parser ---
//
// See `docs/superpowers/plans/2026-08-23-g13-execution-plans.md` (Task 2)
// for the binding design this section implements.

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

/// Dispatches by engine; SQLite is NOT routed through here — its parser
/// needs typed rows, not raw text (T1's `parse_sqlite_rows`, called
/// directly by T6's tab-construction code). MSSQL routes here once T3
/// lands (dead code until the driver phase).
pub fn parse_plan(engine: dbc_state::Engine, is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    match engine {
        dbc_state::Engine::Postgres => parse_pg_json(is_analyze, raw_text),
        // T3 (parallel batch, not yet landed on this branch): stubbed so
        // T2 compiles and merges independently; T3 replaces this arm with
        // `parse_mssql_xml(is_analyze, raw_text)`.
        dbc_state::Engine::Mssql => Err("MSSQL parser not yet available (T3)".to_string()),
        dbc_state::Engine::Sqlite => Err(
            "parse_plan: SQLite plans are row-shaped — call parse_sqlite_rows directly (see plan.rs's parser entry point doc)".to_string(),
        ),
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

// --- G13 T2: pg_parser_tests ---
//
// NOTE (deviation from the plan's grounding, T2, per this task's explicit
// live-fixture-capture mandate): every fixture file under
// `tests/fixtures/pg_explain_*.json` was captured live against a real
// `postgres:16.13` docker container (schema/data volumes per the plan's own
// grounding note: `users` 20,000 rows, `orders` 50,000 rows referencing
// `users`, index `idx_orders_user` on `orders(user_id)`), NOT copy-pasted
// from the plan document's inline JSON text. Real 16.13 output differs from
// the plan's hand-transcribed numbers in a few places (an extra `Parallel
// Aware`/`Async Capable`/`Local * Blocks`/`Inner Unique` key set the planner
// includes on this exact minor version, and query-planner row-estimate /
// buffer-count / timing figures that depend on actual data placement) — the
// assertions below were adjusted to match the REAL captured numbers rather
// than the plan's illustrative ones; the shapes (node types, tree
// structure, which fields are populated) are unchanged and match the plan
// exactly.
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
        // Live capture: real planner row estimate for `age = 42` against
        // the actual data distribution is 333, not the plan's illustrative
        // 250.
        assert_eq!(result.root.est_rows, Some(333.0));
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
        // Live capture: real Planning Time / Execution Time / buffer counts
        // for this exact container run — differ from the plan's
        // illustrative 0.281/0.068/3/2.
        assert_eq!(result.total_planning_time_ms, Some(0.472));
        assert_eq!(result.total_execution_time_ms, Some(0.158));
        let buffers = result.root.buffers.as_ref().expect("buffers present under ANALYZE, BUFFERS");
        assert_eq!(buffers.shared_hit, Some(5));
        assert_eq!(buffers.shared_read, Some(0));
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
    use testcontainers_modules::{
        postgres::Postgres,
        testcontainers::{runners::AsyncRunner, ImageExt},
    };

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
        let mut buf = buffer.expect("Started must have fired");
        assert_eq!(buf.row_count(), 1, "EXPLAIN (FORMAT JSON) always returns exactly one row");
        buf.cell_text(0, 0)
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

    // NOTE (deviation from the plan's grounding, T2 — real hazard found
    // while proving this test end to end): the plan's grounding sketch
    // wraps this test in `#[tokio::test]`, which runs it inside ITS OWN
    // ambient tokio runtime. `QueryRunner::new()` builds a SECOND, fully
    // independent multi-thread runtime (see `runner.rs`'s doc comment:
    // "Owns the tokio runtime. All DB I/O lives here") — dropping that
    // second runtime at the end of the test (when `runner` goes out of
    // scope) panics with "Cannot drop a runtime in a context where
    // blocking is not allowed" because the drop happens from inside the
    // FIRST (tokio::test's) async context. `runner.rs`'s own test module
    // avoids this by never constructing a full `QueryRunner` inside
    // `#[tokio::test]` (its tests drive a `Connection` directly instead).
    // Fixed here the same way `main.rs` uses `QueryRunner` for real: a
    // plain, NON-async `#[test]` owns `runner` on a normal OS thread and
    // drives all the async work through `runner.handle().block_on(...)` —
    // `runner` then drops from ordinary (non-async) sync context, which is
    // exactly the blocking-safe context `Runtime::drop` requires.
    #[test]
    #[ignore]
    fn live_parallel_explain_analyze_round_trips_through_parser() {
        let runner = QueryRunner::new();
        let handle = runner.handle();
        handle.block_on(async {
            // Task brief mandates this test run against `postgres:16.13`
            // specifically (matching the version the fixtures above were
            // captured against) — `testcontainers_modules::postgres::Postgres`'s
            // own `Default` pins `11-alpine`, so pin the tag explicitly here.
            let node = Postgres::default().with_tag("16.13").start().await.unwrap();
            let url = pg_url(&node).await;

            // NOTE (deviation from the plan's grounding, T2 — real bug
            // found while proving this test end to end): `connect_and_run`
            // opens a FRESH physical connection per call (see `runner.rs`'s
            // `connect_and_run` — there is no persistent-connection handle
            // `ConnectSpec::Url` reuses across calls). A session-scoped
            // `SET ...` on one connection is therefore invisible to the
            // next connection the `EXPLAIN` statement below runs on — the
            // plan's grounding code's `SET`-per-statement setup silently
            // never forces a parallel plan (confirmed live: without this
            // fix, the captured plan has zero `Gather` nodes despite the
            // `SET`s "succeeding"). `ALTER DATABASE ... SET` persists at
            // the catalog level and is loaded as the session default by
            // every NEW connection to this database, so it survives the
            // fresh-connection-per-call pattern.
            let setup_sql = "\
CREATE TABLE orders(id INT PRIMARY KEY, amount NUMERIC);
INSERT INTO orders SELECT g, (g % 500) FROM generate_series(1, 50000) g;
ALTER DATABASE postgres SET max_parallel_workers_per_gather = 4;
ALTER DATABASE postgres SET parallel_setup_cost = 0;
ALTER DATABASE postgres SET parallel_tuple_cost = 0;
ALTER DATABASE postgres SET min_parallel_table_scan_size = 0;"
                .to_string();
            // Multi-statement setup — connect_and_run runs one statement;
            // drive each piece separately over the SAME url (autocommit
            // DDL/DML is fine to split across connections here, unlike a
            // transaction).
            for stmt in setup_sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                run_and_capture_single_cell_ignore_shape(&runner, ConnectSpec::Url(url.clone()), stmt.to_string())
                    .await;
            }

            let explain = crate::plan::explain_analyze_sql(
                dbc_state::Engine::Postgres,
                "SELECT count(*) FROM orders WHERE amount > 100",
            )
            .unwrap();
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
        });
    }
}
