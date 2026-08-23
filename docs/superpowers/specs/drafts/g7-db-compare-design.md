# G7 — DB Compare — Design

Date: 2026-08-23
Status: draft, designed autonomously under the standing mandate (style/format
per §4 "G5 design pass" block in `2026-08-22-gui-target-design.md`); decisions
recorded here for later user review.
Scope: schema diff between two saved connections (`SchemaSnapshot`-based);
data diff for a chosen table pair via an in-process Arrow diff over
`dbc-buffer`. Read-only end to end — no write path, no "generate sync
script" executor.

Inputs read: `docs/superpowers/specs/2026-08-22-gui-target-design.md` (§2 G7
row, §3 constraints, G5 pass as style model); `crates/dbc-core/src/schema.rs`
(full `SchemaSnapshot` v2); `crates/dbc-core/src/ddl.rs`;
`crates/dbc-buffer/src/lib.rs`; `crates/dbc-ui/src/main.rs`,
`connect.rs`, `runner.rs`, `tabs.rs`, `sandbox.rs`, `connections_ui.rs`,
`grid.rs`; `crates/dbc-state/src/{config,vault}.rs`.

---

## 0. New crate: `dbc-diff`

- **Decision:** new pure crate `crates/dbc-diff`, added to the workspace
  members list, depending on `dbc-core` (for `SchemaSnapshot`/`TableInfo`/
  `ColumnInfo`/… and `ddl::quote_ident`) and `dbc-buffer`/`arrow` (for the
  data-diff comparator, which operates on `RecordBatch`/`SchemaRef`). No
  `gpui` dependency anywhere in this crate — upholds "dbc-core never sees
  GPUI" by extension (diff logic is exactly the kind of pure, deeply
  unit-testable module that doesn't belong bolted onto `dbc-core` itself,
  mirroring why `dbc-buffer` is its own crate rather than living in
  `dbc-core`).
- **Rationale for a new crate over extending `dbc-core`:** `dbc-core` is the
  driver-abstraction crate (the `Connection` trait, `SchemaSnapshot`, `ddl`).
  A diff engine is a *consumer* of that model, not part of it — folding it in
  would make `dbc-core` depend on `dbc-buffer` (for the data-diff half),
  which is backwards (drivers/buffer already depend on `dbc-core`, not the
  other way round). A separate crate keeps the dependency graph a DAG:
  `dbc-core` ← `dbc-buffer` ← `dbc-diff` ← `dbc-ui`, drivers ← `dbc-ui`
  (via `connect.rs` only, unchanged).
- Module layout inside `dbc-diff`:
  - `schema_diff.rs` — object matching + `SchemaDiff` model (§1).
  - `data_diff.rs` — PK-based row comparator over two `RecordBatch` sets
    (§4).
  - `text_diff.rs` — thin wrapper around the `similar` crate (new workspace
    dependency, MIT, pure Rust, no unsafe, ~small) for line-level DDL diffing
    used by drill-down (§3).

---

## 1. Schema diff semantics

- **Object matching key:** `(schema, name)` for tables/views (schema
  `None` normalized to `""` for matching, so a SQLite snapshot — which never
  sets `schema` — still matches sensibly against itself and, cross-engine,
  against a Postgres `public`-schema object only if the user's mental model
  treats `None`≈`public`; **decision: no such normalization** — `None` only
  matches `None`. A SQLite-vs-Postgres compare will therefore show every
  Postgres table under "only in B" and vice versa unless schema is truly
  absent on both sides. This is called out as a known limitation (§6), not
  silently patched, because guessing "which schema is the default one" per
  engine is exactly the kind of cross-engine heuristic that erodes trust
  when wrong.
  Columns/indexes/constraints match by `name` within the already-matched
  table. Routines match by `(schema, name, kind)` — **overloads are not
  resolved**: if a name has a different `signature` set on each side, v1
  treats it as one Removed (old signature) + one Added (new signature)
  rather than attempting to pair signatures up; matching overloads correctly
  needs a real type-compatibility model, out of scope. Triggers match by
  `(schema, table, name)`. Sequences match by `(schema, name)`.
- **Diff result model** (`schema_diff.rs`):
  ```rust
  pub enum ObjectDiff<T> {
      Added(T),
      Removed(T),
      Changed { left: T, right: T, fields: Vec<FieldChange> },
      Unchanged(T),
  }
  pub struct FieldChange { pub field: String, pub left: String, pub right: String }

  pub struct TableDiff {
      pub schema: Option<String>,
      pub name: String,
      pub status: TableStatus, // Added | Removed | Changed | Unchanged
      pub table_fields: Vec<FieldChange>,   // e.g. "kind" (Table vs View)
      pub columns: Vec<ObjectDiff<ColumnInfo>>,
      pub indexes: Vec<ObjectDiff<IndexInfo>>,
      pub constraints: Vec<ObjectDiff<ConstraintInfo>>,
  }
  pub struct SchemaDiff {
      pub tables: Vec<TableDiff>,
      pub routines: Vec<ObjectDiff<RoutineInfo>>,
      pub triggers: Vec<ObjectDiff<TriggerInfo>>,
      pub sequences: Vec<ObjectDiff<SequenceInfo>>,
  }
  ```
  `Unchanged` is kept in the model (needed for a "show unchanged" toggle and
  for tests asserting non-changes) but the UI default-filters it out (§3).
  Top-level `diff_schema(left: &SchemaSnapshot, right: &SchemaSnapshot,
  mode: CompareMode) -> SchemaDiff` is the crate's one entry point for this
  half; deterministic (sorted by schema then name) so output/tests are
  stable regardless of catalog query order.
- **What counts as "changed" per object type** (all comparisons are
  case-sensitive string/bool/vec compares over the `SchemaSnapshot` fields —
  no engine-specific parsing):
  - **Table:** `kind` (Table/View/MaterializedView) differs → field change
    `"kind"`. `ddl` is intentionally EXCLUDED from the changed-check (it's a
    rendering convenience, not semantic — two engines format the same
    definition differently even when nothing user-relevant changed); DDL is
    only used for the drill-down text diff (§3), not to flag "Changed".
  - **Column:** `data_type`, `nullable`, `default`, `is_pk` are compared —
    but see the normalization decision below. `fk` (the FK target) is
    compared structurally (`schema`+`table`+`column` equality) — always
    engine-independent since it's already parsed into `FkRef`.
  - **Index:** `columns` (order-sensitive — column order matters for a
    real index) and `unique`.
  - **Constraint:** matched by `name`; `kind` and `definition` compared as
    raw strings (`definition` is already engine-specific free text in the
    model — no attempt to normalize `CHECK (x > 0)` vs `CHECK ((x > 0))`).
  - **Routine:** `kind`, `signature` (raw string compare).
  - **Trigger:** `table`, `ddl` (raw string — triggers have no other
    structured fields to compare).
  - **Sequence:** presence-only (the model carries no further fields to
    diff — `Added`/`Removed`/`Unchanged` only, never `Changed`).
- **Cross-engine type/default normalization — decision: same-engine-only
  for the semantic field diff of columns; cross-engine falls back to
  structural (existence) diff only.** Rationale: `sqlite`'s `PRAGMA
  table_info` reports raw declared-type text (`"INTEGER"`, `"TEXT"`,
  sometimes just whatever the `CREATE TABLE` author typed —
  affinity-based, not canonical); Postgres's driver formats via
  `format_type()` (`"integer"`, `"character varying(20)"`); defaults are
  even worse (`nextval('orders_id_seq'::regclass)` vs a SQLite
  `AUTOINCREMENT` marker vs nothing at all). Building a real
  cross-engine type-equivalence table (mapping every SQLite affinity and
  every Postgres type OID string to a canonical bucket, then deciding
  which default expressions are "the same" across two completely
  different expression grammars) is a correctness minefield: a wrong
  equivalence silently HIDES a real incompatibility (worse than a false
  positive, which is merely annoying). So: `CompareMode::SameEngine`
  (the only mode exposed when `left.engine == right.engine`, which
  `SchemaSnapshot` itself doesn't carry — engine comes from the two
  `ConnectionConfig`s passed alongside the snapshots) does the full
  field-by-field column diff above, unnormalized (safe — same driver
  produced both strings, so a real difference is a real difference).
  `CompareMode::CrossEngine` (used whenever the two connections' `Engine`
  differ) suppresses `data_type`/`default`/`nullable` from the
  `Changed`-detection for columns entirely — a column present-with-same-name
  on both sides is always `Unchanged` at the column level cross-engine,
  full stop — and the UI shows a persistent banner ("porovnání mezi různými
  databázovými systémy: typy a výchozí hodnoty sloupců se neporovnávají")
  rather than silently omitting the caveat. Existence-level diff (table/
  column/index/constraint/routine/trigger/sequence Added/Removed) is engine-
  independent and always runs fully in both modes — only the *field-level*
  column semantics are gated. `is_pk` IS still compared cross-engine (PK-ness
  is structural, not a type-normalization problem).
- **Cross-engine allowed at all — decision: yes, allowed, degraded mode
  above** (not blocked outright), because "which two tables/columns exist"
  is still a genuinely useful cross-engine question (e.g. comparing a
  legacy SQLite export against a migrated Postgres instance) even without
  type-level confidence.

## 2. Cross-engine compare — summary

Already decided in §1: **allowed**, with `CompareMode` picked automatically
from the two connections' `Engine` (no user toggle — always the strictest
correct mode for the pair, so there's no way to accidentally ask for
same-engine rigor across engines). MSSQL is a future driver; when it lands
this needs no change (`Engine` already has an `Mssql` variant, `CompareMode`
only checks equality).

## 3. UI

- **Entry point:** new palette (Ctrl+K) action "Porovnat databáze…" (also
  reachable from a top-bar menu item, mirroring how "New connection…" is
  reached) opens a new `ModalState::CompareDialog` variant (added to the
  existing enum in `connections_ui.rs`, following the same
  `render_modal_overlay` shape `ConnectionDialog`/`MasterPasswordPrompt`
  already use). Contents: two connection pickers ("Databáze A" / "Databáze
  B"), each the same dropdown-of-saved-connections widget the top bar
  already uses (reused component, not reimplemented) — deliberately NOT
  restricted to the currently-active connection; either side (or both) can
  differ from what's open in the main window. A "Spustit porovnání" button
  is disabled until both sides pick a (different, or even the same —
  comparing a connection against itself is allowed and simply yields an
  all-`Unchanged` result, useful as a smoke test) connection.
- **Secrets/vault:** the app already unlocks ONE global vault per app start
  (`dbc_state::Vault`, keyed by connection id — see `vault.rs`) before any
  connection UI is usable; both pickers draw from the same already-unlocked
  vault, so there is no additional unlock step — this reuses the exact
  existing flow, not a new one. If a picked connection's secret is missing
  (vault entry absent, e.g. an SSO/trust-based connection with no stored
  password) it's passed as `None` exactly like every other connect path
  already handles (`connect.rs::open_config`'s `secret: Option<String>`).
- **Running the compare:** on "Spustit porovnání", `QueryRunner` gets a new
  one-shot method `fetch_schema_pair(spec_a, spec_b) ->
  oneshot::Receiver<(Result<SchemaSnapshot, QueryError>,
  Result<SchemaSnapshot, QueryError>)>` — literally two independent
  `fetch_schema`-style one-shot connects run concurrently inside the
  existing runtime (`tokio::join!`), reusing `open_spec` unchanged. Neither
  side touches `active_connection_id` — this is the same "ephemeral one-shot
  connection, opened and dropped" pattern `fetch_schema`/`fetch_lookup`/
  `test_connect` already use, just issued twice. Either leg failing (e.g.
  connection B unreachable) surfaces as an error banner in the compare tab
  with a "Zkusit znovu" button re-issuing only the failed leg; the modal
  itself closes as soon as the request is dispatched (matching
  `trigger_schema_fetch`'s fire-and-forget-with-generation-guard style) so
  the UI thread is never blocked on either connect.
- **Result rendering — new tab kind:** `TabContent` (in `tabs.rs`) gains a
  `Compare { view: Entity<CompareView> }` variant (same shape as
  `Grid { grid: Entity<ResultGrid>, .. }` — a typed GPUI handle, no new
  GPUI dependency surface beyond what `Tabs` already tolerates). Title:
  `"Porovnání: {A} ↔ {B}"` (collapsed via the existing `collapse_title` if
  long). Opened via `Tabs::open` exactly like a query result tab — subject
  to the same `TAB_CAP`/eviction/pin rules, no special-casing needed there.
- **`CompareView` layout (new `dbc-ui/src/compare.rs`):** left pane is a
  flat, filterable list (not a full tree — schema diffs are shallow enough
  that grouping by kind with collapsible headers, DataGrip-style, is
  simpler than reusing the full `schema_tree.rs` speed-search machinery,
  which is built around a live single-connection catalog, not a diff
  result) — sections "Tabulky", "Pohledy", "Funkce/procedury", "Triggery",
  "Sekvence", each row tinted by status using the SAME colour convention
  already established for sandbox edits in `grid.rs` (green = Added, red =
  Removed, yellow = Changed); `Unchanged` rows are hidden by default behind
  a "Zobrazit beze změn (N)" toggle at the top of each section, consistent
  with keeping the default view focused on what matters. A count badge per
  status sits in the tab's own header row ("+3 -1 ~5"). Right pane: clicking
  a row shows its detail —
  - `Added`/`Removed` table/routine/trigger: the object's DDL (from
    `TableInfo.ddl` / synthesized via `ddl::synthesize_create_table` when
    `None`, exactly like the existing schema-tree DDL preview) rendered
    read-only, single-sided (nothing to diff against).
  - `Changed` table: a field-change table (`FieldChange` rows: field name,
    left value, right value, side by side in two columns) for the
    table-level + column/index/constraint list, PLUS a "Zobrazit DDL diff"
    button that runs `text_diff::diff_lines` over the two DDL strings
    (synthesizing where `ddl` is `None`) and renders a unified diff (+
    lines green, − lines red) — this is the "drill-down to DDL" requirement,
    reusing the exact green/red convention again rather than inventing a
    third colour scheme.
  - `Changed` routine/trigger: same DDL-diff view directly (no field table —
    the model has nothing structured to show besides the DDL itself).
- **Non-goals made explicit in the UI:** no "Apply"/"sync" button anywhere
  in this tab — the detail pane is read-only text/tables, full stop, per
  the binding constraint (compare is read-only; the sandbox Apply flow
  from G5 remains the app's only write path). See §6 for the "SQL text
  export" question.

## 4. Data diff

- **Table-pair selection:** only from within an already-open Compare tab,
  and only for tables that matched (`ObjectDiff::Changed` or `Unchanged` —
  i.e. present on both sides) AND have a detected PK on both sides
  (`ColumnInfo::is_pk`, same source `sandbox::detect_editable_pk`-style
  logic already uses for G5 — reused as a plain predicate, not the whole
  edit-detection function, since there's no read-only-connection nor
  engine-allowlist gate here: data diff is read-only so it works on
  read-only connections and any engine, including SQLite/Postgres/whatever
  drivers exist). A "Porovnat data" button appears next to a matched table
  row in the left pane; disabled with a tooltip ("tabulka nemá primární
  klíč") when either side lacks one. No table-pair *re-mapping* UI (e.g.
  diffing `orders` on the left against `orders_v2` on the right) — v1 only
  diffs same-named matched tables, consistent with the schema-diff matching
  key.
- **Fetch strategy — decision: full `SELECT` per side, streamed through the
  existing `QueryRunner`/`ResultBuffer` machinery, capped, not chunked
  key-range comparison.** Concretely: a new `QueryRunner::fetch_diff_side`
  one-shot (structurally the same as `fetch_lookup_inner` — connect, run
  `SELECT * FROM {quoted table}` — note: NOT `LIMIT`-bounded like preview
  tabs, since a diff must see the whole table or explicitly say it
  didn't) drains the stream into a `dbc_buffer::ResultBuffer`, which
  ALREADY provides the spill-to-disk-past-500k-rows/256MB behaviour this
  needs for free — no new memory-management code. Both sides fetch
  concurrently (`tokio::join!`, same as schema fetch). A hard row cap
  (`DIFF_ROW_CAP`, decision: 1,000,000 rows per side — double
  `ResultBuffer`'s own default in-memory cap, since spill absorbs the rest)
  aborts the fetch with an actionable error ("tabulka má víc než 1 000 000
  řádků — porovnání dat na tak velké tabulce zatím není podporováno; zúžsi
  výběr přes WHERE" — v1 has no WHERE-filter UI; this is a stated
  limitation, §6, not a half-built filter). This is deliberately simpler
  than a streaming/chunked merge-join over sorted key ranges (which would
  need both sides to guarantee stable ORDER BY-by-PK cursoring, adds
  driver-level pagination the `Connection` trait doesn't have, and buys
  nothing for the sizes this tool actually targets — a DB client's compare
  feature is for day-to-day dev/staging reconciliation, not billion-row
  warehouse tables).
- **PK-based row matching (`data_diff.rs`):** builds
  `HashMap<Vec<Option<String>>, usize>` (composite PK key → row index) per
  side from the PK columns' `cell_text`/`cell_is_null` (via `ResultBuffer`,
  same null-vs-empty-string distinction `sandbox.rs`'s SQL generation
  already relies on). Then: keys only in left → `Removed` (row shown as
  fetched from left); keys only in right → `Added`; keys in both →
  cell-by-cell compare over the INTERSECTION of column names present in
  both result sets (order-independent, case-sensitive name match — decision
  matches schema-diff's object-matching philosophy: no engine-driven
  casing normalization). Columns unique to one side are listed as a small
  note above the results ("sloupce jen vlevo: …" / "jen vpravo: …"), not
  folded into per-row change detection — mirrors §1's "existence vs field"
  split, applied to data.
- **Value comparison — decision: typed compare where Arrow says both
  sides' column is the same numeric/bool family, else trimmed-string
  compare.** Rationale: identical to why `sandbox.rs`'s `sql_value` treats
  `numeric_cols` specially — raw text compare would flag `"1"` vs `"1.0"`
  or `"t"`/`"true"` as changed when they're the same value differently
  formatted by two engines. Concretely: if both sides' Arrow `DataType` for
  the matched column `is_numeric()`, parse both `cell_text` results as
  `f64` and compare numerically (parse failure on either side falls back to
  string compare, never panics — same defensive posture `sandbox.rs`
  documents for its own numeric path); if both are `Boolean`, compare as
  bool; otherwise (text, date/time, everything else) compare
  `cell_text` after `.trim()` — NOT full normalization (a "same value,
  different format" date/time mismatch across engines will still show as
  Changed; called out in §6, not silently swallowed). `cell_is_null` on
  either side makes NULL-vs-NULL `Unchanged` and NULL-vs-value `Changed`
  (never treated as equal-to-empty-string).
- **Streaming/memory strategy:** as above — both sides' fetch goes through
  `ResultBuffer` (existing spill-past-cap behaviour, no new disk I/O code);
  the PK-index build and the diff pass themselves are a single in-memory
  pass over `cell_text` calls (O(rows) HashMap build + O(rows) merge), which
  is what `DIFF_ROW_CAP` exists to bound — no separate "diff-specific" spill
  path is needed because the expensive resident state is the two
  `ResultBuffer`s, which already spill.
- **Result presentation — decision: three sub-sections in the right pane
  of the SAME `CompareView` (not three separate tabs):** "Přidané řádky"
  (renders directly from the right side's `ResultBuffer`/matched row
  indices, reusing `ResultGrid` as a read-only grid — no new grid variant),
  "Odebrané řádky" (same, from the left side), "Změněné řádky" (a
  SYNTHETIC all-`Utf8` `RecordBatch` built by `data_diff.rs`: one row per
  changed PK, one column per matched result column, cell text
  `"{old} → {new}"` for a changed cell and the plain unchanged value
  otherwise, fed into a plain `ResultGrid` with a `HashSet<(row, col)>`
  side-channel — new, small, `EditState`-independent — driving yellow
  tinting on exactly the changed cells, reusing the grid's existing yellow
  convention without reusing `EditState`/sandbox machinery at all, since
  this data is never "staged for Apply"). A summary line above the three
  sections: "N přidáno, M odebráno, K změněno (z X řádků na obou stranách)".
  Row cap / truncation state (if `DIFF_ROW_CAP` was hit on fetch) shows as a
  banner, not silently.
- **DuckDB vs pure-Arrow — decision: pure-Arrow (as designed above), NOT
  routed through DuckDB.** Rationale (as requested, concrete):
  1. PK-based row matching is a hash-join, not an analytical query — SQL
     buys expressiveness the task doesn't need; a `HashMap` over
     `Vec<Option<String>>` keys is both simpler to reason about and
     trivially unit-testable without spinning up an engine.
  2. `dbc-buffer` already IS the exact streaming/spill/cell-access
     primitive this needs (see `ResultBuffer::{push,cell_text,
     cell_is_null}` — built for this class of problem already, verified by
     reading its own spill tests). Routing through DuckDB would mean
     converting `RecordBatch`es into DuckDB's own storage (via Arrow
     scan/`register`), running SQL, then converting query results back out
     into something the grid can render — three format hops to do a join
     `ResultBuffer` already lets us do with zero hops.
  3. Cross-engine value comparison (the numeric/bool normalization above)
     needs precise, auditable control over what "equal" means per typed
     column family — DuckDB's own implicit coercion across two
     differently-sourced Arrow schemas is an extra layer of "what exactly
     did the engine just decide" that works against the "errors/behaviour
     must be values, not surprises" ethos already established for the
     sandbox write path.
  4. Layering: the parallel `dbc-driver-duckdb` crate exists to let the app
     CONNECT TO a DuckDB *database* as a data source (mirrors
     `dbc-driver-sqlite`) — using it as an internal compute engine for an
     unrelated feature would blur that boundary and, worse, would make
     `dbc-diff` (or `dbc-ui`) depend on a *driver* crate outside
     `connect.rs`, directly conflicting with the binding constraint
     "`dbc-ui` never imports drivers outside connect.rs". Pure-Arrow avoids
     that entirely.
  5. Cost: DuckDB-rs is a substantial bundled dependency (compile time,
     binary size) to take on for a join the standard library already does
     well at the row counts this tool targets (§ above: ~1M rows/side cap).
  This is scoped to v1/data-diff specifically, not a blanket rejection of
  DuckDB — if a future phase needs real analytical reconciliation (fuzzy
  matching, aggregate-level diffing, windowed comparisons), that's a
  reason to revisit, not a reason to build it in now.

## 5. Task decomposition

Pure-logic tasks (T1–T4) have zero GPUI surface and are fully unit-testable
without a window; UI tasks (T5–T7) need the running app / existing test
harness patterns (`Entity`-free construction where `Tabs`/`ResultTab` already
demonstrate how to test tab logic without a window).

| # | Task | Files | Tests | Depends on |
|---|---|---|---|---|
| **T1** | `dbc-diff` crate scaffold + `ObjectDiff`/`FieldChange`/`SchemaDiff`/`TableDiff` model, `CompareMode` | `crates/dbc-diff/{Cargo.toml,src/lib.rs}`, workspace `Cargo.toml` member list | compiles; model round-trip / `Default` where useful | none |
| **T2** | `schema_diff::diff_schema` — matching + per-object-type Changed detection, same-engine vs cross-engine field gating | `crates/dbc-diff/src/schema_diff.rs` | table/column/index/constraint Added/Removed/Changed/Unchanged; routine overload split-not-paired case; cross-engine suppresses column field diff but keeps existence diff; deterministic ordering; `None`-schema non-normalization case | T1 |
| **T3** | `text_diff::diff_lines` (thin `similar` wrapper) for DDL drill-down | `crates/dbc-diff/src/text_diff.rs`, add `similar` to workspace deps | identical text → no diff lines; single-line change → one +/− pair; synthesized-vs-engine-DDL input both work (just strings in) | T1 |
| **T4** | `data_diff` — PK-index build, typed value compare, `RowDiff`/`ChangedCell` model, synthetic "old → new" batch builder | `crates/dbc-diff/src/data_diff.rs` | Added/Removed/Changed classification over hand-built `RecordBatch` pairs; numeric "1" vs "1.0" treated equal; NULL-vs-NULL equal, NULL-vs-value changed; column-set intersection when sides differ; `DIFF_ROW_CAP` exceeded → explicit error, not silent truncation | T1 |
| **T5** | `QueryRunner::fetch_schema_pair` + `fetch_diff_side` (two-leg concurrent one-shots, reusing `open_spec`) | `crates/dbc-ui/src/runner.rs` | integration test against sqlite fixtures (mirrors existing `fetch_lookup` test style, if any — else a focused new test using two `SqliteConnection`s over tempfiles) verifying both legs run and either-leg-failure surfaces independently | T1 (types only) |
| **T6** | `ModalState::CompareDialog` (connection-pair picker) + palette action "Porovnat databáze…" | `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/palette.rs` | modal open/close/dispatch wiring (reuse existing modal test patterns if present); dispatch fires `fetch_schema_pair` with correct specs/secrets | T5 |
| **T7** | `CompareView` (`dbc-ui/src/compare.rs`): left tree/list rendering with status tint + unchanged toggle, right detail pane (field table + DDL diff render), `TabContent::Compare` wiring into `tabs.rs`/`main.rs` tab-strip rendering | `crates/dbc-ui/src/compare.rs`, `crates/dbc-ui/src/tabs.rs`, `crates/dbc-ui/src/main.rs` | `Tabs`-level tests for opening/closing a Compare tab (no window needed, same style as existing `tabs.rs` tests); rendering itself verified manually (GPUI render paths aren't unit-tested elsewhere in this codebase either) | T2, T3, T6 |
| **T8** | Data-diff UI: "Porovnat data" affordance on a matched+PK'd table row, three-section result rendering (Added/Removed grids + synthetic Changed grid with cell tint), row-cap banner | `crates/dbc-ui/src/compare.rs`, possibly a small addition to `grid.rs` for the cell-tint side-channel if `ResultGrid` can't take an externally-built batch + tint set as-is | integration test over two sqlite fixture files (temp dir, small tables) driving `fetch_diff_side` + `data_diff` end-to-end and asserting the three buckets | T4, T5, T7 |

- **Parallelization:** T1 is a hard prerequisite for everything. Once T1
  lands, **T2, T3, T4 run fully in parallel** (independent files, no shared
  state, each with its own test suite — ideal for concurrent subagents).
  T5 can start in parallel with T2–T4 (only needs T1's types, not the diff
  logic itself). T6 depends on T5. T7 depends on T2+T3+T6 (needs the schema
  diff model AND the DDL-diff text AND a place to put the tab). T8 is last
  (needs T4 for the data model and T7 for a UI to hang it off).
- Suggested grouping for parallel dispatch: **Group A** = T2 (schema diff
  logic); **Group B** = T3 (text diff) + T4 (data diff logic) — small
  enough to combine, or split further if desired; **Group C** = T5 (runner
  plumbing). All three groups start together after T1. T6→T7→T8 are
  sequential UI work after Groups A–C land.

## 6. Risks / needs verification

- **`None`-schema matching (§1):** SQLite-vs-Postgres compares will show
  false Added/Removed noise for every table because SQLite snapshots never
  populate `schema`. Flagged, not fixed — a heuristic ("treat SQLite's
  `None` as Postgres's `public`") would be a guess baked into diff output;
  needs a real user call before building it. Needs verification: confirm
  with the user whether this is acceptable for v1 or must be addressed
  before ship.
- **Overload-unaware routine matching (§1):** a Postgres function with two
  overloads sharing a name will produce confusing Removed+Added pairs if
  only one overload actually changed. Low-frequency case (most user schemas
  don't overload heavily) but a real gap.
- **DDL-diff quality depends on `ddl` field / synthesis quality:** for
  engines/objects where `TableInfo.ddl`/`RoutineInfo.ddl` is `None`,
  drill-down falls back to `ddl::synthesize_create_table`, which — per its
  own doc comment — doesn't capture everything a real DDL would (e.g. it
  emits column-level `DEFAULT`/`NOT NULL`/PK but the exact fidelity vs.
  hand-written DDL hasn't been audited end-to-end for this feature's
  purposes). Worth a manual spot-check once T7 lands.
- **`1,000,000`-row data-diff cap is a guess**, not measured against actual
  memory/latency on real hardware — needs a quick benchmark once T4/T5 land
  (similar to `dbc-buffer`'s existing `push_1m` bench) to confirm the cap is
  conservative rather than already too slow in practice.
- **Value-comparison normalization (§4) is intentionally shallow**
  (numeric + bool typed compare, else trimmed string) — date/time and other
  semantically-equal-but-differently-formatted values across engines will
  false-positive as Changed. Explicitly out of scope for v1 correctness
  (documented in-UI via the section note), but worth a follow-up sizing pass
  once real cross-engine users hit it.
- **No WHERE-filter / row-range UI for data diff (§4):** the `DIFF_ROW_CAP`
  failure path tells the user to "narrow via WHERE" but v1 ships no such
  control — this is a real gap for any table over the cap, not just a nice-
  to-have; likely the first data-diff follow-up once this phase ships.
  Needs a decision on whether to pull a minimal WHERE-text-box into v1
  before merge, or explicitly accept the gap for the first release.
- **`similar` crate is a new workspace dependency** — small and pure Rust,
  but wasn't vetted against the project's existing dependency-approval bar
  (if one exists beyond what's already in `[workspace.dependencies]|`);
  flagging for the same review any other new dependency would get.
- **"Generate sync script" — explicit non-goal, not a soft SQL-text-only
  compromise:** the brief allowed either framing; this design picks the
  harder line (no SQL generation of any kind, not even copy-pasteable,
  execute-button-free text) because even inert SQL text generated from a
  cross-engine diff (§1's normalization gaps) risks being copy-pasted and
  run somewhere by a user who reasonably assumes the tool got the type
  mapping right. If the user wants the softer version (SQL-text export,
  no execute button) instead, that's a scope call to make explicitly before
  implementation, not something this design defaults into.
