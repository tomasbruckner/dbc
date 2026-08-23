# G8 ER Diagram Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers for every task, a **sonnet** adversarial code review per task before it's considered done, and a **default-model** final review once all tasks land (same staffing convention as the G9/G13 plans).

**Goal:** FK-graph rendering of one schema in a GPUI canvas — collapsed table boxes (PK/FK columns only), a hand-rolled layered (Sugiyama-style) layout with straight-line edges, pan/zoom, click-a-node-to-open-its-DDL-tab, and a lossless SVG export. Read-only end to end: no `execute()` call, no new query, no new catalog fetch anywhere in this phase — the whole feature is a pure transform of the `SchemaSnapshot` the schema tree has already fetched.

**Architecture:** Two new pure modules in `dbc-core` (`crates/dbc-core/src/erd.rs` — graph model, `crates/dbc-core/src/erd/layout.rs` — layered layout, `crates/dbc-core/src/erd/svg.rs` — SVG serializer), all plain structs/`Vec`/`HashMap`/`BTreeMap` over `dbc_core::{TableInfo, ColumnInfo, FkRef}`, zero GPUI, zero new dependency, all directly unit-tested. One new GPUI module in `dbc-ui` (`crates/dbc-ui/src/er_diagram_view.rs`) holding `ErDiagramView` (a `gpui::canvas()`-painted entity: node quads, straight-line/self-loop-bezier edges, pan-drag, anchored-zoom-scroll, hit-test-driven click-to-DDL) built on primitives (`canvas`, `paint_quad`, `paint_path`, `PathBuilder`, `text_system().shape_line()`) already exercised elsewhere in this codebase (`grid.rs`, `sql_input.rs`) or freshly retired as a risk by this plan's own spike step (T4). `main.rs` grows one new `TabContent::Diagram { view: Entity<ErDiagramView> }` arm (three exhaustive matches need it — enumerated in T6), a palette action, and a schema-tree row icon; `schema_tree.rs` grows one small icon-button affordance on `NodeId::Schema(_)` rows, reusing the existing ★-toggle pattern and `TreeEvent::OpenDdl` verbatim for the click-to-DDL path (zero new tab-content plumbing, zero new event enum).

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — every primitive this plan uses is confirmed present in the vendored checkout by file:line below; no new GPUI primitive beyond what T4's spike explicitly retires as a risk). No new crate anywhere in this plan — `lyon` (backing `PathBuilder`) is already a transitive GPUI dependency; SVG export is hand-built XML string formatting, no rasterization library.

**Spec:** `docs/superpowers/specs/drafts/g8-er-diagram-design.md` — the CURATION block (top of file, dated 2026-08-23) is binding and overrides surrounding draft prose where the two conflict. The two hardest CURATION requirements — SVG XML-escaping of all interpolated text, and a mandatory feasibility spike before building the canvas view — are both load-bearing in this plan's task shape (T3 and T4 respectively), not soft suggestions. Every API claim below is grounded against the actual code on this branch as read while drafting: `crates/dbc-core/src/schema.rs`, `crates/dbc-core/src/ddl.rs`, `crates/dbc-core/src/lib.rs`, `crates/dbc-ui/src/{schema_tree,grid,sql_input,tabs,palette,main}.rs`, and the vendored GPUI checkout at `C:\Users\tomas\.cargo\git\checkouts\zed-a70e2ad075855582\907ed09` (`crates/gpui/src/{elements/canvas.rs,path_builder.rs,window.rs,platform.rs,app.rs}`, `crates/gpui_windows/src/platform.rs`).

**Note on branch state (per brief):** this plan is written against `main`'s current `TabContent` (`Grid`/`Text` only, confirmed `crates/dbc-ui/src/tabs.rs:29-37` on this worktree). The brief states `main.rs` may already carry a `Monitor` variant by the time this plan lands (G9 in flight on a parallel branch). T6 (the only task touching `tabs.rs`/`main.rs`'s `TabContent` matches) is a serialized tail task regardless (see Global Constraints) — at merge time, re-locate every exhaustive `match ... TabContent` site by symbol, add the `Diagram` arm alongside whatever other new arms (`Monitor`, `Plan`, `Compare`) have landed from sibling phases, and confirm the total match arm count against the enum's actual variant count rather than trusting this plan's line numbers.

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; every GPUI primitive this plan uses (`canvas`, `Window::paint_quad`, `Window::paint_path`, `PathBuilder::{stroke, move_to, line_to, curve_to, build}`, `Window::text_system().shape_line()`, `on_mouse_down`/`on_mouse_move`/`on_mouse_up`, `on_scroll_wheel`, `cx.prompt_for_new_path`) is confirmed present in the vendored checkout by file:line in the relevant task's Grounding section — no new API surface assumed. T4 opens with a literal, run-it-on-Windows spike (not just a source read) before any of `ErDiagramView`'s real code is written, because `canvas()`/`PathBuilder::curve_to` have never been exercised at runtime anywhere in this codebase (existing `paint_quad`/`shape_line` call sites in `grid.rs`/`sql_input.rs` prove those two primitives work; the custom-drawing escape hatch and the one curve this plan needs do not have an existing call site to lean on).
- **G8 is READ-ONLY end to end.** No `execute()` call, no new `query()` call, no `Connection` reference of any kind may appear anywhere in `crates/dbc-core/src/erd.rs`, `crates/dbc-core/src/erd/*.rs`, or `crates/dbc-ui/src/er_diagram_view.rs` — the entire feature is a pure transform of a `SchemaSnapshot` the schema tree has already fetched (`self.tree.read(cx).snapshot()`, confirmed accessor at `crates/dbc-ui/src/schema_tree.rs:710`). Every task's last code step includes `grep -n "\.execute(\|\.query(" <files touched>` and must return nothing.
- **No credentials/result data in logs.** Trivially satisfied by construction — this phase never touches `Vault`, `ConnectSpec`, or row/cell data; it renders table/column *names and types* from an already-in-memory `SchemaSnapshot`, the same metadata the schema tree already displays. Stated explicitly per project convention, not a no-op.
- **SVG XML-escape is mandatory, not optional.** Every table name, schema name, and column name interpolated into the exported SVG string MUST pass through a single, dedicated `erd::svg::escape_xml` function — table/column names come straight off the wire (a Postgres identifier can legally contain `<`, `>`, `&`, `"`, `'` if quoted at creation) and are never pre-sanitized anywhere upstream. **REQUIRED test (T3, non-negotiable):** a table literally named `we"ird<x>` (plus a column and a schema name each carrying a different one of `<`/`>`/`&`/`"`/`'`) produces SVG containing zero un-escaped instances of any of those five characters outside of the fixed XML syntax the serializer itself emits, and the escaped entities are present at the exact expected positions. A plan (or an implementation) that omits this test is defective by definition of this constraint, not merely incomplete.
- **File dialogs: `cx.prompt_for_new_path`, no filter of any kind — corrected from the brief's "PathPromptOptions without file filters."** Grounded against `crates/gpui/src/platform.rs:190-198`: `PathPromptOptions` (the `files`/`directories`/`multiple`/`prompt` struct) is the parameter type of `prompt_for_paths` (the OPEN/browse dialog) — it is not used by `prompt_for_new_path` (the SAVE-AS dialog) at all, and nothing in `dbc-ui` calls `prompt_for_paths` anywhere today. T6's SVG export reuses the exact pattern `grid.rs`'s CSV/TSV/JSON export already uses (`crates/dbc-ui/src/grid.rs:1331`): `cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name))`, a two-argument call with no filter parameter to pass in the first place. One further correction found while grounding this: the claim "`gpui_windows` never calls `SetFileTypes`" is **empirically false** — `crates/gpui_windows/src/platform.rs:1358`'s `file_save_dialog` (the backend behind `prompt_for_new_path`) DOES call `IFileSaveDialog::SetFileTypes`, but with a single hard-coded `{ pszName: "All files", pszSpec: "*.*" }` entry — a wildcard, not an extension-specific filter, and not something any caller (including this plan's export button) can parameterize; `prompt_for_paths`'s Windows backend (`crates/gpui_windows/src/platform.rs:1270-1322`, `file_open_dialog`) never calls `SetFileTypes` at all, which is presumably what the brief's claim actually refers to. Net effect for this plan is identical either way: there is no code path by which T6's "Export…" button could restrict the save dialog to `.svg`, nor any need to — `suggested_name`'s `.svg` extension (mirroring `grid.rs`'s `{table}.{ext}` convention) is the only signal the user gets, exactly like every existing export in this app.
- **Deep-graph hazard (binding, applies to T2's cycle-breaking and layering):** the FK graph this plan lays out is real, user-controlled, and can legitimately contain cycles — self-referencing tables (`employees.manager_id -> employees.id`) and bidirectional pairs (`A` FKs to `B` and `B` FKs to `A`) are both explicitly in scope per the design (§1), and a hostile/malformed schema could in principle contain a much longer cycle. Consequently: **the cycle-breaking DFS (`classify_back_edges`, T2) and the longest-path layering (`assign_layers`, T2) are both written iteratively — an explicit `Vec`-based frame stack / `VecDeque` work queue, never a self-calling recursive function** — and both carry a defensive iteration cap (`10 * (V+E) + 10_000`, an `assert!` that only ever trips on an implementation bug, never in correct operation against a well-formed graph, since both algorithms are provably `O(V+E)`) so a future refactor that accidentally reintroduces unbounded recursion or an infinite loop fails loudly in CI rather than hanging the app or overflowing the stack on a real customer schema. This mirrors the project's established deep-structure discipline (`PlanNode`'s iterative `Drop` + iterative tree-builders in `docs/superpowers/plans/2026-08-23-g13-execution-plans.md`'s Global Constraints) — `ErdGraph`/`DiagramLayout` are flat `Vec`-of-struct (not a recursive tree type like `PlanNode`), so no custom `Drop`/no-`Clone` restriction is needed here; the hazard is purely "don't write a recursive graph-walk," not "don't derive `Clone` on a self-referential type."
- **Layout determinism:** `compute_layout` (T2) never uses randomness; every tie (crossing-reduction order, isolated-row placement) is broken by `TableKey` (`(schema, name)`), ascending. Same input `ErdGraph` twice must produce byte-identical `DiagramLayout` (`f32` bit-equality, proven by a dedicated test, T2).
- **UI strings are Czech** (labels, statuses, tooltips, the large-schema notice) — English only in code/comments/tests, matching every other phase in this codebase.
- Tests green before every commit: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-ui` must pass with the task's new tests included; each task must leave every crate it touches at least as green as it found it.
- **Docker validation: NOT included, and this is a deliberate conclusion, not an oversight.** This plan's brief says to add a docker task only if catalog shapes matter beyond what `SchemaSnapshot` already provides. G8 issues zero new catalog SQL of any kind — it consumes the exact same, already-fetched, already-tested `SchemaSnapshot` the schema tree renders today (T1's `build_graph` takes `&[TableInfo]`, nothing engine-specific). There is no new "does this actually work against a real server" claim for a docker test to prove; the schema-fetch path it rides on is validated by whichever phase owns `Connection::schema()`, not this one. Restated in Self-Review.
- **Task-ordering (single-writer files):** `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/tabs.rs`, `crates/dbc-ui/src/palette.rs`, and `crates/dbc-ui/src/schema_tree.rs` are being edited concurrently by other in-flight phases in this repo (G9/G10/G12/G13, per the brief). T6 (the only task touching any of those four files) is a **serialized tail task, dispatched only after that in-flight work has merged to `main`** and this branch has rebased onto that merge — re-locate every line reference in T6 by symbol, not line number, after the rebase, and re-run the exhaustive-`TabContent`-match audit (see the branch-state note above) since the arm count will have grown. T1, T2, T3 touch only new files inside `dbc-core` and are fully parallelizable in separate worktrees the moment their type-level dependency is available (T2 needs T1's structs; T3 needs T2's `DiagramLayout` shape, available as soon as T2's public types are frozen, even before the algorithm internals are fully tuned). T4 (the GPUI canvas view) is scheduled strictly AFTER T1-T3 land, per this plan's explicit task-ordering brief (a stricter sequencing than the design draft's own suggestion that T3 and T4 could run concurrently — see Self-Review note 1). T5 depends on T4. T7 rides with T6.
- **Version bump at merge** (phase-numbered convention, `crates/dbc-ui/Cargo.toml` at `0.6.0` at time of writing): `dbc-ui` bumps to `0.8.0` at branch finish (T6's tail, not any individual task's commit). `dbc-core` stays at `0.1.0` — same "no bump for a same-major internal addition" precedent the G7 plan set for its own `dbc-core`-adjacent work.

### Task dependency graph

| Task | Files | Depends on | Notes |
|---|---|---|---|
| T1 | `crates/dbc-core/src/erd.rs`, `crates/dbc-core/src/lib.rs` | — | graph model; solo, first — every other task needs its types |
| T2 | `crates/dbc-core/src/erd/layout.rs`, `crates/dbc-core/src/lib.rs` | T1 | layered layout algorithm; parallel-worktree eligible the moment T1's structs are frozen |
| T3 | `crates/dbc-core/src/erd/svg.rs`, `crates/dbc-core/src/lib.rs` | T2 | SVG serializer + mandatory escape/injection tests; needs T2's `DiagramLayout` shape |
| T4 | `crates/dbc-ui/src/er_diagram_view.rs`, `crates/dbc-ui/src/main.rs` (`mod` list only) | T2 | **spike-gated** — Step 1 is a literal on-Windows runtime check before any real code; static (no interaction) canvas render of a `DiagramLayout` |
| T5 | `crates/dbc-ui/src/er_diagram_view.rs` | T4 | hit-testing, click-to-DDL, selection highlight, pan/zoom |
| T6 | `crates/dbc-ui/src/{main,tabs,palette,schema_tree}.rs`, `crates/dbc-ui/Cargo.toml` | T3, T5 | **serialized tail** — palette action, schema-tree icon, `TabContent::Diagram`, export button, version bump |
| T7 | `crates/dbc-ui/src/er_diagram_view.rs` | T2, T5 | large-schema cap (`DIAGRAM_TABLE_CAP = 150`) + notice banner; small, rides with T6 |

**Parallelization:** T1 is the hard prerequisite for everything. Once T1 lands, T2 proceeds alone (it's the only consumer of T1's types at first). Once T2's public `DiagramLayout`/`PositionedNode`/`RoutedEdge` shape is frozen, T3 can start in its own worktree even before T2's algorithm internals are fully tuned — the type signature is the contract, same precedent the design draft states for its own T2/T3 split. T4 starts only after T1-T3 have all landed (this plan's explicit ordering brief, stricter than the design draft — see Self-Review note 1) and opens with the spike. T5 needs T4 merged. T6 and T7 both need T5 merged; T7 is small enough to ride inside T6's commit rather than being a fully separate worktree, but is kept as its own task below for traceability against the design's own T7.

---

### Task 1 (T1): Graph model — `erd::build_graph`

**Files:**
- Create: `crates/dbc-core/src/erd.rs`
- Modify: `crates/dbc-core/src/lib.rs` (add `pub mod erd;`)

**Interfaces:**
- Consumes: `dbc_core::{TableInfo, ColumnInfo, FkRef}` (already `pub use`d, `crates/dbc-core/src/lib.rs:16-19`).
- Produces (consumed by T2, T3, T4):
  ```rust
  pub const MAX_VISIBLE_COLS: usize = 6;

  #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
  pub struct TableKey {
      pub schema: Option<String>,
      pub name: String,
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ErdColumnRow {
      pub name: String,
      pub data_type: String,
      pub is_pk: bool,
      pub is_fk: bool,
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ErdNode {
      pub key: TableKey,
      /// PK columns first, then FK columns, capped at `MAX_VISIBLE_COLS`.
      /// A column that is both PK and FK appears once, with both flags set.
      pub visible_cols: Vec<ErdColumnRow>,
      /// Footer count for "+N dalších" (0 = no footer row).
      pub hidden_col_count: usize,
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct FkEdge {
      pub from: TableKey,
      pub to: TableKey,
      /// (from_column, to_column) pairs — every column of every FK
      /// constraint from `from` to `to` collapses into ONE edge (design §0).
      pub columns: Vec<(String, String)>,
  }

  #[derive(Debug, Clone, PartialEq, Eq, Default)]
  pub struct ErdGraph {
      pub nodes: Vec<ErdNode>,
      pub edges: Vec<FkEdge>,
  }

  /// Pure, no I/O. `tables` is caller-selected (design §3: always exactly
  /// one schema's worth, scoped upstream by T6/T7 — this function has no
  /// opinion on selection, it just builds a graph over whatever slice it's
  /// given).
  pub fn build_graph(tables: &[TableInfo]) -> ErdGraph;
  ```

**Grounding:** `TableInfo { schema: Option<String>, name: String, columns: Vec<ColumnInfo>, .. }`, `ColumnInfo { name, data_type, is_pk: bool, fk: Option<FkRef>, .. }`, `FkRef { schema: Option<String>, table: String, column: String }` — all confirmed verbatim at `crates/dbc-core/src/schema.rs:24-55`. `TableKey` derives `Ord` (tuple order on `(Option<String>, String)`, `None < Some(_)`) because T2's layering/crossing-reduction needs a total, deterministic order for every tie-break (Global Constraints). Edge grouping (design §0): every `ColumnInfo` in table A whose `fk` resolves to table B contributes one `(a_col, b_col)` pair to the SAME `FkEdge` for the ordered pair `(A, B)` — composite FKs naturally collapse (all their columns hit the same map key); two SEPARATE FK constraints between the same ordered pair (e.g. `orders.billing_addr_id`/`orders.shipping_addr_id` both -> `addresses.id`) ALSO collapse into one edge carrying two column pairs (documented simplification, design §0 — `ConstraintInfo` has no structured column list to disambiguate them, see T2's Self-Review-equivalent note). Self-references (`from == to`) and bidirectional pairs (`A->B` and `B->A` as two distinct `FkEdge`s, since they're different map keys) both fall out of this construction with no special-casing needed at graph-build time — T2 special-cases self-loops at layout time, not here.

```rust
use std::collections::BTreeMap;

use crate::schema::TableInfo;

pub const MAX_VISIBLE_COLS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableKey {
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErdColumnRow {
    pub name: String,
    pub data_type: String,
    pub is_pk: bool,
    pub is_fk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErdNode {
    pub key: TableKey,
    pub visible_cols: Vec<ErdColumnRow>,
    pub hidden_col_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkEdge {
    pub from: TableKey,
    pub to: TableKey,
    pub columns: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ErdGraph {
    pub nodes: Vec<ErdNode>,
    pub edges: Vec<FkEdge>,
}

pub fn build_graph(tables: &[TableInfo]) -> ErdGraph {
    let mut nodes = Vec::with_capacity(tables.len());
    // BTreeMap keyed by the ordered (from, to) pair -> deterministic
    // iteration order for free, and collapses every FK column pair between
    // the same ordered table pair into one edge (design §0).
    let mut edge_map: BTreeMap<(TableKey, TableKey), Vec<(String, String)>> = BTreeMap::new();

    for t in tables {
        let key = TableKey { schema: t.schema.clone(), name: t.name.clone() };

        let mut rows: Vec<ErdColumnRow> = t
            .columns
            .iter()
            .filter(|c| c.is_pk || c.fk.is_some())
            .map(|c| ErdColumnRow {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                is_pk: c.is_pk,
                is_fk: c.fk.is_some(),
            })
            .collect();
        // Stable sort: PK rows first, FK-only rows after, original catalog
        // order preserved within each group.
        rows.sort_by_key(|c| !c.is_pk);
        let hidden_col_count = rows.len().saturating_sub(MAX_VISIBLE_COLS);
        rows.truncate(MAX_VISIBLE_COLS);

        nodes.push(ErdNode { key: key.clone(), visible_cols: rows, hidden_col_count });

        for c in &t.columns {
            if let Some(fk) = &c.fk {
                let to = TableKey { schema: fk.schema.clone(), name: fk.table.clone() };
                edge_map
                    .entry((key.clone(), to))
                    .or_default()
                    .push((c.name.clone(), fk.column.clone()));
            }
        }
    }

    nodes.sort_by(|a, b| a.key.cmp(&b.key));
    let edges = edge_map
        .into_iter()
        .map(|((from, to), columns)| FkEdge { from, to, columns })
        .collect();

    ErdGraph { nodes, edges }
}
```

- [ ] **Step 1: Write the code above** in `crates/dbc-core/src/erd.rs`.

- [ ] **Step 2: Tests** (same file, `#[cfg(test)] mod tests`):

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::schema::{ColumnInfo, FkRef, TableInfo};

      fn col(name: &str, ty: &str, pk: bool, fk: Option<(&str, &str)>) -> ColumnInfo {
          ColumnInfo {
              name: name.into(),
              data_type: ty.into(),
              nullable: !pk,
              default: None,
              is_pk: pk,
              fk: fk.map(|(table, column)| FkRef { schema: None, table: table.into(), column: column.into() }),
          }
      }
      fn table(name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
          TableInfo { schema: None, name: name.into(), columns: cols, ..Default::default() }
      }
      fn key(name: &str) -> TableKey {
          TableKey { schema: None, name: name.into() }
      }

      #[test]
      fn composite_fk_collapses_to_one_edge_with_two_column_pairs() {
          let orders = table(
              "orders",
              vec![
                  col("id", "int4", true, None),
                  col("addr_country", "text", false, Some(("addresses", "country"))),
                  col("addr_id", "int4", false, Some(("addresses", "id"))),
              ],
          );
          let addresses = table("addresses", vec![col("id", "int4", true, None), col("country", "text", true, None)]);
          let g = build_graph(&[orders, addresses]);
          assert_eq!(g.edges.len(), 1, "two FK columns to the same table must collapse into one edge");
          let e = &g.edges[0];
          assert_eq!(e.from, key("orders"));
          assert_eq!(e.to, key("addresses"));
          assert_eq!(e.columns.len(), 2);
      }

      #[test]
      fn self_reference_is_a_normal_edge_with_from_equal_to() {
          let employees = table(
              "employees",
              vec![col("id", "int4", true, None), col("manager_id", "int4", false, Some(("employees", "id")))],
          );
          let g = build_graph(&[employees]);
          assert_eq!(g.edges.len(), 1);
          assert_eq!(g.edges[0].from, g.edges[0].to);
          assert_eq!(g.edges[0].from, key("employees"));
      }

      #[test]
      fn bidirectional_pair_is_two_distinct_edges() {
          let a = table("a", vec![col("id", "int4", true, None), col("b_id", "int4", false, Some(("b", "id")))]);
          let b = table("b", vec![col("id", "int4", true, None), col("a_id", "int4", false, Some(("a", "id")))]);
          let g = build_graph(&[a, b]);
          assert_eq!(g.edges.len(), 2);
          assert!(g.edges.iter().any(|e| e.from == key("a") && e.to == key("b")));
          assert!(g.edges.iter().any(|e| e.from == key("b") && e.to == key("a")));
      }

      #[test]
      fn isolated_table_is_present_with_zero_edges() {
          let lonely = table("lonely", vec![col("id", "int4", true, None)]);
          let g = build_graph(&[lonely]);
          assert_eq!(g.nodes.len(), 1);
          assert!(g.edges.is_empty());
      }

      #[test]
      fn node_columns_cap_at_max_visible_with_footer_count() {
          let cols: Vec<ColumnInfo> = (0..9).map(|i| col(&format!("c{i}"), "int4", i == 0, None)).collect();
          let t = table("wide", cols);
          let g = build_graph(&[t]);
          assert_eq!(g.nodes[0].visible_cols.len(), MAX_VISIBLE_COLS);
          assert_eq!(g.nodes[0].hidden_col_count, 9 - MAX_VISIBLE_COLS);
      }

      #[test]
      fn non_pk_non_fk_columns_are_never_shown() {
          let t = table("t", vec![col("id", "int4", true, None), col("note", "text", false, None)]);
          let g = build_graph(&[t]);
          assert_eq!(g.nodes[0].visible_cols.len(), 1);
          assert_eq!(g.nodes[0].hidden_col_count, 0, "note isn't PK/FK, so it's just absent, not counted as hidden");
      }
  }
  ```

- [ ] **Step 3: Export from `lib.rs`.** `crates/dbc-core/src/lib.rs` — add `pub mod erd;` to the module list (a full `pub mod`, not the file's usual `mod x; pub use x::{items};` pattern, because `erd` has real internal submodule structure once T2/T3 land — `dbc_core::erd::layout::*`, `dbc_core::erd::svg::*` — matching the design draft's own dotted-path notation; the flat modules `schema.rs`/`ddl.rs`/`guards.rs` stay on the item-re-export pattern since they have no submodules of their own).

- [ ] **Step 4: Run to green and confirm read-only.**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core erd::`
  Expected: 6 tests pass, zero warnings.
  Run: `grep -n "\.execute(\|\.query(" crates/dbc-core/src/erd.rs`
  Expected: no output.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/dbc-core/src/erd.rs crates/dbc-core/src/lib.rs
  git commit -m "feat: erd::build_graph — FK graph model over SchemaSnapshot (G8 T1)"
  ```

---

### Task 2 (T2): Layout algorithm — `erd::layout::compute_layout`

**Files:**
- Create: `crates/dbc-core/src/erd/layout.rs`
- Modify: `crates/dbc-core/src/erd.rs` (add `pub mod layout;`)

**Interfaces:**
- Consumes: T1's `ErdGraph`/`ErdNode`/`FkEdge`/`TableKey`.
- Produces (consumed by T3, T4, T5, T7):
  ```rust
  pub const NODE_WIDTH: f32 = 220.0;
  pub const HEADER_H: f32 = 24.0;
  pub const ROW_H: f32 = 18.0;
  pub const FOOTER_H: f32 = 16.0;
  pub const LAYER_GAP: f32 = 60.0;
  pub const COL_GAP: f32 = 40.0;
  pub const ISOLATED_COLS_PER_ROW: usize = 6;

  #[derive(Debug, Clone, PartialEq)]
  pub struct PositionedNode { pub key: TableKey, pub x: f32, pub y: f32, pub w: f32, pub h: f32 }

  #[derive(Debug, Clone, PartialEq)]
  pub struct RoutedEdge {
      pub from: TableKey,
      pub to: TableKey,
      /// 2 points (straight line, border-to-border) for a normal edge; 3
      /// points (start, control, end — one `PathBuilder::curve_to`) when
      /// `is_self_loop`.
      pub points: Vec<(f32, f32)>,
      pub is_self_loop: bool,
  }

  #[derive(Debug, Clone, PartialEq, Default)]
  pub struct DiagramLayout { pub nodes: Vec<PositionedNode>, pub edges: Vec<RoutedEdge> }

  /// Deterministic, iterative, no recursion (Global Constraints — deep-graph
  /// hazard). O(V+E) layering, O(4E) crossing reduction.
  pub fn compute_layout(graph: &ErdGraph) -> DiagramLayout;
  ```

**Grounding — algorithm shape (design §2, five steps):**
1. **Cycle breaking**, iterative DFS with an explicit `Vec<(TableKey, usize)>` frame stack (node + "next neighbour index to visit"), white/gray/black coloring, visiting undiscovered roots in `TableKey` order for determinism. A back edge is one that points at a currently-gray (on-stack) node. Self-loops (`from == to`) are partitioned out before this step and never enter the DFS at all.
2. **Longest-path layering** via Kahn's algorithm on the acyclic skeleton (back edges logically reversed for THIS step only — the renderer, T4/T5, always draws the arrowhead per the original `FkEdge` direction): `layer(v) = 1 + max(layer(u))` over incoming edges, computed by relaxing `layer[v]` every time an edge `u->v` is processed and only enqueuing `v` once its indegree hits zero (i.e., once every predecessor has already contributed its relaxation) — this is what makes plain Kahn's topological sort correct for longest-path layering, not just topological order.
3. **Crossing reduction**: barycenter heuristic, fixed 4 iterations (2 down-sweeps, 2 up-sweeps, alternating), ties broken by table name ascending, nodes with no neighbour in the adjacent layer sort after ones that do.
4. **Coordinate assignment**: fixed node box size per `ErdNode` (`HEADER_H + min(visible_cols.len(), 6) * ROW_H + (hidden_col_count > 0 ? FOOTER_H : 0)`), layers stacked in `y` by cumulative max-height-per-layer + `LAYER_GAP`, nodes within a layer spread in `x` at `NODE_WIDTH + COL_GAP`, centered as a block. Isolated nodes (zero edges touching them, self-loops don't count as isolated — a self-loop-only table has one edge, just to itself, and is NOT isolated) get their own row below the last connected layer, wrapped at `ISOLATED_COLS_PER_ROW` per row, alphabetical.
5. **Edge routing**: straight line between the two boxes' border-intersection points (the point where the center-to-center segment crosses each rectangle's edge) for normal edges; a fixed 3-point bezier stub (start/control/end, all on the node's right edge) for self-loops — the one curve in v1.

```rust
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use super::{ErdGraph, ErdNode, TableKey};

pub const NODE_WIDTH: f32 = 220.0;
pub const HEADER_H: f32 = 24.0;
pub const ROW_H: f32 = 18.0;
pub const FOOTER_H: f32 = 16.0;
pub const LAYER_GAP: f32 = 60.0;
pub const COL_GAP: f32 = 40.0;
pub const ISOLATED_COLS_PER_ROW: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct PositionedNode {
    pub key: TableKey,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEdge {
    pub from: TableKey,
    pub to: TableKey,
    pub points: Vec<(f32, f32)>,
    pub is_self_loop: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiagramLayout {
    pub nodes: Vec<PositionedNode>,
    pub edges: Vec<RoutedEdge>,
}

fn node_height(n: &ErdNode) -> f32 {
    let rows = n.visible_cols.len().min(6) as f32;
    let footer = if n.hidden_col_count > 0 { FOOTER_H } else { 0.0 };
    HEADER_H + rows * ROW_H + footer
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

/// Iterative DFS (Global Constraints — deep-graph hazard: no recursion on a
/// user-controlled FK graph). Returns the set of (u, v) edges classified as
/// back edges. Iteration cap is a defensive backstop only — this algorithm
/// is provably O(V+E); the cap only ever trips on a bug.
fn classify_back_edges(nodes: &[TableKey], adj: &BTreeMap<TableKey, Vec<TableKey>>) -> HashSet<(TableKey, TableKey)> {
    let mut color: HashMap<TableKey, Color> = nodes.iter().cloned().map(|k| (k, Color::White)).collect();
    let mut back_edges = HashSet::new();
    let edge_count: usize = adj.values().map(|v| v.len()).sum();
    let cap = 10 * (nodes.len() + edge_count).max(1) + 10_000;
    let mut steps = 0usize;

    for start in nodes {
        if color.get(start).copied() != Some(Color::White) {
            continue;
        }
        let mut stack: Vec<(TableKey, usize)> = vec![(start.clone(), 0)];
        color.insert(start.clone(), Color::Gray);
        while let Some((node, idx)) = stack.last().cloned() {
            steps += 1;
            assert!(steps < cap, "erd layout: DFS exceeded iteration cap — graph invariant violated");
            let neighbours = adj.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
            if idx < neighbours.len() {
                stack.last_mut().unwrap().1 += 1;
                let next = neighbours[idx].clone();
                match color.get(&next).copied().unwrap_or(Color::Black) {
                    Color::White => {
                        color.insert(next.clone(), Color::Gray);
                        stack.push((next, 0));
                    }
                    Color::Gray => {
                        back_edges.insert((node.clone(), next));
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node.clone(), Color::Black);
                stack.pop();
            }
        }
    }
    back_edges
}

/// Kahn's algorithm doubling as longest-path layering: `layer[v]` is
/// relaxed on EVERY incoming edge, but `v` is only enqueued once its
/// indegree reaches zero — by then every predecessor has already relaxed
/// it, so the final value is the true longest-path layer. Iterative by
/// construction (a work queue, not recursion); same defensive cap as above.
fn assign_layers(nodes: &[TableKey], acyclic_edges: &[(TableKey, TableKey)]) -> HashMap<TableKey, usize> {
    let mut indegree: HashMap<TableKey, usize> = nodes.iter().cloned().map(|k| (k, 0)).collect();
    let mut succ: BTreeMap<TableKey, Vec<TableKey>> = BTreeMap::new();
    for (u, v) in acyclic_edges {
        *indegree.get_mut(v).expect("edge endpoint must be a known node") += 1;
        succ.entry(u.clone()).or_default().push(v.clone());
    }

    let mut layer: HashMap<TableKey, usize> = HashMap::new();
    let mut initial: Vec<TableKey> = nodes.iter().filter(|k| indegree[*k] == 0).cloned().collect();
    initial.sort();
    for k in &initial {
        layer.insert(k.clone(), 0);
    }
    let mut queue: VecDeque<TableKey> = initial.into();

    let cap = 10 * (nodes.len() + acyclic_edges.len()).max(1) + 10_000;
    let mut steps = 0usize;
    while let Some(u) = queue.pop_front() {
        steps += 1;
        assert!(steps < cap, "erd layout: layering exceeded iteration cap — graph invariant violated");
        let ul = layer[&u];
        if let Some(children) = succ.get(&u) {
            for v in children {
                let candidate = ul + 1;
                let better = layer.get(v).map_or(true, |&cur| candidate > cur);
                if better {
                    layer.insert(v.clone(), candidate);
                }
                let d = indegree.get_mut(v).expect("known node");
                *d -= 1;
                if *d == 0 {
                    queue.push_back(v.clone());
                }
            }
        }
    }
    layer
}

/// Barycenter crossing reduction, fixed 4 sweeps (no convergence loop —
/// Global Constraints/design §2). Ties (including "no neighbour in the
/// adjacent layer") broken by table name, ascending.
fn barycenter_reorder(
    layers: &mut [Vec<TableKey>],
    preds: &HashMap<TableKey, Vec<TableKey>>,
    succs: &HashMap<TableKey, Vec<TableKey>>,
) {
    fn position_index(layer: &[TableKey]) -> HashMap<TableKey, usize> {
        layer.iter().enumerate().map(|(i, k)| (k.clone(), i)).collect()
    }
    fn reorder_one(layer: &mut [TableKey], neighbour_pos: &HashMap<TableKey, usize>, neighbours_of: &HashMap<TableKey, Vec<TableKey>>) {
        let mut scored: Vec<(Option<f64>, TableKey)> = layer
            .iter()
            .map(|k| {
                let ns = neighbours_of.get(k).map(|v| v.as_slice()).unwrap_or(&[]);
                let idxs: Vec<f64> = ns.iter().filter_map(|n| neighbour_pos.get(n).map(|&i| i as f64)).collect();
                let bary = if idxs.is_empty() { None } else { Some(idxs.iter().sum::<f64>() / idxs.len() as f64) };
                (bary, k.clone())
            })
            .collect();
        scored.sort_by(|a, b| match (a.0, b.0) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.name.cmp(&b.1.name)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.name.cmp(&b.1.name),
        });
        for (slot, (_, k)) in layer.iter_mut().zip(scored.into_iter()) {
            *slot = k;
        }
    }

    for iter in 0..4 {
        if iter % 2 == 0 {
            for i in 1..layers.len() {
                let pos = position_index(&layers[i - 1]);
                reorder_one(&mut layers[i], &pos, preds);
            }
        } else {
            for i in (0..layers.len().saturating_sub(1)).rev() {
                let pos = position_index(&layers[i + 1]);
                reorder_one(&mut layers[i], &pos, succs);
            }
        }
    }
}

fn clip_to_rect_edge(center: (f32, f32), toward: (f32, f32), half_w: f32, half_h: f32) -> (f32, f32) {
    let dx = toward.0 - center.0;
    let dy = toward.1 - center.1;
    if dx == 0.0 && dy == 0.0 {
        return center;
    }
    let scale_x = if dx != 0.0 { half_w / dx.abs() } else { f32::INFINITY };
    let scale_y = if dy != 0.0 { half_h / dy.abs() } else { f32::INFINITY };
    let scale = scale_x.min(scale_y);
    (center.0 + dx * scale, center.1 + dy * scale)
}

fn self_loop_stub_points(n: &PositionedNode) -> Vec<(f32, f32)> {
    let right = n.x + n.w;
    let top = n.y + n.h * 0.3;
    let bottom = n.y + n.h * 0.7;
    vec![(right, top), (right + 30.0, n.y + n.h * 0.5), (right, bottom)]
}

pub fn compute_layout(graph: &ErdGraph) -> DiagramLayout {
    let self_loops: Vec<&super::FkEdge> = graph.edges.iter().filter(|e| e.from == e.to).collect();
    let plain_edges: Vec<&super::FkEdge> = graph.edges.iter().filter(|e| e.from != e.to).collect();

    let touched: HashSet<&TableKey> = graph.edges.iter().flat_map(|e| [&e.from, &e.to]).collect();
    let mut connected: Vec<TableKey> = graph.nodes.iter().map(|n| n.key.clone()).filter(|k| touched.contains(k)).collect();
    let mut isolated: Vec<TableKey> = graph.nodes.iter().map(|n| n.key.clone()).filter(|k| !touched.contains(k)).collect();
    connected.sort();
    isolated.sort();

    let mut adj: BTreeMap<TableKey, Vec<TableKey>> = BTreeMap::new();
    for e in &plain_edges {
        adj.entry(e.from.clone()).or_default().push(e.to.clone());
    }
    let back_edges = classify_back_edges(&connected, &adj);

    let acyclic: Vec<(TableKey, TableKey)> = plain_edges
        .iter()
        .map(|e| {
            if back_edges.contains(&(e.from.clone(), e.to.clone())) {
                (e.to.clone(), e.from.clone())
            } else {
                (e.from.clone(), e.to.clone())
            }
        })
        .collect();

    let layer_of = assign_layers(&connected, &acyclic);
    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<TableKey>> = vec![Vec::new(); max_layer + 1];
    for k in &connected {
        layers[layer_of[k]].push(k.clone());
    }

    let mut preds: HashMap<TableKey, Vec<TableKey>> = HashMap::new();
    let mut succs: HashMap<TableKey, Vec<TableKey>> = HashMap::new();
    for (u, v) in &acyclic {
        succs.entry(u.clone()).or_default().push(v.clone());
        preds.entry(v.clone()).or_default().push(u.clone());
    }
    barycenter_reorder(&mut layers, &preds, &succs);

    let node_by_key: HashMap<&TableKey, &ErdNode> = graph.nodes.iter().map(|n| (&n.key, n)).collect();
    let mut xy: HashMap<TableKey, (f32, f32, f32, f32)> = HashMap::new();

    let mut cursor_y = 0.0f32;
    for layer in &layers {
        let layer_h = layer
            .iter()
            .filter_map(|k| node_by_key.get(k).map(|n| node_height(n)))
            .fold(0.0f32, f32::max);
        let total_w = layer.len() as f32 * NODE_WIDTH + layer.len().saturating_sub(1) as f32 * COL_GAP;
        let mut x = -total_w / 2.0;
        for key in layer {
            let h = node_by_key.get(key).map(|n| node_height(n)).unwrap_or(HEADER_H);
            xy.insert(key.clone(), (x, cursor_y, NODE_WIDTH, h));
            x += NODE_WIDTH + COL_GAP;
        }
        cursor_y += layer_h + LAYER_GAP;
    }

    for (i, key) in isolated.iter().enumerate() {
        let row = i / ISOLATED_COLS_PER_ROW;
        let col = i % ISOLATED_COLS_PER_ROW;
        let h = node_by_key.get(key).map(|n| node_height(n)).unwrap_or(HEADER_H);
        let x = col as f32 * (NODE_WIDTH + COL_GAP);
        let y = cursor_y + row as f32 * (h + LAYER_GAP);
        xy.insert(key.clone(), (x, y, NODE_WIDTH, h));
    }

    let mut nodes: Vec<PositionedNode> = graph
        .nodes
        .iter()
        .filter_map(|n| xy.get(&n.key).map(|&(x, y, w, h)| PositionedNode { key: n.key.clone(), x, y, w, h }))
        .collect();
    nodes.sort_by(|a, b| a.key.cmp(&b.key));

    let pos_lookup: HashMap<&TableKey, &PositionedNode> = nodes.iter().map(|p| (&p.key, p)).collect();
    let mut edges: Vec<RoutedEdge> = Vec::with_capacity(graph.edges.len());
    for e in &plain_edges {
        if let (Some(a), Some(b)) = (pos_lookup.get(&e.from), pos_lookup.get(&e.to)) {
            let ca = (a.x + a.w / 2.0, a.y + a.h / 2.0);
            let cb = (b.x + b.w / 2.0, b.y + b.h / 2.0);
            let p1 = clip_to_rect_edge(ca, cb, a.w / 2.0, a.h / 2.0);
            let p2 = clip_to_rect_edge(cb, ca, b.w / 2.0, b.h / 2.0);
            edges.push(RoutedEdge { from: e.from.clone(), to: e.to.clone(), points: vec![p1, p2], is_self_loop: false });
        }
    }
    for e in &self_loops {
        if let Some(a) = pos_lookup.get(&e.from) {
            edges.push(RoutedEdge { from: e.from.clone(), to: e.to.clone(), points: self_loop_stub_points(a), is_self_loop: true });
        }
    }
    edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));

    DiagramLayout { nodes, edges }
}
```

- [ ] **Step 1: Write the code above** in `crates/dbc-core/src/erd/layout.rs`, and add `pub mod layout;` to `crates/dbc-core/src/erd.rs`.

- [ ] **Step 2: Tests** (same file, `#[cfg(test)] mod tests`), per design §2's exact list:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::erd::{build_graph, ErdColumnRow};
      use crate::schema::{ColumnInfo, FkRef, TableInfo};

      fn col(name: &str, pk: bool, fk: Option<(&str, &str)>) -> ColumnInfo {
          ColumnInfo {
              name: name.into(), data_type: "int4".into(), nullable: !pk, default: None, is_pk: pk,
              fk: fk.map(|(t, c)| FkRef { schema: None, table: t.into(), column: c.into() }),
          }
      }
      fn table(name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
          TableInfo { schema: None, name: name.into(), columns: cols, ..Default::default() }
      }
      fn key(name: &str) -> TableKey { TableKey { schema: None, name: name.into() } }
      fn layer_of<'a>(layout: &'a DiagramLayout, name: &str) -> f32 {
          layout.nodes.iter().find(|n| n.key.name == name).unwrap().y
      }

      #[test]
      fn single_table_no_edges() {
          let g = build_graph(&[table("t", vec![col("id", true, None)])]);
          let l = compute_layout(&g);
          assert_eq!(l.nodes.len(), 1);
          assert!(l.edges.is_empty());
      }

      #[test]
      fn simple_chain_layers_zero_one_two() {
          let a = table("a", vec![col("id", true, None)]);
          let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
          let c = table("c", vec![col("id", true, None), col("b_id", false, Some(("b", "id")))]);
          // Note: build_graph reads FK direction from the FK-holding side, so
          // edges are b->a and c->b; roots (indegree 0 in the reversed sense
          // used for layering) come from whichever side has no incoming FK.
          let g = build_graph(&[a, b, c]);
          let l = compute_layout(&g);
          let ya = layer_of(&l, "a");
          let yb = layer_of(&l, "b");
          let yc = layer_of(&l, "c");
          assert!(ya < yb && yb < yc, "a should layer above b above c (longest-path from the root)");
      }

      #[test]
      fn self_reference_does_not_affect_other_layers_and_is_marked() {
          let e = table("employees", vec![col("id", true, None), col("manager_id", false, Some(("employees", "id")))]);
          let g = build_graph(&[e]);
          let l = compute_layout(&g);
          assert_eq!(l.nodes.len(), 1);
          assert_eq!(l.edges.len(), 1);
          assert!(l.edges[0].is_self_loop);
          assert_eq!(l.edges[0].points.len(), 3);
      }

      #[test]
      fn bidirectional_pair_terminates_and_keeps_both_edges() {
          let a = table("a", vec![col("id", true, None), col("b_id", false, Some(("b", "id")))]);
          let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
          let g = build_graph(&[a, b]);
          let l = compute_layout(&g); // must return, not hang
          assert_eq!(l.edges.len(), 2);
      }

      #[test]
      fn composite_fk_edge_survives_layout_with_two_column_pairs() {
          let orders = table("orders", vec![
              col("id", true, None),
              col("addr_a", false, Some(("addresses", "a"))),
              col("addr_b", false, Some(("addresses", "b"))),
          ]);
          let addresses = table("addresses", vec![col("a", true, None), col("b", true, None)]);
          let g = build_graph(&[orders, addresses]);
          assert_eq!(g.edges[0].columns.len(), 2);
          let l = compute_layout(&g);
          assert_eq!(l.edges.len(), 1);
      }

      #[test]
      fn diamond_layers_sink_at_longest_path_not_first_arrival() {
          // a -> b -> d, a -> c -> d: FK direction is child->parent in this
          // codebase's schema model, so build the diamond as: b,c FK to a;
          // d FKs to BOTH b and c. Longest path to d must be 2 (via b or c),
          // not 1 (if d were laid out right after a on a first-arrival BFS).
          let a = table("a", vec![col("id", true, None)]);
          let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
          let c = table("c", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
          let d = table("d", vec![
              col("id", true, None),
              col("b_id", false, Some(("b", "id"))),
              col("c_id", false, Some(("c", "id"))),
          ]);
          let g = build_graph(&[a, b, c, d]);
          let l = compute_layout(&g);
          let ya = layer_of(&l, "a");
          let yb = layer_of(&l, "b");
          let yd = layer_of(&l, "d");
          assert!(ya < yb, "a above b");
          assert!(yb < yd, "b above d — d sinks to the longest path, not the first one found");
      }

      #[test]
      fn isolated_table_is_in_a_separate_row_below_connected_layers() {
          let a = table("a", vec![col("id", true, None)]);
          let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
          let lonely = table("lonely", vec![col("id", true, None)]);
          let g = build_graph(&[a, b, lonely]);
          let l = compute_layout(&g);
          let y_lonely = layer_of(&l, "lonely");
          let y_b = layer_of(&l, "b");
          assert!(y_lonely > y_b, "isolated row must sit below every connected layer");
      }

      #[test]
      fn deterministic_same_input_twice_is_byte_identical() {
          let a = table("z_table", vec![col("id", true, None)]);
          let b = table("a_table", vec![col("id", true, None), col("z_id", false, Some(("z_table", "id")))]);
          let g = build_graph(&[a, b]);
          let l1 = compute_layout(&g);
          let l2 = compute_layout(&g);
          assert_eq!(l1, l2);
      }

      #[test]
      fn wide_node_column_row_metadata_is_preserved_through_graph_build() {
          // Sanity: layout doesn't need to inspect ErdColumnRow itself, but
          // confirms the type import above is exercised and the pipeline
          // compiles end to end with a non-trivial node.
          let t = table("t", vec![col("id", true, None)]);
          let g = build_graph(&[t]);
          let row: &ErdColumnRow = &g.nodes[0].visible_cols[0];
          assert!(row.is_pk);
      }
  }
  ```

- [ ] **Step 3: Run to green and confirm read-only.**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core erd::layout::`
  Expected: 9 tests pass, zero warnings.
  Run: `grep -n "\.execute(\|\.query(" crates/dbc-core/src/erd/layout.rs`
  Expected: no output.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/dbc-core/src/erd.rs crates/dbc-core/src/erd/layout.rs
  git commit -m "feat: erd::layout::compute_layout — iterative layered layout, cycle-safe (G8 T2)"
  ```

---

### Task 3 (T3): SVG export — `erd::svg::export_svg`

**Files:**
- Create: `crates/dbc-core/src/erd/svg.rs`
- Modify: `crates/dbc-core/src/erd.rs` (add `pub mod svg;`)

**Interfaces:**
- Consumes: T2's `DiagramLayout`/`PositionedNode`/`RoutedEdge`, T1's `TableKey`, `dbc_core::TableInfo`.
- Produces (consumed by T6's export button):
  ```rust
  /// Escapes the five XML-significant characters. The ONE function every
  /// interpolated string (table/column/schema name) MUST pass through
  /// before reaching the output string — CURATION-binding (Global
  /// Constraints).
  pub fn escape_xml(s: &str) -> String;

  /// Builds a complete, standalone SVG document from the SAME
  /// `DiagramLayout` the canvas paints from (T4/T5) — screen and export
  /// can never visually diverge. `tables` is looked up by `TableKey` to
  /// recover full column lists/types for node text; a `PositionedNode`
  /// with no matching `TableInfo` is silently skipped (defensive, should
  /// never happen given both are derived from the same snapshot).
  pub fn export_svg(layout: &DiagramLayout, tables: &[TableInfo]) -> String;
  ```

**Grounding:** Reuses the app's existing Catppuccin Mocha palette hex values verbatim (CURATION point 2 — no new color scheme), the SAME hex constants already used in `schema_tree.rs`/`grid.rs` (`0x1e1e2e` background, `0x313244` node fill, `0x45475a` node border/selection, `0xcdd6f4` text, `0x89b4fa` edge/accent, `0x6c7086` muted) — confirmed live at `crates/dbc-ui/src/grid.rs`/`crates/dbc-ui/src/schema_tree.rs` (e.g. `rgb(0x313244)` at `schema_tree.rs:1013`, `rgb(0xcdd6f4)` at `schema_tree.rs:1010`). No new dependency: SVG is hand-built XML string formatting (design §4's explicit decision, driven by `render_to_image` being platform-gated to macOS-only on this pinned GPUI rev, confirmed by reading `crates/gpui/src/platform.rs:986`'s unconditional `bail!` default and the absence of any `crates/gpui_windows` override — see the design draft §4 for the full citation trail, not re-verified here since this plan doesn't touch that code path at all).

```rust
use std::collections::HashMap;

use super::TableKey;
use super::layout::{DiagramLayout, PositionedNode, RoutedEdge};
use crate::schema::TableInfo;

const BG: &str = "#1e1e2e";
const NODE_FILL: &str = "#313244";
const NODE_BORDER: &str = "#45475a";
const TEXT_COLOR: &str = "#cdd6f4";
const EDGE_COLOR: &str = "#89b4fa";
const MUTED: &str = "#6c7086";

pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn header_text(t: &TableInfo) -> String {
    match &t.schema {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn node_to_svg(pos: &PositionedNode, t: &TableInfo) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"4\" fill=\"{NODE_FILL}\" stroke=\"{NODE_BORDER}\" stroke-width=\"1\"/>\n",
        pos.x, pos.y, pos.w, pos.h
    ));
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{TEXT_COLOR}\" font-weight=\"bold\" font-family=\"sans-serif\" font-size=\"13\">{}</text>\n",
        pos.x + 8.0,
        pos.y + 16.0,
        escape_xml(&header_text(t))
    ));
    let mut row_y = pos.y + 24.0 + 13.0;
    for c in t.columns.iter().filter(|c| c.is_pk || c.fk.is_some()).take(6) {
        let marker = if c.is_pk { "PK " } else if c.fk.is_some() { "FK " } else { "" };
        let line = format!("{marker}{}: {}", c.name, c.data_type);
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{TEXT_COLOR}\" font-family=\"sans-serif\" font-size=\"11\">{}</text>\n",
            pos.x + 8.0, row_y, escape_xml(&line)
        ));
        row_y += 18.0;
    }
    s
}

fn edge_to_svg(e: &RoutedEdge) -> String {
    if e.is_self_loop && e.points.len() == 3 {
        let (x0, y0) = e.points[0];
        let (cx, cy) = e.points[1];
        let (x1, y1) = e.points[2];
        format!("<path d=\"M {x0:.1} {y0:.1} Q {cx:.1} {cy:.1} {x1:.1} {y1:.1}\" fill=\"none\" stroke=\"{EDGE_COLOR}\" stroke-width=\"1.5\"/>\n")
    } else if e.points.len() == 2 {
        let (x0, y0) = e.points[0];
        let (x1, y1) = e.points[1];
        format!("<line x1=\"{x0:.1}\" y1=\"{y0:.1}\" x2=\"{x1:.1}\" y2=\"{y1:.1}\" stroke=\"{EDGE_COLOR}\" stroke-width=\"1.5\"/>\n")
    } else {
        String::new() // defensive: malformed RoutedEdge, never emitted by compute_layout
    }
}

pub fn export_svg(layout: &DiagramLayout, tables: &[TableInfo]) -> String {
    let by_key: HashMap<TableKey, &TableInfo> =
        tables.iter().map(|t| (TableKey { schema: t.schema.clone(), name: t.name.clone() }, t)).collect();

    let (w, h) = layout.nodes.iter().fold((800.0f32, 600.0f32), |(mw, mh), n| {
        (mw.max(n.x + n.w + 40.0), mh.max(n.y + n.h + 40.0))
    });

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.1}\" height=\"{h:.1}\" viewBox=\"0 0 {w:.1} {h:.1}\">\n"
    ));
    svg.push_str(&format!("<rect width=\"{w:.1}\" height=\"{h:.1}\" fill=\"{BG}\"/>\n"));
    let _ = MUTED; // reserved for the footer "+N dalších" row (T6 visual polish, not load-bearing on export correctness)

    for e in &layout.edges {
        svg.push_str(&edge_to_svg(e));
    }
    for n in &layout.nodes {
        if let Some(t) = by_key.get(&n.key) {
            svg.push_str(&node_to_svg(n, t));
        }
    }
    svg.push_str("</svg>\n");
    svg
}
```

- [ ] **Step 1: Write the code above** in `crates/dbc-core/src/erd/svg.rs`, and add `pub mod svg;` to `crates/dbc-core/src/erd.rs`.

- [ ] **Step 2: Tests, including the REQUIRED injection test** (same file, `#[cfg(test)] mod tests`):

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::erd::{build_graph, layout::compute_layout};
      use crate::schema::{ColumnInfo, FkRef, TableInfo};

      fn col(name: &str, pk: bool, fk: Option<(&str, &str)>) -> ColumnInfo {
          ColumnInfo {
              name: name.into(), data_type: "int4".into(), nullable: !pk, default: None, is_pk: pk,
              fk: fk.map(|(t, c)| FkRef { schema: None, table: t.into(), column: c.into() }),
          }
      }

      #[test]
      fn escape_xml_covers_all_five_characters() {
          assert_eq!(escape_xml("a<b>c&d\"e'f"), "a&lt;b&gt;c&amp;d&quot;e&apos;f");
          assert_eq!(escape_xml("plain"), "plain");
      }

      // REQUIRED test (Global Constraints, CURATION-binding): a table named
      // `we"ird<x>` — plus a column and a schema each carrying a different
      // dangerous character — must never leak an un-escaped `<`, `>`, `&`,
      // `"`, or `'` into the output outside the fixed SVG syntax this
      // serializer itself emits.
      #[test]
      fn hostile_identifiers_produce_fully_escaped_svg() {
          let evil_table = TableInfo {
              schema: Some("sch&ema".into()),
              name: "we\"ird<x>".into(),
              columns: vec![
                  col("id", true, None),
                  ColumnInfo {
                      name: "a'b".into(),
                      data_type: "text".into(),
                      nullable: false,
                      default: None,
                      is_pk: false,
                      fk: Some(FkRef { schema: None, table: "we\"ird<x>".into(), column: "id".into() }),
                  },
              ],
              ..Default::default()
          };
          let g = build_graph(&[evil_table.clone()]);
          let l = compute_layout(&g);
          let svg = export_svg(&l, &[evil_table]);

          // Escaped forms present.
          assert!(svg.contains("we&quot;ird&lt;x&gt;"), "table name must be escaped: {svg}");
          assert!(svg.contains("sch&amp;ema"), "schema name must be escaped: {svg}");
          assert!(svg.contains("a&apos;b"), "column name must be escaped: {svg}");

          // Extract only the <text>...</text> payload substrings (the one
          // place hostile identifier text is interpolated) and assert none
          // of them contain a raw dangerous character — the fixed SVG
          // syntax around them (attribute quotes, tag brackets) legitimately
          // contains '<'/'>'/'"' and must not be flagged as a false
          // positive, so this check is scoped to text-node payloads only.
          let mut idx = 0;
          let mut payloads = Vec::new();
          while let Some(start) = svg[idx..].find('>').map(|p| idx + p + 1) {
              if let Some(end) = svg[start..].find("</text>") {
                  payloads.push(&svg[start..start + end]);
                  idx = start + end;
              } else {
                  break;
              }
          }
          for p in &payloads {
              assert!(!p.contains('<') && !p.contains('>'), "text payload must never contain a raw angle bracket: {p}");
          }

          // Well-formedness sanity (no XML parser dependency added to
          // dbc-core for this — a balanced open/close <text> tag count is a
          // cheap, dependency-free proxy that would fail if escaping ever
          // let a raw '<' truncate/split a tag).
          let opens = svg.matches("<text").count();
          let closes = svg.matches("</text>").count();
          assert_eq!(opens, closes, "every <text> must be balanced — a raw '<' in payload would break this");
      }

      #[test]
      fn export_contains_expected_shape_and_coordinates() {
          let t = TableInfo { schema: None, name: "t".into(), columns: vec![col("id", true, None)], ..Default::default() };
          let g = build_graph(&[t.clone()]);
          let l = compute_layout(&g);
          let svg = export_svg(&l, &[t]);
          assert!(svg.starts_with("<svg"));
          assert!(svg.trim_end().ends_with("</svg>"));
          assert!(svg.contains("<rect"));
          assert!(svg.contains(&format!("x=\"{:.1}\"", l.nodes[0].x)));
      }

      #[test]
      fn self_loop_edge_renders_as_a_quadratic_path() {
          let t = TableInfo {
              schema: None, name: "employees".into(),
              columns: vec![col("id", true, None), col("manager_id", false, Some(("employees", "id")))],
              ..Default::default()
          };
          let g = build_graph(&[t.clone()]);
          let l = compute_layout(&g);
          let svg = export_svg(&l, &[t]);
          assert!(svg.contains("<path d=\"M"));
      }

      #[test]
      fn missing_table_for_a_positioned_node_is_skipped_not_a_panic() {
          let l = crate::erd::layout::DiagramLayout {
              nodes: vec![crate::erd::layout::PositionedNode {
                  key: crate::erd::TableKey { schema: None, name: "ghost".into() },
                  x: 0.0, y: 0.0, w: 220.0, h: 24.0,
              }],
              edges: vec![],
          };
          let svg = export_svg(&l, &[]); // no matching TableInfo — must not panic
          assert!(!svg.contains("<rect"));
      }
  }
  ```

- [ ] **Step 3: Run to green and confirm read-only.**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core erd::svg::`
  Expected: 5 tests pass, zero warnings.
  Run: `grep -n "\.execute(\|\.query(" crates/dbc-core/src/erd/svg.rs`
  Expected: no output.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/dbc-core/src/erd.rs crates/dbc-core/src/erd/svg.rs
  git commit -m "feat: erd::svg::export_svg — hand-built SVG serializer, mandatory XML escaping (G8 T3)"
  ```

---

### Task 4 (T4): Canvas rendering — spike, then `ErDiagramView` static paint

**Files:**
- Create: `crates/dbc-ui/src/er_diagram_view.rs`
- Modify: `crates/dbc-ui/src/main.rs` (`mod er_diagram_view;` added to the mod list, alphabetically after `mod connect;`)
- Temporary, NOT committed: `crates/dbc-ui/examples/erd_canvas_spike.rs` (Step 1 only — deleted before Step 3's commit)

**Interfaces:**
- Consumes: T2's `DiagramLayout`, T1's `TableKey`.
- Produces (consumed by T5, T6, T7):
  ```rust
  pub struct ErDiagramView {
      layout: dbc_core::erd::layout::DiagramLayout,
      tables: Vec<dbc_core::TableInfo>,
      schema_label: String,
      pan: gpui::Point<f32>,
      zoom: f32,
      // T5/T7 fields (selected, hit_boxes, drag_state, truncated_notice)
      // are added in those tasks — T4 ships the static, un-interactive
      // paint only, per this plan's task-ordering brief.
  }

  impl ErDiagramView {
      pub fn new(layout: DiagramLayout, tables: Vec<TableInfo>, schema_label: String) -> Self;
  }
  ```

**Grounding — every primitive, confirmed at the pinned rev:**
- `gpui::canvas(prepaint, paint) -> Canvas<T>` — `crates/gpui/src/elements/canvas.rs:10-19`; `Canvas<T>` is `Element`/`IntoElement` (same file, `:29-46`), so it drops straight into a normal `div().child(canvas(...))` render tree.
- `Window::paint_quad(&mut self, quad: PaintQuad)` — `crates/gpui/src/window.rs:4103-4118`. `PaintQuad` (`window.rs:6783-6796`) has a builder-style `.corner_radii(...)`/`.border_widths(...)` (`window.rs:6800-6813`) — the SAME builder pattern `grid.rs` already uses for cell backgrounds.
- `Window::paint_path(&mut self, path: Path<Pixels>, color: impl Into<Background>)` — `crates/gpui/src/window.rs:4174-4186`.
- `PathBuilder::stroke(width: Pixels) -> Self`, `.move_to(Point<Pixels>)`, `.line_to(Point<Pixels>)`, `.curve_to(to: Point<Pixels>, ctrl: Point<Pixels>)` (quadratic bezier), `.build(self) -> Result<Path<Pixels>, Error>` — all confirmed at `crates/gpui/src/path_builder.rs:86-139,244-250`.
- `Window::text_system().shape_line(text, font_size, &runs, None)` then `.paint(...)` — confirmed live call site at `crates/dbc-ui/src/sql_input.rs:1079-1081`, reused verbatim for node headers/column rows.
- **What is NOT yet confirmed at runtime on this project's Windows target:** `canvas()`'s custom-drawing path and `PathBuilder::curve_to` specifically — every other primitive above already has a live call site in this codebase; these two do not (design §6's first risk). This is exactly what Step 1's spike retires before any of the real `ErDiagramView` code is written.

- [ ] **Step 1: 30-minute spike — paint one rounded quad + one bezier inside `canvas()`, on Windows, before writing any real code.**

  Create `crates/dbc-ui/examples/erd_canvas_spike.rs` (a self-contained GPUI example — no dependency on any `dbc-ui` internal module, so it needs no `lib.rs`; Cargo supports `examples/*.rs` against a binary-only crate the same way `dbc-driver-postgres`'s integration tests already prove the workspace's Cargo setup handles auxiliary targets):

  ```rust
  //! G8 T4 spike (design CURATION point 3): confirms canvas()/paint_quad/
  //! PathBuilder::curve_to actually rasterize correctly on this project's
  //! Windows target, not just that they compile. Throwaway — run once,
  //! confirm visually, delete. NOT part of any commit in this plan.

  use gpui::{
      canvas, div, point, prelude::*, px, rgb, size, App, Bounds, PathBuilder, Pixels, Window,
      WindowBounds, WindowOptions,
  };

  fn spike_paint(bounds: Bounds<Pixels>, _state: (), window: &mut Window, _app: &mut App) {
      window.paint_quad(
          gpui::fill(
              Bounds::new(bounds.origin + point(px(20.), px(20.)), size(px(160.), px(60.))),
              rgb(0x313244),
          )
          .corner_radii(px(6.))
          .border_widths(px(1.))
          .border_color(rgb(0x89b4fa)),
      );
      let mut builder = PathBuilder::stroke(px(2.));
      builder.move_to(bounds.origin + point(px(20.), px(140.)));
      builder.curve_to(bounds.origin + point(px(180.), px(140.)), bounds.origin + point(px(100.), px(80.)));
      if let Ok(path) = builder.build() {
          window.paint_path(path, rgb(0xf5c2e7));
      }
  }

  fn main() {
      gpui::Application::new().run(|cx: &mut App| {
          let bounds = Bounds::centered(None, size(px(400.), px(300.)), cx);
          cx.open_window(
              WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
              |_window, cx| cx.new(|_cx| SpikeRoot),
          )
          .unwrap();
      });
  }

  struct SpikeRoot;
  impl gpui::Render for SpikeRoot {
      fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
          div().size_full().bg(rgb(0x1e1e2e)).child(canvas(|bounds, _w, _cx| bounds, spike_paint).size_full())
      }
  }
  ```

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui --example erd_canvas_spike`
  **Expected (Windows, visual confirmation required — GPUI render paths aren't unit-tested anywhere in this codebase, same established precedent as every other GPUI-heavy task in this repo's plans):** a window opens showing a rounded blue-bordered box and a visibly curved pink line (NOT a straight line — a straight line would mean `curve_to` silently degraded or the control point math is backwards).

  - **If confirmed:** delete `crates/dbc-ui/examples/erd_canvas_spike.rs`, proceed to Step 2. Nothing from this step is committed.
  - **If it fails to compile, panics, or renders a flat/incorrect line (fallback path, per design CURATION point 3):** do NOT proceed to Step 2 as written. Instead: (a) node boxes stay identical (plain `div()` with `.rounded_md()`/`.border_1()`/`.bg(...)` — already proven risk-free by every popup in `grid.rs`, no `canvas()`/`paint_quad` needed for boxes at all, only for edges); (b) edges degrade from a true diagonal line to an axis-aligned two-segment "Manhattan" polyline built from two thin, absolutely-positioned `div()`s per edge (`.absolute().left(px(x)).top(px(y)).w(px(w)).h(px(1.))` for the horizontal run, `.h(px(h)).w(px(1.))` for the vertical run — the exact `.absolute()` positioning idiom already used eight times over in `grid.rs`'s popups, e.g. `grid.rs:1718`), routed from each node's border point straight across then straight down/up to the target's border point — no rotation, no `PathBuilder`, no `canvas()` at all; (c) self-loop stubs degrade to a small three-sided `div()` bracket (`┐`-shaped, built from the same two-segment primitive) instead of a bezier curve. Record which path was taken in this task's commit message and in T5/T6's Grounding sections (both reference `RoutedEdge.points`, which is renderer-agnostic either way — the fallback changes ONLY how T4/T5 interpret those points, not T1-T3's types).

- [ ] **Step 2: `ErDiagramView` — static paint of a `DiagramLayout` (no interaction yet — T5).**

  `crates/dbc-ui/src/er_diagram_view.rs`:
  ```rust
  use gpui::{canvas, div, point, prelude::*, px, rgb, size, Bounds, Context, Entity, PathBuilder, Pixels, Point, Window};

  use dbc_core::erd::layout::{DiagramLayout, PositionedNode, RoutedEdge};
  use dbc_core::TableInfo;

  const NODE_FILL: u32 = 0x313244;
  const NODE_BORDER: u32 = 0x45475a;
  const TEXT_COLOR: u32 = 0xcdd6f4;
  const EDGE_COLOR: u32 = 0x89b4fa;

  pub struct ErDiagramView {
      pub(crate) layout: DiagramLayout,
      pub(crate) tables: Vec<TableInfo>,
      pub(crate) schema_label: String,
      pub(crate) pan: Point<f32>,
      pub(crate) zoom: f32,
  }

  impl ErDiagramView {
      pub fn new(layout: DiagramLayout, tables: Vec<TableInfo>, schema_label: String) -> Self {
          Self { layout, tables, schema_label, pan: point(0.0, 0.0), zoom: 1.0 }
      }

      /// World-space (x, y) -> screen-space Pixels, given the current pan/
      /// zoom and the canvas element's own screen-space origin. Shared by
      /// paint (T4) and hit-testing (T5) so the two can never drift apart.
      fn world_to_screen(&self, origin: Point<Pixels>, world: (f32, f32)) -> Point<Pixels> {
          point(
              origin.x + px((world.0 + self.pan.x) * self.zoom),
              origin.y + px((world.1 + self.pan.y) * self.zoom),
          )
      }
  }

  impl Render for ErDiagramView {
      fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
          let layout = self.layout.clone();
          let tables = self.tables.clone();
          let pan = self.pan;
          let zoom = self.zoom;

          div().id("er-diagram-root").size_full().bg(rgb(0x1e1e2e)).child(
              canvas(
                  move |bounds, _window, _app| bounds,
                  move |bounds, canvas_bounds, window, _app| {
                      paint_diagram(canvas_bounds, &layout, &tables, pan, zoom, window);
                  },
              )
              .size_full(),
          )
      }
  }

  fn to_screen(origin: Point<Pixels>, world: (f32, f32), pan: Point<f32>, zoom: f32) -> Point<Pixels> {
      point(origin.x + px((world.0 + pan.x) * zoom), origin.y + px((world.1 + pan.y) * zoom))
  }

  fn paint_diagram(
      bounds: Bounds<Pixels>,
      layout: &DiagramLayout,
      tables: &[TableInfo],
      pan: Point<f32>,
      zoom: f32,
      window: &mut Window,
  ) {
      for e in &layout.edges {
          paint_edge(bounds.origin, e, pan, zoom, window);
      }
      for n in &layout.nodes {
          if let Some(t) = tables.iter().find(|t| t.schema == n.key.schema && t.name == n.key.name) {
              paint_node(bounds.origin, n, t, pan, zoom, window);
          }
      }
  }

  fn paint_node(origin: Point<Pixels>, n: &PositionedNode, t: &TableInfo, pan: Point<f32>, zoom: f32, window: &mut Window) {
      let top_left = to_screen(origin, (n.x, n.y), pan, zoom);
      let sz = size(px(n.w * zoom), px(n.h * zoom));
      window.paint_quad(
          gpui::fill(Bounds::new(top_left, sz), rgb(NODE_FILL))
              .corner_radii(px(4.))
              .border_widths(px(1.))
              .border_color(rgb(NODE_BORDER)),
      );
      let header = match &t.schema {
          Some(s) => format!("{s}.{}", t.name),
          None => t.name.clone(),
      };
      let font_size = window.text_style().font_size.to_pixels(window.rem_size()) * zoom;
      let run = gpui::TextRun {
          len: header.len(),
          font: window.text_style().font(),
          color: rgb(TEXT_COLOR).into(),
          background_color: None,
          underline: None,
          strikethrough: None,
      };
      if let Ok(shaped) = window.text_system().shape_line(header.into(), font_size, &[run], None) {
          let _ = shaped.paint(top_left + point(px(8. * zoom), px(4. * zoom)), font_size, gpui::TextAlign::Left, None, window, &mut gpui::App::default());
      }
  }

  fn paint_edge(origin: Point<Pixels>, e: &RoutedEdge, pan: Point<f32>, zoom: f32, window: &mut Window) {
      if e.points.len() < 2 {
          return;
      }
      let mut builder = PathBuilder::stroke(px(1.5));
      builder.move_to(to_screen(origin, e.points[0], pan, zoom));
      if e.is_self_loop && e.points.len() == 3 {
          builder.curve_to(to_screen(origin, e.points[2], pan, zoom), to_screen(origin, e.points[1], pan, zoom));
      } else {
          builder.line_to(to_screen(origin, e.points[1], pan, zoom));
      }
      if let Ok(path) = builder.build() {
          window.paint_path(path, rgb(EDGE_COLOR));
      }
  }
  ```

  **Implementer note on `shape_line`/`ShapedLine::paint`'s exact argument list:** `sql_input.rs:1079-1081`'s live call site is the ground truth for the precise signature (this plan's sketch above may drift by an argument or two against the exact pinned-rev signature — copy `sql_input.rs`'s call verbatim and adapt, rather than trusting this snippet's argument order byte-for-byte; this is exactly the kind of small signature drift `cargo build`'s first error will catch immediately). Column-row text (PK/FK rows below the header, `visible_cols`/`hidden_col_count`) follows the identical `shape_line` pattern one row at a time, `ROW_H * zoom` apart — omitted above for length, written in full during implementation from the same primitive.

- [ ] **Step 3: `mod er_diagram_view;` in `main.rs`, build, manual visual smoke test.**

  `crates/dbc-ui/src/main.rs` — add `mod er_diagram_view;` to the mod list (alphabetically after `mod connect;`), with `#[allow(dead_code)]` on the line (nothing constructs `ErDiagramView` yet — T6 is the real consumer, same "allow, then remove at the wiring task" precedent `docs/superpowers/plans/2026-08-23-g13-execution-plans.md` T1 uses for `mod plan;`).

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
  Expected: builds clean (the `#[allow(dead_code)]` covers the not-yet-consumed `pub` items).
  Manual: temporarily construct an `ErDiagramView` with a hand-built `DiagramLayout` (2-3 nodes, one edge) from a throwaway debug binding (same disposable-spike posture as Step 1, not committed) and confirm on screen: boxes render with visible borders, the header text is legible, the edge line connects two boxes' borders (not their centers — confirms `clip_to_rect_edge` from T2 is being fed correctly).

- [ ] **Step 4: Confirm read-only, commit.**

  Run: `grep -n "\.execute(\|\.query(" crates/dbc-ui/src/er_diagram_view.rs`
  Expected: no output.

  ```bash
  git add crates/dbc-ui/src/er_diagram_view.rs crates/dbc-ui/src/main.rs
  git commit -m "feat: ErDiagramView static canvas paint — quads, straight/self-loop edges, spike-verified (G8 T4)"
  ```

---

### Task 5 (T5): Interaction — pan, zoom, hit-test, click-to-DDL, selection

**Files:**
- Modify: `crates/dbc-ui/src/er_diagram_view.rs`

**Interfaces:**
- Produces (consumed by T6):
  ```rust
  pub struct ErDiagramView {
      // ...T4's fields, plus:
      selected: Option<dbc_core::erd::TableKey>,
      hit_boxes: Vec<(dbc_core::erd::TableKey, gpui::Bounds<gpui::Pixels>)>,
      drag_state: Option<(gpui::Point<gpui::Pixels>, gpui::Point<f32>)>, // (start mouse pos, start pan)
  }

  /// Reuses `schema_tree::TreeEvent` verbatim (design §3: "reuses
  /// TreeEvent::OpenDdl's existing TabContent::Text path... zero new
  /// tab-content plumbing") — only the `OpenDdl` variant is ever emitted.
  impl gpui::EventEmitter<crate::schema_tree::TreeEvent> for ErDiagramView {}
  ```

**Grounding:**
- **Pan (drag):** the SAME `(start_mouse_pos, start_pan_offset)`-captured-on-mouse-down, updated-on-mouse-move, committed-on-mouse-up shape `grid.rs` already uses for column-resize drag (`grid.rs:2225` mouse-down capture, `grid.rs:2861` mouse-move update — cited in the design draft §3, confirmed present in this codebase at those approximate locations; re-locate by symbol if drifted).
- **Zoom (scroll):** the exact `ScrollWheelEvent`/`ScrollDelta::{Lines,Pixels}` extraction idiom already live at `grid.rs:1840-1844` (cell-detail popup scroll) and `main.rs:3112-3116` (DDL text-tab scroll) — `delta_lines = match e.delta { ScrollDelta::Lines(p) => p.y, ScrollDelta::Pixels(p) => p.y.as_f32() / 20.0 }`.
- **Anchored zoom math** (keep the point under the cursor fixed): given screen-space `mouse_local = mouse_pos - canvas_origin`, and the invariant `screen = (world + pan) * zoom`, the world point under the cursor is `world_under = mouse_local / zoom - pan`. Solving for the new pan after a zoom change: `pan' = mouse_local / new_zoom - world_under`. Standalone, unit-testable pure function (no GPUI types needed for the test — plain `f32` tuples):
  ```rust
  pub fn zoom_at(pan: (f32, f32), zoom: f32, mouse_local: (f32, f32), factor: f32) -> ((f32, f32), f32) {
      let new_zoom = (zoom * factor).clamp(0.2, 3.0);
      let world_under = (mouse_local.0 / zoom - pan.0, mouse_local.1 / zoom - pan.1);
      let new_pan = (mouse_local.0 / new_zoom - world_under.0, mouse_local.1 / new_zoom - world_under.1);
      (new_pan, new_zoom)
  }
  ```
  **Unit tests (no GPUI needed — this is the one piece of T5's math that doesn't require a live window):**
  ```rust
  #[cfg(test)]
  mod zoom_math_tests {
      use super::*;

      #[test]
      fn zoom_in_keeps_cursor_world_point_fixed() {
          let (new_pan, new_zoom) = zoom_at((0.0, 0.0), 1.0, (100.0, 50.0), 1.1);
          assert!((new_zoom - 1.1).abs() < 1e-6);
          // Re-derive the world point under the cursor at the NEW pan/zoom
          // and confirm it matches what it was before the zoom.
          let world_before = (100.0 / 1.0 - 0.0, 50.0 / 1.0 - 0.0);
          let world_after = (100.0 / new_zoom - new_pan.0, 50.0 / new_zoom - new_pan.1);
          assert!((world_before.0 - world_after.0).abs() < 1e-4);
          assert!((world_before.1 - world_after.1).abs() < 1e-4);
      }

      #[test]
      fn zoom_clamps_to_bounds() {
          let (_, z_min) = zoom_at((0.0, 0.0), 0.21, (0.0, 0.0), 0.5);
          assert!((z_min - 0.2).abs() < 1e-6);
          let (_, z_max) = zoom_at((0.0, 0.0), 2.9, (0.0, 0.0), 2.0);
          assert!((z_max - 3.0).abs() < 1e-6);
      }
  }
  ```
- **Hit-testing:** decision (design §3) is a plain `Vec<(TableKey, Bounds<Pixels>)>` recomputed in `canvas()`'s `prepaint` closure (screen-space box bounds after applying current pan/zoom) and tested against in an `on_mouse_down` handler on the canvas's wrapping `div` — the same "occlude + explicit hit-test list" shape the grid's context-menu dismiss handling already uses (`grid.rs:1732`/`:1783`/`:2329`'s `on_mouse_down_out` sites), NOT GPUI's per-element hit-test machinery (the canvas paints raw primitives with no per-node element identity for GPUI itself to hit-test). A point-in-rect test is trivial arithmetic (`bounds.contains(&point)`, standard GPUI `Bounds` method already used throughout `grid.rs`).
- **Click-to-DDL:** on a hit, `t.ddl.clone().unwrap_or_else(|| dbc_core::synthesize_create_table(t))` then `cx.emit(TreeEvent::OpenDdl { title: t.name.clone(), ddl })` — the EXACT pattern at `schema_tree.rs:779-780`, reused verbatim (design §3's "zero new tab-content plumbing" — `main.rs`'s existing `on_tree_event`'s `OpenDdl` arm, `main.rs:2944-2957`, already does the right thing for any emitter, it just needs a second `cx.subscribe` call wired in T6).
- **Selection highlight:** `selected: Option<TableKey>` set on a node hit, cleared on an empty-canvas click; `paint_node`/`paint_edge` (T4) both grow a `selected` parameter — selected node's border color becomes the accent (`0xf5c2e7` or similar, matching the app's existing selection-accent convention), and every `RoutedEdge` touching the selected key is repainted a second time, last, in the accent color (z-order: paint all edges once at normal color, then re-paint just the highlighted subset — cheap, no new state beyond `selected` itself).

- [ ] **Step 1: Add pan/zoom/hit-test/selection state and wire the mouse/scroll handlers** into `ErDiagramView::render`'s `div()` (the canvas's wrapping element, matching `.on_mouse_down`/`.on_mouse_move`/`.on_mouse_up`/`.on_scroll_wheel` call sites' existing shape in `grid.rs`) — `prepaint` computes and stores `hit_boxes` on `self` via the closure's captured `Entity` handle (same pattern `ResultGrid`'s own `prepaint` state caching already uses), `on_mouse_down` walks `hit_boxes` in reverse-paint order (topmost node wins on overlap, though the layout algorithm guarantees no overlap by construction) and either emits `OpenDdl` or sets `selected = None` on a miss, `on_mouse_move`/`on_mouse_up` implement the drag-to-pan capture-shape above, `on_scroll_wheel` calls `zoom_at`.

- [ ] **Step 2: `zoom_at` unit tests** (above) — pure function, no GPUI, lives at the top of `er_diagram_view.rs` outside any `impl` block so it's trivially testable.

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui er_diagram_view::zoom_math_tests::`
  Expected: 2 tests pass, zero warnings.

- [ ] **Step 3: Manual smoke test** (GPUI render/interaction isn't unit-tested anywhere in this codebase — established precedent, restated per task): open a diagram with 3+ connected tables via the same throwaway debug construction T4 Step 3 used; confirm scroll-wheel zooms anchored at the cursor (a table under the cursor stays under the cursor as you zoom), drag pans smoothly, clicking a node opens its DDL tab (title `"DDL: {name}"`, exact text from `synthesize_create_table` or the table's own `ddl`), clicking empty canvas clears any existing selection border/edge highlight.

- [ ] **Step 4: Confirm read-only, commit.**

  Run: `grep -n "\.execute(\|\.query(" crates/dbc-ui/src/er_diagram_view.rs`
  Expected: no output.

  ```bash
  git add crates/dbc-ui/src/er_diagram_view.rs
  git commit -m "feat: ErDiagramView interaction — pan, anchored zoom, hit-test click-to-DDL, selection highlight (G8 T5)"
  ```

---

### Task 6 (T6): Tab/entry wiring — palette action, schema-tree icon, `TabContent::Diagram`, export button

**Files:**
- Modify: `crates/dbc-ui/src/tabs.rs` (`TabContent::Diagram` variant)
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::ShowErDiagram`)
- Modify: `crates/dbc-ui/src/schema_tree.rs` (icon-button affordance on `NodeId::Schema(_)` rows, new `TreeEvent` variant)
- Modify: `crates/dbc-ui/src/main.rs` (three exhaustive `TabContent` match sites, palette dispatch arm, tree-event subscription, "Export…" button dispatch)
- Modify: `crates/dbc-ui/Cargo.toml` (version bump to `0.8.0`)

**Interfaces:**
- Consumes: T3's `erd::svg::export_svg`, T5's `ErDiagramView`.
- Produces:
  ```rust
  // tabs.rs
  pub enum TabContent {
      Grid { .. },
      Text { .. },
      Diagram { view: Entity<crate::er_diagram_view::ErDiagramView> },
  }

  // palette.rs
  pub enum PaletteAction {
      RunQuery, ToggleTree, ToggleHistory, NewConnection, RefreshSchema,
      ShowErDiagram,
  }

  // schema_tree.rs
  pub enum TreeEvent {
      OpenPreview { .. }, OpenDdl { .. }, RefreshRequested, ToggleFavourite(..),
      /// Emitted by the new per-schema-row icon button (design §3's
      /// "schema-tree context action on a schema node").
      OpenErDiagram { schema: Option<String> },
  }
  ```

**Grounding:**
- **`TabContent`'s three exhaustive match sites needing a `Diagram` arm** (confirmed by grepping `main.rs` for every `match ... .content`/`match &t.content` on this branch — re-locate by symbol at merge time per Global Constraints, since G9/G10/G12/G13 may have already added their own arms):
  1. `crates/dbc-ui/src/main.rs:1297` (`on_stream_finished` dispatch after a query lands) — add `TabContent::Diagram { .. } => None,` alongside the existing `Grid`/`Text` arms.
  2. `crates/dbc-ui/src/main.rs:2999` (`render_tab_strip`'s row-count/dirty tuple) — add `TabContent::Diagram { .. } => (0, false),`.
  3. `crates/dbc-ui/src/main.rs:3086` (`render_tab_content`) — add `TabContent::Diagram { view } => view.clone().into_any_element(),` (the exact pattern `TabContent::Grid { grid, .. } => grid.clone().into_any_element()` already uses one arm above it).
- **No schema-tree context-menu component exists in this codebase today** — confirmed by grepping `schema_tree.rs` for `ContextMenu`/right-click handling (none found). The design's "schema-tree context action on a schema node" is therefore implemented the same way the existing ★/☆ favourite toggle already is: a small icon-button `div()` child added to a tree row, right-aligned, `.on_click` emitting a `TreeEvent` — the EXACT live pattern at `schema_tree.rs:986-1000` (the `star` closure, built inside `uniform_list`'s row-rendering `cx.processor`). This plan adds a second such icon, gated to `NodeId::Schema(_)` rows only (unlike the star, which is gated to `favourite_object_for(&id).is_some()`), glyph `"⊞"` or similar, `.on_click` emitting `TreeEvent::OpenErDiagram { schema: Some(schema_name.clone()) }` with `cx.stop_propagation()` first (same as the star's click handler) so it doesn't also toggle the row's expand/collapse.
- **Palette action resolves its target schema, since `PaletteAction` variants are all zero-argument** (confirmed: `RunQuery`/`ToggleTree`/etc. all take no payload, `palette.rs:98-104`) — `ShowErDiagram`'s dispatch in `main.rs` resolves the schema by: (a) if the current `SchemaSnapshot` has exactly one distinct schema across its tables, use it directly (`None` counts as one distinct value too — a schema-less SQLite snapshot works with `schema: None`); (b) else, if the tree's currently-selected `NodeId` resolves to a `Schema`/`Table`/etc. with a known schema, use that; (c) else, set `self.status = "Vyberte schéma ve stromu (klikněte na ikonu vedle schématu)".to_string()` and do nothing further. This is a plan-level decision (not stated explicitly in the design draft, which only sketches the two ENTRY points, not the palette's schema-resolution logic) — flagged in Self-Review.
- **Export button:** `ErDiagramView` grows a `start_export_svg` method mirroring `grid.rs::start_export`'s shape (`grid.rs:1313-1331` cited above) but simpler (no chunking — an SVG string for ≤150 nodes is small, no `LARGE_EXPORT_ROWS` background-executor split needed): resolve `suggested_name = format!("{schema_label}.svg")`, call `cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name))`, on `Ok(Ok(Some(path)))` call `dbc_core::erd::svg::export_svg(&self.layout, &self.tables)` and `std::fs::write(path, svg)`, surface success/cancel/error via a `status_note: Option<String>` field on `ErDiagramView` (same "status_note, taken once by `render_tab_content`" convention `ResultGrid` already establishes, `main.rs:3093-3095`).
- **Entry-point tab title/content:** `"ER: {schema}"` (design §3), `TabContent::Diagram { view }` where `view = cx.new(|_cx| ErDiagramView::new(compute_layout(&build_graph(&scoped_tables)), scoped_tables, schema_label))`; `scoped_tables` is `snapshot.tables.iter().filter(|t| t.schema.as_deref() == target_schema.as_deref()).cloned().collect()` — reusing whichever `SchemaSnapshot` the tree already holds, zero new fetch (Global Constraints).

- [ ] **Step 1: `tabs.rs` — add the `Diagram` variant.**
  ```rust
  pub enum TabContent {
      Grid { grid: Entity<ResultGrid>, buffer: Rc<RefCell<ResultBuffer>> },
      #[allow(dead_code)]
      Text { text: String, scroll_lines: usize },
      Diagram { view: Entity<crate::er_diagram_view::ErDiagramView> },
  }
  ```
  (Remove the `#[allow(dead_code)]` from `Text` only if this task happens to be the first real Text-tab consumer on this branch lineage — otherwise leave it exactly as found; `Diagram` needs no `#[allow(dead_code)]` since this task is its own first consumer.)

- [ ] **Step 2: `palette.rs` — add `ShowErDiagram`.**
  ```rust
  pub enum PaletteAction {
      RunQuery,
      ToggleTree,
      ToggleHistory,
      NewConnection,
      RefreshSchema,
      ShowErDiagram,
  }
  ```
  And in the static action-list builder (`palette.rs:137-139`'s literal tuple list), append:
  ```rust
  ("ER diagram".to_string(), PaletteAction::ShowErDiagram),
  ```

- [ ] **Step 3: `schema_tree.rs` — `TreeEvent::OpenErDiagram` + the per-schema-row icon.**
  ```rust
  pub enum TreeEvent {
      OpenPreview { schema: Option<String>, table: String },
      OpenDdl { title: String, ddl: String },
      RefreshRequested,
      ToggleFavourite(FavouriteObject),
      OpenErDiagram { schema: Option<String> },
  }
  ```
  In the row-rendering `cx.processor` (`schema_tree.rs`, alongside the existing `star` closure at `:986-1000`):
  ```rust
  let diagram_icon = matches!(&id, NodeId::Schema(_)).then(|| {
      let schema_for_click = match &id {
          NodeId::Schema(s) => Some(s.clone()),
          _ => unreachable!(),
      };
      div()
          .id(("tree-erd", ix))
          .px_1()
          .flex_shrink_0()
          .cursor_pointer()
          .text_color(rgb(0x89b4fa))
          .child("⊞")
          .on_click(cx.listener(move |_this, _: &ClickEvent, _window, cx| {
              cx.stop_propagation();
              cx.emit(TreeEvent::OpenErDiagram { schema: schema_for_click.clone() });
          }))
  });
  ```
  Add `.children(diagram_icon)` to the row's child chain (same `Option<Div>` -> `.children(...)` idiom the `star` uses at whatever its own attach-point is in the existing row-building chain).

- [ ] **Step 4: `main.rs` — the three `TabContent` match arms** (Grounding above), plus:
  ```rust
  // on_tree_event's match, new arm alongside OpenPreview/OpenDdl/etc.
  TreeEvent::OpenErDiagram { schema } => {
      self.open_er_diagram(schema.clone(), cx);
  }

  // palette dispatch's match, new arm alongside RunQuery/ToggleTree/etc.
  PaletteAction::ShowErDiagram => {
      let target = self.resolve_er_diagram_schema(cx);
      match target {
          Some(schema) => self.open_er_diagram(schema, cx),
          None => {
              self.status = "Vyberte schéma ve stromu (klikněte na ikonu vedle schématu)".to_string();
              cx.notify();
          }
      }
  }
  ```
  New `AppView` methods:
  ```rust
  /// design §3 CURATION: the entry action always operates on ONE schema.
  /// `schema` is `None` for an engine/snapshot with no schema concept
  /// (SQLite) — matches every other `Option<String>` schema field in this
  /// codebase (strict, no "public" guessing, same posture G7's schema-diff
  /// plan established for its own `None`-schema handling).
  fn open_er_diagram(&mut self, schema: Option<String>, cx: &mut Context<Self>) {
      let Some(snapshot) = self.tree.read(cx).snapshot() else {
          self.status = "Nejprve načtěte schéma".to_string();
          cx.notify();
          return;
      };
      let mut scoped: Vec<TableInfo> =
          snapshot.tables.iter().filter(|t| t.schema == schema).cloned().collect();
      let (truncated_notice, hidden_count) = if scoped.len() > er_diagram_view::DIAGRAM_TABLE_CAP {
          scoped.sort_by(|a, b| a.name.cmp(&b.name));
          let hidden = scoped.len() - er_diagram_view::DIAGRAM_TABLE_CAP;
          scoped.truncate(er_diagram_view::DIAGRAM_TABLE_CAP);
          (
              Some(format!(
                  "Schéma má {} tabulek — zobrazeno prvních {} podle názvu; použijte filtr.",
                  hidden + er_diagram_view::DIAGRAM_TABLE_CAP,
                  er_diagram_view::DIAGRAM_TABLE_CAP
              )),
              hidden,
          )
      } else {
          (None, 0)
      };
      let _ = hidden_count; // folded into the notice text above; named for clarity at the call site
      let label = schema.clone().unwrap_or_else(|| "(bez schématu)".to_string());
      let graph = dbc_core::erd::build_graph(&scoped);
      let layout = dbc_core::erd::layout::compute_layout(&graph);
      let view = cx.new(|_cx| {
          let mut v = er_diagram_view::ErDiagramView::new(layout, scoped, label.clone());
          v.truncated_notice = truncated_notice;
          v
      });
      cx.subscribe(&view, Self::on_er_diagram_event).detach();
      self.tabs.open(ResultTab {
          id: 0,
          title: format!("ER: {label}"),
          pinned: false,
          preview_key: None,
          conn_identity: self.current_conn_identity(),
          content: TabContent::Diagram { view },
      });
      self.status = "ER diagram otevřen".to_string();
      cx.notify();
  }

  /// T5's `ErDiagramView` reuses `TreeEvent` verbatim (only ever emits
  /// `OpenDdl`) — this handler mirrors `on_tree_event`'s `OpenDdl` arm
  /// exactly rather than duplicating tab-open logic a third time.
  fn on_er_diagram_event(&mut self, _emitter: Entity<er_diagram_view::ErDiagramView>, event: &TreeEvent, cx: &mut Context<Self>) {
      if let TreeEvent::OpenDdl { title, ddl } = event {
          self.tabs.open(ResultTab {
              id: 0,
              title: format!("DDL: {title}"),
              pinned: false,
              preview_key: None,
              conn_identity: self.current_conn_identity(),
              content: TabContent::Text { text: ddl.clone(), scroll_lines: 0 },
          });
          self.status = format!("DDL otevřeno: {title}");
          cx.notify();
      }
  }

  /// PaletteAction::ShowErDiagram's zero-argument -> one-schema resolution
  /// (Grounding above): exactly one distinct schema in the snapshot wins
  /// outright; otherwise fall back to whatever the tree's current selection
  /// implies; otherwise None (caller shows the Czech refusal status text).
  fn resolve_er_diagram_schema(&self, cx: &Context<Self>) -> Option<Option<String>> {
      let snapshot = self.tree.read(cx).snapshot()?;
      let mut schemas: Vec<Option<String>> = snapshot.tables.iter().map(|t| t.schema.clone()).collect();
      schemas.sort();
      schemas.dedup();
      if schemas.len() == 1 {
          return schemas.into_iter().next();
      }
      // Multiple schemas and nothing selected in the tree that pins one down
      // -> ambiguous, caller refuses (design's entry point is schema-tree-
      // icon-first; the palette path is a secondary convenience that only
      // resolves unambiguous cases).
      None
  }
  ```

- [ ] **Step 5: `ErDiagramView`'s "Export…" button** (`er_diagram_view.rs`, this task's file):
  ```rust
  impl ErDiagramView {
      fn start_export_svg(&mut self, cx: &mut Context<Self>) {
          let suggested_name = format!("{}.svg", self.schema_label);
          self.status_note = Some("volím cíl exportu…".to_string());
          cx.notify();
          let dialog = cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name));
          let layout = self.layout.clone();
          let tables = self.tables.clone();
          cx.spawn(async move |this, cx| {
              let path = match dialog.await {
                  Ok(Ok(Some(p))) => p,
                  Ok(Ok(None)) => {
                      let _ = this.update(cx, |v, cx| { v.status_note = Some("export zrušen".to_string()); cx.notify(); });
                      return;
                  }
                  _ => {
                      let _ = this.update(cx, |v, cx| { v.status_note = Some("error: export dialog selhal".to_string()); cx.notify(); });
                      return;
                  }
              };
              let svg = dbc_core::erd::svg::export_svg(&layout, &tables);
              let result = std::fs::write(&path, svg);
              let _ = this.update(cx, |v, cx| {
                  v.status_note = Some(match result {
                      Ok(()) => format!("exportováno: {}", path.display()),
                      Err(e) => format!("error: {e}"),
                  });
                  cx.notify();
              });
          })
          .detach();
      }
  }
  ```
  Wire an "Export…" `div()` button into `ErDiagramView::render`'s top bar (a thin header row above the `canvas()`, showing `schema_label`, the truncated-notice banner if `Some`, and this button — `.on_click(cx.listener(|v, _, _, cx| v.start_export_svg(cx)))`).

- [ ] **Step 6: Version bump.** `crates/dbc-ui/Cargo.toml`: `version = "0.8.0"`.

- [ ] **Step 7: Build, run the full suite, manual smoke test, confirm read-only.**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
  Expected: zero warnings (remove `#[allow(dead_code)]` on `mod er_diagram_view;` now that T6 is a real consumer).
  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-ui`
  Expected: all pass.
  Run: `grep -rn "\.execute(\|\.query(" crates/dbc-ui/src/er_diagram_view.rs`
  Expected: no output.
  Manual (per `/run` skill): open a connection with a real multi-table schema, click the "⊞" icon next to a schema in the tree, confirm the ER tab opens titled `"ER: {schema}"` with legible boxes/edges; click a node, confirm its DDL tab opens; click "Export…", save, open the resulting `.svg` in a browser and confirm it visually matches the on-screen diagram; run the palette action "ER diagram" against a single-schema connection and confirm it opens the same tab without any picker.

- [ ] **Step 8: Commit**

  ```bash
  git add crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/palette.rs crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/main.rs crates/dbc-ui/src/er_diagram_view.rs crates/dbc-ui/Cargo.toml
  git commit -m "feat: ER diagram tab wiring — palette action, schema-tree entry, export button (G8 T6, v0.8.0)"
  ```

---

### Task 7 (T7): Large-schema cap — `DIAGRAM_TABLE_CAP` + notice banner

**Files:**
- Modify: `crates/dbc-ui/src/er_diagram_view.rs`

**Interfaces:**
```rust
/// design §3: past this many tables in the scoped selection, truncate
/// (alphabetical) rather than lay out an unreadable/slow graph. No
/// viewport culling in v1 (design's explicit decision — GPUI repaints
/// whole scenes every frame regardless, so culling buys nothing below this
/// cap without real profiling evidence of a problem).
pub const DIAGRAM_TABLE_CAP: usize = 150;
```

**Grounding:** T6 Step 4's `open_er_diagram` already performs the truncation and builds the notice string (this task's Interfaces constant is what that code references — `DIAGRAM_TABLE_CAP` is defined here, in `er_diagram_view.rs`, since it's a rendering/scale concern of the view itself, not the pure `dbc-core` layout algorithm, which has no opinion on how many nodes it's asked to lay out). `ErDiagramView` grows the `truncated_notice: Option<String>` field T6 already sets, and `render` grows a one-line banner `div()` (amber/warning-tinted, matching the app's existing status-note color convention) shown above the canvas whenever it's `Some`.

- [ ] **Step 1: Add the constant and the `truncated_notice` field + banner render.**
  ```rust
  pub const DIAGRAM_TABLE_CAP: usize = 150;

  pub struct ErDiagramView {
      // ...T4/T5's fields, plus:
      pub(crate) truncated_notice: Option<String>,
      pub(crate) status_note: Option<String>, // T6's export status, defined here since T7 lands first in file-edit order within this task pairing
  }
  ```
  In `render`, above the `canvas()` child:
  ```rust
  let banner = self.truncated_notice.clone().map(|msg| {
      div()
          .w_full()
          .px_2()
          .py_1()
          .bg(rgb(0x45475a))
          .text_color(rgb(0xf9e2af))
          .child(msg)
  });
  // ...div().children(banner).child(canvas(...))
  ```

- [ ] **Step 2: Unit test the truncation predicate as a pure function** (extracted so it doesn't need a live `SchemaSnapshot`/GPUI to test):
  ```rust
  pub fn cap_tables(mut tables: Vec<dbc_core::TableInfo>, cap: usize) -> (Vec<dbc_core::TableInfo>, Option<usize>) {
      if tables.len() <= cap {
          return (tables, None);
      }
      tables.sort_by(|a, b| a.name.cmp(&b.name));
      let hidden = tables.len() - cap;
      tables.truncate(cap);
      (tables, Some(hidden))
  }

  #[cfg(test)]
  mod cap_tests {
      use super::*;
      use dbc_core::TableInfo;

      fn t(name: &str) -> TableInfo { TableInfo { name: name.into(), ..Default::default() } }

      #[test]
      fn under_cap_is_untouched_and_unsorted() {
          let (out, hidden) = cap_tables(vec![t("z"), t("a")], 150);
          assert_eq!(hidden, None);
          assert_eq!(out.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["z", "a"]);
      }

      #[test]
      fn over_cap_truncates_alphabetically_and_reports_hidden_count() {
          let tables: Vec<TableInfo> = (0..5).map(|i| t(&format!("t{i}"))).collect();
          let (out, hidden) = cap_tables(tables, 3);
          assert_eq!(hidden, Some(2));
          assert_eq!(out.len(), 3);
          assert_eq!(out[0].name, "t0"); // already alphabetical in this fixture; a shuffled-input variant below proves the sort itself
      }

      #[test]
      fn over_cap_sorts_before_truncating() {
          let tables = vec![t("zeta"), t("alpha"), t("mid")];
          let (out, hidden) = cap_tables(tables, 2);
          assert_eq!(hidden, Some(1));
          assert_eq!(out.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["alpha", "mid"]);
      }
  }
  ```
  **Note for T6:** once this lands, `open_er_diagram`'s inline truncation block (T6 Step 4) should call `er_diagram_view::cap_tables(scoped, er_diagram_view::DIAGRAM_TABLE_CAP)` instead of its own hand-rolled sort/truncate — if T6 and T7 are implemented by different agents in the suggested order (T6 before T7 per the dependency table), T6's inline version is correct-but-duplicated; this refactor is a one-line simplification, noted here rather than silently left as drift.

- [ ] **Step 3: Run to green, confirm read-only.**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui er_diagram_view::cap_tests::`
  Expected: 3 tests pass, zero warnings.
  Run: `grep -n "\.execute(\|\.query(" crates/dbc-ui/src/er_diagram_view.rs`
  Expected: no output.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/dbc-ui/src/er_diagram_view.rs
  git commit -m "feat: DIAGRAM_TABLE_CAP truncation + notice banner (G8 T7)"
  ```

---

## Task ordering

1. **T1** (`erd::build_graph`) — solo, blocking, dbc-core only.
2. **T2** (`erd::layout::compute_layout`) — depends on T1; dbc-core only.
3. **T3** (`erd::svg::export_svg`) — depends on T2's type shape; dbc-core only. **T1-T3 are the "pure tasks first, in parallel worktrees" batch** — in practice T2 can't start meaningfully before T1's structs exist and T3 can't start before T2's `DiagramLayout` shape is frozen, so within this batch the real parallelism is at the type-boundary level (an agent can begin T3 the moment T2's public structs are written, even mid-T2-algorithm-implementation) rather than three fully independent worktrees running from time zero.
4. **T4** (`ErDiagramView` static paint, spike-gated) — starts ONLY after T1-T3 have all landed (this plan's explicit ordering brief — stricter than the design draft's own suggestion that T3/T4 could run concurrently; see Self-Review note 1). Step 1 is the mandatory on-Windows spike before any of T4's real code is written.
5. **T5** (interaction: pan/zoom/hit-test/click-to-DDL/selection) — depends on T4.
6. **T6** (tab/entry wiring: palette, schema-tree icon, `TabContent::Diagram`, export button, version bump) — **serialized tail**, depends on T3 (export) and T5 (the view to wire), dispatched only after G9/G10/G12/G13's `main.rs`/`tabs.rs`/`palette.rs`/`schema_tree.rs` work has merged.
7. **T7** (large-schema cap) — depends on T2 (nothing algorithmic, just the constant/predicate) and T5 (the view to attach the banner to); small enough to ride alongside T6's worktree/commit sequence, kept as its own task for traceability against the design's own T7.

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §0 (data-model gap: composite FK vs. two-separate-FKs collapsing into one edge) → T1's `build_graph` (BTreeMap-keyed grouping) + T1's `composite_fk_collapses_to_one_edge_with_two_column_pairs` test, documented as an honest simplification in the code comment, not silently patched.
- §1 (graph model: node = collapsed table box, PK/FK-only columns capped at 6, isolated tables included, self-refs/bidirectional pairs as normal edges) → T1 in full.
- §2 (layout algorithm: cycle breaking, longest-path layering, barycenter crossing reduction at fixed 4 iterations, coordinate assignment, straight-line + self-loop edge routing, determinism) → T2 in full, plus the Global Constraints "deep-graph hazard" bullet (iterative DFS/Kahn's, capped) which the design draft itself flags only as a §6 risk ("nobody has stress-tested this against a pathological schema") — this plan promotes that risk to a binding implementation constraint rather than leaving it as a post-hoc "look at it later" note, per this plan's own explicit brief.
- §3 (rendering: `canvas()`/`paint_quad`/`paint_path`/`PathBuilder`/`shape_line` mechanism, pan/zoom idiom reuse, hit-testing shape, click-to-DDL reuse, entry points, large-schema scoping) → T4 (mechanism + static paint), T5 (interaction), T6 (entry points), T7 (large-schema cap).
- §4 (export: `render_to_image`'s platform gap, SVG-instead decision, PNG deferred) → T3 (the SVG serializer itself; the `render_to_image` investigation is not re-verified by this plan since T3 never touches that code path at all — the design draft's own citation trail, `platform.rs:986` etc., stands as-is).
- §5 (task decomposition: T1-T7) → this plan's T1-T7, same numbering, same rough shape, with the T3/T4 parallelization loosened to strict sequencing per this plan's explicit brief (see note 1).
- §6 risks: GPUI Windows canvas/path maturity → T4's mandatory spike (promoted from "flagged risk" to "gating step," per this plan's brief). `render_to_image` platform gap → not re-litigated (already resolved by design's own decision to use SVG, T3). Barycenter-at-fixed-4-iterations unverified against pathological hub tables → left as a flagged, not-blocking risk (T2's tests prove correctness on hand-built graphs, not performance/legibility on a real large schema — no task in this plan claims to have benchmarked it, matching the design's own "flagged for a manual look after T2, not a blocker" framing). `ConstraintInfo` structural gap → T1's code comment, same as design §0. SVG-vs-canvas text metric drift → left as-is (design's own explicit "acceptable for v1" call).

**Placeholder scan:** T1/T2/T3 (the pure, unit-tested `dbc-core` half) show complete, real code for every function and every test — no TBDs, no placeholder bodies. T4-T7 (the GPUI half) show real, complete code for every LOGIC-bearing piece (the spike's paint closure, `world_to_screen`/`to_screen`, `zoom_at` and its unit tests, hit-testing's data shape, the export flow, the palette schema-resolution function, the truncation predicate and its tests) — the one place this plan uses a contract/sketch instead of literal byte-exact code is `paint_node`'s `shape_line`/`ShapedLine::paint` call in T4 Step 2, explicitly flagged inline with an implementer note pointing at `sql_input.rs:1079-1081` as the ground-truth signature to copy rather than trusting this plan's own transcription — the same "GPUI render paths aren't unit-tested elsewhere in this codebase either, verified manually" precedent `docs/superpowers/plans/2026-08-23-g9-server-monitor.md` and the G13 plan both state outright for their own render-heavy steps.

**Type-name consistency across tasks:** `dbc_core::erd::{TableKey, ErdColumnRow, ErdNode, FkEdge, ErdGraph, build_graph, MAX_VISIBLE_COLS}` (T1) match T2's `use super::{ErdGraph, ErdNode, TableKey}` and T4's `dbc_core::erd::layout::DiagramLayout` import path. `dbc_core::erd::layout::{PositionedNode, RoutedEdge, DiagramLayout, compute_layout, NODE_WIDTH, HEADER_H, ROW_H, FOOTER_H, LAYER_GAP, COL_GAP, ISOLATED_COLS_PER_ROW}` (T2) match T3's `use super::layout::{DiagramLayout, PositionedNode, RoutedEdge}` and T4/T5's `paint_node`/`paint_edge`/hit-test code. `dbc_core::erd::svg::{escape_xml, export_svg}` (T3) match T6's `start_export_svg`. `er_diagram_view::{ErDiagramView, DIAGRAM_TABLE_CAP, cap_tables, zoom_at}` (T4/T5/T7) match T6's `open_er_diagram`/`TabContent::Diagram`. `schema_tree::TreeEvent::OpenErDiagram`/reused `OpenDdl` (T6) match `main.rs`'s `on_tree_event`/new `on_er_diagram_event`.

**Resolved design ambiguities / deviations (flagged for controller review, not vetoed unilaterally):**
1. **T3/T4 sequencing is stricter than the design draft's own suggestion.** The design draft (§5) says "T3 and T4 can both start the moment T2's `DiagramLayout` shape is fixed... built by two people/agents in parallel." This plan's explicit task-ordering brief instead says "pure tasks first in parallel worktrees... THEN the GPUI canvas/view task" — read as a hard sequencing gate (all of T1-T3 complete before T4 begins), not a suggestion. Followed literally in this plan's Task Ordering section; a controller who prefers the design's own looser parallelization can safely start T4's Step 1 spike (which only needs `gpui` itself, not any `dbc-core` type) concurrently with T3, since the spike example file has zero dependency on T1-T3's output — only T4 Step 2 onward genuinely needs T3 to exist (for the eventual SVG-parity claim, not for T4's own compile graph, which only needs T2's `DiagramLayout`). Noted as a safe relaxation, not adopted as this plan's default.
2. **`PaletteAction::ShowErDiagram`'s schema-resolution logic (`resolve_er_diagram_schema`, T6) is new, not specified in the design draft**, which only sketches the two entry points (palette action, schema-tree icon) without saying how a zero-argument palette action picks a schema. This plan's resolution (single-schema snapshots resolve automatically, ambiguous ones refuse with a Czech status message pointing at the tree icon) is a deliberate, documented judgment call — the schema-tree icon remains the PRIMARY, unambiguous entry point; the palette action is a convenience for the common single-schema case.
3. **`DIAGRAM_TABLE_CAP`/`cap_tables` live in `dbc-ui::er_diagram_view`, not `dbc-core::erd`** — a file-location decision this plan makes explicit (design §3 doesn't say which crate owns the cap): the cap is a rendering/UX-scale concern ("how many boxes can a human usefully look at"), not a graph-correctness concern, so `dbc-core`'s `compute_layout` stays scale-agnostic (it will happily lay out 10,000 nodes if asked — correctness, not performance, is its contract) and the cap is applied by the caller BEFORE `build_graph`/`compute_layout` ever run, exactly as T6 Step 4's code does.
4. **The T4 spike's fallback path (div-based Manhattan-routed edges, no `canvas()`/`PathBuilder` at all) is fully specified but not expected to be needed** — every primitive T4 depends on was confirmed present and structurally sound by reading the vendored source while drafting this plan (`canvas.rs:10-46`, `path_builder.rs:86-139,244-250`, `window.rs:4103-4118,4174-4186`); the spike exists to catch a RUNTIME/rasterization gap that static source-reading cannot rule out (per the design's own §6 framing — "nobody has yet exercised... in this codebase"), not because there's a known compile-time or documented issue. If the spike passes (the expected outcome), the fallback branch of T4 Step 1 is simply never taken and can be deleted from the codebase's history without further action (it was never committed in the first place).
5. **No docker validation task** — restated from Global Constraints: this phase issues zero new catalog SQL; `build_graph`'s only input is an already-fetched, already-tested `SchemaSnapshot`. A future phase that changes what `Connection::schema()` returns would need its OWN docker validation, not this one's.
