# G8 — ER Diagram: Design Pass

Date: 2026-08-23
Status: designed autonomously under the standing mandate (per §4 of
`2026-08-22-gui-target-design.md`); decisions recorded here for later user
review, in the terse decision-per-bullet style of the G5 design pass block.

Scope (spec row G8): FK-graph rendering of a schema in a GPUI canvas; export
image.

Sources read: `2026-08-22-gui-target-design.md` §2/§3 + G5 block (style
model); `crates/dbc-core/src/schema.rs` (`SchemaSnapshot` v2, `FkRef` on
`ColumnInfo`); `crates/dbc-ui/src/tabs.rs`, `grid.rs`, `main.rs`,
`palette.rs` (tab infra, drag/scroll conventions); vendored GPUI at
`C:\Users\tomas\.cargo\git\checkouts\zed-a70e2ad075855582\907ed09`, files
cited inline below.

> **CURATION (2026-08-23, binding):**
> 1. **SVG export must XML-escape all interpolated text** (table/column
>    names, schema names) — `<`, `>`, `&`, `"`, `'` via a small local
>    escape helper in `erd::svg`. REQUIRED test: a table named `a<b&"c`
>    produces valid escaped SVG. The draft omits this entirely and it is a
>    correctness hole (a `<` in an identifier yields broken XML).
> 2. **Palette:** node fill/border, edge, selection-accent and hot colors
>    reuse the app's Catppuccin Mocha palette (G6 curation) in BOTH the
>    canvas renderer and the SVG exporter — no new color scheme.
> 3. **T4 opens with a 30-minute spike:** paint one rounded quad + one
>    `PathBuilder` bezier inside `canvas()` on Windows before building
>    `ErDiagramView` — §6's first risk (never-exercised `canvas()`/curve
>    path on this platform) must be retired before the full view is built.
> 4. **Scheduling note:** T1–T3 (dbc-core `erd` module) are worktree-
>    parallelizable; T4–T7 serialize with other dbc-ui phases.
> 5. **Read-only phase:** zero `execute()` surface anywhere in G8.

## 0. Data-model gap, decided up front

`ColumnInfo.fk: Option<FkRef>` is per-column; there is no structural link
from a `ConstraintInfo` (kind `"FOREIGN KEY"`) to the set of columns that
form one composite key — `ConstraintInfo.definition` is a free-text
engine string, not parsed. **Decision:** v1 groups FK columns into one
graph edge per `(source_table, target_table)` pair — i.e. every
`ColumnInfo` in table A whose `fk.table`/`fk.schema` resolve to table B
contributes one `(a_col, b_col)` pair to the SAME edge A→B, however many
columns that is. This is exactly right for composite FKs (all columns of
one real composite key point at the same target table, so they collapse
into the one edge we'd want anyway) and is an honest, documented
simplification for the rare case of two *separate* FK constraints between
the same table pair (e.g. `orders.billing_addr_id` and
`orders.shipping_addr_id` both → `addresses.id`): v1 renders that as ONE
edge carrying two column-pair labels, not two parallel edges. Flagged in
§6 as a follow-up if `ConstraintInfo` ever grows structured column lists.

## 1. Graph model (dbc-core, pure, unit-testable)

New module `crates/dbc-core/src/erd.rs` (no GPUI, no new dependency —
plain structs + `Vec`/`HashMap`), exported from `lib.rs` alongside
`schema`.

- **Node = one collapsed table box**, not full column lists. Decision:
  v1 shows table name (+ schema prefix if the snapshot has >1 schema in
  play) as header, then PK columns and FK columns only (name + type, PK
  marked, FK columns marked with a small arrow glyph), capped at
  `MAX_VISIBLE_COLS = 6` rows with a `"+N dalších"` footer beyond that.
  Rationale: full column lists blow up box height and make layout/legibility
  fall apart past ~15 tables; PK/FK-only boxes are what every ER tool
  (dbdiagram.io, DataGrip's own diagram) converges on for an overview
  diagram, and it's the columns that carry graph-relevant information
  anyway. Non-PK/FK columns remain one click away (hit-test → DDL tab,
  §3).
- **Node set = v1 includes every `TableInfo` in the snapshot subset passed
  in** (kind `Table` or `View`, whatever the caller selected — see §3
  large-schema handling for how selection is scoped), including tables
  with zero FK edges (drawn, unconnected, off to the side — see layout
  §2). Isolated tables are common and hiding them would be surprising.
- **Edge = one `FkEdge { from: TableKey, to: TableKey, columns: Vec<(String,
  String)> }`** per §0's grouping. `TableKey = (schema: Option<String>,
  name: String)`, matching `TableInfo`/`FkRef` fields directly (no new
  identity scheme).
- **Self-references** (`from == to`, e.g. `employees.manager_id →
  employees.id`): kept as a normal `FkEdge` with `from == to`; the layout
  algorithm special-cases these (§2) and the renderer draws a small loop
  stub (§3) rather than routing back into the layered graph.
- **Multiple FKs between the same ordered pair** collapse per §0.
  **A↔B in both directions** (A has an FK to B AND B has an FK to A) are
  kept as two distinct `FkEdge`s (different `from`/`to`) — this is a real,
  meaningful cycle for the layout algorithm, not a data-model gap.
- Public entry point: `pub fn build_graph(tables: &[TableInfo]) ->
  ErdGraph { nodes: Vec<ErdNode>, edges: Vec<FkEdge> }`. Pure function,
  no I/O, directly unit-testable against hand-built `TableInfo` fixtures
  (composite FK, self-ref, bidirectional pair, isolated table — one test
  each).

## 2. Layout algorithm (dbc-core, pure, unit-testable)

**Decision: hand-rolled layered (Sugiyama-style) layout, zero new
dependencies.** Considered and rejected:
- *petgraph*: gives graph algorithms (toposort, SCC, DFS) but ships no
  coordinate-assignment/layering — we'd still hand-roll the actual layout
  on top of it, for the cost of a dependency that buys us little beyond
  `Vec<Vec<usize>>` adjacency we can write in 20 lines ourselves.
- *`layout-rs`-style crates* (Graphviz-port crates on crates.io): exist but
  are thin-maintained, pull in their own geometry types we'd have to
  adapt at the dbc-core/dbc-ui boundary, and their DOT-oriented API is a
  poor fit for "give me typed node/edge structs, get typed
  positions back". Not worth the dependency risk for a well-understood,
  ~300-line algorithm.
- *Force-directed simulation*: harder to unit-test (convergence is
  iteration-count/tolerance dependent, not a single deterministic
  answer), produces less legible diagrams for the hierarchical
  parent→child shape typical of FK graphs, and doesn't obviously beat
  layered layout in implementation cost once you add jitter-avoidance and
  a stopping rule. Rejected for v1.
- *Grid-by-connectivity*: too crude past a handful of tables; keeps for
  nothing.

**Algorithm (deterministic, no RNG — ties broken by table name so output
is stable across runs and byte-identical in tests):**

1. **Cycle breaking.** DFS from each undiscovered node (visiting in
   `TableKey` sort order for determinism); classify edges as tree/forward/
   cross/**back** (back edge = points to a node currently on the DFS
   stack). Self-loops (`from == to`) are pulled out before this step and
   never enter the DFS. Back edges are logically reversed for layering
   purposes only — the renderer (§3) always draws the arrowhead per the
   ORIGINAL `FkEdge` direction, layout just needs an acyclic skeleton to
   assign layers.
2. **Longest-path layering.** On the now-acyclic graph: nodes with no
   incoming edge get layer 0; otherwise `layer(v) = 1 + max(layer(u))`
   over incoming edges `u→v`, computed by a topological-order sweep
   (Kahn's algorithm — queue of zero-indegree nodes). Isolated nodes
   (§1) get layer 0 too, but are placed in a visually separate row below
   the connected layers (see step 4) rather than mixed into layer 0's
   ordering — avoids them stealing horizontal space from real roots.
3. **Crossing reduction (ordering within layer).** Barycenter heuristic:
   for each node, order = mean position-index of its neighbours in the
   adjacent layer; alternate a down-sweep (order by predecessors' layer
   above) and up-sweep (by successors' layer below) for a **fixed 4
   iterations** (no convergence loop — keeps the function total and
   trivially bounded, matches typical practical Sugiyama
   implementations which see diminishing returns past ~4 sweeps). Ties
   broken by table name, ascending.
4. **Coordinate assignment.** Fixed node box size (`NODE_WIDTH = 220.0`,
   height = `HEADER_H + min(cols_shown, 6) * ROW_H + (footer? FOOTER_H :
   0)`, all `f32` constants in the module). Layer `i` → `y = i *
   (NODE_MAX_HEIGHT + LAYER_GAP)`; within a layer, nodes spread along `x`
   at `NODE_WIDTH + COL_GAP` spacing, centered as a block. Isolated-node
   row placed at `y = (max_layer + 1) * (...)`, wrapped into a grid of
   fixed column count (e.g. 6 per row) rather than one long strip.
   Self-loop edges get a fixed stub geometry (small loop on the node's
   right edge, no layout impact).
5. Output: `pub struct DiagramLayout { pub nodes: Vec<PositionedNode>,
   pub edges: Vec<RoutedEdge> }` — `PositionedNode { key: TableKey, x: f32,
   y: f32, w: f32, h: f32 }`, `RoutedEdge { from: TableKey, to: TableKey,
   points: Vec<(f32, f32)>, is_self_loop: bool }`. All `f32` (logical
   units, not GPUI `Pixels` — dbc-core doesn't see GPUI; dbc-ui multiplies
   by a zoom factor and offsets by pan at paint time).

**Edge routing (v1): straight lines** between the border points of `from`
and `to` boxes (computed as the intersection of the connecting segment
with each box's rectangle — cheap, no orthogonal-routing bend logic).
Rationale: orthogonal/bezier routing avoids overlaps but is materially
more code (obstacle-aware routing) for a v1 whose real audience is "get
oriented in a schema", not publication-quality diagrams; straight lines
with node boxes that don't overlap (guaranteed by step 4's spacing) stay
readable up to the density this tool targets (§3 caps at 100 tables).
Self-loop edges use a fixed small bezier stub (via `PathBuilder`, §3) —
the one place v1 draws a curve, because a "straight line back to the same
point" isn't renderable.

Complexity: O(V+E) for layering, O(4·E) for crossing reduction — no
correctness risk at the table counts this tool will see (see §3 cap).

**Unit tests (dbc-core, no GPUI):** single table (no edges); simple chain
A→B→C (checks layer assignment 0,1,2); self-reference (node present,
edge marked `is_self_loop`, doesn't affect any other node's layer);
bidirectional pair A↔B (both edges present, no infinite loop in cycle
breaking); composite FK (one edge, `columns.len() == 2`); diamond
A→B, A→C, B→D, C→D (checks layer(D) == 2 via longest path, not first
arrival); isolated table (layer 0 but placed in the separate row, not
mixed into connected layer-0 ordering); determinism (same input twice →
byte-identical `DiagramLayout`, via `f32` bit-equality since no RNG is
involved).

## 3. Rendering (dbc-ui only)

**GPUI mechanism confirmed in the pinned checkout:**
- `gpui::canvas(prepaint, paint) -> Canvas<T>`
  (`crates/gpui/src/elements/canvas.rs`) — exactly the "custom drawing"
  escape hatch; `prepaint` computes/caches layout-derived state (e.g. the
  already-computed `DiagramLayout` plus current pan/zoom), `paint` gets
  raw `&mut Window` access for the low-level paint calls below. This is
  the same mechanism the GPUI docs comment calls out for "short term
  custom drawing".
- `Window::paint_quad` (`window.rs:4103`) — node boxes (rounded rect,
  fill + border via `PaintQuad`/`corner_radii`), reusing the same
  `PaintQuad` builder pattern already used for grid cell backgrounds in
  `grid.rs`.
- `Window::paint_path` (`window.rs:4174`) + `gpui::PathBuilder`
  (`crates/gpui/src/path_builder.rs`, backed by `lyon` tessellation,
  already a transitive GPUI dependency — no new crate) — straight edges
  are a 2-point path; self-loop stubs use `PathBuilder`'s SVG-style
  curve-to for the one bezier segment.
- Text: `Window::text_system().shape_line(...)` then `.paint(...)` —
  confirmed as the existing dbc-ui convention (already used in
  `sql_input.rs`, `connections_ui.rs`) — reused verbatim for node headers
  and column rows, no new text-painting code path.
- **Pan:** drag-to-pan reuses the exact `on_mouse_down` /
  `on_mouse_move` / `on_mouse_up` pattern already in `grid.rs` for
  column-resize drag (state = `(start_mouse_pos, start_pan_offset)`
  captured on mouse-down, `pan_offset` updated on mouse-move, committed
  on mouse-up) — no new interaction primitive needed.
- **Zoom:** `on_scroll_wheel` (`ScrollWheelEvent`, already used in
  `grid.rs` for the cell-detail popup) — `ScrollDelta::Lines`/`Pixels`
  mapped to a zoom-factor multiplier (e.g. `1.1^lines`), clamped to
  `[0.2, 3.0]`, anchored at the mouse position (recompute `pan_offset` so
  the point under the cursor stays fixed — standard anchored-zoom math,
  done in dbc-ui, not dbc-core).
- Diagram state (pan, zoom, selected node) lives in a GPUI `Entity`
  (`ErDiagramView`, analogous to `ResultGrid`), holding a cached
  `DiagramLayout` (recomputed only when the schema selection changes,
  not per frame/per pan-tick).

**Hit-testing / interaction:** node boxes are painted inside the canvas
paint closure, but hit-testing is done in dbc-ui BEFORE/alongside paint —
decision: keep a `Vec<(TableKey, Bounds<Pixels>)>` computed in `prepaint`
(screen-space box bounds after applying current pan/zoom) and test clicks
against it in an `on_mouse_down` handler on the canvas's wrapping `div`
(same "occlude + explicit hit-test list" shape already used for the grid's
context-menu dismiss handling, not GPUI's per-element hit-test machinery,
because the canvas itself has no per-node element identity). **Click on a
node → open its DDL tab** (reuses `TreeEvent::OpenDdl`'s existing
`TabContent::Text { title: "DDL: {name}", ddl }` path in `main.rs` verbatim
— zero new tab-content plumbing). **Click on empty canvas → clears
selection**, which (decision) highlights: selected node's border color
changes, AND every `RoutedEdge` touching it gets drawn in an accent color
on top (z-order: highlighted edges repainted last in the same paint
closure) — cheap, no new state beyond `selected: Option<TableKey>` on
`ErDiagramView`.

**Entry point (how a user opens the diagram):** not mocked up in §1 of
the target-UI spec (G8 is a "future, own brainstorm" row) — decision:
palette action `PaletteAction::ShowErDiagram` ("ER diagram") plus a
schema-tree context action on a schema node ("Zobrazit ER diagram"),
both opening a new `TabContent` variant `Diagram { view: Entity<ErDiagramView>
}` (parallel to `Grid`/`Text`) titled `"ER: {schema}"`. Reuses `Tabs`
infra unchanged (new enum arm handled in `main.rs`'s existing
`render_tab_content` match, `TAB_CAP` eviction applies identically — a
diagram tab is just another tab).

**Large-schema behavior (100+ tables):** decision — **scope selection,
not silent capping.** The entry action always operates on ONE schema (not
"all schemas in the connection") — for engines/snapshots with many
schemas this is already a natural filter. Within one schema, if table
count exceeds `DIAGRAM_TABLE_CAP = 150`, the tab still opens but shows a
status-bar-style notice inside the tab ("Schéma má {n} tabulek — zobrazeno
prvních 150 podle názvu; použijte filtr.") and truncates the node set
(deterministic: alphabetical) rather than laying out an unreadable/slow
graph. No viewport culling in v1 (decision: at ≤150 nodes with straight-
line edges, painting the full scene every frame is cheap — GPUI repaints
whole scenes each frame regardless; adding culling logic buys nothing
until real profiling shows a problem). A future free-text filter
(palette-style fuzzy match narrowing the node set before layout) is a
natural fast-follow, not v1.

## 4. Export image

**Investigated:** `Window::render_to_image(&self) -> anyhow::Result<image::RgbaImage>`
exists (`crates/gpui/src/window.rs:2458`) and looks like exactly what's
needed, BUT:
- It's gated `#[cfg(any(test, feature = "test-support"))]` — not compiled
  into a normal (non-test) build at all.
- Its platform backend is only implemented for macOS
  (`crates/gpui_macos/src/window.rs:2143`, Metal-backed) and the in-memory
  test window (`crates/gpui/src/platform/test/window.rs:410`). The
  cross-platform default in `crates/gpui/src/platform.rs:986` is:
  `fn render_to_image(&self, _scene: &Scene) -> Result<RgbaImage> { anyhow::bail!("render_to_image not implemented for this platform") }`
  — i.e. on Windows (this project's dev/target platform per env) it
  compiles to an unconditional error at runtime, even if the
  `test-support` feature were force-enabled in a release build (which
  would itself be an abuse of a test-only feature flag).

**Decision: do not use `render_to_image`. Export is a pure-Rust SVG
serializer, in dbc-core, over the SAME `DiagramLayout` the canvas paints
from.** `pub fn export_svg(layout: &DiagramLayout, tables: &[TableInfo]) ->
String` builds an SVG document by hand (`<svg>`, `<rect>` per node,
`<text>` per header/column row, `<path>`/`<line>` per edge) — no new
crate needed (SVG is just XML string formatting; no rasterization
required to produce a valid, viewable SVG). Rationale: (a) it's honest
about what the pinned GPUI rev can actually do on this platform, (b) it
reuses the exact geometry already computed and unit-tested for on-screen
rendering — screen and export can never visually diverge, (c) SVG is
lossless, human-diffable, and directly usable (opens in any browser,
embeds in docs) without any raster-encoding dependency, (d) it keeps the
exporter in dbc-core, pure and unit-testable (assert on presence of
expected `<rect>`/`<text>`/`<path>` substrings and coordinate values —
snapshot-style string tests), consistent with the binding constraint.
PNG export is explicitly deferred: if wanted later, the honest path is a
follow-up dependency on `resvg`/`tiny-skia` (pure-Rust SVG rasterizer,
no GPUI/platform dependency) converting the same SVG string — not
attempted in v1 to avoid adding a rasterization dependency before anyone
asks for PNG specifically. dbc-ui's "Export…" button on the diagram tab
writes the SVG string to a user-chosen path (same file-dialog pattern
already used by `export.rs` for CSV/TSV/JSON/INSERT grid export).

## 5. Task decomposition

- **T1 — Graph model** (`dbc-core::erd::build_graph`): `TableKey`,
  `ErdNode`, `FkEdge`, §0/§1 grouping logic. Unit tests: composite FK
  merge, self-ref, bidirectional pair, isolated table. No GPUI, no
  dependency on T2.
- **T2 — Layout algorithm** (`dbc-core::erd::layout`, depends on T1's
  types): cycle breaking, longest-path layering, barycenter ordering,
  coordinate assignment, straight-line + self-loop edge routing. Unit
  tests per §2's list. Independently testable/parallelizable with T3
  once T1's structs are merged (T2 only needs the `ErdGraph` shape, not
  T3/T4's code).
- **T3 — SVG export** (`dbc-core::erd::svg`, depends on T2's
  `DiagramLayout`): pure string builder. Unit tests: string-contains
  assertions for node/edge/text presence and coordinates. Can be built
  and merged in parallel with T4 (both only need T2's output type, not
  each other).
- **T4 — Canvas rendering** (`dbc-ui::er_diagram_view`, depends on T2):
  `ErDiagramView` entity, `canvas()` paint closure (quads, paths, text
  per §3), pan (drag) + zoom (scroll wheel), static (no interaction yet)
  render of a `DiagramLayout`.
- **T5 — Interaction** (depends on T4): hit-testing, click→DDL-tab (reuse
  `TreeEvent::OpenDdl`), selection highlight, empty-click clear.
- **T6 — Tab/entry wiring** (depends on T4, and T3 for the export
  button; independent of T5): new `TabContent::Diagram` arm in
  `main.rs`, palette action, schema-tree context action, "Export…"
  button calling T3 + file dialog.
- **T7 — Large-schema cap** (depends on T2/T4): `DIAGRAM_TABLE_CAP`
  truncation + notice banner. Small, can ride with T6.

**Parallelization:** T1 first (blocking). Once T1 lands, T2 proceeds
alone; T3 and T4 can both start the moment T2's `DiagramLayout` shape is
fixed (even before T2's algorithm internals are fully tuned — the type
signature is the contract) and can be built by two people/agents in
parallel since neither touches the other's files. T5 and T6 both need T4
merged but are independent of each other (different concerns: mouse
hit-testing vs. tab plumbing) — safe to parallelize. T7 rides with T6.

## 6. Risks / needs-verification

- **GPUI Windows backend maturity for `canvas()`/`paint_path`/text
  painting in a scrollable+zoomable custom element** is asserted from
  reading the trait/struct definitions (`canvas.rs`, `window.rs`,
  `path_builder.rs`) and from EXISTING dbc-ui usage of `paint_quad` and
  `shape_line` elsewhere in the same codebase (`grid.rs`, `sql_input.rs`)
  — those call sites prove the primitives work on this project's actual
  Windows target today, but nobody has yet exercised `PathBuilder`'s
  curve-to (used only for the self-loop stub) or `canvas()` itself in
  this codebase; first real risk to retire in T4.
- **`render_to_image` platform gap is confirmed by reading source**, not
  by running it — `crates/gpui/src/platform.rs:986`'s default bail and
  the absence of any `crates/gpui_windows` (or similar) override in the
  vendored tree were checked via grep across
  `crates/gpui/src/platform/` (only `test/` subdir exists) and across
  all `crates/*` for `fn render_to_image` (only `gpui_apple` and
  `gpui/src/platform/test` implement it). High confidence, not
  execution-verified.
- **Barycenter crossing reduction at fixed 4 iterations** is a standard
  practical choice but unverified against pathological real schemas
  (very high fan-out hub tables, e.g. an `audit_log` FK'd from
  everything) — may need `MAX_VISIBLE_COLS`/spacing tuning once tried
  against a real large schema; flagged for a manual look after T2, not a
  blocker.
- **`ConstraintInfo` has no structured column list** (§0) — the
  same-pair-multiple-FK simplification is a real (if rare) information
  loss; worth a note back to whoever owns `dbc-core::schema` for a
  possible v2 enrichment (`ConstraintInfo.columns: Vec<String>`) if a
  future phase needs constraint-level fidelity.
- **SVG export visual fidelity vs. on-screen canvas** — both consume the
  same `DiagramLayout`, but text metrics (SVG default font vs. GPUI's
  shaped glyph widths) will differ slightly; acceptable for v1 (export
  is "get a diagram out", not pixel-perfect parity) but not verified
  against a real SVG viewer.
