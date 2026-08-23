# G13 — Execution Plans: Design Pass

Date: 2026-08-23
Status: designed autonomously under the standing mandate (per §4 of
`2026-08-22-gui-target-design.md`); decisions recorded here for later user
review, in the terse decision-per-bullet style of the G5 design pass block.

Scope (spec row G13): "Explain" action next to Run — estimated plan and REAL
plan (EXPLAIN ANALYZE / MSSQL actual execution plan) rendered as a tree
visualization; per node operation, cost, estimated vs actual rows, timing,
buffers; hot nodes highlighted; engine-provided hints (e.g. missing index)
surfaced; raw plan text available.

Sources read: `2026-08-22-gui-target-design.md` §1–§4 (layout mockup, G5
design-pass style model); `crates/dbc-core/src/connection.rs` (`Connection`
trait — `query`/`execute`/`schema`, the transaction/session-sharing caveats
on `execute`); `crates/dbc-core/src/guards.rs` (`is_read_statement`,
`WRITE_KEYWORDS`, the existing `EXPLAIN ANALYZE UPDATE` bypass-closure and
its test `explain_analyze_write_fails_closed`); `crates/dbc-core/src/schema.rs`,
`error.rs`, `stream.rs`, `cancel.rs`, `lib.rs` (module/dependency style —
zero serde in `dbc-core` today); `crates/dbc-ui/src/schema_tree.rs` (pure
`flatten`/`NodeId`/`FlatNode` + GPUI entity colocated in one file, rendered
via `uniform_list` — the pattern this reuses for the plan tree);
`crates/dbc-ui/src/sandbox.rs` (pure, GPUI-free, dbc-ui-resident model +
SQL-string generation, fixture/unit-tested — the pattern this reuses for
plan parsing); `crates/dbc-ui/src/tabs.rs` (`ResultTab`/`TabContent`,
extended here with a `Plan` variant); `crates/dbc-ui/src/runner.rs`
(`QueryRunner::connect_and_run`, `QueryEvent`, off-UI-thread dispatch);
`crates/dbc-ui/src/main.rs` (`run_query_with`'s read-only Guard 1, `status`
bar, `ModalState`/`modal` overlay pattern); `crates/dbc-state/src/config.rs`
(`Engine::{Postgres,Mssql,Sqlite}`, `ConnectionConfig.read_only`);
`crates/dbc-core/Cargo.toml`, `crates/dbc-ui/Cargo.toml` (current
dependency sets — no `serde`/`serde_json`/xml crate anywhere in `dbc-core`
or `dbc-ui` today; `serde`/`serde_json` already workspace deps used by
`dbc-state`). No MSSQL driver crate exists yet (orthogonal, unscheduled) —
every MSSQL-specific decision below is flagged needs-verification in §6.

> **CURATION (2026-08-23, binding):**
> 1. **Stale driver claim superseded:** `dbc-driver-mssql` (odbc-api 29) and
>    `dbc-driver-duckdb` exist as of v0.5.0, both unwired. §1b's open
>    question is partly answered: odbc-api DOES expose multiple result sets
>    (`Cursor::more_results()`), so "skip statement result set, read the
>    trailing plan XML" is feasible driver-side without a trait change —
>    still needs-verification against a live server, T7 stays deferred.
>    DuckDB plans (`EXPLAIN` / `EXPLAIN ANALYZE`, own format) are NOT in
>    G13 v1 scope — follow-up at DuckDB wiring time.
> 2. **§3-novela alignment (see G12 curation item 1):** the Analyze-write
>    sequence (dedicated connection, BEGIN → EXPLAIN ANALYZE → ROLLBACK)
>    is a new sanctioned runner write method. It MUST (a) live in
>    `runner.rs` as its own method, (b) call the SHARED read-only guard
>    helper (no fresh logic), (c) be dispatched only from the
>    `AnalyzeWriteConfirm` modal. Add it to `execute()`'s
>    sanctioned-caller doc list.
> 3. **REQUIRED test (read-only discipline):** `analyze_gate` with a write
>    statement + `read_only == true` returns `Blocked` before any driver
>    call — plus the three-case gate table and the CTE/comment bypass edges
>    from `guards.rs`'s test suite.
> 4. **Palette naming fix:** the app palette is Catppuccin Mocha (G6
>    curation), not "Tokyo-Night-ish" — the hex values `0xf38ba8`/`0xf9e2af`
>    in §2 are correct Mocha red/yellow; keep them.
> 5. **T2 fixture capture against docker-pg is REQUIRED before T2 closes**
>    (including one parallel-worker plan with `Workers Launched > 0` per §7)
>    — the serde derive alone is not a correctness gate.

## 0. Architecture decision, up front: plans ride the existing `query()` path

The brief's read-first note says it explicitly and the trait bears it out:
`Connection::query(sql, cancel) -> Result<QueryStream, QueryError>` is
already generic over "any SQL that returns rows." An `EXPLAIN` variant is,
mechanically, just another SQL string that returns rows (one JSON/XML row
for pg/MSSQL, N simple rows for SQLite's `EXPLAIN QUERY PLAN`).
**Decision: no `Connection` trait change, no driver changes.** G13 builds
the engine-specific `EXPLAIN ...` SQL string in `dbc-ui`, runs it through
`QueryRunner::connect_and_run` exactly like a normal query, drains the
resulting `RecordBatch`es into a `dbc_buffer::ResultBuffer` exactly like a
normal query, then hands the single cell (pg/MSSQL: one row, one text/JSON
column; SQLite: the whole small row set) to a pure parser. This is the
smallest change that satisfies "plans arrive via the normal `query()` path"
and keeps `dbc-core`/drivers completely untouched by this phase — the ONLY
new binding-constraint-relevant piece is the write-gating flow in §5, which
reuses `Connection::execute` (already on the trait, already the app's only
write path per §3 of the target-UI spec — see decision in §5 for why
`EXPLAIN ANALYZE` on a write is treated as a second instance of that path,
not an exception to it).

This also resolves the brief's "decide location — dbc-core or dbc-ui pure
module" question for the parsers: **decision — `dbc-ui`, new pure module
`crates/dbc-ui/src/plan.rs`**, not `dbc-core`. Rationale: (a) since
acquisition is just `query()` + a `ResultBuffer`, both already dbc-ui
concepts, converting that buffer's text into a tree is UI-orchestration
logic, not a core domain concept like `SchemaSnapshot`; (b) `dbc-core` has
zero `serde` dependency today (checked `Cargo.toml`) — adding
`serde_json`/`quick-xml` there for a single phase's feature is the wrong
crate boundary, whereas `dbc-ui` already depends on `dbc-state` which pulls
`serde`/`serde_json` transitively and can trivially take them directly;
(c) direct precedent: `sandbox.rs` is pure, GPUI-free, unit-tested, and
lives in `dbc-ui` for exactly this reason (it turns UI-observed state into
strings for a UI dialog); plan parsing turns UI-observed query-result text
into a UI tree-rendering model — same shape of concern. `plan.rs` follows
`schema_tree.rs`'s colocation convention: pure model + parsers + unit tests
in the first half of the file, the GPUI tree-rendering entity (`PlanView`)
in the second half, one file, clearly section-commented.

## 1. Plan acquisition, per engine

### 1a. PostgreSQL

- **Estimated:** `EXPLAIN (FORMAT JSON) {sql}`. **Actual:**
  `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}`. Returns exactly one row,
  one column (pg type `json`), whose text is a JSON **array** with exactly
  one element: `[{ "Plan": {...}, "Planning Time": 1.23, "Execution Time":
  4.56 }]` — square brackets included, not a bare object. Parser must strip
  the outer array (`serde_json::from_str::<Vec<PgExplainRoot>>` and take
  `[0]`, erroring if the array is empty — fail-closed, never panic).
- **Version note (flagged, non-blocking):** `"Planning Time"`/
  `"Execution Time"` are top-level siblings of `"Plan"` and are present
  **only** when `ANALYZE` was requested — plain `EXPLAIN (FORMAT JSON)`
  omits both. Parser models both as `Option<f64>`, never assumes presence.
  `"Triggers": [...]` (per-trigger firing stats) appears only when at least
  one trigger fired during an `ANALYZE` run of a write statement — modeled
  as raw text folded into `PlanResult.raw_text`, not parsed into the node
  tree (out of scope: trigger stats aren't part of the node tree the brief
  asks for).
- **`"Plan"` node fields consumed** (the rest of pg's ~40 possible keys —
  `"Parallel Aware"`, `"Async Capable"`, `"Sort Key"`, `"Hash Cond"`,
  `"Index Cond"`, `"Output"` under VERBOSE, etc. — fold into `extra` per
  §2, never dropped): `"Node Type"` → `operation`; `"Relation Name"` /
  `"Index Name"` / `"Alias"` → `target` (relation name preferred, else
  index name, else alias — first present wins); `"Startup Cost"` /
  `"Total Cost"` → `est_cost` uses `"Total Cost"`; `"Plan Rows"` →
  `est_rows`; `"Plan Width"` folds into `extra`; `"Actual Startup Time"` /
  `"Actual Total Time"` → `actual_time_ms` uses `"Actual Total Time"`
  (pg reports this **inclusive of children** and **per-loop-averaged** —
  see the hot-node formula in §2, which must multiply by `"Actual Loops"`
  before subtracting children); `"Actual Rows"` → `actual_rows`;
  `"Actual Loops"` → `loops`; `"Rows Removed by Filter"` /
  `"Rows Removed by Join Filter"` → summed into `rows_removed_by_filter`;
  `"Filter"` / `"Index Cond"` / `"Join Type"` / `"Hash Cond"` fold into
  `extra` (shown in the node's raw-detail popover, §4); buffer keys
  (`"Shared Hit Blocks"`, `"Shared Read Blocks"`, `"Shared Dirtied Blocks"`,
  `"Shared Written Blocks"`, `"Temp Read Blocks"`, `"Temp Written Blocks"`,
  `"Local *"` folded into `extra` since local buffers are a rare temp-table
  case) → `buffers: Some(BufferStats { .. })` only under `ANALYZE, BUFFERS`
  — `None` for a plain estimated plan; `"Plans"` (array) → `children`,
  recursively.
- Postgres has **no built-in structured "missing index" hint** the way
  MSSQL does — `top_level_hints` is always empty for pg in v1. (A future
  phase could heuristically flag `Seq Scan` nodes with a large
  `rows_removed_by_filter` as "candidate for an index," but that's an
  invented heuristic, not an engine-provided hint, and is explicitly out of
  scope per the brief's "engine-provided hints" wording.)

### 1b. MSSQL (driver not yet implemented — needs-verification throughout)

- **Estimated:** `SET SHOWPLAN_XML ON; {sql}` — per Microsoft's documented
  behaviour, while this session option is ON the server does **not execute**
  any subsequent batch; it returns the estimated plan as XML instead. Must
  be turned back `OFF` before any further normal query on the same
  connection/session (`SET SHOWPLAN_XML OFF;` issued immediately after, or
  by dropping the connection — v1 uses a dedicated one-shot connection for
  every Explain/Analyze request, same pattern G5's Apply flow uses for its
  dedicated transaction connection, so the ON/OFF pairing is scoped to a
  connection this feature owns end-to-end and never leaks into the
  session the SQL editor's own `query()` calls run over).
- **Actual:** `SET STATISTICS XML ON; {sql}; SET STATISTICS XML OFF;` — the
  statement **actually executes**; in addition to whatever result set(s)
  the statement itself produces, the server appends one extra result set
  containing a single row/column of XML (the actual plan, with runtime
  counters). This is §5's sharp edge for MSSQL, same shape as pg's
  `ANALYZE`.
- **Result-set column name (needs-verification):** historically fixed as
  `"Microsoft SQL Server 2005 XML Showplan"` regardless of server version —
  carried forward for backward compatibility. The driver-side query() must
  select the result set whose single column has this name, not assume it's
  the first or only result set (`STATISTICS XML` mode: the statement's own
  result set, if any, comes first; the plan XML is an extra trailing result
  set). **Needs-verification:** whether `odbc-api` (the crate named for the
  future MSSQL driver in the target-UI spec) exposes multiple result sets
  per batch via `SQLMoreResults` cleanly, and whether this repo's
  `Connection::query()` shape (one `QueryStream` per call) can represent
  "skip result set 1, return result set 2's one row" without a trait
  change — flagged as an open question for whoever builds the MSSQL driver
  phase, not resolved here since no driver crate exists to prototype
  against yet.
- **XML shape consumed** (attribute names per Microsoft's public Showplan
  XML schema, **needs-verification** — no fixture available without a live
  MSSQL instance): `<RelOp NodeId="" PhysicalOp="" LogicalOp=""
  EstimateRows="" EstimatedTotalSubtreeCost="" EstimateIO="" EstimateCPU=""
  AvgRowSize="">` → `operation` = `PhysicalOp` (falls back to `LogicalOp` if
  absent), `est_cost` = `EstimatedTotalSubtreeCost`, `est_rows` =
  `EstimateRows`; nested `<RelOp>` children under
  `<RelOp>/<.../><RelOp>...` (the exact wrapper element — e.g.
  `<NestedLoops><InnerSide><RelOp>...` — varies per operator, so the parser
  walks **any** descendant `<RelOp>` element, not a fixed child path,
  stopping recursion at the next `<RelOp>` boundary so it doesn't collapse
  the tree); target table from `<Object Table="" Index="" Schema="">`
  (child of `<IndexScan>`/`<TableScan>` etc., itself a child of `<RelOp>`).
  `STATISTICS XML` additionally nests `<RunTimeInformation>
  <RunTimeCountersPerThread ActualRows="" ActualElapsedms=""
  ActualExecutions="" />`, averaged/summed across threads for
  `actual_rows`/`actual_time_ms` (needs-verification: exact aggregation
  MSSQL intends across parallel threads — v1 sums `ActualRows` and takes
  `max(ActualElapsedms)` across `<RunTimeCountersPerThread>` entries as the
  conservative "how long did this node take from start to finish"
  approximation; flagged for correction once a real fixture exists).
- **Missing-index hints:** a top-level `<MissingIndexes>` element (sibling
  of the root `<QueryPlan>`, not per-node) containing one or more
  `<MissingIndexGroup Impact="">` → `<MissingIndex Database="" Schema=""
  Table="">` → `<ColumnGroup Usage="EQUALITY|INEQUALITY|INCLUDE">` →
  `<Column Name="" ColumnId="">`. Parser flattens each `MissingIndexGroup`
  into one `PlanHint { message: "Chybějící index: dopad {Impact}%",
  detail: Some(<synthesized CREATE INDEX ... suggestion text>) }` in
  `PlanResult.top_level_hints` (§4 surfaces these above the tree, not
  attached to any one node — the XML itself doesn't attach them to a
  specific `RelOp`).
- Missing driver = this entire subsection ships as **dead code behind the
  parser boundary** in v1 (see §5 task list): the parser module and its
  unit tests (against hand-authored XML fixtures built from documentation
  samples, since no live server is available) land now; the UI wiring that
  actually sends `SET SHOWPLAN_XML ON`/`STATISTICS XML ON` over a real
  MSSQL connection is a no-op until the MSSQL driver phase exists, at which
  point this parser is exercised against real captures and corrected.

### 1c. SQLite

- **Only mode: `EXPLAIN QUERY PLAN {sql}`.** Never executes the statement
  (SQLite guarantees this unconditionally, unlike pg/MSSQL's ANALYZE
  modes) — so SQLite has **no actual/ANALYZE variant at all**. Decision
  carried into §4: the "Analyze" button is hidden entirely for SQLite
  connections, not merely disabled — there is nothing for it to do.
- Returns simple rows, four columns per SQLite's docs: `id INTEGER, parent
  INTEGER, notused INTEGER, detail TEXT` (`notused` is reserved, always 0,
  ignored by the parser). No costs, no row estimates, no timings —
  `est_cost`/`est_rows`/`actual_rows`/`actual_time_ms`/`buffers` are all
  `None` for every SQLite node; `operation`/`target` are derived from
  `detail`'s free text (e.g. `"SCAN TABLE t"`, `"SEARCH TABLE t USING INDEX
  idx (col=?)"` — `operation` = the leading verb (`SCAN`/`SEARCH`/others),
  `target` = best-effort table-name extraction via a small fixed regex
  (`r"(?:TABLE|INDEX) (\w+)"` first match), falling back to the full
  `detail` text as `operation` with `target: None` when the pattern
  doesn't match — fail-open on display text, never panic).
- Tree built from the explicit `id`/`parent` columns (not row order) —
  `parent == 0` (or whatever the root sentinel is; SQLite uses 0 for "no
  parent") are roots; SQLite can legally return **multiple root rows**
  for a compound statement (e.g. a `UNION`) — `PlanResult.root` requires
  exactly one root node, so the parser synthesizes a synthetic
  `operation: "QUERY PLAN"` wrapper root when SQLite returns more than one
  top-level row, with the real rows as its children (documented behaviour,
  unit-tested).
- This is by far the cheapest of the three — pure row parsing over an
  already-materialized `ResultBuffer`, no JSON/XML dependency, no new
  crate. Included per the brief's "cheap win."

## 2. Unified plan model (`dbc-ui/src/plan.rs`, pure, `derive(Debug, Clone,
PartialEq)`, `serde`-free itself — only the pg *parser* uses `serde_json`,
the model types are plain structs)

```rust
pub struct PlanResult {
    pub root: PlanNode,
    pub is_analyze: bool,               // false = estimated, true = actual
    pub engine: Engine,                 // dbc_state::Engine
    pub total_planning_time_ms: Option<f64>,   // pg only
    pub total_execution_time_ms: Option<f64>,  // pg ANALYZE / MSSQL STATISTICS
    pub top_level_hints: Vec<PlanHint>, // MSSQL missing-index hints; empty elsewhere
    pub raw_text: String,               // original JSON/XML/rows text, verbatim
}

pub struct PlanNode {
    pub operation: String,              // "Seq Scan" / "Index Scan" / "SCAN TABLE" / MSSQL PhysicalOp
    pub target: Option<String>,         // table/index name touched, if any
    pub est_cost: Option<f64>,
    pub est_rows: Option<f64>,
    pub actual_rows: Option<f64>,
    pub actual_time_ms: Option<f64>,    // pg: "Actual Total Time" * "Actual Loops" (see §2 formula note)
    pub loops: Option<u64>,
    pub rows_removed_by_filter: Option<f64>,
    pub buffers: Option<BufferStats>,
    pub extra: Vec<(String, String)>,   // engine-specific key/value, string-rendered, shown in detail popover
    pub children: Vec<PlanNode>,
}

pub struct BufferStats {
    pub shared_hit: Option<u64>,
    pub shared_read: Option<u64>,
    pub shared_dirtied: Option<u64>,
    pub shared_written: Option<u64>,
    pub temp_read: Option<u64>,
    pub temp_written: Option<u64>,
}

pub struct PlanHint {
    pub message: String,
    pub detail: Option<String>,
}
```

- **Option-heavy by design** per the brief — every metric field is `None`
  where the engine/mode doesn't provide it (SQLite: almost everything;
  pg estimated: actual_*/buffers None; pg ANALYZE without BUFFERS would
  leave buffers None too, but v1 always requests BUFFERS alongside
  ANALYZE per §1a so this combination doesn't arise in practice).
- **Hot-node metric — exact formula, decided (no TBD):**
  - **Actual plans (`is_analyze: true`):** `self_time_ms(node) =
    (node.actual_time_ms.unwrap_or(0.0) * node.loops.unwrap_or(1) as f64)
    - sum(child.actual_time_ms.unwrap_or(0.0) * child.loops.unwrap_or(1) as
    f64 for child in node.children)`, clamped to `>= 0.0` (floating-point
    noise can make it slightly negative; clamp rather than display a
    negative hot fraction). `hot_fraction(node) = self_time_ms(node) /
    total_execution_time_ms` (falls back to `root.actual_time_ms *
    root.loops` as the denominator if `total_execution_time_ms` is `None`
    — MSSQL's `STATISTICS XML` has no top-level total, only per-node
    counters). Denominator `0.0` → `hot_fraction = 0.0` for every node
    (avoid NaN/div-by-zero).
  - **Estimated plans (`is_analyze: false`):** same shape, substituting
    `est_cost` for `actual_time_ms * loops` (pg's `"Total Cost"` is
    likewise cumulative-over-subtree, so `self_cost(node) = est_cost(node)
    - sum(children est_cost)`) and the root's `est_cost` as denominator.
    SQLite estimated plans have no `est_cost` anywhere → `hot_fraction` is
    `None` for every node (§4: no highlighting rendered at all for
    SQLite, not "everything red").
  - **Thresholds (§4 rendering):** `hot_fraction >= 0.30` → red;
    `>= 0.10` → amber; else default text colour. Chosen to match the
    Tokyo-Night-ish palette already in use elsewhere in the app
    (`0xf38ba8` red / `0xf9e2af` amber, both already used —
    `schema_tree.rs`'s error text and favourite-star colours respectively)
    rather than inventing new colours.

## 3. Parsers (`dbc-ui/src/plan.rs`, unit-tested against fixtures)

- **Postgres:** `serde_json`, already a workspace dependency (used by
  `dbc-state`) — add `serde_json.workspace = true` to `dbc-ui/Cargo.toml`.
  Deserialize into an intermediate `#[derive(Deserialize)] struct
  PgPlanJson { #[serde(rename = "Node Type")] node_type: String, ... }`
  with `#[serde(flatten)]`-free explicit fields for the ones §1a lists
  (explicit, not `#[serde(flatten)]` into a generic map, so a missing field
  is a compile-time-checked `Option` not a runtime map lookup) plus
  `#[serde(flatten)] extra: serde_json::Map<String, serde_json::Value>` to
  capture everything else automatically for `PlanNode.extra` (stringified
  via `.to_string()` per value) — this is what keeps `extra` complete
  without hand-listing pg's ~40 keys. Then a `From<PgPlanJson> for
  PlanNode` conversion, recursive over `"Plans"`. **Fixture-tested**: since
  a docker pg is available per the brief, capture real
  `EXPLAIN (FORMAT JSON) SELECT ...` and
  `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT ...` output for a handful
  of shapes (seq scan, index scan, nested loop join, a query with a filter
  that removes rows, a query touching two tables for a hash join) into
  `crates/dbc-ui/tests/fixtures/pg_explain_*.json`, `include_str!`'d by unit
  tests. This is the parser's real correctness gate — the derive alone
  proves nothing about pg's actual output shape.
- **MSSQL:** **new dependency `quick-xml`** (justify: needed to parse
  attribute-heavy nested XML — `RelOp`/`MissingIndexes`/
  `RunTimeCountersPerThread` — which a regex or hand-rolled scanner cannot
  do safely once nesting depth varies per operator; `quick-xml` is the
  de-facto low-allocation streaming XML reader for Rust, MIT/Apache-2.0
  dual-licensed matching this repo's other dependencies' typical licensing,
  actively maintained, and does not require a DOM crate on top — the
  `Reader`/`Event` streaming API is enough to walk `RelOp`'s recursive
  structure with an explicit stack, matching the recursive `PlanNode` we're
  building anyway). Add `quick-xml = "0.31"` (pin exact version at
  implementation time to whatever's current) to `dbc-ui/Cargo.toml`.
  **Fixture-tested against hand-authored XML** built from Microsoft's
  published Showplan XML documentation/samples (no live MSSQL instance
  available in this repo) — `crates/dbc-ui/tests/fixtures/mssql_showplan_*.xml`.
  Every fixture and the parser code that consumes MSSQL-specific attribute
  names is flagged needs-verification (§6) until corrected against a real
  capture.
- **SQLite:** trivial — no new dependency, operates directly on the
  already-materialized `ResultBuffer`'s 4 columns (§1c). Unit-tested with
  hand-built row vectors (single scan, indexed search, a multi-root UNION
  case, a row whose `detail` doesn't match the table-name regex).
- **Parser entry point:** `pub fn parse_plan(engine: Engine, is_analyze:
  bool, raw_text: &str) -> Result<PlanResult, String>` (string error, not
  `QueryError` — this is a local parse failure, not a `Connection` error;
  `main.rs` wraps it into the tab's error display). Dispatches to
  `parse_pg_json` / `parse_mssql_xml` / `parse_sqlite_rows` by `engine`.
  SQLite's parser actually needs the row **vectors**, not raw text, so its
  real signature is `parse_sqlite_rows(rows: &[(i64, i64, String)]) ->
  PlanNode` called directly by the tab-construction code in `main.rs`
  rather than through the `raw_text` dispatcher — `raw_text` for SQLite is
  still captured (rows re-joined as `"id\tparent\tdetail"` lines) purely
  for the raw-text toggle (§4), not re-parsed from it.

## 4. UI

- **Tab content, not a modal:** a new `TabContent::Plan { result: PlanResult
  }` variant on `tabs::TabContent` (alongside `Grid`/`Text`), opened as a
  normal result tab titled `"Plan: {collapsed sql}"` (`tabs::collapse_title`
  reused) — reuses `Tabs::open`/`TAB_CAP` eviction/pin/close exactly like
  every other tab, no new tab-management code.
- **Trigger buttons:** there is no dedicated "Run" *button* in the current
  UI (Run is Ctrl+Enter only, per `main.rs` — confirmed, no widget to sit
  "next to"). Decision: two small clickable text buttons, **"Explain"** and
  **"Analyze"**, added to the bottom status bar (where `self.status` renders
  today), left of the status text — same minimal `div().id().cursor_pointer()`
  button style already used for the schema tree's "DDL"/"⟳" affordances
  (`schema_tree.rs`). Both run against the SQL editor's current text (same
  source `run_query` reads), enabled whenever that text is non-empty and no
  query/explain is currently in flight (`self.cancel.is_none()`), disabled
  (dimmed, same `rgb(0x45475a)` pattern as the tree's disabled "DDL") while
  one is. "Analyze" is **hidden entirely** (not just disabled) when the
  active connection's engine is SQLite, per §1c.
- **Plan tab layout:** header row (engine + estimated/actual badge + total
  planning/execution time when known + a "Raw" toggle button, right-aligned)
  above a `uniform_list` tree — reuses `schema_tree.rs`'s exact pattern:
  a pure `fn flatten_plan(root: &PlanNode, expanded: &HashSet<PlanNodeId>) ->
  Vec<PlanFlatNode>` (id = path-based `Vec<usize>` child-index chain, stable
  across expand/collapse, mirroring `NodeId`'s path-based-not-index-based
  rationale) feeding a `uniform_list("plan-tree-rows", ...)`. **Decision
  per the brief's explicit framing: indented tree rows with metric columns,
  not graphical node boxes** — reuses the schema tree's row-rendering
  machinery wholesale (chevron/indent/click-to-expand) rather than a GPUI
  canvas; a graphical box-and-arrow view is explicitly deferred (own
  brainstorm if ever picked up, same as G8's ER diagram canvas work — no
  reason to build a second canvas renderer for this phase when a tree reads
  every field the brief asks for without one).
- **Row layout, one `PlanFlatNode` per row:** `[chevron][indent][operation +
  target, flex_1][est cost][est rows][actual rows][time][buffers-glyph]` —
  fixed-width metric columns (`px(70.)`-ish each) right-aligned, `"—"` for
  `None` fields (never blank, so columns stay visually aligned row to row).
  Hot-node coloring (§2 thresholds) applies to the **row's background**,
  not just the time column, so a hot node is visually obvious scanning down
  the tree — same `row.bg(rgb(...))` mechanism `schema_tree.rs` already uses
  for `is_selected`.
- **Node detail:** clicking a row's operation text (not the chevron) opens
  the SAME read-only cell-detail popup pattern the grid uses for a non-
  editable cell (`Enter`/double-click → full-content popup, per §1 of the
  target-UI spec) showing every `extra` key/value plus buffer breakdown —
  reuses that popup component rather than building a new one.
- **Hints:** `top_level_hints` render as a small warning-coloured
  (`0xf9e2af` amber, consistent with §2's threshold colour re-use) banner
  strip between the header and the tree, one line per hint
  (`message` + a "Zobrazit CREATE INDEX" expandable line for `detail` when
  present) — always-visible, not buried in a menu, since a missing-index
  hint is exactly the kind of thing a user opens the Explain tab to find.
- **Raw toggle:** flips the tree view to a plain scrollable text block of
  `raw_text` (reuses `TabContent::Text`'s existing scroll-lines rendering
  code path) — same tab, not a new tab, so the toggle state lives on the
  `Plan` tab's own local UI state (a bool the render function reads),
  not in `PlanResult` itself (which stays a pure data value).
- **UI text: Czech**, consistent with the rest of the app — "Odhad" /
  "Analýza" (or "Explain"/"Analyze" left as-is if the existing button
  vocabulary elsewhere already mixes English verbs; **decision: Czech**,
  "Vysvětlit" for Explain, "Analyzovat" for Analyze, matching e.g.
  "Aplikovat"/"Zahodit"/"Smazat řádek" elsewhere) for both buttons and all
  static labels ("Odhadovaný plán" / "Skutečný plán" badge,
  "Chybějící index" hint prefix, "Nezjištěno" for a `None` metric shown in
  the detail popover — though the tree's own metric columns use the denser
  `"—"` per the row-layout decision above, not the longer word).

## 5. The sharp edge: `ANALYZE`/`STATISTICS XML` on a write statement

The brief is explicit this is the sharpest edge of the phase — decided
precisely, no TBD, mirroring the existing `is_read_statement`/`execute()`
machinery rather than inventing a parallel one.

- **Step 1, always run first, client-side, free:** compute
  `dbc_core::is_read_statement(sql)` on the RAW editor SQL (before any
  `EXPLAIN` wrapping — exactly the same call `run_query_with`'s Guard 1
  already makes). This already correctly classifies `sql` as read or write
  regardless of whether the user is about to wrap it in `EXPLAIN` (the
  existing `explain_analyze_write_fails_closed` test already proves
  `is_read_statement("EXPLAIN ANALYZE UPDATE t SET a=1")` is `false` — but
  that's testing the *wrapped* string; here we test the *unwrapped* `sql`,
  which is the input to the Explain/Analyze buttons, so the same function
  applies unchanged, called on the pre-wrap text).
- **"Explain" (estimated) is ALWAYS safe, on every connection, for any
  `sql`** — pg's plain `EXPLAIN (FORMAT JSON)`, MSSQL's `SET SHOWPLAN_XML
  ON`, and SQLite's `EXPLAIN QUERY PLAN` are all guaranteed by their engines
  to never execute the statement, full stop, independent of read/write
  classification or the connection's `read_only` flag. No gating needed —
  the "Explain" button has no confirm step, ever.
- **"Analyze" (actual) gating, three cases:**
  1. `is_read_statement(sql) == true` (a `SELECT`/`WITH`/etc.) → runs
     immediately, no confirmation, on any connection (including
     `read_only` ones) — it's a read, `ANALYZE` timing it doesn't change
     that.
  2. `is_read_statement(sql) == false` (a write) **and** the active
     connection's `read_only == true` → **blocked outright**, same
     no-override message pattern as `run_query_with`'s existing Guard 1
     (`QueryError::msg("connection is read-only")` → status bar
     `"error: connection is read-only"`), status bar only, no modal.
     Rationale: a read-only connection blocks every write path app-wide
     per §3 of the target-UI spec; `ANALYZE` of a write is a write path by
     construction (it executes the statement), so it inherits that rule
     unconditionally — there is no engine-level escape hatch worth
     building for a connection the user explicitly marked read-only.
  3. `is_read_statement(sql) == false` **and** `read_only == false` →
     **confirm modal**, new `ModalState::AnalyzeWriteConfirm { sql:
     String, engine: Engine }` variant (same `modal: Option<ModalState>`
     field `main.rs` already has, same rendering dispatch
     `render_modal_overlay` already switches on) — text (Czech): *"Toto SQL
     bude SKUTEČNĚ PROVEDENO, aby bylo možné změřit skutečný plán, a poté
     vráceno zpět (ROLLBACK). Vedlejší efekty MIMO transakci (např.
     hodnoty sekvencí/IDENTITY, volání externích funkcí) NEBUDOU vráceny
     zpět."* followed by the exact `sql` text (same "show the exact SQL"
     transparency principle as G5's Apply dialog), with "Analyzovat" /
     "Zrušit" buttons. Confirming dispatches the per-engine wrapped
     sequence below.
- **Wrapped sequence — pg:** a **dedicated, one-shot connection** (opened
  and dropped for this single Explain/Analyze request only — never the
  editor's shared connection, exactly matching G5's Apply-flow rationale
  in `connection.rs`'s doc comment: `execute`'s session-sharing caveat
  means a caller in an open transaction must not interleave other `query()`
  calls on the same instance) runs, sequentially, over that one connection:
  `execute("BEGIN", ...)` → `query("EXPLAIN (ANALYZE, BUFFERS, FORMAT
  JSON) {sql}", ...)` (captures the one-row JSON result) →
  `execute("ROLLBACK", ...)` (always, even if the `EXPLAIN` step itself
  errored — same "stop at first error, always attempt rollback, tolerate
  rollback failing" discipline `connection.rs` already documents for
  `execute`). Pg supports running `EXPLAIN ANALYZE` inside an open
  transaction and rolling it back afterward — the statement's data-visible
  effects (the INSERT/UPDATE/DELETE rows) are undone by the `ROLLBACK`;
  the confirm-modal copy above is explicit that non-transactional side
  effects (sequence nextval, `dblink`/external calls, etc.) are the
  documented exception.
- **Wrapped sequence — MSSQL (needs-verification, no driver yet):**
  analogous — `SET STATISTICS XML ON; BEGIN TRAN; {sql}; ROLLBACK TRAN;`
  over a dedicated one-shot connection, reading the trailing XML result set
  per §1b. SQL Server supports `BEGIN TRAN`/`ROLLBACK TRAN` around an
  arbitrary statement the same way pg does; flagged needs-verification only
  because the exact interaction between `SET STATISTICS XML ON` and an
  explicit `BEGIN TRAN`/`ROLLBACK TRAN` wrapper (does the XML result set
  still arrive if the transaction is rolled back before the client reads
  it? within one batch it should, since the plan is generated during
  execution, before rollback — but unverified without a live server).
- **SQLite never reaches this path at all** — no Analyze button exists
  for it (§1c/§4), so there is no write-execution edge to gate.
- **Read-only enforcement is still defense-in-depth, not the only line** —
  same posture `run_query_with`'s comment already documents for Guard 1:
  Postgres's `default_transaction_read_only=on` and SQLite's
  `SQLITE_OPEN_READ_ONLY` (set at connect time by `connect::open_config`)
  remain the actual server-side backstop if the client-side check above
  were ever wrong; this phase adds no NEW server-side enforcement, it only
  adds the client-side gate for a request shape (`EXPLAIN ANALYZE`) the
  existing Guard 1 doesn't see (Guard 1 only runs inside
  `run_query_with`, which the Explain/Analyze buttons do NOT go through —
  they have their own dispatch, per §4, precisely so `bypass_auto_limit`/
  history-recording/preview-tab logic doesn't leak into this flow).

## 6. Task decomposition

Parsers first (pure, parallelizable, no UI dependency); UI after (depends
on the model types from T1, not on T2/T3/T4's specific parser correctness —
UI work can start against hand-built `PlanResult` fixtures in parallel with
T3/T4 finishing their real parsers).

- **T1 — Model + SQLite parser** (`plan.rs` §2 structs, `parse_sqlite_rows`,
  hot-fraction formula functions from §2, unit tests). No new dependency.
  Foundation for everything else — do first, solo.
- **T2 — Postgres parser** (`parse_pg_json`, `serde_json` dep add, docker-pg
  fixture capture + fixture tests per §3). Depends on T1's model types only.
  **Parallelizable with T3.**
- **T3 — MSSQL parser** (`parse_mssql_xml`, `quick-xml` dep add,
  hand-authored fixture tests per §3, all needs-verification flags from
  §1b/§3 carried as doc comments on the parser functions themselves so
  they're impossible to miss when the MSSQL driver phase lands). Depends on
  T1's model types only. **Parallelizable with T2.**
- **T4 — Write-gating logic** (§5's three-case dispatch as a pure function
  `fn analyze_gate(sql: &str, read_only: bool) -> AnalyzeGate` returning an
  enum `{ Run, Blocked, NeedsConfirm }`, unit-tested against the three cases
  + the existing CTE/comment-bypass edge cases `guards.rs` already tests
  for `is_read_statement`, since this function is a thin wrapper around it).
  Depends on nothing but `dbc_core::is_read_statement` — **parallelizable
  with T1/T2/T3.**
- **T5 — Plan tree GPUI entity** (`PlanView` in `plan.rs`'s second half:
  `flatten_plan`, `uniform_list` rendering, hot-node coloring, hint banner,
  raw-text toggle, node-detail popover reuse). Depends on T1's model types
  (can build/test against hand-written `PlanResult` fixtures before T2/T3
  land their real parsers). **Parallelizable with T2/T3/T4.**
- **T6 — Tab + trigger-button wiring** (`tabs::TabContent::Plan` variant;
  status-bar "Vysvětlit"/"Analyzovat" buttons in `main.rs`; dispatch through
  `QueryRunner` for the estimated path; wires T4's gate + the
  `ModalState::AnalyzeWriteConfirm` variant + the dedicated-connection
  BEGIN/EXPLAIN/ROLLBACK sequence for the actual path). Depends on T1, T4,
  T5 all landing; the actual-path pg wiring additionally depends on T2 for
  end-to-end correctness (though it can be stubbed against T1 fixtures
  first). **Sequenced after T1/T4/T5; can start once those three merge.**
- **T7 — MSSQL end-to-end wiring** — explicitly **deferred**, not part of
  this phase's mergeable scope: T3's parser lands and is unit-tested now,
  but the actual `SET SHOWPLAN_XML`/`STATISTICS XML` SQL dispatch has
  nothing to run against without a driver. Tracked as a follow-up item for
  whenever the MSSQL driver phase (orthogonal, unscheduled per the
  target-UI spec) lands — at that point T3's parser gets corrected against
  real captures and T6's dispatch gets an MSSQL branch.

Suggested parallel batches: **{T1}** solo first → **{T2, T3, T4, T5}** in
parallel (four independent agents/sessions, each depending only on T1's
merged model types) → **{T6}** solo last (integrates T1/T4/T5, stubs T2/T3
where useful) → **T7** whenever the MSSQL driver phase happens.

## 7. Risks / needs-verification (consolidated)

- **MSSQL Showplan XML attribute names and result-set delivery mechanics**
  (§1b, §3) — no live server or driver crate to verify against; every
  MSSQL-specific claim above is best-effort from documented behaviour and
  must be corrected against real captures before T7.
- **MSSQL missing-index XML shape** (`<MissingIndexes>`/`<MissingIndexGroup>`
  nesting, §1b) — same caveat; the synthesized "CREATE INDEX" suggestion
  text in `PlanHint.detail` is a v1 best-effort string, not guaranteed to
  be directly runnable SQL without review.
- **pg `"Actual Total Time"` per-loop-averaging** (§1a/§2) — the
  multiply-by-`"Actual Loops"`-before-subtracting-children formula is
  standard practice (matches what `pgAdmin`/`explain.depesz.com` do) but is
  not verified against a captured parallel-worker plan in this repo; flag
  for a fixture specifically covering a `Workers Launched > 0` node in T2.
- **pg `EXPLAIN ANALYZE` inside a `BEGIN`/`ROLLBACK` still reports accurate
  timing** (§5 — the plan JSON, including `"Actual Total Time"`/buffers, is
  generated during statement execution, before the `ROLLBACK`, so the
  measurement itself is unaffected by the later rollback) is asserted from
  documented pg transactional semantics, not verified against a live
  capture with actual sequence advancement in this repo — low risk (pg's
  documentation is explicit and long-standing on this point) but worth a
  manual smoke test against the docker-pg instance during T6.
- **`quick-xml` version pin** — "0.31" in §3 is illustrative; pin whatever
  is current non-yanked at T3 implementation time.
- **Hot-node thresholds (30%/10%, §2)** are a judgment call, not derived
  from any engine's own convention (pg/MSSQL don't define a standard
  "hot" cutoff) — flagged as a tuning knob likely to get user feedback
  after first real use, not a correctness risk.
