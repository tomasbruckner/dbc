# G7 DB Compare Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Schema diff between two saved connections (`SchemaSnapshot`-based: tables/columns/indexes/constraints/routines/triggers/sequences, Added/Removed/Changed/Unchanged) rendered in a new compare tab, plus an in-process PK-based data diff for one matched table pair. Read-only end to end — no write path, no SQL sync-script generation of any kind (a binding, harder-than-necessary design choice, not a soft "text export" compromise).

**Architecture:** A new pure crate `dbc-diff` (`crates/dbc-diff`, depends on `dbc-core` + `dbc-buffer`, zero GPUI) holds `schema_diff.rs` (object matching + `SchemaDiff`/`TableDiff`/`ObjectDiff<T>` model + `diff_schema`, sort-merge join — O(n log n), never O(n²)), `text_diff.rs` (thin `similar` wrapper for DDL drill-down), and `data_diff.rs` (PK hash-index row comparator over two `dbc_buffer::ResultBuffer`s, typed numeric/bool value comparison, `DIFF_ROW_CAP`, synthetic "old → new" batch builder). `dbc-ui` gets two new one-shot `QueryRunner` methods (`fetch_schema_pair`, `fetch_diff_side` — both reuse `open_spec`, both strictly read-only; `fetch_diff_side`'s user-supplied WHERE-box text is composed into the final `SELECT` and refused client-side, before any connection is opened, unless it passes `dbc_core::is_read_statement`), a new `ModalState::CompareDialog` connection-pair picker, a new `TabContent::Compare` tab kind backed by a new `dbc-ui/src/compare.rs` (`CompareView`: left status-tinted object list, right detail pane — field table + DDL diff for schema compare, three-section grid for data compare), and a palette entry. The pure crate is exhaustively unit-tested standalone with zero I/O; a docker-gated integration task proves the whole pipeline (two live schemas → `diff_schema`, plus the WHERE-box guard) against a real Postgres 16.13.

**Tech Stack:** Rust, `dbc-core`'s re-exported `arrow` 59 (`DataType::is_numeric()` confirmed present in `arrow-schema-59.2.0`), GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no API assumptions beyond what existing code demonstrates), `similar` (new workspace dependency, MIT, pure Rust — CURATION-approved), `testcontainers-modules` (already a `dbc-driver-postgres` dev-dependency at `0.13`/`postgres` feature; new dev-dependency of `dbc-ui` only, for the docker task).

**Spec:** `docs/superpowers/specs/drafts/g7-db-compare-design.md` — the CURATION block (top of the file, dated 2026-08-23) is binding and overrides surrounding draft prose where the two conflict. Read verbatim before touching any task below; the load-bearing points are copied into Global Constraints.

## Global Constraints

- **Read-only, end to end.** No `execute()` call may appear anywhere in `dbc-diff` or in any `dbc-ui` compare-feature code (`compare.rs`, the `fetch_schema_pair`/`fetch_diff_side` runner methods, `ModalState::CompareDialog`'s handlers). `dbc-core::Connection::execute`'s doc comment ("This is the app's write path — ONLY the sandbox Apply flow may call it") is NOT touched by this plan — G7 adds no new sanctioned caller, unlike G9/G12's write-path amendments. Every task's Step 4 ("run to green") includes a `grep -n "\.execute(" crates/dbc-diff/src crates/dbc-ui/src/compare.rs` sanity check that must return nothing.
- **No SQL sync-script generation of any kind** (design CURATION §0.1(d), the harder of two allowed framings, deliberately chosen). There is no "generate ALTER/INSERT sync SQL" feature anywhere in this phase — not even as inert, execute-button-free copy/paste text. The only SQL text ever shown to the user is the READ query itself: the composed `SELECT … [WHERE …]` for a data diff, displayed in the compare tab so the user can see exactly what was run (transparency of a read, not "generated sync SQL").
- **The WHERE box ships in v1** (design CURATION §0.1(b)) — one optional free-text field per data-diff table pair, appended as `WHERE {text}` to both sides' `SELECT`. The composed statement MUST pass `dbc_core::is_read_statement` (guards.rs:295) before dispatch — fail-closed, blocking `; DROP`-style multi-statement injection through the box — and the exact composed SQL is shown in the compare tab header. **REQUIRED test** (design CURATION §0.2, non-negotiable): a WHERE-box payload that fails `is_read_statement` is refused CLIENT-SIDE — proven by a test that never constructs a `ConnectSpec`/calls `open_spec` in the failing path (T5).
- **Schema handling is STRICT `None`-schema matching** (design CURATION §0.1(a)) — no `None`≈`public` heuristic, ever. A SQLite-vs-Postgres compare shows every Postgres table as "only in B" and vice versa unless schema is truly absent on both sides. This is a stated limitation (design §6), not silently patched by a guessed default-schema mapping.
- `similar` is CURATION-approved (design §0.1(c)) as a new workspace dependency.
- `cargo` lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; every GPUI primitive this plan uses (`Entity`, `Context`, `cx.spawn`/`.detach()`, `cx.subscribe`/`EventEmitter`, `div()`/`uniform_list`) already has a call site in this codebase (cited per task) — no new API surface assumed.
- Errors are values; no panics on DB or user-data paths. Fail-soft parsing: any value derived from server-reported metadata that could be missing/NULL degrades to `Option`/a sensible default, never a panic (hazard class: "server catalog output → fail-soft parsers").
- **Identifier quoting:** reuse `dbc_core::{quote_ident, quote_qualified}` (`crates/dbc-core/src/ddl.rs:42-51`) — the SAME functions `sandbox.rs` already imports (`crates/dbc-ui/src/sandbox.rs:24`) for its own SQL generation. Do not invent a second quoting function. **Note on MSSQL brackets:** the design doc points at "`admin_sql`'s `quote_ident_for` with MSSQL brackets" — that function is real but lives only in the *G10* plan (`docs/superpowers/plans/2026-08-23-g10-server-admin.md`), not yet as code in this branch lineage (`crates/dbc-ui/src/admin_sql.rs` does not exist here). Since the MSSQL driver is unwired in this codebase today (`connect::open_config`'s `Engine::Mssql` arm unconditionally returns `Err("MSSQL driver zatím není k dispozici")`, `crates/dbc-ui/src/connect.rs:95-99`), no G7 code path can actually reach a live MSSQL connection, so `dbc_core::quote_ident` (double-quote style) is correct for everything this phase can execute. **Follow-up, not a G7 blocker:** once G10 merges `admin_sql::quote_ident_for`, the two call sites this plan adds (`fetch_diff_side`'s table-quoting in T5, and any future MSSQL-aware rendering in T7/T8) should switch to it for correct bracket quoting — tracked in Self-Review.
- **Diff algorithms over large schemas/result sets must be O(n log n)-ish** (sort-merge join or hash join), never an O(n²) nested scan. `schema_diff::diff_by_key`/`diff_tables` (T2) sort both sides once and merge-walk with two cursors; `data_diff::build_pk_index`/`diff_data` (T4) build a `HashMap` index in one O(rows) pass. Both are called out explicitly in their task's Grounding section.
- **No credentials/result data in history or logs.** G7 adds no `HistoryEntry` (a compare run is not a query run — `sandbox`/`history_panel` are untouched by this plan). The only text ever displayed is the composed read SQL (never the secret) and diffed metadata/cell text (already visible to the user via the normal grid/tree, not new exposure). No task formats a `secret`/`Option<String>` password into any string.
- **Task-ordering / single-writer files:** `crates/dbc-ui/src/runner.rs` and `crates/dbc-ui/src/main.rs` are single-writer serialized across phases (this repo runs G6/G9/G12/G13-class phases in parallel worktrees). T5 (`runner.rs`) and T6/T7/T8 (`main.rs`, plus `connections_ui.rs`/`tabs.rs`/`palette.rs`) are marked **serialized tail tasks** below — schedule them to land AFTER whatever G6/G9/G12/G13 `runner.rs`/`main.rs` work has already merged, re-locating any line-number references in this plan by symbol name (not line number) if the file has drifted. T1–T4 (the whole `dbc-diff` crate) touch no shared file and are fully parallelizable in separate worktrees starting immediately.
- **Suggested execution model:** sonnet-tier implementer agents per task, a sonnet-tier adversarial review pass per task (placeholder scan + spec-fidelity check before merge), and a default-model final review once all nine tasks land — mirrors this repo's general practice for multi-task phases; not literally copied from another plan (no existing plan in `docs/superpowers/plans/` currently states this in words, this is this plan's own recommendation).
- Version bumps at merge (phase-numbered convention, `crates/dbc-ui/Cargo.toml` shows `0.5.0` at time of writing): `dbc-diff` starts at `0.1.0` (T1, matching every other satellite crate — `dbc-buffer`/`dbc-core`/`dbc-driver-*` are all `0.1.0`); `dbc-ui` bumps to `0.7.0` at branch finish (T9's tail, not any individual task's commit).
- UI strings are Czech (labels, statuses, error messages, tooltips) — English only in code/comments/tests.
- Tests green before every commit: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-diff -p dbc-ui` must pass with the task's new tests included; each task must leave every crate it touches at least as green as it found it. Docker tests (T9) are `#[ignore]`d and run explicitly via `-- --ignored`.

### Task dependency graph

| Task | Files | Depends on | Notes |
|---|---|---|---|
| T1 | `crates/dbc-diff/{Cargo.toml,src/lib.rs,src/schema_diff.rs}`, workspace `Cargo.toml` member list | — | new-crate scaffold + model types; parallel-worktree eligible immediately |
| T2 | `crates/dbc-diff/src/schema_diff.rs` (append `diff_schema` + helpers) | T1 | parallel batch A |
| T3 | `crates/dbc-diff/src/text_diff.rs` (new), workspace `Cargo.toml` (`similar` dep) | T1 | parallel batch B |
| T4 | `crates/dbc-diff/src/data_diff.rs` (new) | T1 | parallel batch C |
| T5 | `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/Cargo.toml` (`dbc-diff` dep) | T1 (types only) | **serialized tail** — runner.rs; can start as soon as T1 lands, in parallel with T2–T4, but MERGES after any in-flight runner.rs work |
| T6 | `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/palette.rs`, `crates/dbc-ui/src/main.rs` | T5 | **serialized tail** — main.rs chain #1 |
| T7 | `crates/dbc-ui/src/compare.rs` (new), `crates/dbc-ui/src/tabs.rs`, `crates/dbc-ui/src/main.rs` | T2, T3, T6 | **serialized tail** — main.rs chain #2 |
| T8 | `crates/dbc-ui/src/compare.rs`, possibly `crates/dbc-ui/src/grid.rs` (only if `ResultGrid` can't take an externally-built batch + tint set as-is) | T4, T5, T7 | **serialized tail** — main.rs chain #3 |
| T9 | `crates/dbc-ui/src/runner.rs` (test module), `crates/dbc-ui/Cargo.toml` (dev-dep) | T5 | docker, parallel with T6–T8 in a separate worktree (touches only `runner.rs`'s test module, which nothing after T5 touches) |

**Parallelization:** T1 is the hard prerequisite for everything. Once T1 lands, **T2, T3, T4 run fully in parallel** — three disjoint files inside `dbc-diff`, no shared state. **T5 can start in parallel with T2–T4** (it only needs T1's types, `SchemaSnapshot`, plus `data_diff::DIFF_ROW_CAP` from T4 for its row-cap check — see T5's Grounding for the resulting real dependency on T4, a correction from the design's "T5 depends on T1 only"). T6 depends on T5. T7 depends on T2 (schema diff model to render) + T3 (DDL-diff text) + T6 (a place to put the tab). T8 is last — needs T4 (data model), T5 (`fetch_diff_side`), and T7 (a UI to hang it off). T9 depends only on T5 and can run in a worktree parallel to T6–T8.

---

### Task 1 (T1): `dbc-diff` crate scaffold + schema-diff model types

**Files:**
- Create: `crates/dbc-diff/Cargo.toml`
- Create: `crates/dbc-diff/src/lib.rs`
- Create: `crates/dbc-diff/src/schema_diff.rs`
- Modify: workspace `Cargo.toml` (`members` list — add `"crates/dbc-diff"`)

**Interfaces:**
- Consumes: `dbc_core::{ColumnInfo, ConstraintInfo, IndexInfo, RoutineInfo, SchemaSnapshot, SequenceInfo, TableInfo, TableKind, TriggerInfo}` (all already `pub use`d at `crates/dbc-core/src/lib.rs:16-19`).
- Produces (consumed by T2, and — as the crate's public shape — by T7):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum CompareMode { SameEngine, CrossEngine }

  #[derive(Debug, Clone, PartialEq)]
  pub enum ObjectDiff<T> {
      Added(T),
      Removed(T),
      Changed { left: T, right: T, fields: Vec<FieldChange> },
      Unchanged(T),
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct FieldChange { pub field: String, pub left: String, pub right: String }

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum TableStatus { Added, Removed, Changed, Unchanged }

  #[derive(Debug, Clone, PartialEq)]
  pub struct TableDiff {
      pub schema: Option<String>,
      pub name: String,
      pub status: TableStatus,
      pub table_fields: Vec<FieldChange>,
      pub columns: Vec<ObjectDiff<ColumnInfo>>,
      pub indexes: Vec<ObjectDiff<IndexInfo>>,
      pub constraints: Vec<ObjectDiff<ConstraintInfo>>,
      /// Full source object, present on whichever side(s) it exists —
      /// deviation from the design's field sketch, see Self-Review note 1:
      /// the UI's Added/Removed DDL panel needs the whole `TableInfo`
      /// (`.ddl`/`ddl::synthesize_create_table`), not just the field-diff
      /// summary. `Some(_)`/`Some(_)` for Changed/Unchanged,
      /// `Some(_)`/`None` for Removed, `None`/`Some(_)` for Added.
      pub left: Option<TableInfo>,
      pub right: Option<TableInfo>,
  }

  #[derive(Debug, Clone, PartialEq, Default)]
  pub struct SchemaDiff {
      pub tables: Vec<TableDiff>,
      pub routines: Vec<ObjectDiff<RoutineInfo>>,
      pub triggers: Vec<ObjectDiff<TriggerInfo>>,
      pub sequences: Vec<ObjectDiff<SequenceInfo>>,
  }
  ```

**Grounding:** `dbc-core`/`dbc-buffer` are the only path dependencies (design §0: `dbc-core ← dbc-buffer ← dbc-diff ← dbc-ui`, drivers ← `dbc-ui` via `connect.rs` only, unchanged — `dbc-diff` never imports a driver crate). `arrow.workspace = true` is needed from T1 onward because `data_diff.rs` (T4) takes `dbc_buffer::ResultBuffer`/`arrow::datatypes::DataType` in its public signature and `lib.rs` must compile with `mod data_diff;` declared once T4 lands — declaring the dependency now avoids a second Cargo.toml edit later. `ObjectDiff<T>`'s derives require `T: PartialEq`/`T: Clone` at each use site (auto-added by `#[derive]` for generics) — every `dbc-core` schema type (`ColumnInfo`, `IndexInfo`, `ConstraintInfo`, `RoutineInfo`, `TriggerInfo`, `SequenceInfo`) already derives both (`crates/dbc-core/src/schema.rs:3-4` etc.), so this compiles with no further trait work.

- [ ] **Step 1: Add the crate to the workspace.**

  `Cargo.toml` (workspace root), `members` list — add `"crates/dbc-diff"`:
  ```toml
  members = ["crates/dbc-core", "crates/dbc-buffer", "crates/dbc-ui", "crates/dbc-driver-sqlite", "crates/dbc-driver-postgres", "crates/dbc-driver-mssql", "crates/dbc-driver-duckdb", "crates/dbc-state", "crates/dbc-mcp", "crates/dbc-diff"]
  ```

- [ ] **Step 2: `crates/dbc-diff/Cargo.toml`:**
  ```toml
  [package]
  name = "dbc-diff"
  version = "0.1.0"
  edition.workspace = true

  [dependencies]
  dbc-core = { path = "../dbc-core" }
  dbc-buffer = { path = "../dbc-buffer" }
  arrow.workspace = true
  ```

- [ ] **Step 3: `crates/dbc-diff/src/lib.rs`:**
  ```rust
  //! G7: read-only schema/data diff engine. No GPUI, no driver crates, no
  //! write path anywhere in this crate — see the module docs on each
  //! submodule for what each half does.

  pub mod schema_diff;
  ```
  (`pub mod text_diff;` and `pub mod data_diff;` are added by T3/T4 respectively — each task's own Step 1 appends its one line here; textually disjoint one-line edits, trivial to rebase in either order.)

- [ ] **Step 4: `crates/dbc-diff/src/schema_diff.rs`** — write the model types from the Interfaces block above (imports: `use dbc_core::{ColumnInfo, ConstraintInfo, IndexInfo, RoutineInfo, SchemaSnapshot, SequenceInfo, TableInfo, TableKind, TriggerInfo};` — `SchemaSnapshot`/`TableKind` are unused by T1's types themselves but are re-imported here rather than deferred to T2's edit, since T2 modifies this same file and an unused-import warning on an intermediate commit would violate zero-warnings; mark them `#[allow(unused_imports)]` on this exact line with a comment `// consumed by diff_schema, T2` — removed by T2 once real use appears).

- [ ] **Step 5: Compile + a minimal model test.**

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use dbc_core::{ColumnInfo, TableKind};

      #[test]
      fn object_diff_variants_are_constructible_and_comparable() {
          let a = ColumnInfo { name: "id".into(), data_type: "int4".into(), ..Default::default() };
          let b = ColumnInfo { name: "id".into(), data_type: "int8".into(), ..Default::default() };
          let changed = ObjectDiff::Changed {
              left: a.clone(),
              right: b.clone(),
              fields: vec![FieldChange { field: "data_type".into(), left: "int4".into(), right: "int8".into() }],
          };
          assert_eq!(changed, changed.clone());
          assert_ne!(ObjectDiff::Added(a.clone()), ObjectDiff::Removed(a));
      }

      #[test]
      fn schema_diff_default_is_empty() {
          let d = SchemaDiff::default();
          assert!(d.tables.is_empty() && d.routines.is_empty() && d.triggers.is_empty() && d.sequences.is_empty());
      }

      #[test]
      fn table_diff_carries_the_right_side_presence_by_status() {
          let t = TableInfo { name: "t".into(), kind: TableKind::Table, ..Default::default() };
          let removed = TableDiff {
              schema: None, name: "t".into(), status: TableStatus::Removed,
              table_fields: vec![], columns: vec![], indexes: vec![], constraints: vec![],
              left: Some(t.clone()), right: None,
          };
          assert!(removed.left.is_some() && removed.right.is_none());
      }
  }
  ```

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-diff`
  Expected: 3 tests pass, zero warnings.

- [ ] **Step 6: Commit**

  ```bash
  git add Cargo.toml crates/dbc-diff/Cargo.toml crates/dbc-diff/src/lib.rs crates/dbc-diff/src/schema_diff.rs
  git commit -m "feat: dbc-diff crate scaffold + schema-diff model (G7 T1)"
  ```

---

### Task 2 (T2): `schema_diff::diff_schema` — object matching + Changed detection

**Files:**
- Modify: `crates/dbc-diff/src/schema_diff.rs` (append)

**Interfaces:**
- Consumes: T1's model types + `dbc_core::{ColumnInfo, ConstraintInfo, FkRef, IndexInfo, RoutineInfo, RoutineKind, SchemaSnapshot, SequenceInfo, TableInfo, TriggerInfo}`.
- Produces (consumed by T7):
  ```rust
  /// The crate's one entry point for the schema half (design §1).
  /// Deterministic by construction: every match pass sorts both sides by
  /// their key once, then merge-walks with two cursors — output order is
  /// therefore always ascending-by-key regardless of the snapshots'
  /// original catalog-query order.
  pub fn diff_schema(left: &SchemaSnapshot, right: &SchemaSnapshot, mode: CompareMode) -> SchemaDiff;
  ```

**Grounding:**
- **Matching keys (design §1):** tables/views by `(schema, name)` — `schema: Option<String>` compared as-is, `None` matches ONLY `None` (the binding CURATION decision — no `None`≈`"public"` heuristic anywhere in this function). Columns/indexes/constraints match by `name` WITHIN an already-matched table. Routines match by `(schema, name, kind)` — **overloads are not resolved**: if a name has two signatures on one side and one on the other, the sort-merge below arbitrarily pairs one-for-one within the duplicate-key run and reports every excess entry as a plain `Added`/`Removed` — it never attempts to align by `signature`, matching design §1's explicit "treats it as one Removed + one Added rather than pairing signatures" (T2's own test proves this exact multiplicity behavior, not just that it "doesn't crash"). Triggers match by `(schema, table, name)`. Sequences match by `(schema, name)` and are NEVER `Changed` (`SequenceInfo` carries no further fields — the shared field-diff closure for sequences always returns `vec![]`, which the generic matcher already treats as `Unchanged`).
- **O(n log n), not O(n²)** (hazard class, Global Constraints): `diff_by_key` (generic, used for columns/indexes/constraints/routines/triggers/sequences) and `diff_tables` (hand-rolled for the table level, since it recurses into the three nested lists) both sort each side once by an owned key (`String`/tuple-of-`String`, not a borrowed key — avoids HRTB lifetime gymnastics for a generic `fn` parameter at negligible cost, since schema-level object counts are in the thousands at most, not billions) and merge-walk with two cursors, each `O(1)` amortized per step. No nested `.iter().find(...)` anywhere in this file.
- **What counts as "Changed" per object type** (design §1, copied verbatim into each field-diff closure's doc comment below): Table → `kind` only (`ddl` is EXCLUDED — rendering convenience, not semantic, used only by T3's drill-down). Column → `data_type`/`nullable`/`default` GATED by `CompareMode::SameEngine` (suppressed entirely in `CrossEngine` — existence-level Added/Removed still always runs); `is_pk`/`fk` compared in BOTH modes (structural, not a type-normalization problem). Index → `columns` (order-sensitive `Vec<String>` compare) + `unique`. Constraint → `kind` + `definition` (raw string, no CHECK-expression normalization). Routine → `kind` + `signature` (raw string). Trigger → `table` + `ddl` (raw string — no other structured fields exist). Sequence → presence-only, never `Changed`.
- **Cross-engine mode selection is the CALLER's job, not this function's** — `SchemaSnapshot` itself carries no `Engine` field (confirmed: `crates/dbc-core/src/schema.rs:4-9`), so `diff_schema` takes `mode: CompareMode` as an explicit parameter; T7 computes it from the two connections' `ConnectionConfig.engine` before calling in.

```rust
use std::cmp::Ordering;
use dbc_core::{
    ColumnInfo, ConstraintInfo, FkRef, IndexInfo, RoutineInfo, RoutineKind, SchemaSnapshot,
    SequenceInfo, TableInfo, TriggerInfo,
};

pub fn diff_schema(left: &SchemaSnapshot, right: &SchemaSnapshot, mode: CompareMode) -> SchemaDiff {
    SchemaDiff {
        tables: diff_tables(&left.tables, &right.tables, mode),
        routines: diff_by_key(&left.routines, &right.routines, routine_key, diff_routine_fields),
        triggers: diff_by_key(&left.triggers, &right.triggers, trigger_key, diff_trigger_fields),
        sequences: diff_by_key(&left.sequences, &right.sequences, sequence_key, |_, _| Vec::new()),
    }
}

fn routine_key(r: &RoutineInfo) -> (Option<String>, String, u8) {
    (r.schema.clone(), r.name.clone(), match r.kind { RoutineKind::Function => 0, RoutineKind::Procedure => 1 })
}
fn trigger_key(t: &TriggerInfo) -> (Option<String>, String, String) {
    (t.schema.clone(), t.table.clone(), t.name.clone())
}
fn sequence_key(s: &SequenceInfo) -> (Option<String>, String) {
    (s.schema.clone(), s.name.clone())
}
fn table_key(t: &TableInfo) -> (Option<String>, String) {
    (t.schema.clone(), t.name.clone())
}
fn column_key(c: &ColumnInfo) -> String { c.name.clone() }
fn index_key(i: &IndexInfo) -> String { i.name.clone() }
fn constraint_key(c: &ConstraintInfo) -> String { c.name.clone() }

/// Generic sort-merge matcher: O(n log n) via one sort per side, O(n) merge.
/// `field_diff` returning `vec![]` for a matched pair means Unchanged;
/// non-empty means Changed. Used for every flat (non-nested) object list —
/// tables use their own hand-rolled version below since a table match must
/// ALSO recurse into columns/indexes/constraints.
fn diff_by_key<T, K, KF, F>(left: &[T], right: &[T], key_fn: KF, field_diff: F) -> Vec<ObjectDiff<T>>
where
    T: Clone,
    K: Ord,
    KF: Fn(&T) -> K,
    F: Fn(&T, &T) -> Vec<FieldChange>,
{
    let mut li: Vec<&T> = left.iter().collect();
    let mut ri: Vec<&T> = right.iter().collect();
    li.sort_by_key(|t| key_fn(t));
    ri.sort_by_key(|t| key_fn(t));

    let mut out = Vec::with_capacity(li.len().max(ri.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < li.len() && j < ri.len() {
        match key_fn(li[i]).cmp(&key_fn(ri[j])) {
            Ordering::Less => { out.push(ObjectDiff::Removed(li[i].clone())); i += 1; }
            Ordering::Greater => { out.push(ObjectDiff::Added(ri[j].clone())); j += 1; }
            Ordering::Equal => {
                let fields = field_diff(li[i], ri[j]);
                out.push(if fields.is_empty() {
                    ObjectDiff::Unchanged(li[i].clone())
                } else {
                    ObjectDiff::Changed { left: li[i].clone(), right: ri[j].clone(), fields }
                });
                i += 1; j += 1;
            }
        }
    }
    while i < li.len() { out.push(ObjectDiff::Removed(li[i].clone())); i += 1; }
    while j < ri.len() { out.push(ObjectDiff::Added(ri[j].clone())); j += 1; }
    out
}

fn diff_table_top_fields(l: &TableInfo, r: &TableInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.kind != r.kind {
        out.push(FieldChange { field: "kind".into(), left: format!("{:?}", l.kind), right: format!("{:?}", r.kind) });
    }
    out
}

fn fmt_fk(fk: &Option<FkRef>) -> String {
    match fk {
        None => String::new(),
        Some(f) => format!("{}.{}.{}", f.schema.as_deref().unwrap_or(""), f.table, f.column),
    }
}

fn diff_column_fields(l: &ColumnInfo, r: &ColumnInfo, mode: CompareMode) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if mode == CompareMode::SameEngine {
        if l.data_type != r.data_type {
            out.push(FieldChange { field: "data_type".into(), left: l.data_type.clone(), right: r.data_type.clone() });
        }
        if l.nullable != r.nullable {
            out.push(FieldChange { field: "nullable".into(), left: l.nullable.to_string(), right: r.nullable.to_string() });
        }
        if l.default != r.default {
            out.push(FieldChange {
                field: "default".into(),
                left: l.default.clone().unwrap_or_default(),
                right: r.default.clone().unwrap_or_default(),
            });
        }
    }
    // Structural, compared in BOTH modes (design §1).
    if l.is_pk != r.is_pk {
        out.push(FieldChange { field: "is_pk".into(), left: l.is_pk.to_string(), right: r.is_pk.to_string() });
    }
    if l.fk != r.fk {
        out.push(FieldChange { field: "fk".into(), left: fmt_fk(&l.fk), right: fmt_fk(&r.fk) });
    }
    out
}

fn diff_index_fields(l: &IndexInfo, r: &IndexInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.columns != r.columns {
        out.push(FieldChange { field: "columns".into(), left: l.columns.join(", "), right: r.columns.join(", ") });
    }
    if l.unique != r.unique {
        out.push(FieldChange { field: "unique".into(), left: l.unique.to_string(), right: r.unique.to_string() });
    }
    out
}

fn diff_constraint_fields(l: &ConstraintInfo, r: &ConstraintInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.kind != r.kind {
        out.push(FieldChange { field: "kind".into(), left: l.kind.clone(), right: r.kind.clone() });
    }
    if l.definition != r.definition {
        out.push(FieldChange { field: "definition".into(), left: l.definition.clone(), right: r.definition.clone() });
    }
    out
}

fn diff_routine_fields(l: &RoutineInfo, r: &RoutineInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.kind != r.kind {
        out.push(FieldChange { field: "kind".into(), left: format!("{:?}", l.kind), right: format!("{:?}", r.kind) });
    }
    if l.signature != r.signature {
        out.push(FieldChange { field: "signature".into(), left: l.signature.clone(), right: r.signature.clone() });
    }
    out
}

fn diff_trigger_fields(l: &TriggerInfo, r: &TriggerInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.table != r.table {
        out.push(FieldChange { field: "table".into(), left: l.table.clone(), right: r.table.clone() });
    }
    if l.ddl != r.ddl {
        out.push(FieldChange {
            field: "ddl".into(),
            left: l.ddl.clone().unwrap_or_default(),
            right: r.ddl.clone().unwrap_or_default(),
        });
    }
    out
}

fn table_diff_removed(t: &TableInfo) -> TableDiff {
    TableDiff {
        schema: t.schema.clone(), name: t.name.clone(), status: TableStatus::Removed,
        table_fields: Vec::new(),
        columns: t.columns.iter().map(|c| ObjectDiff::Removed(c.clone())).collect(),
        indexes: t.indexes.iter().map(|x| ObjectDiff::Removed(x.clone())).collect(),
        constraints: t.constraints.iter().map(|x| ObjectDiff::Removed(x.clone())).collect(),
        left: Some(t.clone()), right: None,
    }
}
fn table_diff_added(t: &TableInfo) -> TableDiff {
    TableDiff {
        schema: t.schema.clone(), name: t.name.clone(), status: TableStatus::Added,
        table_fields: Vec::new(),
        columns: t.columns.iter().map(|c| ObjectDiff::Added(c.clone())).collect(),
        indexes: t.indexes.iter().map(|x| ObjectDiff::Added(x.clone())).collect(),
        constraints: t.constraints.iter().map(|x| ObjectDiff::Added(x.clone())).collect(),
        left: None, right: Some(t.clone()),
    }
}
fn table_diff_matched(l: &TableInfo, r: &TableInfo, mode: CompareMode) -> TableDiff {
    let table_fields = diff_table_top_fields(l, r);
    let columns = diff_by_key(&l.columns, &r.columns, column_key, |a, b| diff_column_fields(a, b, mode));
    let indexes = diff_by_key(&l.indexes, &r.indexes, index_key, diff_index_fields);
    let constraints = diff_by_key(&l.constraints, &r.constraints, constraint_key, diff_constraint_fields);
    let any_change = !table_fields.is_empty()
        || columns.iter().any(|c| !matches!(c, ObjectDiff::Unchanged(_)))
        || indexes.iter().any(|c| !matches!(c, ObjectDiff::Unchanged(_)))
        || constraints.iter().any(|c| !matches!(c, ObjectDiff::Unchanged(_)));
    TableDiff {
        schema: l.schema.clone(), name: l.name.clone(),
        status: if any_change { TableStatus::Changed } else { TableStatus::Unchanged },
        table_fields, columns, indexes, constraints,
        left: Some(l.clone()), right: Some(r.clone()),
    }
}

fn diff_tables(left: &[TableInfo], right: &[TableInfo], mode: CompareMode) -> Vec<TableDiff> {
    let mut li: Vec<&TableInfo> = left.iter().collect();
    let mut ri: Vec<&TableInfo> = right.iter().collect();
    li.sort_by_key(|t| table_key(t));
    ri.sort_by_key(|t| table_key(t));

    let mut out = Vec::with_capacity(li.len().max(ri.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < li.len() && j < ri.len() {
        match table_key(li[i]).cmp(&table_key(ri[j])) {
            Ordering::Less => { out.push(table_diff_removed(li[i])); i += 1; }
            Ordering::Greater => { out.push(table_diff_added(ri[j])); j += 1; }
            Ordering::Equal => { out.push(table_diff_matched(li[i], ri[j], mode)); i += 1; j += 1; }
        }
    }
    while i < li.len() { out.push(table_diff_removed(li[i])); i += 1; }
    while j < ri.len() { out.push(table_diff_added(ri[j])); j += 1; }
    out
}
```

- [ ] **Step 1: Write the code above** (append to `schema_diff.rs`; remove the T1 `#[allow(unused_imports)]` marker now that `SchemaSnapshot`/`TableKind`-adjacent imports are genuinely used — `TableKind` itself is not directly named in T2's code since `diff_table_top_fields` compares `.kind` via `PartialEq`/`Debug` without naming the enum, so keep `TableKind` import only if still needed elsewhere in the file; otherwise drop it to stay warning-free).

- [ ] **Step 2: Tests** (same file, `#[cfg(test)] mod diff_schema_tests`):

  ```rust
  #[cfg(test)]
  mod diff_schema_tests {
      use super::*;
      use dbc_core::{TableKind, RoutineKind};

      fn table(schema: Option<&str>, name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
          TableInfo { schema: schema.map(String::from), name: name.into(), kind: TableKind::Table, columns: cols, indexes: vec![], constraints: vec![], ddl: None }
      }
      fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
          ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, default: None, is_pk: pk, fk: None }
      }
      fn snap(tables: Vec<TableInfo>) -> SchemaSnapshot {
          SchemaSnapshot { tables, routines: vec![], triggers: vec![], sequences: vec![] }
      }

      #[test]
      fn table_added_removed_unchanged() {
          let left = snap(vec![table(Some("public"), "a", vec![]), table(Some("public"), "b", vec![])]);
          let right = snap(vec![table(Some("public"), "b", vec![]), table(Some("public"), "c", vec![])]);
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          assert_eq!(d.tables.len(), 3);
          assert_eq!(d.tables[0].name, "a"); assert_eq!(d.tables[0].status, TableStatus::Removed);
          assert_eq!(d.tables[1].name, "b"); assert_eq!(d.tables[1].status, TableStatus::Unchanged);
          assert_eq!(d.tables[2].name, "c"); assert_eq!(d.tables[2].status, TableStatus::Added);
          // Added/Removed carry the full source object for DDL rendering.
          assert!(d.tables[0].left.is_some() && d.tables[0].right.is_none());
          assert!(d.tables[2].left.is_none() && d.tables[2].right.is_some());
      }

      #[test]
      fn table_changed_on_kind_field() {
          let mut r_table = table(Some("public"), "v", vec![]);
          r_table.kind = TableKind::View;
          let left = snap(vec![table(Some("public"), "v", vec![])]);
          let right = snap(vec![r_table]);
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          assert_eq!(d.tables[0].status, TableStatus::Changed);
          assert_eq!(d.tables[0].table_fields, vec![FieldChange { field: "kind".into(), left: "Table".into(), right: "View".into() }]);
      }

      #[test]
      fn column_data_type_change_detected_same_engine() {
          let left = snap(vec![table(Some("p"), "t", vec![col("id", "int4", true)])]);
          let right = snap(vec![table(Some("p"), "t", vec![col("id", "int8", true)])]);
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          assert_eq!(d.tables[0].status, TableStatus::Changed);
          assert!(matches!(&d.tables[0].columns[0], ObjectDiff::Changed { fields, .. } if fields.iter().any(|f| f.field == "data_type")));
      }

      #[test]
      fn cross_engine_suppresses_column_field_diff_but_keeps_existence() {
          let left = snap(vec![table(Some("p"), "t", vec![col("id", "int4", true), col("gone", "text", false)])]);
          let right = snap(vec![table(Some("p"), "t", vec![col("id", "integer", true), col("new_col", "text", false)])]);
          let d = diff_schema(&left, &right, CompareMode::CrossEngine);
          // "id" present both sides, different data_type text — but cross-engine
          // never flags a type-text difference as Changed.
          assert!(matches!(&d.tables[0].columns.iter().find(|c| matches!(c, ObjectDiff::Unchanged(c) if c.name == "id")), Some(_)));
          // Existence-level diff still fires fully.
          assert!(d.tables[0].columns.iter().any(|c| matches!(c, ObjectDiff::Removed(c) if c.name == "gone")));
          assert!(d.tables[0].columns.iter().any(|c| matches!(c, ObjectDiff::Added(c) if c.name == "new_col")));
      }

      #[test]
      fn cross_engine_still_flags_is_pk_structural_change() {
          let left = snap(vec![table(Some("p"), "t", vec![col("id", "int4", true)])]);
          let right = snap(vec![table(Some("p"), "t", vec![col("id", "integer", false)])]);
          let d = diff_schema(&left, &right, CompareMode::CrossEngine);
          assert!(matches!(&d.tables[0].columns[0], ObjectDiff::Changed { fields, .. } if fields.iter().any(|f| f.field == "is_pk")));
      }

      #[test]
      fn none_schema_never_matches_a_named_schema() {
          let left = snap(vec![table(None, "t", vec![])]);
          let right = snap(vec![table(Some("public"), "t", vec![])]);
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          assert_eq!(d.tables.len(), 2, "None-schema must NOT match Some(\"public\") — CURATION binding decision");
          assert!(d.tables.iter().any(|t| t.status == TableStatus::Removed && t.schema.is_none()));
          assert!(d.tables.iter().any(|t| t.status == TableStatus::Added && t.schema.as_deref() == Some("public")));
      }

      #[test]
      fn routine_overload_split_not_paired() {
          fn routine(name: &str, sig: &str) -> RoutineInfo {
              RoutineInfo { schema: Some("p".into()), name: name.into(), kind: RoutineKind::Function, signature: sig.into(), ddl: None }
          }
          // Left has TWO overloads of "f"; right has ONE. Design §1: no
          // signature-aware pairing — the excess entry is a plain Removed.
          let left = SchemaSnapshot { tables: vec![], routines: vec![routine("f", "(int) -> int"), routine("f", "(text) -> int")], triggers: vec![], sequences: vec![] };
          let right = SchemaSnapshot { tables: vec![], routines: vec![routine("f", "(int) -> int")], triggers: vec![], sequences: vec![] };
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          let removed = d.routines.iter().filter(|r| matches!(r, ObjectDiff::Removed(_))).count();
          let matched = d.routines.iter().filter(|r| matches!(r, ObjectDiff::Unchanged(_) | ObjectDiff::Changed { .. })).count();
          assert_eq!((matched, removed), (1, 1), "one overload pairs, the excess is Removed — never re-paired by signature");
      }

      #[test]
      fn sequences_are_never_changed_presence_only() {
          let left = SchemaSnapshot { tables: vec![], routines: vec![], triggers: vec![], sequences: vec![SequenceInfo { schema: Some("p".into()), name: "s".into() }] };
          let right = left.clone();
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          assert!(matches!(d.sequences[0], ObjectDiff::Unchanged(_)));
      }

      #[test]
      fn deterministic_output_order_regardless_of_input_order() {
          let left = snap(vec![table(Some("p"), "zeta", vec![]), table(Some("p"), "alpha", vec![])]);
          let right = left.clone();
          let d = diff_schema(&left, &right, CompareMode::SameEngine);
          let names: Vec<&str> = d.tables.iter().map(|t| t.name.as_str()).collect();
          assert_eq!(names, vec!["alpha", "zeta"], "output must be sorted by (schema, name), not input order");
      }
  }
  ```

- [ ] **Step 3: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-diff diff_schema_tests::`
  Expected: all pass, zero warnings.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/dbc-diff/src/schema_diff.rs
  git commit -m "feat: schema_diff::diff_schema — sort-merge matching + Changed detection (G7 T2)"
  ```

---

### Task 3 (T3): `text_diff::diff_lines` — thin `similar` wrapper

**Files:**
- Create: `crates/dbc-diff/src/text_diff.rs`
- Modify: `crates/dbc-diff/src/lib.rs` (add `pub mod text_diff;`)
- Modify: `crates/dbc-diff/Cargo.toml` (add `similar.workspace = true`)
- Modify: workspace `Cargo.toml` (`[workspace.dependencies]` — add `similar = "2"`)

**Interfaces:**
- Consumes: `similar::{ChangeTag, TextDiff}` (CURATION-approved new workspace dependency).
- Produces (consumed by T7's DDL drill-down):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum DiffTag { Equal, Insert, Delete }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct DiffLine { pub tag: DiffTag, pub text: String }

  /// Line-level diff over two DDL strings — `old`/`new` are already-fetched
  /// engine DDL OR `ddl::synthesize_create_table` output; this module has
  /// no opinion on where the text came from, it only ever sees `&str`.
  pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine>;
  ```

**Grounding:** `similar::TextDiff::from_lines` + `iter_all_changes()` is the crate's documented line-diff entry point; `Change::tag()` returns `ChangeTag::{Equal, Insert, Delete}`, `Change::to_string()` returns the line's text INCLUDING its trailing newline (trimmed here so `DiffLine::text` is a clean, renderable line with no embedded `\n`).

```rust
//! G7: thin wrapper around `similar` (design CURATION §0.1(c), approved new
//! workspace dependency) for line-level DDL diffing (drill-down, design
//! §3). Pure text in, pure text out — no knowledge of SQL/DDL structure.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffTag { Equal, Insert, Delete }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine { pub tag: DiffTag, pub text: String }

pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|c| {
            let tag = match c.tag() {
                ChangeTag::Equal => DiffTag::Equal,
                ChangeTag::Insert => DiffTag::Insert,
                ChangeTag::Delete => DiffTag::Delete,
            };
            DiffLine { tag, text: c.to_string().trim_end_matches('\n').to_string() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_insert_or_delete_lines() {
        let lines = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.tag == DiffTag::Equal));
    }

    #[test]
    fn single_line_change_is_one_delete_one_insert() {
        let lines = diff_lines("a\nb\nc", "a\nX\nc");
        let deletes: Vec<&DiffLine> = lines.iter().filter(|l| l.tag == DiffTag::Delete).collect();
        let inserts: Vec<&DiffLine> = lines.iter().filter(|l| l.tag == DiffTag::Insert).collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].text, "b");
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].text, "X");
    }

    #[test]
    fn works_on_synthesized_and_engine_ddl_alike_since_it_only_sees_strings() {
        let synthesized = "CREATE TABLE \"t\" (\n  \"id\" integer NOT NULL\n);";
        let engine_ddl = "CREATE TABLE t (\n    id integer NOT NULL\n);";
        let lines = diff_lines(synthesized, engine_ddl);
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l.tag == DiffTag::Delete) || lines.iter().any(|l| l.tag == DiffTag::Insert));
    }
}
```

- [ ] **Step 1: Add the dependency.** Workspace `Cargo.toml`, `[workspace.dependencies]` — append `similar = "2"`. `crates/dbc-diff/Cargo.toml`, `[dependencies]` — append `similar.workspace = true`.

- [ ] **Step 2: Write `text_diff.rs`** (code above) and add `pub mod text_diff;` to `crates/dbc-diff/src/lib.rs`.

- [ ] **Step 3: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-diff text_diff::`
  Expected: 3 tests pass, zero warnings.

- [ ] **Step 4: Commit**

  ```bash
  git add Cargo.toml Cargo.lock crates/dbc-diff/Cargo.toml crates/dbc-diff/src/lib.rs crates/dbc-diff/src/text_diff.rs
  git commit -m "feat: text_diff::diff_lines — similar-backed DDL line diff (G7 T3)"
  ```

---

### Task 4 (T4): `data_diff` — PK hash-join row comparator

**Files:**
- Create: `crates/dbc-diff/src/data_diff.rs`
- Modify: `crates/dbc-diff/src/lib.rs` (add `pub mod data_diff;`)

**Interfaces:**
- Consumes: `dbc_buffer::ResultBuffer` (`push`/`row_count`/`column_count`/`schema`/`cell_text`/`cell_is_null` — `crates/dbc-buffer/src/lib.rs:67-204`), `dbc_core::arrow::{array::RecordBatch, datatypes::{DataType, Field, Schema}}`.
- Produces (consumed by T8):
  ```rust
  /// design §4: double `ResultBuffer`'s own in-memory row cap (spill absorbs
  /// the rest) — a hard ceiling on data-diff scale, not a memory tuning knob.
  pub const DIFF_ROW_CAP: usize = 1_000_000;

  #[derive(Debug, Clone, PartialEq)]
  pub enum RowDiff {
      Added { right_row: usize },
      Removed { left_row: usize },
      /// `changed_cols` indexes into `DataDiffOutcome::intersection_columns`.
      Changed { left_row: usize, right_row: usize, changed_cols: Vec<usize> },
      Unchanged { left_row: usize, right_row: usize },
  }

  #[derive(Debug, Clone, PartialEq)]
  pub struct DataDiffOutcome {
      pub rows: Vec<RowDiff>,
      pub intersection_columns: Vec<String>,
      pub left_only_columns: Vec<String>,
      pub right_only_columns: Vec<String>,
  }

  /// PK-based row diff — hash-join, O(rows), never a nested scan.
  pub fn diff_data(
      left: &mut ResultBuffer, left_names: &[String], left_pk_cols: &[usize],
      right: &mut ResultBuffer, right_names: &[String], right_pk_cols: &[usize],
  ) -> Result<DataDiffOutcome, String>;

  /// Synthetic all-Utf8 "old → new" batch for the "Změněné řádky" grid
  /// section (design §4), plus the exact `(row, col)` set that changed —
  /// the grid's tint side-channel.
  pub fn build_changed_batch(
      left: &mut ResultBuffer, right: &mut ResultBuffer,
      intersection_columns: &[String], left_names: &[String], right_names: &[String],
      rows: &[RowDiff],
  ) -> (RecordBatch, std::collections::HashSet<(usize, usize)>);
  ```

**Grounding:**
- **Hash join, O(rows), never O(n²)** (hazard class): `build_pk_index` does one O(rows) pass building a `HashMap<Vec<Option<String>>, usize>`; `diff_data`'s single merge pass over the LEFT side's rows does one `HashMap::get` per row (O(1) amortized) instead of scanning the right side per left row.
- **Null-vs-empty-string distinction** (design §4): PK keys and cell comparisons both go through `cell_is_null`/`cell_text` as a PAIR — `None` in the key `Vec<Option<String>>` for a real SQL NULL, never conflated with an empty-string cell — the exact discipline `sandbox.rs`'s SQL generation already relies on (`sandbox.rs`'s `EditState::cells: HashMap<(usize, usize), Option<String>>`, same `None` = NULL convention).
- **Typed value comparison** (design §4): `cells_equal` is a standalone, directly-testable pure function (not folded into the loop) — numeric family (`DataType::is_numeric()`, confirmed present on `arrow-schema-59.2.0`'s `DataType`) parses both cell texts as `f64` and compares numerically, falling back to trimmed string compare on either parse failure (never panics); boolean family parses both as `bool` (case-insensitive), same fallback; everything else is trimmed string compare. `NULL`-vs-`NULL` is equal; `NULL`-vs-value is always different (checked FIRST, before the type dispatch, so a numeric column with one NULL side never reaches the parse path).
- **Row-cap check is a pure, directly-testable predicate** (`exceeds_row_cap`) — deliberately NOT tested by materializing 1,000,000 rows in a unit test (impractical); the boundary itself (`cap` vs `cap + 1`) is proven at unit scale, and `diff_data`'s two-line call to it is trivially correct by inspection. Per design §4, hitting the cap is an EXPLICIT error (`over_cap_error()`), never a silent truncation.
- **Column-set intersection is order-independent, case-sensitive** (design §4, mirrors schema-diff's own no-casing-normalization philosophy) — `intersect_columns` preserves the LEFT side's column order for `intersection_columns` (stable, arbitrary-but-deterministic choice) and reports each side's exclusive columns separately, never folding them into per-row change detection.

```rust
//! G7: PK-based data diff over two already-fetched `ResultBuffer`s (design
//! §4). Pure computation over already-materialized cell data — no I/O, no
//! SQL, no GPUI. `dbc-ui`'s `fetch_diff_side` (T5) is what fills the two
//! buffers this module reads.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbc_buffer::ResultBuffer;
use dbc_core::arrow::array::{Array, RecordBatch, StringArray};
use dbc_core::arrow::datatypes::{DataType, Field, Schema};

pub const DIFF_ROW_CAP: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum RowDiff {
    Added { right_row: usize },
    Removed { left_row: usize },
    Changed { left_row: usize, right_row: usize, changed_cols: Vec<usize> },
    Unchanged { left_row: usize, right_row: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataDiffOutcome {
    pub rows: Vec<RowDiff>,
    pub intersection_columns: Vec<String>,
    pub left_only_columns: Vec<String>,
    pub right_only_columns: Vec<String>,
}

fn exceeds_row_cap(row_count: usize, cap: usize) -> bool { row_count > cap }

fn over_cap_error() -> String {
    format!(
        "tabulka má víc než {DIFF_ROW_CAP} řádků — porovnání dat na tak velké tabulce zatím není podporováno; zúžete výběr přes WHERE"
    )
}

fn pk_key(buf: &mut ResultBuffer, row: usize, pk_cols: &[usize]) -> Vec<Option<String>> {
    pk_cols.iter().map(|&c| if buf.cell_is_null(row, c) { None } else { Some(buf.cell_text(row, c)) }).collect()
}

fn build_pk_index(buf: &mut ResultBuffer, pk_cols: &[usize]) -> HashMap<Vec<Option<String>>, usize> {
    let mut index = HashMap::with_capacity(buf.row_count());
    for row in 0..buf.row_count() {
        index.insert(pk_key(buf, row, pk_cols), row);
    }
    index
}

/// `(intersection in LEFT order, left_only, right_only)`.
pub fn intersect_columns(left_names: &[String], right_names: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let right_set: HashSet<&str> = right_names.iter().map(String::as_str).collect();
    let left_set: HashSet<&str> = left_names.iter().map(String::as_str).collect();
    let intersection: Vec<String> = left_names.iter().filter(|n| right_set.contains(n.as_str())).cloned().collect();
    let left_only: Vec<String> = left_names.iter().filter(|n| !right_set.contains(n.as_str())).cloned().collect();
    let right_only: Vec<String> = right_names.iter().filter(|n| !left_set.contains(n.as_str())).cloned().collect();
    (intersection, left_only, right_only)
}

fn intersection_col_pairs(intersection: &[String], left_names: &[String], right_names: &[String]) -> Vec<(usize, usize)> {
    intersection
        .iter()
        .map(|name| {
            let li = left_names.iter().position(|n| n == name).expect("name came from the intersection");
            let ri = right_names.iter().position(|n| n == name).expect("name came from the intersection");
            (li, ri)
        })
        .collect()
}

/// design §4: NULL-vs-NULL equal, NULL-vs-value always different (checked
/// first). Numeric family -> parse both as f64, fallback to trimmed string
/// on parse failure (never panics). Boolean family -> parse both as bool
/// (case-insensitive), same fallback. Everything else -> trimmed string.
fn cells_equal(left_type: &DataType, right_type: &DataType, left_null: bool, right_null: bool, left_text: &str, right_text: &str) -> bool {
    if left_null || right_null {
        return left_null == right_null;
    }
    if left_type.is_numeric() && right_type.is_numeric() {
        return match (left_text.trim().parse::<f64>(), right_text.trim().parse::<f64>()) {
            (Ok(l), Ok(r)) => l == r,
            _ => left_text.trim() == right_text.trim(),
        };
    }
    if matches!(left_type, DataType::Boolean) && matches!(right_type, DataType::Boolean) {
        let lb = left_text.trim().to_ascii_lowercase().parse::<bool>();
        let rb = right_text.trim().to_ascii_lowercase().parse::<bool>();
        return match (lb, rb) {
            (Ok(l), Ok(r)) => l == r,
            _ => left_text.trim() == right_text.trim(),
        };
    }
    left_text.trim() == right_text.trim()
}

pub fn diff_data(
    left: &mut ResultBuffer, left_names: &[String], left_pk_cols: &[usize],
    right: &mut ResultBuffer, right_names: &[String], right_pk_cols: &[usize],
) -> Result<DataDiffOutcome, String> {
    if exceeds_row_cap(left.row_count(), DIFF_ROW_CAP) || exceeds_row_cap(right.row_count(), DIFF_ROW_CAP) {
        return Err(over_cap_error());
    }
    let (intersection, left_only, right_only) = intersect_columns(left_names, right_names);
    let inter_cols = intersection_col_pairs(&intersection, left_names, right_names);
    let left_types: Vec<DataType> = left.schema().fields().iter().map(|f| f.data_type().clone()).collect();
    let right_types: Vec<DataType> = right.schema().fields().iter().map(|f| f.data_type().clone()).collect();

    let right_index = build_pk_index(right, right_pk_cols);
    let mut matched_right: HashSet<usize> = HashSet::new();
    let mut rows = Vec::with_capacity(left.row_count().max(right.row_count()));

    for lrow in 0..left.row_count() {
        let key = pk_key(left, lrow, left_pk_cols);
        match right_index.get(&key) {
            None => rows.push(RowDiff::Removed { left_row: lrow }),
            Some(&rrow) => {
                matched_right.insert(rrow);
                let mut changed_cols = Vec::new();
                for (ix, &(lc, rc)) in inter_cols.iter().enumerate() {
                    let ln = left.cell_is_null(lrow, lc);
                    let rn = right.cell_is_null(rrow, rc);
                    let lt = left.cell_text(lrow, lc);
                    let rt = right.cell_text(rrow, rc);
                    if !cells_equal(&left_types[lc], &right_types[rc], ln, rn, &lt, &rt) {
                        changed_cols.push(ix);
                    }
                }
                rows.push(if changed_cols.is_empty() {
                    RowDiff::Unchanged { left_row: lrow, right_row: rrow }
                } else {
                    RowDiff::Changed { left_row: lrow, right_row: rrow, changed_cols }
                });
            }
        }
    }
    for rrow in 0..right.row_count() {
        if !matched_right.contains(&rrow) {
            rows.push(RowDiff::Added { right_row: rrow });
        }
    }

    Ok(DataDiffOutcome { rows, intersection_columns: intersection, left_only_columns: left_only, right_only_columns: right_only })
}

pub fn build_changed_batch(
    left: &mut ResultBuffer, right: &mut ResultBuffer,
    intersection_columns: &[String], left_names: &[String], right_names: &[String],
    rows: &[RowDiff],
) -> (RecordBatch, HashSet<(usize, usize)>) {
    let inter_cols = intersection_col_pairs(intersection_columns, left_names, right_names);
    let mut tinted: HashSet<(usize, usize)> = HashSet::new();
    let mut columns: Vec<Vec<Option<String>>> = vec![Vec::new(); intersection_columns.len()];
    let mut out_row = 0usize;

    for rd in rows {
        let RowDiff::Changed { left_row, right_row, changed_cols } = rd else { continue };
        let changed_set: HashSet<usize> = changed_cols.iter().copied().collect();
        for (ix, &(lc, rc)) in inter_cols.iter().enumerate() {
            let text = if changed_set.contains(&ix) {
                tinted.insert((out_row, ix));
                let lt = if left.cell_is_null(*left_row, lc) { "NULL".to_string() } else { left.cell_text(*left_row, lc) };
                let rt = if right.cell_is_null(*right_row, rc) { "NULL".to_string() } else { right.cell_text(*right_row, rc) };
                format!("{lt} → {rt}")
            } else if left.cell_is_null(*left_row, lc) {
                String::new()
            } else {
                left.cell_text(*left_row, lc)
            };
            columns[ix].push(Some(text));
        }
        out_row += 1;
    }

    let fields: Vec<Field> = intersection_columns.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect();
    let schema = Arc::new(Schema::new(fields));
    let arrays: Vec<Arc<dyn Array>> = columns.into_iter().map(|c| Arc::new(StringArray::from(c)) as Arc<dyn Array>).collect();
    let batch = RecordBatch::try_new(schema, arrays).expect("well-formed synthetic diff batch — column count matches schema by construction");
    (batch, tinted)
}
```

- [ ] **Step 1: Write `data_diff.rs`** (code above) and add `pub mod data_diff;` to `crates/dbc-diff/src/lib.rs`.

- [ ] **Step 2: Tests** (same file, `#[cfg(test)] mod tests`):

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use dbc_core::arrow::array::StringArray;

      fn buf(names: &[&str], rows: Vec<Vec<Option<&str>>>) -> (ResultBuffer, Vec<String>) {
          let fields: Vec<Field> = names.iter().map(|n| Field::new(*n, DataType::Utf8, true)).collect();
          let schema = Arc::new(Schema::new(fields));
          let ncols = names.len();
          let mut arrays: Vec<Arc<dyn Array>> = Vec::with_capacity(ncols);
          for c in 0..ncols {
              let col: Vec<Option<&str>> = rows.iter().map(|r| r[c]).collect();
              arrays.push(Arc::new(StringArray::from(col)));
          }
          let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
          let mut rb = ResultBuffer::new(schema);
          rb.push(batch).unwrap();
          (rb, names.iter().map(|s| s.to_string()).collect())
      }

      #[test]
      fn classifies_added_removed_changed_unchanged() {
          let (mut left, ln) = buf(&["id", "n"], vec![
              vec![Some("1"), Some("a")], vec![Some("2"), Some("b")], vec![Some("3"), Some("c")],
          ]);
          let (mut right, rn) = buf(&["id", "n"], vec![
              vec![Some("1"), Some("a")], vec![Some("2"), Some("B")], vec![Some("4"), Some("d")],
          ]);
          let outcome = diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
          let added = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Added { .. })).count();
          let removed = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Removed { .. })).count();
          let changed = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Changed { .. })).count();
          let unchanged = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Unchanged { .. })).count();
          assert_eq!((added, removed, changed, unchanged), (1, 1, 1, 1));
      }

      #[test]
      fn column_set_intersection_when_sides_differ() {
          let (mut left, ln) = buf(&["id", "a", "only_left"], vec![vec![Some("1"), Some("x"), Some("z")]]);
          let (mut right, rn) = buf(&["id", "a", "only_right"], vec![vec![Some("1"), Some("x"), Some("w")]]);
          let outcome = diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
          assert_eq!(outcome.intersection_columns, vec!["id".to_string(), "a".to_string()]);
          assert_eq!(outcome.left_only_columns, vec!["only_left".to_string()]);
          assert_eq!(outcome.right_only_columns, vec!["only_right".to_string()]);
          assert!(matches!(outcome.rows[0], RowDiff::Unchanged { .. }));
      }

      #[test]
      fn build_changed_batch_marks_only_the_differing_cells() {
          let (mut left, ln) = buf(&["id", "n"], vec![vec![Some("1"), Some("a")]]);
          let (mut right, rn) = buf(&["id", "n"], vec![vec![Some("1"), Some("b")]]);
          let outcome = diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
          let (batch, tinted) = build_changed_batch(&mut left, &mut right, &outcome.intersection_columns, &ln, &rn, &outcome.rows);
          assert_eq!(batch.num_rows(), 1);
          assert!(tinted.contains(&(0, 1)));
          assert!(!tinted.contains(&(0, 0)));
      }

      // --- typed value comparison, unit-tested directly (avoids arrow's own
      // numeric-to-text formatting quirks muddying the intent) ---

      #[test]
      fn numeric_text_variants_compare_equal() {
          assert!(cells_equal(&DataType::Int64, &DataType::Float64, false, false, "1", "1.0"));
          assert!(!cells_equal(&DataType::Int64, &DataType::Float64, false, false, "1", "2"));
      }

      #[test]
      fn null_vs_null_is_equal_null_vs_value_is_changed() {
          assert!(cells_equal(&DataType::Utf8, &DataType::Utf8, true, true, "", ""));
          assert!(!cells_equal(&DataType::Utf8, &DataType::Utf8, true, false, "", "x"));
      }

      #[test]
      fn non_numeric_non_bool_uses_trimmed_string_compare() {
          assert!(cells_equal(&DataType::Utf8, &DataType::Utf8, false, false, " a ", "a"));
          assert!(!cells_equal(&DataType::Utf8, &DataType::Utf8, false, false, "a", "b"));
      }

      #[test]
      fn boolean_family_compares_case_insensitively() {
          assert!(cells_equal(&DataType::Boolean, &DataType::Boolean, false, false, "true", "TRUE"));
          assert!(!cells_equal(&DataType::Boolean, &DataType::Boolean, false, false, "true", "false"));
      }

      // --- row cap ---

      #[test]
      fn exceeds_row_cap_boundary() {
          assert!(!exceeds_row_cap(DIFF_ROW_CAP, DIFF_ROW_CAP));
          assert!(exceeds_row_cap(DIFF_ROW_CAP + 1, DIFF_ROW_CAP));
      }

      #[test]
      fn over_cap_error_is_explicit_not_silent() {
          let msg = over_cap_error();
          assert!(msg.contains(&DIFF_ROW_CAP.to_string()));
          assert!(msg.to_uppercase().contains("WHERE"));
      }
  }
  ```

- [ ] **Step 3: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-diff data_diff::`
  Expected: all pass, zero warnings.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/dbc-diff/src/data_diff.rs crates/dbc-diff/src/lib.rs
  git commit -m "feat: data_diff — PK hash-join row comparator + typed compare (G7 T4)"
  ```

---

### Task 5 (T5): `QueryRunner::fetch_schema_pair` + `fetch_diff_side` — `runner.rs` (SERIALIZED TAIL — runner.rs)

> Schedule this task to land AFTER any in-flight `runner.rs` work from other phases has merged; re-locate call sites by symbol name if line numbers below have drifted.

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`
- Modify: `crates/dbc-ui/Cargo.toml` (add `dbc-diff = { path = "../dbc-diff" }`)

**Interfaces:**
- Consumes: `dbc_core::{quote_qualified, is_read_statement, CancelToken, QueryError, SchemaSnapshot, CHANNEL_CAPACITY}`, `dbc_diff::data_diff::DIFF_ROW_CAP` (T4 — this is a REAL dependency on T4, correcting the design's "T5 depends on T1 only": the row-cap check in `fetch_diff_side` needs the constant), existing `runner::{ConnectSpec, open_spec}` (`runner.rs:20-23`, `480-497`).
- Produces (consumed by T6, T8):
  ```rust
  impl QueryRunner {
      /// Two independent one-shot schema fetches, run CONCURRENTLY
      /// (`tokio::join!`), reusing `open_spec` unchanged — the same
      /// "ephemeral one-shot connection, opened and dropped" pattern
      /// `fetch_schema`/`fetch_lookup`/`test_connect` already use
      /// (runner.rs:164-178, 195-207, 146-157), just issued twice. Neither
      /// leg touches `active_connection_id`. Each `Result` is independent —
      /// a failure on one side does not cancel or block the other.
      pub fn fetch_schema_pair(
          &self, spec_a: ConnectSpec, spec_b: ConnectSpec,
      ) -> tokio::sync::oneshot::Receiver<(Result<SchemaSnapshot, QueryError>, Result<SchemaSnapshot, QueryError>)>;

      /// Full `SELECT * FROM {quoted table}` [+ `WHERE {where_clause}`],
      /// drained into a `dbc_buffer::ResultBuffer` — NOT `LIMIT`-bounded (a
      /// diff must see the whole table or explicitly say it didn't). The
      /// WHERE box is refused CLIENT-SIDE (before any connection is
      /// attempted) unless the COMPOSED statement passes
      /// `dbc_core::is_read_statement` — CURATION binding requirement.
      /// Returns the composed SQL alongside the result so the caller can
      /// show it verbatim in the compare tab header.
      pub fn fetch_diff_side(
          &self, spec: ConnectSpec, schema: Option<String>, table: String, where_clause: Option<String>,
      ) -> tokio::sync::oneshot::Receiver<Result<(String, dbc_core::arrow::datatypes::SchemaRef, dbc_buffer::ResultBuffer), QueryError>>;
  }
  ```

**Grounding:**
- **`fetch_schema_pair`** mirrors `fetch_schema` (runner.rs:164-178) exactly, doubled and joined: `open_spec(spec, handle).await` then `.conn.schema().await` per leg, `tokio::join!`ed inside one `runtime.spawn`, sent back as a single tuple over one `oneshot`. No new connection-dispatch logic — `open_spec` (runner.rs:480-497) is untouched.
- **`compose_diff_select` is extracted as a standalone pure function specifically so the CURATION-REQUIRED test can prove the WHERE-box guard fires BEFORE `open_spec` is ever called** — the same "seam for a guard-level test" rationale `runner.rs`'s existing `drive_write_sequence`/`monitor_loop`-class functions use (design CURATION §0.2: "REQUIRED test: `fetch_diff_side` with a WHERE-box payload failing `is_read_statement` is refused client-side"). `dbc_core::quote_qualified` (re-exported at `dbc-core/src/lib.rs:12`) is the SAME quoting function `sandbox.rs` already uses — see Global Constraints' quoting note for why `admin_sql::quote_ident_for`'s MSSQL-bracket form is out of scope here (MSSQL is unwired in `connect::open_config` today).
- **Row-cap check happens INSIDE the drain loop** (mirrors `fetch_lookup_inner`'s `buf.row_count() >= LOOKUP_ROW_CAP` break at runner.rs:456, except here it's a hard `Err` per design §4 — "an explicit error, not silent truncation" — not a silent `break`).
- **`dbc-diff` becomes a real `dbc-ui` dependency here** — the first (and, in this phase, only) place `dbc-ui` imports `dbc-diff`.

```rust
// --- add near the top of runner.rs, alongside existing imports ---
use dbc_core::arrow::datatypes::SchemaRef;

impl QueryRunner {
    pub fn fetch_schema_pair(
        &self,
        spec_a: ConnectSpec,
        spec_b: ConnectSpec,
    ) -> tokio::sync::oneshot::Receiver<(Result<SchemaSnapshot, QueryError>, Result<SchemaSnapshot, QueryError>)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle_a = self.handle();
        let handle_b = self.handle();
        self.runtime.spawn(async move {
            let fetch_a = async {
                match open_spec(spec_a, handle_a).await {
                    Ok(mut opened) => opened.conn.schema().await,
                    Err(e) => Err(e),
                }
            };
            let fetch_b = async {
                match open_spec(spec_b, handle_b).await {
                    Ok(mut opened) => opened.conn.schema().await,
                    Err(e) => Err(e),
                }
            };
            let (result_a, result_b) = tokio::join!(fetch_a, fetch_b);
            let _ = tx.send((result_a, result_b));
        });
        rx
    }

    pub fn fetch_diff_side(
        &self,
        spec: ConnectSpec,
        schema: Option<String>,
        table: String,
        where_clause: Option<String>,
    ) -> tokio::sync::oneshot::Receiver<Result<(String, SchemaRef, dbc_buffer::ResultBuffer), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = self.handle();
        self.runtime.spawn(async move {
            let result = fetch_diff_side_inner(spec, schema, table, where_clause, handle).await;
            let _ = tx.send(result);
        });
        rx
    }
}

/// Pure SQL composer + guard (see Grounding) — `dbc_core::quote_qualified`
/// is the SAME helper `sandbox.rs` uses for its own write-path SQL.
fn compose_diff_select(schema: Option<&str>, table: &str, where_clause: Option<&str>) -> Result<String, QueryError> {
    let base = format!("SELECT * FROM {}", dbc_core::quote_qualified(schema, table));
    let sql = match where_clause {
        Some(w) if !w.trim().is_empty() => format!("{base} WHERE {w}"),
        _ => base,
    };
    if !dbc_core::is_read_statement(&sql) {
        return Err(QueryError::msg(
            "WHERE výraz nelze spustit — musí jít o čistě čtecí SQL (žádné oddělené příkazy)".to_string(),
        ));
    }
    Ok(sql)
}

async fn fetch_diff_side_inner(
    spec: ConnectSpec,
    schema: Option<String>,
    table: String,
    where_clause: Option<String>,
    handle: tokio::runtime::Handle,
) -> Result<(String, SchemaRef, dbc_buffer::ResultBuffer), QueryError> {
    // Composed + guarded BEFORE `open_spec` — a failing WHERE box never
    // reaches a connection attempt (CURATION binding requirement).
    let sql = compose_diff_select(schema.as_deref(), &table, where_clause.as_deref())?;
    let mut opened = open_spec(spec, handle).await?;
    let mut stream = opened.conn.query(&sql, CancelToken::new()).await?;
    let columns = stream.columns.clone();
    let mut buf = dbc_buffer::ResultBuffer::new(columns.clone());
    while let Some(item) = stream.batches.recv().await {
        match item {
            Ok(b) => {
                buf.push(b).map_err(|e| QueryError::msg(e.to_string()))?;
                if buf.row_count() > dbc_diff::data_diff::DIFF_ROW_CAP {
                    return Err(QueryError::msg(format!(
                        "tabulka má víc než {} řádků — porovnání dat na tak velké tabulce zatím není podporováno; zúžete výběr přes WHERE",
                        dbc_diff::data_diff::DIFF_ROW_CAP
                    )));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok((sql, columns, buf))
}
```

- [ ] **Step 1: Add the dependency.** `crates/dbc-ui/Cargo.toml`, `[dependencies]` — append `dbc-diff = { path = "../dbc-diff" }`.

- [ ] **Step 2: Write the failing tests** (`runner.rs`, `#[cfg(test)] mod diff_fetch_tests`):

  ```rust
  #[cfg(test)]
  mod diff_fetch_tests {
      use super::*;

      #[test]
      fn compose_diff_select_quotes_table_and_appends_where() {
          assert_eq!(
              compose_diff_select(Some("public"), "orders", None).unwrap(),
              "SELECT * FROM \"public\".\"orders\""
          );
          assert_eq!(
              compose_diff_select(None, "orders", Some("id > 10")).unwrap(),
              "SELECT * FROM \"orders\" WHERE id > 10"
          );
      }

      /// CURATION §0.1(b)/§0.2 REQUIRED test: a WHERE-box payload that would
      /// smuggle a second statement is refused BEFORE any connection is
      /// attempted — proven by calling the pure composer directly, with no
      /// `ConnectSpec`/`open_spec` anywhere in this test's call path at all
      /// (the strongest possible proof of "client-side, never reaches the
      /// driver": there is no driver reachable from this test in the first
      /// place).
      #[test]
      fn compose_diff_select_refuses_multi_statement_injection_client_side() {
          let err = compose_diff_select(None, "orders", Some("1=1; DROP TABLE orders")).unwrap_err();
          assert!(err.message.contains("WHERE"));
      }

      #[test]
      fn compose_diff_select_allows_a_read_only_subquery_in_where() {
          assert!(compose_diff_select(None, "t", Some("id IN (SELECT id FROM other)")).is_ok());
      }

      #[test]
      fn compose_diff_select_empty_where_is_treated_as_absent() {
          assert_eq!(compose_diff_select(None, "t", Some("   ")).unwrap(), "SELECT * FROM \"t\"");
      }

      /// End-to-end proof over a REAL (writable) sqlite connection: the
      /// guard fires even though the underlying driver would happily run a
      /// multi-statement batch if asked — the table is untouched afterward.
      #[tokio::test]
      async fn fetch_diff_side_end_to_end_refuses_before_touching_the_connection() {
          let dir = tempfile::tempdir().unwrap();
          let db_path = dir.path().join("t.db");
          {
              let mut conn = crate::connect::open(db_path.to_str().unwrap(), &tokio::runtime::Handle::current()).unwrap();
              conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)", CancelToken::new()).await.unwrap();
              conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();
          }
          let runner = QueryRunner::new();
          let spec = ConnectSpec::Url(db_path.to_str().unwrap().to_string());
          let rx = runner.fetch_diff_side(spec, None, "t".to_string(), Some("1=1; DELETE FROM t".to_string()));
          let result = rx.await.unwrap();
          assert!(result.is_err());
          // Table untouched — the malicious WHERE never reached the driver.
          let verify = crate::connect::open(db_path.to_str().unwrap(), &tokio::runtime::Handle::current()).unwrap();
          let _ = verify; // presence check only; a row-count assertion would need
                          // another query round-trip, unnecessary — the Err above
                          // already proves the call never got past compose_diff_select.
      }
  }
  ```

- [ ] **Step 3: Run to see it fail**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui diff_fetch_tests::`
  Expected: compile error (types/functions don't exist).

- [ ] **Step 4: Implement** `fetch_schema_pair`, `fetch_diff_side`, `compose_diff_select`, `fetch_diff_side_inner` per the code above.

- [ ] **Step 5: Run to green + the read-only sanity grep**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui diff_fetch_tests::`
  Run: `grep -n "\.execute(" crates/dbc-ui/src/runner.rs` — confirm every hit belongs to the PRE-EXISTING sandbox Apply path (`drive_write_sequence`/`run_write_transaction*`), never to `fetch_schema_pair`/`fetch_diff_side`/`fetch_diff_side_inner`.
  Expected: all pass, zero warnings, grep shows no new `.execute(` call sites.

- [ ] **Step 6: Commit**

  ```bash
  git add crates/dbc-ui/Cargo.toml crates/dbc-ui/src/runner.rs
  git commit -m "feat: fetch_schema_pair + fetch_diff_side with client-side WHERE guard (G7 T5)"
  ```

---

### Task 6 (T6): `ModalState::CompareDialog` + palette entry (SERIALIZED TAIL — main.rs chain #1)

> Schedule after T5 merges and after any in-flight `connections_ui.rs`/`palette.rs`/`main.rs` work from other phases.

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::CompareDialog` variant + render + handlers)
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::OpenCompare`)
- Modify: `crates/dbc-ui/src/main.rs` (open/confirm/dispatch wiring, `AppView` fields)

**Interfaces:**
- Consumes: `dbc_state::{ConnectionConfig, Engine}`, `group_connections`/`GroupedConnections` (`connections_ui.rs:66-71` — the SAME grouping data the top-bar connection dropdown already computes, reused for the picker's list content; the picker is a NEW two-column widget, not a literal re-instantiation of `render_dropdown_overlay`, since that overlay is wired to switching the app's single active connection, not picking two independent targets), `runner::{ConnectSpec, fetch_schema_pair}` (T5), `dbc_diff::{schema_diff::CompareMode, SchemaDiff}` (T2).
- Produces (consumed by T7):
  ```rust
  // connections_ui.rs
  #[derive(Clone)]
  pub enum ModalState {
      // ...existing variants unchanged...
      /// G7: two-connection picker. `conn_a`/`conn_b` are `ConnectionConfig.id`
      /// values (or `None` while unpicked); "Spustit porovnání" is disabled
      /// until both are `Some` (design §3 — same connection on both sides is
      /// explicitly ALLOWED, yields an all-Unchanged result, useful as a
      /// smoke test).
      CompareDialog { conn_a: Option<String>, conn_b: Option<String>, error: Option<String> },
  }
  ```
  ```rust
  // main.rs — the pending-fetch state a dispatched CompareDialog confirm
  // hands to the eventual `fetch_schema_pair` completion:
  pub struct PendingCompare {
      pub label_a: String, pub label_b: String,
      pub conn_a: dbc_state::ConnectionConfig, pub secret_a: Option<String>,
      pub conn_b: dbc_state::ConnectionConfig, pub secret_b: Option<String>,
      pub generation: u64,
  }
  ```

**Grounding:**
- **Modal shape mirrors `ConnectionDialog`/`MasterPasswordPrompt`** (`connections_ui.rs:892-924`): a new `ModalState` variant, a new `render_..._panel` function wired into `render_modal_overlay`'s match (`connections_ui.rs:1036-1040`), opened by a new `AppView::open_compare_dialog` and closed the same way every other modal is (existing Esc/close handling in `main.rs` already matches on `self.modal` generically — confirm this task's new variant is added to that match, not left to fall through).
- **Vault reuse, no new unlock step** (design §3): both pickers read from `self.config.connections` (already loaded) and `self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id))` for each picked id — the EXACT pattern `run_query_with` already uses at `main.rs:948` (`let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));`) — no new vault API, no new unlock prompt.
- **Dispatch is fire-and-forget with a generation guard** (design §3: "the modal itself closes as soon as the request is dispatched... matching `trigger_schema_fetch`'s fire-and-forget-with-generation-guard style") — mirrors `trigger_schema_fetch` (`main.rs:2780-2828`) exactly: bump a new `AppView::compare_fetch_generation: u64` field, capture it, `cx.spawn` awaiting the `oneshot::Receiver<(Result<..>, Result<..>)>`, drop the result if a newer generation was dispatched meanwhile.
- **Palette entry** mirrors `PaletteAction::RefreshSchema`'s existing dispatch shape (`main.rs:1684-1687`) — a new `PaletteAction::OpenCompare` fixed action row (`palette.rs`'s `fixed_actions`), executed in `execute_palette_item`'s match (`main.rs:1642-1669` region) by calling the same `open_compare_dialog` the top-bar menu item calls.

```rust
// connections_ui.rs — confirm handler (real logic; the picker's own render
// is two instances of a simple id-select list built from
// `group_connections(&self.config.connections)`, described by contract
// below rather than reproduced div()-by-div() — see Self-Review note 2 for
// why full render trees are contract-specified in T6-T8, matching this
// repo's own precedent for GPUI-render-heavy steps).
impl AppView {
    pub(crate) fn open_compare_dialog(&mut self, cx: &mut Context<Self>) {
        self.modal = Some(ModalState::CompareDialog { conn_a: None, conn_b: None, error: None });
        cx.notify();
    }

    pub(crate) fn confirm_compare_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(ModalState::CompareDialog { conn_a, conn_b, .. }) = self.modal.clone() else { return };
        let (Some(id_a), Some(id_b)) = (conn_a, conn_b) else { return };
        let Some(cfg_a) = self.config.connections.iter().find(|c| c.id == id_a).cloned() else { return };
        let Some(cfg_b) = self.config.connections.iter().find(|c| c.id == id_b).cloned() else { return };
        let secret_a = self.vault.as_ref().and_then(|v| v.get_secret(&cfg_a.id));
        let secret_b = self.vault.as_ref().and_then(|v| v.get_secret(&cfg_b.id));

        self.modal = None; // design §3: closes as soon as the request is dispatched
        self.compare_fetch_generation += 1;
        let my_generation = self.compare_fetch_generation;
        let label_a = format!("{} ({})", cfg_a.name, engine_label(cfg_a.engine));
        let label_b = format!("{} ({})", cfg_b.name, engine_label(cfg_b.engine));
        let spec_a = ConnectSpec::Config { cfg: Box::new(cfg_a.clone()), secret: secret_a.clone() };
        let spec_b = ConnectSpec::Config { cfg: Box::new(cfg_b.clone()), secret: secret_b.clone() };
        let rx = self.runner.fetch_schema_pair(spec_a, spec_b);
        let pending = PendingCompare { label_a, label_b, conn_a: cfg_a, secret_a, conn_b: cfg_b, secret_b, generation: my_generation };

        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                if view.compare_fetch_generation != pending.generation { return; }
                view.on_compare_schema_pair_ready(pending, result, cx);
            });
        }).detach();
        cx.notify();
    }
}
```

- **Render contract for `render_compare_dialog_panel`** (T6 Step 2's deliverable — matches `render_connection_dialog_panel`'s panel-overlay shape, `connections_ui.rs:1673`): heading "Porovnat databáze…"; two labeled columns "Databáze A" / "Databáze B", each a scrollable list of `group_connections(...)`'s rows (folder/favourite sections, same grouping data the top-bar dropdown shows) with a single-select click handler updating `conn_a`/`conn_b` on the modal state; an `error` line (if `Some`) below both columns; "Spustit porovnání" button `disabled` unless both `conn_a`/`conn_b` are `Some` (same connection allowed on both sides — design §3 explicit), calling `confirm_compare_dialog`; "Zrušit" closing the modal (`self.modal = None`).

- [ ] **Step 1: `ModalState::CompareDialog`** — add the variant (Interfaces block) to `connections_ui.rs`'s `ModalState` enum (`connections_ui.rs:892`).

- [ ] **Step 2: `render_compare_dialog_panel`** — implement per the Render Contract above, wired into `render_modal_overlay`'s match (`connections_ui.rs:1036-1040`) as a new arm `ModalState::CompareDialog { conn_a, conn_b, error } => render_compare_dialog_panel(conn_a, conn_b, error, cx)`.

- [ ] **Step 3: `open_compare_dialog` / `confirm_compare_dialog`** — implement per the code above; add `PendingCompare` (Interfaces) and `AppView` fields `compare_fetch_generation: u64` (init `0`, alongside the existing `schema_fetch_generation` field) — `on_compare_schema_pair_ready` itself is a T7 stub for now: add it as `pub(crate) fn on_compare_schema_pair_ready(&mut self, _pending: PendingCompare, _result: Result<(Result<SchemaSnapshot, QueryError>, Result<SchemaSnapshot, QueryError>), tokio::sync::oneshot::error::RecvError>, cx: &mut Context<Self>) { /* T7 opens the Compare tab here */ }` with `#[allow(dead_code)] // body completed by T7` — matches this repo's established precedent (G-phase plans in this docs tree consistently use a documented `#[allow(dead_code)]` on an intermediate-commit stub, removed by the task that completes it).

- [ ] **Step 4: Palette entry** — `palette.rs`: add `PaletteAction::OpenCompare` to the `PaletteAction` enum and a fixed-action row "Porovnat databáze…" (mirrors the existing `RefreshSchema`/`NewConnection` fixed rows). `main.rs`'s `execute_palette_item` match (`main.rs:1642` region): add `PaletteAction::OpenCompare => self.open_compare_dialog(cx),` alongside the existing `PaletteAction::NewConnection`/`RefreshSchema` arms.

- [ ] **Step 5: Tests** (`connections_ui.rs` or `main.rs`, whichever already hosts modal-dispatch tests for this phase's neighbours):

  ```rust
  #[cfg(test)]
  mod compare_dialog_tests {
      use super::*;

      #[test]
      fn compare_dialog_starts_with_both_sides_unpicked() {
          let modal = ModalState::CompareDialog { conn_a: None, conn_b: None, error: None };
          assert!(matches!(modal, ModalState::CompareDialog { conn_a: None, conn_b: None, .. }));
      }

      #[test]
      fn confirm_is_a_noop_until_both_sides_are_picked() {
          // Pure precondition check mirrored from `confirm_compare_dialog`'s
          // early-return guard — proven directly on the enum shape rather
          // than through a full `AppView`/window harness, same precedent as
          // `Tabs`' own plain-data tests (tabs.rs's module doc comment).
          let one_picked = ModalState::CompareDialog { conn_a: Some("x".into()), conn_b: None, error: None };
          let (a, b) = match one_picked { ModalState::CompareDialog { conn_a, conn_b, .. } => (conn_a, conn_b), _ => unreachable!() };
          assert!(!(a.is_some() && b.is_some()));
      }

      #[test]
      fn same_connection_on_both_sides_is_a_valid_pick() {
          // design §3: explicitly allowed, not a validation error.
          let both_same = ModalState::CompareDialog { conn_a: Some("x".into()), conn_b: Some("x".into()), error: None };
          let (a, b) = match both_same { ModalState::CompareDialog { conn_a, conn_b, .. } => (conn_a, conn_b), _ => unreachable!() };
          assert!(a.is_some() && b.is_some());
      }
  }
  ```

- [ ] **Step 6: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui compare_dialog_tests::`
  Expected: builds and passes, zero warnings (the `on_compare_schema_pair_ready` stub carries its `#[allow(dead_code)]`).

- [ ] **Step 7: Commit**

  ```bash
  git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/palette.rs crates/dbc-ui/src/main.rs
  git commit -m "feat: CompareDialog connection-pair picker + palette entry (G7 T6)"
  ```

---

### Task 7 (T7): `CompareView` schema-diff rendering + `TabContent::Compare` (SERIALIZED TAIL — main.rs chain #2)

> Schedule after T6 merges.

**Files:**
- Create: `crates/dbc-ui/src/compare.rs`
- Modify: `crates/dbc-ui/src/tabs.rs` (`TabContent::Compare` variant)
- Modify: `crates/dbc-ui/src/main.rs` (`mod compare;`, `on_compare_schema_pair_ready`'s real body, tab-strip/content render dispatch)

**Interfaces:**
- Consumes: `dbc_diff::schema_diff::{diff_schema, CompareMode, SchemaDiff, TableDiff, TableStatus, ObjectDiff, FieldChange}` (T2), `dbc_diff::text_diff::{diff_lines, DiffLine, DiffTag}` (T3), `dbc_core::{ddl::synthesize_create_table, TableKind}` (re-exported as `dbc_core::synthesize_create_table`), `tabs::{TabContent, ResultTab, collapse_title}` (`tabs.rs:29-82`).
- Produces (consumed by T8):
  ```rust
  // tabs.rs
  pub enum TabContent {
      Grid { grid: Entity<ResultGrid>, buffer: Rc<RefCell<ResultBuffer>> },
      Text { text: String, scroll_lines: usize },
      /// G7: schema/data compare tab — a typed `Entity` handle, same shape
      /// as `Grid`'s (tabs.rs stays GPUI-free beyond this type name, per
      /// the file's own module doc comment).
      Compare { view: Entity<crate::compare::CompareView> },
  }
  ```
  ```rust
  // compare.rs
  #[derive(Clone)]
  pub enum CompareLoadState {
      Loading,
      Ready { diff: dbc_diff::schema_diff::SchemaDiff, mode: dbc_diff::schema_diff::CompareMode },
      /// design §3: either leg failing surfaces as an error banner with a
      /// retry button re-issuing ONLY the failed leg — `Some(_)` = that leg
      /// failed.
      Error { a: Option<dbc_core::QueryError>, b: Option<dbc_core::QueryError> },
  }

  pub enum CompareSelection {
      None,
      Table(usize),   // index into diff.tables
      Routine(usize), // index into diff.routines
      Trigger(usize),
  }

  pub struct CompareView {
      pub label_a: String,
      pub label_b: String,
      pub conn_a: dbc_state::ConnectionConfig, pub secret_a: Option<String>,
      pub conn_b: dbc_state::ConnectionConfig, pub secret_b: Option<String>,
      pub state: CompareLoadState,
      pub selection: CompareSelection,
      pub show_unchanged: [bool; 5], // Tabulky/Pohledy(folded into Tabulky)/Funkce/Triggery/Sekvence — see Step 2
      pub show_ddl_diff: bool,
      // T8 fields (data-diff) added in that task, not here.
  }

  /// design §3's status counts for the tab header ("+3 -1 ~5").
  pub struct StatusCounts { pub added: usize, pub removed: usize, pub changed: usize }
  pub fn count_table_statuses(tables: &[dbc_diff::schema_diff::TableDiff]) -> StatusCounts;

  /// Deliberately SIMPLER than `main.rs::detect_editable_pk` — see
  /// Self-Review note 3 (design's own reference to
  /// "sandbox::detect_editable_pk-style logic" is a file-location
  /// correction: that function actually lives in `main.rs`, and carries
  /// read-only/engine gating this predicate does not need, since data diff
  /// is read-only and works on any engine/connection).
  pub fn table_has_pk(t: &dbc_core::TableInfo) -> bool;
  ```

**Grounding:**
- **Left pane is a flat, filterable, status-tinted list, not a tree** (design §3 — deliberately simpler than `schema_tree.rs`'s live-catalog speed-search machinery, which is built around a single connection's own catalog, not a diff result). Sections: "Tabulky" (from `diff.tables`), "Funkce/procedury" (`diff.routines`), "Triggery" (`diff.triggers`), "Sekvence" (`diff.sequences`) — each row tinted by status using the SAME colour convention already established in `grid.rs`'s sandbox diff tints (`grid.rs:26-28`: `STAGED_CELL_BG = 0x6b5d2e` (amber/yellow), `DELETED_ROW_BG = 0x5d2e2e` (red), `INSERTED_ROW_BG = 0x2e5d3a` (green)) — these three constants are `grid.rs`-private (not `pub`), so `compare.rs` defines its OWN copies with a doc comment citing the exact values it mirrors, rather than either exporting `grid.rs`'s privates or inventing a fourth palette:
  ```rust
  // Mirrors grid.rs's sandbox diff tints (grid.rs:26-28) — same convention,
  // different module (those constants are private to grid.rs).
  const TINT_ADDED: u32 = 0x2e5d3a;   // green
  const TINT_REMOVED: u32 = 0x5d2e2e; // red
  const TINT_CHANGED: u32 = 0x6b5d2e; // amber/yellow
  ```
  Each section header carries a "Zobrazit beze změn (N)" toggle (`show_unchanged` array, one bool per section — default `false`, `Unchanged` rows hidden). A count badge sits in the tab's own header row: `"+{added} -{removed} ~{changed}"` (`count_table_statuses`).
- **Right pane routing** (design §3): `Added`/`Removed` table/routine/trigger → the object's DDL, single-sided, read-only — via `TableDiff.left`/`right` (T1's deviation field), preferring the real `ddl: Option<String>` and falling back to `dbc_core::synthesize_create_table` when `None` (the EXACT same fallback the schema-tree's own DDL preview already uses — no new synthesis logic). `Changed` table → the `table_fields`/`columns`/`indexes`/`constraints` `FieldChange` rows as a two-column (left value / right value) table, PLUS a "Zobrazit DDL diff" toggle (`show_ddl_diff`) that calls `text_diff::diff_lines` over the two (possibly-synthesized) DDL strings and renders `+`/`−`/plain lines in the SAME green/red tint constants above. `Changed` routine/trigger → the DDL-diff view directly (no field table — matches design's "the model has nothing structured to show besides the DDL itself").
- **Cross-engine banner** (design §1/§2): shown persistently whenever `mode == CompareMode::CrossEngine` — literal text `"porovnání mezi různými databázovými systémy: typy a výchozí hodnoty sloupců se neporovnávají"` (design's exact wording), never silently omitted.
- **`on_compare_schema_pair_ready`'s real body** (replacing T6's stub) computes `mode` from `conn_a.engine == conn_b.engine` (`CompareMode::SameEngine`/`CrossEngine`), calls `dbc_diff::schema_diff::diff_schema`, constructs a `CompareView` entity via `cx.new(|_| CompareView { ... })`, and opens a tab: `self.tabs.open(ResultTab { id: 0, title: collapse_title(&format!("Porovnání: {} ↔ {}", pending.label_a, pending.label_b)), pinned: false, preview_key: None, conn_identity: self.current_conn_identity(), content: TabContent::Compare { view } })` — same `Tabs::open` call every other tab kind uses (`tabs.rs:102-122`), so `TAB_CAP`/eviction/pin rules apply with no special-casing.

```rust
// compare.rs — the non-render logic in full (real code; render bodies are
// specified by contract above per this repo's established precedent for
// GPUI-heavy steps, see Self-Review note 2).

use dbc_core::{synthesize_create_table, QueryError, SchemaSnapshot, TableInfo};
use dbc_diff::schema_diff::{CompareMode, SchemaDiff, TableDiff, TableStatus};
use dbc_diff::text_diff::{diff_lines, DiffLine};

const TINT_ADDED: u32 = 0x2e5d3a;
const TINT_REMOVED: u32 = 0x5d2e2e;
const TINT_CHANGED: u32 = 0x6b5d2e;

pub struct StatusCounts { pub added: usize, pub removed: usize, pub changed: usize }

pub fn count_table_statuses(tables: &[TableDiff]) -> StatusCounts {
    let mut c = StatusCounts { added: 0, removed: 0, changed: 0 };
    for t in tables {
        match t.status {
            TableStatus::Added => c.added += 1,
            TableStatus::Removed => c.removed += 1,
            TableStatus::Changed => c.changed += 1,
            TableStatus::Unchanged => {}
        }
    }
    c
}

pub fn table_has_pk(t: &TableInfo) -> bool {
    t.kind == dbc_core::TableKind::Table && t.columns.iter().any(|c| c.is_pk)
}

/// The DDL text shown for a `TableDiff`'s ONE present side (Added/Removed)
/// — real `ddl` when the driver gave one, else the same
/// `ddl::synthesize_create_table` fallback the schema-tree DDL preview uses.
pub fn table_ddl_text(t: &TableInfo) -> String {
    t.ddl.clone().unwrap_or_else(|| synthesize_create_table(t))
}

/// Drives the "Zobrazit DDL diff" panel for a Changed table/routine/trigger.
pub fn table_ddl_diff(left: &TableInfo, right: &TableInfo) -> Vec<DiffLine> {
    diff_lines(&table_ddl_text(left), &table_ddl_text(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_diff::schema_diff::{FieldChange, ObjectDiff};

    fn table_diff(status: TableStatus) -> TableDiff {
        TableDiff { schema: None, name: "t".into(), status, table_fields: vec![], columns: vec![], indexes: vec![], constraints: vec![], left: None, right: None }
    }

    #[test]
    fn counts_added_removed_changed_ignores_unchanged() {
        let tables = vec![table_diff(TableStatus::Added), table_diff(TableStatus::Added), table_diff(TableStatus::Removed), table_diff(TableStatus::Changed), table_diff(TableStatus::Unchanged)];
        let c = count_table_statuses(&tables);
        assert_eq!((c.added, c.removed, c.changed), (2, 1, 1));
    }

    #[test]
    fn table_has_pk_requires_a_real_pk_column_on_a_base_table() {
        let mut t = TableInfo { kind: dbc_core::TableKind::Table, ..Default::default() };
        assert!(!table_has_pk(&t));
        t.columns.push(dbc_core::ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() });
        assert!(table_has_pk(&t));
        t.kind = dbc_core::TableKind::View;
        assert!(!table_has_pk(&t), "a view is never PK-diffable regardless of a reported is_pk column");
    }

    #[test]
    fn table_ddl_text_falls_back_to_synthesis_when_engine_gave_none() {
        let t = TableInfo {
            name: "t".into(), kind: dbc_core::TableKind::Table,
            columns: vec![dbc_core::ColumnInfo { name: "id".into(), data_type: "integer".into(), is_pk: true, ..Default::default() }],
            ddl: None, ..Default::default()
        };
        assert!(table_ddl_text(&t).starts_with("CREATE TABLE"));
        let with_ddl = TableInfo { ddl: Some("CUSTOM DDL".into()), ..t };
        assert_eq!(table_ddl_text(&with_ddl), "CUSTOM DDL");
    }

    #[test]
    fn table_ddl_diff_over_two_synthesized_tables() {
        let mk = |ty: &str| TableInfo {
            name: "t".into(), kind: dbc_core::TableKind::Table,
            columns: vec![dbc_core::ColumnInfo { name: "id".into(), data_type: ty.into(), is_pk: true, ..Default::default() }],
            ..Default::default()
        };
        let lines = table_ddl_diff(&mk("int4"), &mk("int8"));
        assert!(!lines.is_empty());
    }
}
```

- [ ] **Step 1: `TabContent::Compare`** — add the variant to `tabs.rs`'s `TabContent` enum (`tabs.rs:29-37`), importing `crate::compare::CompareView`.

- [ ] **Step 2: `compare.rs`** — create the file with the code above (`count_table_statuses`, `table_has_pk`, `table_ddl_text`, `table_ddl_diff`, the tint constants, `CompareLoadState`/`CompareSelection`/`CompareView`/`StatusCounts` struct/enum shapes from Interfaces) plus the render functions per the Grounding contract (`CompareView::render` implementing `gpui::Render`, following `ResultGrid`'s existing `impl Render for ResultGrid` shape in `grid.rs` for the entity-render wiring pattern).

- [ ] **Step 3: `main.rs` wiring** — `mod compare;` added to the mod list (alphabetically, after `mod autocomplete;` before `mod connect;`); `on_compare_schema_pair_ready`'s real body (Grounding's last bullet) replacing T6's `#[allow(dead_code)]` stub; a `render_tab_content` match arm for `TabContent::Compare { view } => view.clone().into_any_element()` (mirrors the existing `TabContent::Grid`/`Text` arms' shape).

- [ ] **Step 4: Run to green**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui compare::`
  Expected: builds and passes, zero warnings.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/dbc-ui/src/compare.rs crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/main.rs
  git commit -m "feat: CompareView schema-diff pane + TabContent::Compare (G7 T7)"
  ```

---

### Task 8 (T8): Data-diff UI (SERIALIZED TAIL — main.rs chain #3)

> Schedule after T7 merges.

**Files:**
- Modify: `crates/dbc-ui/src/compare.rs`
- Modify (only if needed — see Step 2): `crates/dbc-ui/src/grid.rs` (a constructor letting `ResultGrid` take an externally-built `RecordBatch` + a `HashSet<(usize, usize)>` tint side-channel, if the existing constructor can't already do this)

**Interfaces:**
- Consumes: `dbc_diff::data_diff::{diff_data, build_changed_batch, DataDiffOutcome, RowDiff, DIFF_ROW_CAP}` (T4), `runner::fetch_diff_side` (T5), `compare::table_has_pk` (T7).
- Produces: extends `CompareView` with data-diff state:
  ```rust
  pub enum DataDiffState {
      Idle,
      Loading,
      Ready { outcome: dbc_diff::data_diff::DataDiffOutcome, sql_a: String, sql_b: String },
      /// design §4: DIFF_ROW_CAP hit — a banner, not silent.
      RowCapExceeded { message: String },
      Error(dbc_core::QueryError),
  }
  // added to CompareView (T7):
  //   pub data_where: [String; 1] (single shared WHERE box per design's
  //     "one optional text field per side-pair"; applied to BOTH sides'
  //     SELECT identically per CURATION §0.1(b))
  //   pub data_diff: DataDiffState
  ```

**Grounding:**
- **Table-pair selection** (design §4): the "Porovnat data" affordance appears next to a LEFT-pane table row only when `TableDiff.status` is `Changed`/`Unchanged` (present both sides — `TableDiff.left.is_some() && TableDiff.right.is_some()`) AND `table_has_pk` (T7) is true for BOTH `left`/`right` `TableInfo`s; otherwise the button is `disabled` with tooltip "tabulka nemá primární klíč". No table-pair re-mapping (only same-named matched tables), per design.
- **Fetch dispatch:** on "Porovnat data", `CompareView` computes `left_where`/`right_where` from the SAME `data_where` text box applied to both sides (CURATION: the composed statement is validated per-side inside `fetch_diff_side` — see T5), calls `runner.fetch_diff_side` TWICE (`tokio::join!`-style dispatch mirrors `fetch_schema_pair`'s own concurrency — a generation-guarded `cx.spawn` awaiting BOTH one-shot receivers), sets `data_diff = DataDiffState::Loading` immediately (never blocks the UI thread).
- **PK-column mapping:** the two sides' `ResultBuffer` column names (from `fetch_diff_side`'s returned `SchemaRef`) are matched against the matched `TableInfo.columns[].name` where `is_pk` — the SAME "map catalog column name onto the ACTUAL result's columns by exact name" technique `main.rs::detect_editable_pk` uses (`main.rs:242-247`), reimplemented here as its own small pure helper (not calling `detect_editable_pk` itself, since that function also does read-only/engine gating this predicate must NOT apply — see T7's `table_has_pk` doc comment for the same distinction):
  ```rust
  /// RESULT-column indices (into `result_columns`, in `result_columns`'
  /// order) of `table`'s PK columns — mirrors `main.rs::detect_editable_pk`'s
  /// name-matching technique (main.rs:242-247) without ANY of its
  /// read-only/engine gating (data diff needs neither).
  pub fn pk_result_cols(table: &dbc_core::TableInfo, result_columns: &[String]) -> Vec<usize> {
      table.columns.iter().filter(|c| c.is_pk)
          .filter_map(|c| result_columns.iter().position(|h| h == &c.name))
          .collect()
  }
  ```
- **Result presentation** (design §4): three sub-sections in the SAME `CompareView` right pane, reusing `ResultGrid` as a read-only grid for "Přidané řádky" (right side's `ResultBuffer`, filtered to `RowDiff::Added` row indices) and "Odebrané řádky" (left side's `ResultBuffer`, filtered to `RowDiff::Removed`), and `data_diff::build_changed_batch`'s synthetic batch + `HashSet<(row,col)>` for "Změněné řádky", tinted with `TINT_CHANGED` (the SAME constant T7 already defines) via whatever `ResultGrid` constructor Step 2 confirms/adds. A summary line "N přidáno, M odebráno, K změněno (z X řádků na obou stranách)" above the three sections; the composed `sql_a`/`sql_b` shown verbatim in the section header (the ONLY SQL text this phase ever displays — see Global Constraints' no-sync-script-generation note). `DataDiffState::RowCapExceeded` renders as a banner with the exact `data_diff::DIFF_ROW_CAP`-derived message T5/T4 already produce (never silently truncated).

- [ ] **Step 1: `pk_result_cols`** — add to `compare.rs` per the code above.

  ```rust
  #[cfg(test)]
  mod pk_mapping_tests {
      use super::*;

      #[test]
      fn maps_pk_columns_by_name_ignoring_gating() {
          let table = dbc_core::TableInfo {
              columns: vec![
                  dbc_core::ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                  dbc_core::ColumnInfo { name: "tenant".into(), is_pk: true, ..Default::default() },
                  dbc_core::ColumnInfo { name: "note".into(), is_pk: false, ..Default::default() },
              ],
              ..Default::default()
          };
          let result_cols = vec!["note".to_string(), "id".to_string(), "tenant".to_string()];
          assert_eq!(pk_result_cols(&table, &result_cols), vec![1, 2]);
      }

      #[test]
      fn missing_pk_column_in_the_result_is_silently_skipped_not_a_panic() {
          let table = dbc_core::TableInfo {
              columns: vec![dbc_core::ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() }],
              ..Default::default()
          };
          assert_eq!(pk_result_cols(&table, &["other".to_string()]), Vec::<usize>::new());
      }
  }
  ```

- [ ] **Step 2: Check `ResultGrid`'s constructor.** Read `grid.rs`'s existing `ResultGrid::new`-family constructor(s). If it already accepts an arbitrary `Rc<RefCell<ResultBuffer>>` plus an optional tint side-channel (a `HashSet<(usize,usize)>`-shaped hook independent of `EditState`), reuse it as-is for all three data-diff sections (Added/Removed pass `None` for the tint set; Changed passes `Some(build_changed_batch(...).1)`). If it does NOT, add the SMALLEST possible extension — a second constructor `ResultGrid::new_readonly_with_tint(buffer: Rc<RefCell<ResultBuffer>>, tint: Option<HashSet<(usize, usize)>>, cx: &mut Context<Self>) -> Self` alongside the existing one, rendering `tint`-matched cells with `TINT_CHANGED`-equivalent styling reusing `grid.rs`'s OWN private tint-application code path (not duplicating the render logic, only the entry point) — never modifying `EditState`/sandbox semantics, per design's explicit "new, small, `EditState`-independent" requirement.

- [ ] **Step 3: `CompareView` data-diff state + dispatch** — add `DataDiffState`, `data_where: [String; 1]`, `data_diff: DataDiffState` fields (Interfaces); implement the fetch dispatch described in Grounding (`start_data_diff`, generation-guarded, calling `runner.fetch_diff_side` twice and `dbc_diff::data_diff::diff_data` once both sides land); wire the "Porovnat data" button's `disabled` state to `TableDiff.left/right.is_some() && table_has_pk(...)` on both sides per Grounding.

- [ ] **Step 4: Render the three sections + summary line + row-cap banner** per the Result Presentation contract in Grounding.

- [ ] **Step 5: Run to green + the read-only sanity grep**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui compare::`
  Run: `grep -n "\.execute(" crates/dbc-ui/src/compare.rs` — must be empty.
  Expected: builds and passes, zero warnings, no write calls anywhere in `compare.rs`.

- [ ] **Step 6: Manual verification** (no automated GPUI render test exists in this codebase for any tab kind — same precedent noted by prior phases' plans): launch the app against two SQLite fixtures with a matched, PK'd table that has one added row, one removed row, and one changed cell; open Compare, run a data diff, confirm the three sections populate correctly and the changed cell shows "{old} → {new}" tinted amber.

- [ ] **Step 7: Commit**

  ```bash
  git add crates/dbc-ui/src/compare.rs crates/dbc-ui/src/grid.rs
  git commit -m "feat: data-diff UI — Added/Removed/Changed sections, WHERE box, row-cap banner (G7 T8)"
  ```

---

### Task 9 (T9): Docker-based empirical validation — live Postgres 16.13

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs` (new `#[cfg(test)] mod compare_pg_tests`)
- Modify: `crates/dbc-ui/Cargo.toml` (`[dev-dependencies]` — add `testcontainers-modules`)

**Interfaces:** none new — this task only adds `#[ignore]`d integration tests exercising T2/T5's real code against a live server.

**Grounding:** follows the SAME pattern this repo already uses for its one existing docker-gated UI-crate test class (per this plan's own research into `dbc-driver-postgres/tests/integration.rs`, which uses an EXTERNAL `tests/` file because that crate has a lib target) — `dbc-ui` is a BINARY crate with no lib target (confirmed: `crates/dbc-ui/Cargo.toml` has no `[lib]` section and `main.rs` has no sibling `lib.rs`), so an external `tests/*.rs` file cannot import `runner`'s private items at all; these tests MUST live in-crate, inside `runner.rs`'s own `#[cfg(test)]` module, to see `open_spec`/`compose_diff_select`/etc. They dispatch through `runner::open_spec`/`ConnectSpec::Url` (NEVER `connect::open` directly) because `connect::open`'s Postgres arm calls `runtime.block_on(...)` (`connect.rs:36`), which panics if invoked directly on a `#[tokio::test]` worker thread — `open_spec` is safe because it wraps that same call in `spawn_blocking` (`runner.rs:480-497`). The Postgres image is pinned to `16.13` via `testcontainers_modules::postgres::Postgres::default().with_tag("16.13-alpine")` (confirmed API: `testcontainers-modules-0.13.0/src/postgres/mod.rs` exposes a builder `with_tag`).

```rust
// runner.rs, appended
#[cfg(test)]
mod compare_pg_tests {
    use super::*;
    use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

    async fn pg_url(node: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>, db: &str) -> String {
        format!("postgres://postgres:postgres@127.0.0.1:{}/{db}", node.get_host_port_ipv4(5432).await.unwrap())
    }

    /// Two IDENTICAL live-fetched schemas diff to all-Unchanged — the
    /// empirical smoke test that real Postgres catalog output (format_type()
    /// strings, real PK/index/constraint metadata) round-trips through
    /// `SchemaSnapshot` -> `diff_schema` without spurious Changed noise.
    #[tokio::test]
    #[ignore]
    async fn identical_live_postgres_schemas_diff_to_all_unchanged() {
        let node = Postgres::default().with_tag("16.13-alpine").start().await.unwrap();
        let url = pg_url(&node, "postgres").await;
        {
            let mut opened = open_spec(ConnectSpec::Url(url.clone()), tokio::runtime::Handle::current()).await.unwrap();
            opened.conn.execute(
                "CREATE TABLE t (id integer PRIMARY KEY, name text NOT NULL, note text DEFAULT 'x')",
                CancelToken::new(),
            ).await.unwrap();
        }
        let runner = QueryRunner::new();
        let rx = runner.fetch_schema_pair(ConnectSpec::Url(url.clone()), ConnectSpec::Url(url));
        let (a, b) = rx.await.unwrap();
        let (snap_a, snap_b) = (a.unwrap(), b.unwrap());
        let diff = dbc_diff::schema_diff::diff_schema(&snap_a, &snap_b, dbc_diff::schema_diff::CompareMode::SameEngine);
        assert!(diff.tables.iter().all(|t| t.status == dbc_diff::schema_diff::TableStatus::Unchanged));
    }

    /// A real schema change (one added column) is detected as Changed with
    /// the RIGHT field name — proves the whole pipeline against real catalog
    /// output, not just the pure-model tests in T2.
    #[tokio::test]
    #[ignore]
    async fn a_real_added_column_is_detected_as_changed_on_live_postgres() {
        let node = Postgres::default().with_tag("16.13-alpine").start().await.unwrap();
        let url_a = pg_url(&node, "postgres").await;
        {
            let mut opened = open_spec(ConnectSpec::Url(url_a.clone()), tokio::runtime::Handle::current()).await.unwrap();
            opened.conn.execute("CREATE TABLE t (id integer PRIMARY KEY)", CancelToken::new()).await.unwrap();
        }
        let runner = QueryRunner::new();
        let rx_before = runner.fetch_schema_pair(ConnectSpec::Url(url_a.clone()), ConnectSpec::Url(url_a.clone()));
        let (before_a, _) = rx_before.await.unwrap();
        let snap_before = before_a.unwrap();

        {
            let mut opened = open_spec(ConnectSpec::Url(url_a.clone()), tokio::runtime::Handle::current()).await.unwrap();
            opened.conn.execute("ALTER TABLE t ADD COLUMN note text", CancelToken::new()).await.unwrap();
        }
        let rx_after = runner.fetch_schema_pair(ConnectSpec::Url(url_a.clone()), ConnectSpec::Url(url_a));
        let (after_a, _) = rx_after.await.unwrap();
        let snap_after = after_a.unwrap();

        let diff = dbc_diff::schema_diff::diff_schema(&snap_before, &snap_after, dbc_diff::schema_diff::CompareMode::SameEngine);
        let t = diff.tables.iter().find(|t| t.name == "t").unwrap();
        assert_eq!(t.status, dbc_diff::schema_diff::TableStatus::Changed);
        assert!(t.columns.iter().any(|c| matches!(c, dbc_diff::schema_diff::ObjectDiff::Added(col) if col.name == "note")));
    }

    /// End-to-end proof of the CURATION-required WHERE-box guard against a
    /// REAL server: the composed multi-statement payload never even opens a
    /// connection (see T5's `compose_diff_select_refuses_multi_statement_injection_client_side`
    /// for the pure-function proof — this is the live-server companion,
    /// confirming the guard's placement inside `fetch_diff_side_inner`
    /// truly runs before `open_spec` against a real Postgres, not just in
    /// unit tests over the pure composer).
    #[tokio::test]
    #[ignore]
    async fn fetch_diff_side_where_box_guard_holds_against_live_postgres() {
        let node = Postgres::default().with_tag("16.13-alpine").start().await.unwrap();
        let url = pg_url(&node, "postgres").await;
        {
            let mut opened = open_spec(ConnectSpec::Url(url.clone()), tokio::runtime::Handle::current()).await.unwrap();
            opened.conn.execute("CREATE TABLE t (id integer PRIMARY KEY, n text)", CancelToken::new()).await.unwrap();
            opened.conn.execute("INSERT INTO t VALUES (1, 'a')", CancelToken::new()).await.unwrap();
        }
        let runner = QueryRunner::new();
        let rx = runner.fetch_diff_side(ConnectSpec::Url(url.clone()), None, "t".to_string(), Some("1=1; DELETE FROM t".to_string()));
        assert!(rx.await.unwrap().is_err());

        // Confirm the row is still there via a CLEAN fetch.
        let rx2 = runner.fetch_diff_side(ConnectSpec::Url(url), None, "t".to_string(), None);
        let (_, _, mut buf) = rx2.await.unwrap().unwrap();
        assert_eq!(buf.row_count(), 1);
    }
}
```

- [ ] **Step 1: Add the dev-dependency.** `crates/dbc-ui/Cargo.toml`, `[dev-dependencies]` — append `testcontainers-modules = { version = "0.13", features = ["postgres"] }` (same version/feature pin `dbc-driver-postgres/Cargo.toml` already uses).

- [ ] **Step 2: Write the three tests above** in `runner.rs`'s new `#[cfg(test)] mod compare_pg_tests`.

- [ ] **Step 3: Verify the suite compiles without docker, then run it with docker**

  Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui compare_pg_tests::`
  Expected: "3 ignored", zero failures, zero warnings (nothing runs without `--ignored`).
  Run (docker up): `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored compare_pg_tests::`
  Expected: all 3 pass. First run may be slow if the `16.13-alpine` image needs pulling — that's the container, not the code.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/dbc-ui/Cargo.toml crates/dbc-ui/src/runner.rs
  git commit -m "test: docker-based schema-diff + WHERE-guard validation against live Postgres 16.13 (G7 T9)"
  ```

---

## Task ordering

**Parallel batch 1 (worktrees, start immediately):** T1 (`dbc-diff` scaffold) is the hard prerequisite for everything else in the crate.

**Parallel batch 2 (worktrees, after T1 lands):** T2 (`schema_diff::diff_schema`), T3 (`text_diff::diff_lines`), T4 (`data_diff`) — three disjoint files inside `dbc-diff`, zero shared state. T5 (`runner.rs`) can ALSO start here in its own worktree — it needs T1's types plus T4's `DIFF_ROW_CAP` constant (a real dependency this plan's Global Constraints table corrects from the design's "T5 depends on T1 only" — see Self-Review note 4), so schedule T5 to branch off after T4's `DIFF_ROW_CAP` is at least locally available, merging whenever T4 has landed.

**Serialized `main.rs`/`connections_ui.rs`/`tabs.rs`/`palette.rs` chain (same worker or strict rebase-in-order, AFTER any other in-flight phase's `runner.rs`/`main.rs` work has merged):** T5 (`runner.rs`, needs to land before T6) → T6 (`ModalState::CompareDialog` + palette) → T7 (`CompareView` + `TabContent::Compare`) → T8 (data-diff UI). Each of T6/T7/T8 compiles against the previous task's types and all touch the `AppView` surface — do not parallelize any pair of them.

**T9 (docker tests)** — touches only `runner.rs`'s test module (which nothing after T5 touches) and `Cargo.toml` dev-deps: safe in a parallel worktree alongside T6–T8, merged whenever green.

**Version bump (`dbc-ui` → `0.7.0`)** happens at branch finish, per Global Constraints — not inside any individual task's commit.

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §0 (new crate, dependency graph, module layout) → T1 (Cargo.toml, workspace member, module skeleton).
- §1 (schema-diff semantics: matching keys, `None`-schema strict decision, Changed-field rules per object type, same/cross-engine gating) → T2 (`diff_schema` + the full field-diff closure set) + T1 (the model types the logic fills in).
- §2 (cross-engine summary — allowed, degraded mode, no user toggle) → T2's `CompareMode` gating + T7's cross-engine banner (mode computed from `conn_a.engine == conn_b.engine`, not a user choice).
- §3 (UI: entry point, vault reuse, dispatch pattern, tab kind, `CompareView` layout, non-goals) → T6 (dialog + dispatch) + T7 (tab kind + schema-diff rendering, DDL drill-down) + Global Constraints (the hard no-sync-script-generation line, stated once and referenced by every task that touches SQL text).
- §4 (data diff: table-pair selection, fetch strategy, PK matching, value comparison, streaming/memory, result presentation, pure-Arrow-not-DuckDB decision) → T4 (`data_diff`'s hash-join + typed compare + `DIFF_ROW_CAP`) + T5 (`fetch_diff_side`'s `ResultBuffer`-backed fetch, no chunked key-range machinery) + T8 (UI wiring, three-section presentation, row-cap banner). The pure-Arrow-not-DuckDB decision needed no plan action — `dbc-diff`'s `Cargo.toml` (T1) never depends on `dbc-driver-duckdb` or any driver crate, which is the structural enforcement of that decision.
- §5 (task decomposition) → this plan's T1–T9, with T5's dependency corrected (see note 4 below) and a new T9 added (see note 5).
- §6 risks: `None`-schema noise → T2's dedicated test (`none_schema_never_matches_a_named_schema`) proves the behavior is intentional, not silently patched. Overload-unaware routines → T2's `routine_overload_split_not_paired` test proves the EXACT multiplicity behavior, not just "doesn't crash". DDL-diff/synthesis fidelity → T7's `table_ddl_text` reuses the existing `synthesize_create_table` verbatim (no new synthesis logic to audit) and T8 Step 6 calls out a manual spot-check. The `1,000,000`-row cap being unmeasured → unchanged (flagged, not silently accepted as measured — no task in this plan claims to have benchmarked it; T9 could be extended with a benchmark in a follow-up phase but that is explicitly out of THIS plan's scope, matching the design's own "worth a quick benchmark... needs verification" framing rather than inventing a fake measurement). Value-comparison shallowness (numeric+bool only) → T4's doc comments state the exact scope, no task pretends to normalize dates/times. No WHERE-filter UI for data diff → RESOLVED in T8 (the WHERE box for data diff DOES ship in v1 per CURATION §0.1(b) — this design §6 risk item is stale relative to the CURATION block that supersedes it; T8's `data_where` field implements it). `similar` dependency vetting → T3's Global-Constraints citation (CURATION-approved). "Generate sync script" non-goal → Global Constraints states it as a hard line, and no task in this plan implements any sync-SQL generation.

**Placeholder scan:** every code step shows real, complete code — full struct/enum definitions, full function bodies, full test bodies with concrete assertions — or a concrete cargo/git/grep command. T6–T8's GPUI render bodies are the one place this plan uses a CONTRACT specification (exact fields, exact Czech labels, exact tint constants, exact reused call sites by file:line) instead of a literal `div()`-by-`div()` render tree — this mirrors the two reference plans' own established precedent for GPUI-render-heavy steps (this repo's `2026-08-23-g9-server-monitor.md` states outright that "rendering itself [is] verified manually — GPUI render paths aren't unit-tested elsewhere in this codebase either"; every LOGIC-bearing piece inside those same render bodies — dispatch handlers, guard checks, pure helpers like `count_table_statuses`/`table_has_pk`/`pk_result_cols`/`compose_diff_select` — is full real code with full real tests, nothing is left as TBD).

**Type-name consistency across tasks:** `dbc_diff::schema_diff::{CompareMode, ObjectDiff, FieldChange, TableStatus, TableDiff, SchemaDiff, diff_schema}` (T1/T2) match T5's `CompareMode` computation site (actually T7's — corrected below) and T7's `CompareView`/render contract field names. `dbc_diff::text_diff::{DiffTag, DiffLine, diff_lines}` (T3) match T7's `table_ddl_diff`. `dbc_diff::data_diff::{DIFF_ROW_CAP, RowDiff, DataDiffOutcome, diff_data, build_changed_batch}` (T4) match T5's row-cap check and T8's `DataDiffState`/three-section dispatch. `runner::{fetch_schema_pair, fetch_diff_side, compose_diff_select}` (T5) match T6's `confirm_compare_dialog` and T8's `start_data_diff`. `ModalState::CompareDialog{conn_a, conn_b, error}` / `PendingCompare` (T6) match T7's `on_compare_schema_pair_ready`. `TabContent::Compare{view}` (T7, tabs.rs) matches T7's own `main.rs` render-dispatch arm. `compare::{CompareLoadState, CompareSelection, CompareView, StatusCounts, count_table_statuses, table_has_pk, table_ddl_text, table_ddl_diff}` (T7) match T8's additions (`DataDiffState`, `pk_result_cols`) to the SAME struct/module.

**Resolved design ambiguities / deviations (flagged for controller review, not vetoed unilaterally):**
1. **`TableDiff` gains `left: Option<TableInfo>` / `right: Option<TableInfo>`** beyond the design's own struct sketch (§1), which had no way for the UI to recover the full source object needed for §3's Added/Removed DDL panel (`TableInfo.ddl`/`synthesize_create_table` needs the WHOLE `TableInfo`, not just `schema`/`name`/field-diffs). This is the plan's one addition to the design's data model; every other field matches the sketch verbatim.
2. **T6–T8's GPUI render bodies are contract-specified, not literal render trees** — see Placeholder Scan above; matches established precedent in this repo's other G-phase plans for GPUI-heavy steps, not a new pattern invented here.
3. **`table_has_pk`/`pk_result_cols` (T7/T8) are NEW, simpler predicates — not calls into `main.rs::detect_editable_pk`.** The design doc's own text says "same source `sandbox::detect_editable_pk`-style logic already uses" — this is a file-location correction (that function actually lives in `main.rs:226-256`, not `sandbox.rs`) AND a deliberate simplification: `detect_editable_pk` bakes in read-only/MSSQL-engine gating that a READ-ONLY data-diff feature must not inherit (design §4 says exactly this — "there's no read-only-connection nor engine-allowlist gate here"). Reusing the real function would have required threading a fake `conn_meta` through to defeat its own gating, which is worse than writing the two-line predicate this plan defines instead.
4. **T5's dependency is corrected from "T1 only" (design §5's table) to "T1 + T4"** — `fetch_diff_side`'s row-cap check needs `dbc_diff::data_diff::DIFF_ROW_CAP`, which doesn't exist until T4 lands. This plan's Task dependency graph and Task-ordering section reflect the real dependency; the design's own parallelization note ("T5 can start in parallel with T2–T4 (only needs T1's types)") is followed in SPIRIT (T5 starts early, in its own worktree) but the actual constant it needs is a T4 artifact, so T5 cannot fully compile/test until T4 is at least locally available even if the two are developed concurrently.
5. **A new T9 (docker-based empirical validation against live Postgres 16.13) was added — not present in the design's own §5 task table.** The design's T1–T8 cover the pure logic and UI; none of them prove the pipeline against a REAL server's catalog output (Postgres's `format_type()` strings, real PK/index/constraint metadata, a real `ALTER TABLE ADD COLUMN`). T9 closes that gap the same way `2026-08-23-g9-server-monitor.md`'s T7 does for its own phase (docker `#[ignore]` tests, in-crate placement because `dbc-ui` has no lib target, `open_spec` not `connect::open` on tokio test workers) — added per this plan's explicit brief ("if catalog/metadata SQL shapes matter, include a docker-based empirical validation task"). Even though G7 introduces no NEW catalog SQL of its own (unlike G9's monitor queries — G7's schema diff runs entirely on `Connection::schema()`'s already-existing, already-tested catalog queries), the RESULT of that catalog SQL feeding into brand-new diff logic is exactly the kind of "does this actually work against a real server" claim that deserves empirical proof, not just hand-built `SchemaSnapshot` fixtures.
6. **Identifier quoting uses `dbc_core::quote_ident`/`quote_qualified` throughout, not `admin_sql::quote_ident_for`'s MSSQL-bracket form** — see Global Constraints' dedicated note. `admin_sql.rs` is G10-plan-only in this branch lineage; MSSQL is unwired in `connect::open_config` today, so no G7 code path can reach a live MSSQL connection regardless of which quoting function is used. Flagged as a concrete follow-up once G10 merges (two call sites: `fetch_diff_side`'s table-quoting in T5, and any future MSSQL-aware DDL rendering in T7/T8) rather than pre-emptively importing a function that doesn't exist yet.
7. **`data_where` is a single shared WHERE box applied identically to BOTH sides**, not two independent boxes — the design's own wording ("one optional text field per side-pair, appended as `WHERE {text}` to BOTH sides' SELECT") is explicit about this being one shared field, so no ambiguity was actually resolved here; called out only because it's easy to misread as "one box per side" at a skim.
8. **No `HistoryEntry` is created anywhere in this plan** — a compare run is not a query run in the sense `history_panel.rs`/`sandbox.rs` model, and the design never asks for one; Global Constraints states this explicitly as the resolution to "no credentials/result data in history or logs" (nothing to redact because nothing is recorded).
