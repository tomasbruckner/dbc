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
///
/// G15 T7 (curation item 1): MSSQL's broken one-string
/// `"SET SHOWPLAN_XML ON; {sql}"` form is GONE — `SET SHOWPLAN_XML` must
/// be the ONLY statement in its batch (Microsoft's own docs) and is
/// session-scoped, while `MssqlConnection::query()` opens a fresh
/// connection per call, so that string could never have worked. MSSQL
/// never routes through this builder at all any more:
/// `main.rs::run_explain` dispatches `Engine::Mssql` to
/// `dispatch_mssql_plan`/`QueryRunner::run_mssql_plan` (session preludes
/// via `query_with_session`, see `runner.rs::mssql_plan_session`) BEFORE
/// ever calling this function.
///
/// G15 T7 review MINOR fix: the `Mssql` arm used to be a bare passthrough
/// (`sql.to_string()`) — kept the match total, but if `main.rs`'s routing
/// ever regressed (the reviewer confirmed deleting it still compiles and
/// passes the whole suite — GPUI view methods have no direct test seam,
/// see `run_explain`'s own doc comment), `dispatch_plan_query` would hand
/// the RAW user SQL straight to `connect_and_run` and EXECUTE it for
/// real — including an unconfirmed write, with no confirm modal, since
/// the estimated path is specifically the one §5 promises never executes
/// anything. Replaced with an INERT marker — a `SELECT` of a static
/// string literal, no user SQL embedded anywhere — so even if this arm
/// IS ever reached by a routing regression, the worst case is a harmless
/// read-only `SELECT` followed by a "couldn't parse this as a plan" error
/// (`parse_plan` fails closed on non-XML text), never a side effect.
pub fn explain_sql(engine: dbc_state::Engine, sql: &str) -> String {
    match engine {
        dbc_state::Engine::Postgres => format!("EXPLAIN (FORMAT JSON) {sql}"),
        dbc_state::Engine::Mssql => {
            "SELECT 'MSSQL EXPLAIN routing bug — see plan::explain_sql doc comment' AS error"
                .to_string()
        }
        dbc_state::Engine::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
        // G16: DuckDB's JSON explain form — final text (design §8).
        dbc_state::Engine::Duckdb => format!("EXPLAIN (FORMAT JSON) {sql}"),
    }
}

/// `EXPLAIN ANALYZE ...`-family text, or `None` when the engine has no
/// such mode (SQLite — §1c, the "Analyze" button is hidden entirely).
///
/// G15 T7 (curation item 1): MSSQL's broken `"SET STATISTICS XML ON;
/// {sql}; SET STATISTICS XML OFF;"` form is GONE (same "one string can't
/// carry session state across `query()`'s fresh-connection-per-call"
/// problem as `explain_sql`'s old Mssql arm) — `None` here, same as
/// SQLite, but for a DIFFERENT reason: SQLite is `None` because it has no
/// ANALYZE mode at all; MSSQL is `None` because its analyze text is
/// delivered entirely through `runner.rs::mssql_plan_session`'s prelude/
/// postlude instead of a single wrapped string. This makes the generic
/// wrap-and-run path (`main.rs::dispatch_plan_query`, used by
/// pg/sqlite) structurally unable to ever emit the broken form again
/// (fail closed) — `main.rs::run_explain`/`on_confirm_analyze_write`
/// dispatch `Engine::Mssql` to `dispatch_mssql_plan` before this function
/// is ever consulted for that engine. `analyze_button_visible(Mssql)`
/// stays `true` — the button still renders, it just routes elsewhere.
pub fn explain_analyze_sql(engine: dbc_state::Engine, sql: &str) -> Option<String> {
    match engine {
        dbc_state::Engine::Postgres => Some(format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}")),
        dbc_state::Engine::Mssql => None,
        dbc_state::Engine::Sqlite => None,
        // G16: final text (design §8); the analyze-of-a-WRITE refusal for
        // DuckDB is T5's, in `run_explain` (resolved deviation 3).
        dbc_state::Engine::Duckdb => Some(format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}")),
    }
}

/// Column index of the plan payload within an EXPLAIN result set routed
/// through the generic wrap-and-run path: DuckDB returns
/// (explain_key, explain_value) with the JSON in the SECOND column
/// (capture-pinned by capture_duckdb_explain_json_shapes); pg returns a
/// single JSON column. Sqlite/Mssql never route their payloads through
/// this helper (typed rows / query_with_session respectively) — their rows
/// exist so the mapping is total.
pub fn plan_payload_col(engine: dbc_state::Engine) -> usize {
    match engine {
        dbc_state::Engine::Duckdb => 1,
        dbc_state::Engine::Postgres | dbc_state::Engine::Mssql | dbc_state::Engine::Sqlite => 0,
    }
}

/// Whether the "Analyze" button should render at all for `engine`. Stays
/// `true` for Mssql (G15 T7) — the button itself is not what's gated; see
/// `mssql_plan_dispatch_available` for the actual ON/OFF switch clicking
/// it goes through. `Duckdb -> true` (G16, intended): DuckDB HAS an
/// analyze mode (`EXPLAIN (ANALYZE, FORMAT JSON)`); the write-analyze
/// refusal lands in T5's `run_explain`, not here.
pub fn analyze_button_visible(engine: dbc_state::Engine) -> bool {
    !matches!(engine, dbc_state::Engine::Sqlite)
}

/// G15 T8 ON-flip — same "complete, correct code; reachable the MOMENT
/// this flips" discipline `monitor::monitor_available` established for T6.
/// `main.rs::run_explain`/`on_confirm_analyze_write` check this BEFORE
/// routing to `dispatch_mssql_plan`/`run_mssql_plan`. Both the ESTIMATED
/// (`SET SHOWPLAN_XML`) and ACTUAL (`SET STATISTICS XML` + fused
/// XACT_ABORT BEGIN/ROLLBACK) paths are now live-verified end-to-end by
/// `mssql_plan_capture_live_estimated_and_actual_with_rollback_proof`
/// (runner.rs's `mssql_docker_tests`) — including the ACTUAL path's core
/// safety guarantee, that the ROLLBACK its session wrapper always issues
/// truly undoes the write (verified with a live `SELECT` after, not just
/// "no error"). `analyze_button_visible` was already `true` regardless of
/// this flag; a click while this gate was `false` surfaced an honest Czech
/// "not yet available" status instead of silently doing nothing — that
/// status path is now simply unreached for MSSQL.
pub fn mssql_plan_dispatch_available() -> bool {
    true
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
/// wrap) editor SQL — the exact same dialect-aware read classification
/// `run_query_with`'s Guard 1 uses on the pre-wrap text (batch C review
/// BLOCKER 2, main.rs); "Explain" (estimated) never calls this at all
/// (§5: always safe, unconditionally, on every engine).
///
/// G15 T7: closes the KNOWN GAP the batch C review flagged and explicitly
/// deferred here — `dialect` is now threaded through to
/// `is_read_statement_d` (was the pg-only `is_read_statement`), so a
/// bracket-quoted reserved word (e.g. `SELECT [Delete] FROM AuditLog`) on
/// a read-only MSSQL connection no longer wrongly hits `Blocked` instead
/// of `Run`. `main.rs::run_explain` calls this with `sql_dialect(engine)`,
/// same resolution as every other dialect-aware call site.
///
/// DEFENSE-IN-DEPTH, MSSQL HONESTY NOTE (design §2e/§5, G13's original
/// layering): on Postgres, `read_only` here backstops a SERVER-side
/// enforcement too (`default_transaction_read_only=on`, set for the whole
/// session at connect time — `connect.rs`'s pg arm) — a write would be
/// refused by the server even if this gate somehow let it through. MSSQL
/// has NO server-side read-only mode at all (driver integration note 5,
/// restated for this feature): this client-side gate, plus
/// `runner.rs::run_mssql_plan_inner`'s own belt-and-braces
/// `guard_not_read_only` check on the ACTUAL (`analyze == true`) path, are
/// the WHOLE defense for MSSQL. The estimated path needs no such gate on
/// EITHER engine — it's safe by a different, stronger mechanism (`EXPLAIN`
/// never executes on pg; `SET SHOWPLAN_XML ON` never executes on MSSQL —
/// see `runner.rs::mssql_plan_session`'s doc comment).
pub fn analyze_gate(sql: &str, read_only: bool, dialect: dbc_core::Dialect) -> AnalyzeGate {
    if dbc_core::is_read_statement_d(sql, dialect) {
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
fn close_relop_frame(
    mut frame: RelOpFrame,
    stack: &mut [RelOpFrame],
    root: &mut Option<PlanNode>,
    is_analyze: bool,
) {
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
        any.then_some(BufferStats {
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

/// G16 T5 (design §8 branch 1, capture-confirmed against the committed
/// tests/fixtures/duckdb_explain_*.json files — the fixtures win over the
/// plan's original expectations, and they did correct it): DuckDB's
/// `EXPLAIN (FORMAT JSON)` payload is an ARRAY of operator nodes spelled
/// `{name, children, extra_info}`; `EXPLAIN (ANALYZE, FORMAT JSON)` is a
/// single top-level WRAPPER OBJECT (`query_name`, `latency` in seconds,
/// whole-query counters, empty `extra_info`) whose `children` hold the
/// operator tree with the ANALYZE spelling: `operator_name`,
/// `operator_timing` (seconds), `operator_cardinality`. One struct covers
/// both spellings via optional fields; `extra_info` values may be strings
/// OR arrays (`Projections`), so they stay `serde_json::Value`.
#[derive(Debug, Deserialize)]
struct DuckPlanJson {
    /// Estimated-plan node spelling.
    #[serde(default)]
    name: Option<String>,
    /// ANALYZE operator-node spelling.
    #[serde(default)]
    operator_name: Option<String>,
    // NOTE: the ANALYZE wrapper also carries `query_name` (the full SQL
    // text) — deliberately NOT deserialized: far too long for a node
    // label (the wrapper renders as "Query"), and the verbatim payload in
    // `raw_text` preserves it for the raw toggle; serde skips unknown
    // fields by default.
    /// ANALYZE top-level wrapper only: whole-query wall time in SECONDS.
    #[serde(default)]
    latency: Option<f64>,
    #[serde(default)]
    extra_info: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    children: Vec<DuckPlanJson>,
    /// ANALYZE per-operator wall time in SECONDS.
    #[serde(default)]
    operator_timing: Option<f64>,
    /// ANALYZE per-operator produced-row count.
    #[serde(default)]
    operator_cardinality: Option<f64>,
}

/// `extra_info` value → display text: bare strings verbatim, everything
/// else (arrays, numbers) via JSON `to_string` — design §8's "rest, never
/// dropped" stringification for the DuckDB parser.
fn duck_value_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Iterative conversion — copies `convert_pg_tree`'s explicit frame-stack
/// shape verbatim (Global Constraints "Deep recursive tree hazard": never
/// a self-calling function; serde_json's own 128-deep recursion limit
/// bounds the parse itself, the same argument the pg parser relies on).
/// Per-node mapping (capture-grounded): `operation` = `name` else
/// `operator_name` (trimmed), else `"Query"` (the ANALYZE wrapper);
/// `target` = first of `extra_info` keys `Table`/`Relation`/`Function`
/// with a non-empty stringified value — VERBATIM, so DuckDB's qualified
/// `db.schema.table` spelling is preserved, never rewritten; `est_rows` =
/// `extra_info["Estimated Cardinality"]` (a STRING in the capture) parsed
/// as f64; `est_cost` = None (DuckDB reports no cost unit);
/// `actual_time_ms` = `operator_timing` × 1000; `actual_rows` =
/// `operator_cardinality`; `extra` = EVERY `extra_info` entry in map
/// order, including the lifted ones (design §8: "rest, never dropped").
fn convert_duckdb_tree(root: DuckPlanJson) -> PlanNode {
    struct Frame {
        operation: String,
        target: Option<String>,
        est_rows: Option<f64>,
        actual_rows: Option<f64>,
        actual_time_ms: Option<f64>,
        extra: Vec<(String, String)>,
        pending: Vec<DuckPlanJson>,
        done: Vec<PlanNode>,
    }

    fn make_frame(mut j: DuckPlanJson) -> Frame {
        let operation = j
            .name
            .take()
            .or_else(|| j.operator_name.take())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "Query".to_string());
        let target = ["Table", "Relation", "Function"].iter().find_map(|k| {
            j.extra_info
                .get(*k)
                .map(duck_value_str)
                .filter(|s| !s.trim().is_empty())
        });
        let est_rows = finite(
            j.extra_info
                .get("Estimated Cardinality")
                .and_then(|v| duck_value_str(v).trim().parse::<f64>().ok()),
        );
        let extra: Vec<(String, String)> =
            j.extra_info.iter().map(|(k, v)| (k.clone(), duck_value_str(v))).collect();
        let mut pending = std::mem::take(&mut j.children);
        pending.reverse(); // pop() takes from the end -> preserves original order
        Frame {
            operation,
            target,
            est_rows,
            actual_rows: finite(j.operator_cardinality),
            actual_time_ms: finite(j.operator_timing.map(|s| s * 1000.0)),
            extra,
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
            operation: frame.operation,
            target: frame.target,
            est_cost: None, // DuckDB reports no cost unit
            est_rows: frame.est_rows,
            actual_rows: frame.actual_rows,
            actual_time_ms: frame.actual_time_ms,
            loops: None,
            rows_removed_by_filter: None,
            buffers: None,
            extra: frame.extra,
            children: frame.done,
        };
        match stack.last_mut() {
            Some(parent) => parent.done.push(node),
            None => return node,
        }
    }
}

/// G16 T5: the real DuckDB JSON parser — estimated payloads are an ARRAY
/// of roots, ANALYZE payloads a single wrapper OBJECT (capture-pinned);
/// accept both. The wrapper's `latency` (seconds) becomes
/// `total_execution_time_ms`. Errors here never reach the user: `parse_plan`
/// degrades any `Err` to `parse_duckdb_raw`'s verbatim single root.
pub fn parse_duckdb_json(is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    let roots: Vec<DuckPlanJson> = match serde_json::from_str::<Vec<DuckPlanJson>>(raw_text) {
        Ok(v) => v,
        Err(_) => vec![
            serde_json::from_str::<DuckPlanJson>(raw_text)
                .map_err(|e| format!("neplatný JSON plánu: {e}"))?,
        ],
    };
    let root_json =
        roots.into_iter().next().ok_or_else(|| "prázdné pole v odpovědi EXPLAIN".to_string())?;
    let total_execution_time_ms = finite(root_json.latency.map(|s| s * 1000.0));
    Ok(PlanResult {
        root: convert_duckdb_tree(root_json),
        is_analyze,
        engine: dbc_state::Engine::Duckdb,
        total_planning_time_ms: None,
        total_execution_time_ms,
        top_level_hints: Vec::new(), // DuckDB emits no engine hints
        raw_text: raw_text.to_string(),
    })
}

/// Dispatches by engine; SQLite is NOT routed through here — its parser
/// needs typed rows, not raw text (T1's `parse_sqlite_rows`, called
/// directly by T6's tab-construction code).
///
/// Merge-time integration (T2 x T3): T2 originally stubbed the
/// `Engine::Mssql` arm with a "MSSQL parser not yet available (T3)" error
/// because T3 (the MSSQL Showplan XML parser, `parse_mssql_xml` above) had
/// not landed on this branch yet when T2 was authored. T3 is now present,
/// so this arm calls it directly — no remaining stub.
pub fn parse_plan(engine: dbc_state::Engine, is_analyze: bool, raw_text: &str) -> Result<PlanResult, String> {
    match engine {
        dbc_state::Engine::Postgres => parse_pg_json(is_analyze, raw_text),
        dbc_state::Engine::Mssql => parse_mssql_xml(is_analyze, raw_text),
        dbc_state::Engine::Sqlite => Err(
            "parse_plan: SQLite plans are row-shaped — call parse_sqlite_rows directly (see plan.rs's parser entry point doc)".to_string(),
        ),
        // G16 T5: the capture-gated JSON parser, with T3's raw single
        // root as the fallback — still never an Err for a wired engine.
        dbc_state::Engine::Duckdb => Ok(match parse_duckdb_json(is_analyze, raw_text) {
            Ok(parsed) => parsed,
            // Fail-closed for a plan VIEWER = never render a fabricated
            // tree: unrecognized/malformed payloads degrade to the
            // verbatim raw-text single root (T3's parse_duckdb_raw).
            Err(_) => parse_duckdb_raw(is_analyze, raw_text),
        }),
    }
}

/// G16 T3 (design §8, fallback branch pre-landed): a single-root
/// `PlanResult` whose `raw_text` carries DuckDB's EXPLAIN output verbatim —
/// the plan tab's raw-text surface is the primary view until T5's
/// capture-gated JSON parser lands, and remains the fail-open path for
/// output that parser doesn't recognize afterwards. Never an `Err`: a
/// wired engine must not have a dead "Plán" button (design §8).
pub fn parse_duckdb_raw(is_analyze: bool, raw_text: &str) -> PlanResult {
    PlanResult {
        root: PlanNode {
            operation: "DuckDB plán".to_string(),
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
        },
        is_analyze,
        engine: dbc_state::Engine::Duckdb,
        total_planning_time_ms: None,
        total_execution_time_ms: None,
        top_level_hints: Vec::new(),
        raw_text: raw_text.to_string(),
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
        // G16: DuckDB's final estimated-plan text (design §8).
        assert_eq!(
            explain_sql(dbc_state::Engine::Duckdb, "SELECT 1"),
            "EXPLAIN (FORMAT JSON) SELECT 1"
        );
        // G15 T7 review MINOR fix REQUIRED test (the "routing pin" —
        // `main.rs::run_explain`'s Mssql dispatch itself has no direct
        // test seam per GPUI convention, so this is the safety net): even
        // if that routing ever regressed and this function WAS reached
        // with a real write statement, it must NEVER echo the user's SQL
        // back as something `dispatch_plan_query` would execute — proven
        // here by feeding it an actual write and asserting the output
        // contains neither that text nor ANY of the raw input, only the
        // inert static marker.
        let raw_write = "UPDATE accounts SET balance = 0 WHERE 1=1";
        let mssql_out = explain_sql(dbc_state::Engine::Mssql, raw_write);
        assert!(
            !mssql_out.contains(raw_write),
            "explain_sql(Mssql, ..) must never echo the input SQL back verbatim: {mssql_out}"
        );
        assert!(
            mssql_out.trim_start().to_ascii_uppercase().starts_with("SELECT"),
            "even if main.rs's routing ever regressed, this must stay a harmless read-only \
             SELECT of a static marker, never user SQL: {mssql_out}"
        );
    }

    #[test]
    fn explain_analyze_sql_sqlite_and_mssql_are_both_none() {
        assert_eq!(explain_analyze_sql(dbc_state::Engine::Sqlite, "SELECT 1"), None);
        assert!(explain_analyze_sql(dbc_state::Engine::Postgres, "SELECT 1").unwrap().contains("ANALYZE, BUFFERS"));
        // G15 T7: MSSQL's analyze text is delivered entirely through
        // runner.rs::mssql_plan_session's prelude/postlude instead — None
        // here makes the generic wrap-and-run path structurally unable to
        // ever emit the broken one-string form again (fail closed).
        assert_eq!(explain_analyze_sql(dbc_state::Engine::Mssql, "SELECT 1"), None);
        // G16: DuckDB HAS an analyze mode — final text (design §8); the
        // write-analyze refusal is T5's, in run_explain.
        assert_eq!(
            explain_analyze_sql(dbc_state::Engine::Duckdb, "SELECT 1"),
            Some("EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1".to_string())
        );
    }

    #[test]
    fn analyze_button_visible_hides_only_for_sqlite() {
        assert!(analyze_button_visible(dbc_state::Engine::Postgres));
        assert!(analyze_button_visible(dbc_state::Engine::Mssql));
        assert!(!analyze_button_visible(dbc_state::Engine::Sqlite));
        // G16: visible for DuckDB — it has a real analyze mode.
        assert!(analyze_button_visible(dbc_state::Engine::Duckdb));
    }

    /// G16 T3: the pre-decided raw fallback (design §8) — single root,
    /// verbatim text, never an Err (a wired engine must not have a dead
    /// "Plán" button).
    #[test]
    fn parse_duckdb_raw_single_root_preserves_text() {
        let raw = "┌───────────────────────────┐\n│ SEQ_SCAN t │\n└───────────┘";
        for is_analyze in [false, true] {
            let result = parse_duckdb_raw(is_analyze, raw);
            assert_eq!(result.root.operation, "DuckDB plán");
            assert!(result.root.children.is_empty());
            assert_eq!(result.raw_text, raw);
            assert_eq!(result.engine, dbc_state::Engine::Duckdb);
            assert_eq!(result.is_analyze, is_analyze);
            // parse_plan's Duckdb arm routes here and never errors.
            let via_dispatch = parse_plan(dbc_state::Engine::Duckdb, is_analyze, raw).unwrap();
            assert_eq!(via_dispatch.raw_text, raw);
        }
    }

    /// G15 T8 ON-flip — see mssql_plan_dispatch_available's doc comment
    /// for the live evidence.
    #[test]
    fn mssql_plan_dispatch_is_available() {
        assert!(mssql_plan_dispatch_available());
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

    use dbc_core::Dialect;

    #[test]
    fn read_statement_always_runs_regardless_of_read_only() {
        assert_eq!(analyze_gate("SELECT 1", false, Dialect::Postgres), AnalyzeGate::Run);
        assert_eq!(analyze_gate("SELECT 1", true, Dialect::Postgres), AnalyzeGate::Run);
        assert_eq!(
            analyze_gate("WITH x AS (SELECT 1) SELECT * FROM x", true, Dialect::Postgres),
            AnalyzeGate::Run
        );
    }

    #[test]
    fn write_statement_on_read_only_is_blocked() {
        assert_eq!(analyze_gate("UPDATE t SET a = 1", true, Dialect::Postgres), AnalyzeGate::Blocked);
        assert_eq!(analyze_gate("DELETE FROM t", true, Dialect::Postgres), AnalyzeGate::Blocked);
        assert_eq!(
            analyze_gate("INSERT INTO t VALUES (1)", true, Dialect::Postgres),
            AnalyzeGate::Blocked
        );
    }

    #[test]
    fn write_statement_on_writable_needs_confirm() {
        assert_eq!(
            analyze_gate("UPDATE t SET a = 1", false, Dialect::Postgres),
            AnalyzeGate::NeedsConfirm
        );
        assert_eq!(
            analyze_gate("INSERT INTO t VALUES (1)", false, Dialect::Postgres),
            AnalyzeGate::NeedsConfirm
        );
    }

    /// REQUIRED per CURATION item 3: the same bypass edges `guards.rs`
    /// already proves for `is_read_statement`/`is_read_statement_d` must
    /// gate the same way here — this function is a thin wrapper, not a
    /// parallel implementation.
    #[test]
    fn cte_and_comment_bypass_edges_fail_closed_to_needs_confirm_or_blocked() {
        // Data-modifying CTE: lexically starts with WITH/SELECT-shaped but
        // contains an UPDATE token -> is_read_statement_d is false -> a write.
        let cte_write = "WITH x AS (UPDATE t SET a=1 RETURNING *) SELECT * FROM x";
        assert_eq!(analyze_gate(cte_write, false, Dialect::Postgres), AnalyzeGate::NeedsConfirm);
        assert_eq!(analyze_gate(cte_write, true, Dialect::Postgres), AnalyzeGate::Blocked);

        // Nested-block-comment bypass: real leading statement is the UPDATE.
        let nested_comment = "/* /* */ SELECT 1 */ UPDATE t SET a=1";
        assert_eq!(analyze_gate(nested_comment, false, Dialect::Postgres), AnalyzeGate::NeedsConfirm);

        // EXPLAIN ANALYZE UPDATE ... wrapped by the USER themselves (not by
        // this feature) still correctly classifies as a write on the
        // UNWRAPPED text this function receives.
        assert_eq!(
            analyze_gate("EXPLAIN ANALYZE UPDATE t SET a=1", false, Dialect::Postgres),
            AnalyzeGate::NeedsConfirm
        );

        // SELECT ... INTO (legacy CREATE TABLE AS spelling) is a write.
        assert_eq!(
            analyze_gate("SELECT * INTO new_tbl FROM t", true, Dialect::Postgres),
            AnalyzeGate::Blocked
        );

        // Unterminated comment/string fails closed -> not a read -> a write.
        assert_eq!(analyze_gate("SELECT 1 /* unterminated", true, Dialect::Postgres), AnalyzeGate::Blocked);
    }

    #[test]
    fn multi_statement_batch_any_write_anywhere_is_a_write() {
        assert_eq!(
            analyze_gate("SELECT 1; DROP TABLE t", false, Dialect::Postgres),
            AnalyzeGate::NeedsConfirm
        );
        assert_eq!(analyze_gate("SELECT 1; SELECT 2", true, Dialect::Postgres), AnalyzeGate::Run);
    }

    /// G15 T7 REQUIRED (closes the batch C review's KNOWN GAP): on Mssql,
    /// a bracket-quoted reserved word must classify as a read (not
    /// wrongly `Blocked`) — the exact false-reject `is_read_statement`
    /// (pg-only) used to produce, now fixed by `is_read_statement_d`. A
    /// real write on Mssql still gates exactly like every other engine.
    #[test]
    fn mssql_bracket_quoted_reserved_word_is_a_read_not_a_false_block() {
        assert_eq!(
            analyze_gate("SELECT [Delete] FROM AuditLog", true, Dialect::Mssql),
            AnalyzeGate::Run
        );
        assert_eq!(
            analyze_gate("UPDATE t SET x = 1", true, Dialect::Mssql),
            AnalyzeGate::Blocked
        );
        assert_eq!(
            analyze_gate("UPDATE t SET x = 1", false, Dialect::Mssql),
            AnalyzeGate::NeedsConfirm
        );
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
                // G15 T8 whole-branch review B2 fix: only ever sent for an
                // MSSQL WRITE (see `stream_query`'s doc comment) — this
                // helper is pg-only (`pg_url`) and only ever runs
                // EXPLAIN-shaped reads, so unreachable here; panics with a
                // distinct message rather than being silently folded into
                // the `Finished`/`Failed` cases.
                crate::runner::QueryEvent::WriteFinished { .. } => {
                    panic!("run_and_capture_single_cell: unexpected WriteFinished (pg-only, read-only helper)")
                }
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
    div, prelude::*, px, uniform_list, App, ClickEvent, Context, EventEmitter, FocusHandle,
    Focusable, Window,
};

use crate::theme::ActiveTheme;
use crate::ui;

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
        let theme = *cx.theme();
        let badge = if self.result.is_analyze { "Skutečný plán" } else { "Odhadovaný plán" };
        let header = div()
            .h(px(28.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(theme.bg_app)
            .text_color(theme.text_primary)
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
            .bg(theme.bg_panel)
            .child(header);

        if !self.result.top_level_hints.is_empty() {
            // G14 Task 4: this used to be a one-off `rgb(0x2a2a1e)` — now
            // `theme.bg_warn_banner`, the SAME field compare.rs's
            // `rgb(0x3a3a1e)` banner maps onto (Sweep Rulebook / grounding
            // correction 4). The two near-identical dark hexes collapse
            // into one field: in dark mode this banner now renders
            // 0x3a3a1e instead of 0x2a2a1e — an imperceptible tint change
            // on a notice banner, the one deliberate value change in this
            // whole sweep.
            let mut banner = div().flex().flex_col().bg(theme.bg_warn_banner).text_color(theme.warn).px_2().py_1();
            for hint in &self.result.top_level_hints {
                banner = banner.child(div().child(format!("⚠ {}", hint.message)));
            }
            root = root.child(banner);
        }

        if self.show_raw {
            root = root.child(
                div().flex_1().overflow_hidden().p_2().text_color(theme.text_primary).child(self.result.raw_text.clone()),
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
                    let theme = *cx.theme();
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
                            .text_color(theme.text_primary)
                            .hover(|s| s.bg(theme.bg_hover));
                        // Hot-node coloring applies to the ROW background,
                        // not just one column (design §2/§4).
                        match hot_fraction {
                            Some(f) if f >= 0.30 => row = row.bg(theme.danger),
                            Some(f) if f >= 0.10 => row = row.bg(theme.warn),
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
            let panel = ui::surface(theme)
                .id("plan-node-detail-panel")
                .w(px(560.))
                .max_h(px(420.))
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
                        .text_color(theme.text_primary)
                        .child(detail.text.clone()),
                )
                .child(
                    div().flex().flex_row().justify_end().gap_2().p_2().child(
                        ui::button("plan-node-detail-close", "Zavřít", theme)
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
                    .bg(theme.bg_backdrop)
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

/// G16 T5: fixture-driven parser tests over the CAPTURED payloads (the
/// committed tests/fixtures/duckdb_explain_*.json files are verbatim
/// eprintln output of `capture_duckdb_explain_json_shapes`).
#[cfg(test)]
mod duckdb_parser_tests {
    use super::*;

    /// Iterative pre-order walk (no recursion, house rule) returning the
    /// first node matching `pred`.
    fn find_node<'a>(root: &'a PlanNode, pred: &dyn Fn(&PlanNode) -> bool) -> Option<&'a PlanNode> {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if pred(n) {
                return Some(n);
            }
            stack.extend(n.children.iter());
        }
        None
    }

    #[test]
    fn duckdb_seq_scan_fixture_parses() {
        let raw = include_str!("../tests/fixtures/duckdb_explain_seq_scan.json");
        let result = parse_duckdb_json(false, raw).unwrap();
        assert_eq!(result.engine, dbc_state::Engine::Duckdb);
        assert!(!result.is_analyze);
        assert!(!result.root.children.is_empty(), "tree depth must be >= 1");
        // Capture ground truth: DuckDB reports the QUALIFIED table name
        // (`cap.main.t`) — preserved verbatim, never rewritten.
        let scan = find_node(&result.root, &|n| n.operation == "SEQ_SCAN")
            .expect("seq-scan node present");
        assert_eq!(scan.target.as_deref(), Some("cap.main.t"));
        assert!(!scan.extra.is_empty(), "extra_info entries must survive");
        assert_eq!(scan.est_rows, Some(2.0)); // "Estimated Cardinality": "2"
        assert_eq!(result.raw_text, raw); // verbatim, for the raw toggle
    }

    #[test]
    fn duckdb_join_fixture_has_two_children() {
        let raw = include_str!("../tests/fixtures/duckdb_explain_join.json");
        let result = parse_duckdb_json(false, raw).unwrap();
        let join = find_node(&result.root, &|n| n.operation == "HASH_JOIN")
            .expect("join node present");
        assert_eq!(join.children.len(), 2);
        // Both scan children keep their (qualified) targets in order.
        assert_eq!(join.children[0].target.as_deref(), Some("cap.main.t"));
        assert_eq!(join.children[1].target.as_deref(), Some("cap.main.u"));
    }

    #[test]
    fn duckdb_analyze_fixture_carries_timings() {
        let raw = include_str!("../tests/fixtures/duckdb_explain_analyze.json");
        let result = parse_duckdb_json(true, raw).unwrap();
        assert!(result.is_analyze);
        // The ANALYZE wrapper object (query_name/latency, no operator
        // name) renders as a "Query" root with the whole-query wall time.
        assert_eq!(result.root.operation, "Query");
        assert!(result.total_execution_time_ms.is_some(), "wrapper latency must be lifted");
        let timed = find_node(&result.root, &|n| {
            n.actual_time_ms.is_some() && n.actual_rows.is_some()
        })
        .expect("some operator carries ANALYZE timings");
        assert!(timed.actual_time_ms.unwrap() >= 0.0);
    }

    #[test]
    fn duckdb_malformed_json_degrades_to_raw_root() {
        let result = parse_plan(dbc_state::Engine::Duckdb, false, "!! not json").unwrap();
        assert_eq!(result.root.operation, "DuckDB plán");
        assert_eq!(result.raw_text, "!! not json");
        assert!(result.root.children.is_empty());
    }

    #[test]
    fn plan_payload_col_table() {
        assert_eq!(plan_payload_col(dbc_state::Engine::Postgres), 0);
        assert_eq!(plan_payload_col(dbc_state::Engine::Mssql), 0);
        assert_eq!(plan_payload_col(dbc_state::Engine::Sqlite), 0);
        assert_eq!(plan_payload_col(dbc_state::Engine::Duckdb), 1);
    }

    /// End-to-end estimated plan over a REAL temp-file database: driver →
    /// ResultBuffer → payload column → parse_plan, asserting the REAL
    /// parser handled live output (root is not the raw fallback's label).
    #[tokio::test]
    async fn duckdb_estimated_plan_end_to_end_uses_real_parser() {
        use dbc_core::{CancelToken, Connection};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("e2e.duckdb");
        let mut conn = dbc_driver_duckdb::DuckdbConnection::new(&db);
        conn.execute("CREATE TABLE t(id INTEGER)", CancelToken::new()).await.unwrap();
        let mut stream = conn
            .query("EXPLAIN (FORMAT JSON) SELECT * FROM t", CancelToken::new())
            .await
            .unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
        while let Some(item) = stream.batches.recv().await {
            buf.push(item.unwrap()).unwrap();
        }
        let payload = buf.cell_text(0, plan_payload_col(dbc_state::Engine::Duckdb));
        let result = parse_plan(dbc_state::Engine::Duckdb, false, &payload).unwrap();
        assert_ne!(
            result.root.operation, "DuckDB plán",
            "live output must route through parse_duckdb_json, not the raw fallback"
        );
    }
}

/// G16 T5 step 1 — the fixture-capture gate (G13 curation item 5's
/// discipline, embedded so it costs milliseconds): runs both EXPLAIN forms
/// through the REAL driver and pins the result shape the extraction code
/// depends on (two text columns, JSON payload in the LAST one, parseable
/// by serde_json). The eprintln'd payloads are the source of the committed
/// tests/fixtures/duckdb_explain_*.json files — re-capture whenever the
/// vendored duckdb crate is bumped.
#[cfg(test)]
mod duckdb_capture_tests {
    use super::*;
    use dbc_core::{CancelToken, Connection};

    #[tokio::test]
    async fn capture_duckdb_explain_json_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cap.duckdb");
        let mut conn = dbc_driver_duckdb::DuckdbConnection::new(&db);
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)", CancelToken::new())
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO t SELECT range, 'r' || range FROM range(1000)",
            CancelToken::new(),
        )
        .await
        .unwrap();
        conn.execute("CREATE TABLE u(id INTEGER, t_id INTEGER)", CancelToken::new())
            .await
            .unwrap();

        for (label, sql) in [
            ("seq_scan", "EXPLAIN (FORMAT JSON) SELECT * FROM t WHERE name = 'r5'"),
            ("join", "EXPLAIN (FORMAT JSON) SELECT * FROM t JOIN u ON u.t_id = t.id"),
            ("analyze", "EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM t"),
        ] {
            let mut stream = conn.query(sql, CancelToken::new()).await.unwrap();
            let names: Vec<String> =
                stream.columns.fields().iter().map(|f| f.name().to_string()).collect();
            let mut buf = dbc_buffer::ResultBuffer::new(stream.columns.clone());
            while let Some(item) = stream.batches.recv().await {
                buf.push(item.unwrap()).unwrap();
            }
            assert!(buf.row_count() >= 1, "{label}: EXPLAIN returned no rows");
            let payload_col = buf.column_count() - 1;
            assert_eq!(
                payload_col,
                plan_payload_col(dbc_state::Engine::Duckdb),
                "{label}: payload column moved — update plan_payload_col AND the fixtures (cols: {names:?})"
            );
            let payload = buf.cell_text(0, payload_col);
            let parsed: serde_json::Value = serde_json::from_str(&payload)
                .unwrap_or_else(|e| panic!("{label}: payload is not JSON ({e}): {payload}"));
            assert!(parsed.is_array() || parsed.is_object(), "{label}: unexpected JSON shape");
            eprintln!("=== duckdb_explain_{label}.json ===\n{payload}\n");
        }
    }
}
