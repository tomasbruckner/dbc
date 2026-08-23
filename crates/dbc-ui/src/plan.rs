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
    // Sum(ActualRows) / max(ActualElapsedms) across every
    // `<RunTimeCountersPerThread>` seen while this frame is open — design
    // §1b v1 per-thread aggregation.
    actual_rows_sum: f64,
    actual_ms_max: f64,
    has_runtime_counters: bool,
}

fn new_relop_frame(e: &BytesStart) -> RelOpFrame {
    let operation = attr_string(e, "PhysicalOp")
        .or_else(|| attr_string(e, "LogicalOp"))
        .unwrap_or_else(|| "?".to_string());
    let est_cost = finite(attr_string(e, "EstimatedTotalSubtreeCost").and_then(|s| s.parse().ok()));
    let est_rows = finite(attr_string(e, "EstimateRows").and_then(|s| s.parse().ok()));
    RelOpFrame {
        node: leaf_node(operation, None, est_cost, est_rows),
        actual_rows_sum: 0.0,
        actual_ms_max: 0.0,
        has_runtime_counters: false,
    }
}

/// Finalizes an innermost `<RelOp>` frame's aggregated runtime counters
/// (ANALYZE only) and attaches it to its parent frame (or sets `root` when
/// the stack is now empty). Shared by both the `Event::End` (a
/// `<RelOp>...</RelOp>` pair) and `Event::Empty` (a self-closing
/// `<RelOp .../>` leaf) closure paths — the self-closing case pushes then
/// immediately calls this, exactly like a `Start` followed instantly by an
/// `End`.
fn close_relop_frame(mut frame: RelOpFrame, stack: &mut Vec<RelOpFrame>, root: &mut Option<PlanNode>, is_analyze: bool) {
    if is_analyze && frame.has_runtime_counters {
        frame.node.actual_rows = finite(Some(frame.actual_rows_sum));
        frame.node.actual_time_ms = finite(Some(frame.actual_ms_max));
        frame.node.loops = Some(1); // per-thread sums/max are already whole-node totals
    }
    match stack.last_mut() {
        Some(parent) => parent.node.children.push(frame.node),
        None => *root = Some(frame.node),
    }
}

fn apply_object(e: &BytesStart, stack: &mut [RelOpFrame]) {
    if let Some(top) = stack.last_mut() {
        if top.node.target.is_none() {
            top.node.target = attr_string(e, "Table").or_else(|| attr_string(e, "Index"));
        }
    }
}

fn apply_runtime_counters(e: &BytesStart, stack: &mut [RelOpFrame]) {
    if let Some(top) = stack.last_mut() {
        let rows = attr_string(e, "ActualRows").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let ms = attr_string(e, "ActualElapsedms").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        top.actual_rows_sum += rows;
        top.actual_ms_max = top.actual_ms_max.max(ms);
        top.has_runtime_counters = true;
    }
}

/// `(impact, database, schema, table, column names)` — accumulated across a
/// `<MissingIndexGroup>`'s descendant `<MissingIndex>`/`<Column>` elements,
/// then flattened into one `PlanHint` when the group closes.
type PendingHint = (f64, String, String, String, Vec<String>);

fn new_missing_index_group(e: &BytesStart) -> PendingHint {
    let impact = attr_string(e, "Impact").and_then(|s| s.parse().ok()).unwrap_or(0.0);
    (impact, String::new(), String::new(), String::new(), Vec::new())
}

fn apply_missing_index(e: &BytesStart, cur_hint: &mut Option<PendingHint>) {
    if let Some((_, db, schema, table, _)) = cur_hint.as_mut() {
        *db = attr_string(e, "Database").unwrap_or_default();
        *schema = attr_string(e, "Schema").unwrap_or_default();
        *table = attr_string(e, "Table").unwrap_or_default();
    }
}

fn apply_column(e: &BytesStart, cur_hint: &mut Option<PendingHint>) {
    if let Some((_, _, _, _, cols)) = cur_hint.as_mut() {
        if let Some(name) = attr_string(e, "Name") {
            cols.push(name);
        }
    }
}

fn close_missing_index_group(cur_hint: &mut Option<PendingHint>, hints: &mut Vec<PlanHint>) {
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

/// **needs-verification** (design §1b/§6/§7): every attribute name and the
/// result-set delivery mechanics below are best-effort from Microsoft's
/// published Showplan XML documentation — no live MSSQL server or driver
/// exists to capture real output against yet (dbc-ui's `connect::open_config`
/// hard-errors `Engine::Mssql` today). Correct against real captures once
/// the MSSQL driver phase lands (T7).
///
/// Walks the whole document once, iteratively (Global Constraints "Deep
/// recursive tree hazard" — an explicit `<RelOp>` frame stack over
/// `quick-xml`'s own already-iterative `Reader`/`Event` token stream, never
/// a self-calling function), tracking three independent pieces of state as
/// events arrive: (a) the `<RelOp>` frame stack (builds the tree), (b)
/// whether we're inside `<Object .../>` (sets the current frame's
/// `target`), (c) `<MissingIndexGroup>` accumulation (top-level hints,
/// unrelated to any one `RelOp`, per design §1b — attached to
/// `PlanResult.top_level_hints`, not any node).
///
/// `Event::Start` (opens, awaits a matching `Event::End`) and `Event::Empty`
/// (self-closing, no matching `Event::End` ever arrives) are handled as two
/// separate match arms rather than one combined arm — a combined arm cannot
/// tell "just opened, wait for End" apart from "opened and closed in the
/// same instant", which would leave `depth`/the `<RelOp>` frame stack
/// permanently incremented for every self-closing leaf (e.g. a leaf
/// `<RelOp .../>`, or every `<Object .../>`/`<Column .../>` in the fixtures
/// below, all of which are self-closing in real Showplan XML output).
pub fn parse_mssql_xml(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    let mut reader = Reader::from_str(raw_text);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<RelOpFrame> = Vec::new();
    let mut root: Option<PlanNode> = None;
    let mut depth: usize = 0;

    let mut hints: Vec<PlanHint> = Vec::new();
    let mut cur_hint: Option<PendingHint> = None;

    loop {
        match reader.read_event().map_err(|e| format!("chyba XML plánu: {e}"))? {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(format!("XML plán překročil maximální hloubku {MAX_XML_DEPTH}"));
                }
                match e.name().as_ref() {
                    b"RelOp" => stack.push(new_relop_frame(&e)),
                    b"Object" => apply_object(&e, &mut stack),
                    b"RunTimeCountersPerThread" if is_analyze => apply_runtime_counters(&e, &mut stack),
                    b"MissingIndexGroup" => cur_hint = Some(new_missing_index_group(&e)),
                    b"MissingIndex" => apply_missing_index(&e, &mut cur_hint),
                    b"Column" => apply_column(&e, &mut cur_hint),
                    _ => {}
                }
            }
            // Self-closing (`<Foo ... />`) — never gets a matching
            // `Event::End`, so it is opened AND closed in this one step
            // (push-then-immediately-pop-and-attach for `RelOp`,
            // finalize-immediately for `MissingIndexGroup`), and `depth`'s
            // increment here is unwound at the end of this arm rather than
            // left for an `End` event that will never come.
            Event::Empty(e) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(format!("XML plán překročil maximální hloubku {MAX_XML_DEPTH}"));
                }
                match e.name().as_ref() {
                    b"RelOp" => {
                        let frame = new_relop_frame(&e);
                        close_relop_frame(frame, &mut stack, &mut root, is_analyze);
                    }
                    b"Object" => apply_object(&e, &mut stack),
                    b"RunTimeCountersPerThread" if is_analyze => apply_runtime_counters(&e, &mut stack),
                    b"MissingIndexGroup" => {
                        let mut hint = Some(new_missing_index_group(&e));
                        close_missing_index_group(&mut hint, &mut hints);
                    }
                    b"MissingIndex" => apply_missing_index(&e, &mut cur_hint),
                    b"Column" => apply_column(&e, &mut cur_hint),
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                match e.name().as_ref() {
                    b"RelOp" => {
                        let frame = stack.pop().ok_or_else(|| "nepárový </RelOp>".to_string())?;
                        close_relop_frame(frame, &mut stack, &mut root, is_analyze);
                    }
                    b"MissingIndexGroup" => close_missing_index_group(&mut cur_hint, &mut hints),
                    _ => {}
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
