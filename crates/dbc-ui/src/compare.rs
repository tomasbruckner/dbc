//! G7 T7/T8: `CompareView` — the per-tab GPUI entity behind
//! `TabContent::Compare`. Renders the schema diff (`dbc_diff::schema_diff`)
//! computed by `AppView::on_compare_schema_pair_ready` (main.rs) from a
//! `fetch_schema_pair` result: a left-pane status-tinted object list
//! (Tabulky/Funkce/Triggery/Sekvence) and a right-pane detail view
//! (Added/Removed DDL, Changed field table + optional DDL-diff drill-down)
//! — design §3. T8 extends this same entity with an in-process PK-based
//! data diff (`dbc_diff::data_diff`) for one selected matched table pair,
//! dispatched via `runner::fetch_diff_side`.
//!
//! T8's data-diff fetch needs a `QueryRunner`, which `CompareView` (like
//! every other tab-content entity in this codebase, e.g. `MonitorView`)
//! does not own itself — `AppView` is the sole owner. The "Porovnat data"
//! click therefore EMITS `CompareViewEvent::DataDiffRequested` rather than
//! dispatching directly; `AppView::on_compare_view_event` (main.rs) is the
//! subscriber that actually calls `self.runner.fetch_diff_side` and feeds
//! the result back into this entity via `Entity::update` — same "view
//! emits, AppView owns the runner and subscribes" shape `MonitorView`'s
//! `KillRequested` uses.
//!
//! READ-ONLY end to end: nothing in this file ever calls `.execute(` — the
//! only SQL text ever shown to the user is the exact composed `SELECT`
//! `fetch_diff_side` ran (design CURATION §0.1(d)/Global Constraints — no
//! sync-script generation of any kind).

use std::collections::HashSet;

use dbc_core::arrow::array::{Array, RecordBatch, StringArray};
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{synthesize_create_table, QueryError, RoutineInfo, TableInfo, TriggerInfo};
use dbc_diff::data_diff::{self, RowDiff};
use dbc_diff::schema_diff::{CompareMode, FieldChange, ObjectDiff, SchemaDiff, TableDiff, TableStatus};
use dbc_diff::text_diff::{diff_lines, DiffLine, DiffTag};
use gpui::{
    div, prelude::*, px, AnyElement, ClickEvent, Context, Div, EventEmitter, Hsla, SharedString, Stateful, Window,
};

use crate::runner::{ConnectSpec, QueryRunner};
use crate::theme::{ActiveTheme, Theme};

// G14 Task 4: these used to be this file's own TINT_ADDED/TINT_REMOVED/
// TINT_CHANGED consts (same hex family as grid.rs's G5 sandbox diff consts,
// grid.rs:26-28) — now `Theme::diff_inserted_bg`/`diff_deleted_bg`/
// `diff_staged_bg`, the SAME fields grid.rs's sweep maps its consts onto
// (Sweep Rulebook), so a light-mode compare diff matches a light-mode
// sandbox diff automatically.

/// Bounds how many individual rows ANY per-frame render loop in this file
/// ever emits — both the data-diff sections (Added/Removed/Changed row
/// lists, T8) and the schema object lists (Tabulky/Funkce/Triggery/
/// Sekvence, T7 — review fix MINOR 2: those had no cap at all). This is
/// INDEPENDENT of `data_diff::DIFF_ROW_CAP` (1,000,000), which bounds the
/// DIFF COMPUTATION, not on-screen rendering — rendering a million (or even
/// a few thousand) `div`s would freeze the UI thread regardless of how
/// correct the diff/schema is. For the data-diff sections this cap is also
/// applied BEFORE the expensive per-row work runs (see
/// `apply_data_diff_result`/`build_changed_rows_display`), not just at
/// render time — see the MAJOR review fix note on `DataDiffSummary`. The
/// exact SQL each side ran for a data diff is always shown in full (see
/// `render_data_diff_outcome`) so a user who needs the full row set can
/// always re-run it in a normal query tab.
const DISPLAY_ROW_CAP: usize = 200;

// ---------------------------------------------------------------------
// Pure logic (fully unit-tested below).
// ---------------------------------------------------------------------

/// design §3's status counts for the tab header ("+3 -1 ~5").
pub struct StatusCounts {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
}

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

/// Deliberately SIMPLER than `main.rs::detect_editable_pk` — see this
/// phase's plan Self-Review note 3: that function bakes in read-only/MSSQL-
/// engine gating a READ-ONLY data-diff feature must not inherit. A view is
/// never PK-diffable regardless of a (possibly stale) reported `is_pk`
/// column.
pub fn table_has_pk(t: &TableInfo) -> bool {
    t.kind == dbc_core::TableKind::Table && t.columns.iter().any(|c| c.is_pk)
}

/// The DDL text shown for a `TableDiff`'s ONE present side (Added/Removed)
/// — real `ddl` when the driver gave one, else the same
/// `ddl::synthesize_create_table` fallback the schema-tree DDL preview uses.
pub fn table_ddl_text(t: &TableInfo) -> String {
    t.ddl.clone().unwrap_or_else(|| synthesize_create_table(t))
}

/// Drives the "Zobrazit DDL diff" panel for a Changed table.
pub fn table_ddl_diff(left: &TableInfo, right: &TableInfo) -> Vec<DiffLine> {
    diff_lines(&table_ddl_text(left), &table_ddl_text(right))
}

fn routine_ddl_text(r: &RoutineInfo) -> String {
    r.ddl.clone().unwrap_or_else(|| format!("-- {:?} {} {}", r.kind, r.name, r.signature))
}

fn trigger_ddl_text(t: &TriggerInfo) -> String {
    t.ddl.clone().unwrap_or_else(|| format!("-- trigger {} on {}", t.name, t.table))
}

/// Flattens every `Changed` object's `FieldChange`s under `t` into one
/// display-ready list, prefixed with a Czech context label — this IS the
/// "two-column (left value / right value) table" the render contract calls
/// for; kept separate from rendering so it's directly unit-testable.
fn table_field_rows(t: &TableDiff) -> Vec<(String, FieldChange)> {
    let mut rows = Vec::new();
    for fc in &t.table_fields {
        rows.push(("tabulka".to_string(), fc.clone()));
    }
    for c in &t.columns {
        if let ObjectDiff::Changed { left, fields, .. } = c {
            for fc in fields {
                rows.push((format!("sloupec {}", left.name), fc.clone()));
            }
        }
    }
    for i in &t.indexes {
        if let ObjectDiff::Changed { left, fields, .. } = i {
            for fc in fields {
                rows.push((format!("index {}", left.name), fc.clone()));
            }
        }
    }
    for c in &t.constraints {
        if let ObjectDiff::Changed { left, fields, .. } = c {
            for fc in fields {
                rows.push((format!("omezení {}", left.name), fc.clone()));
            }
        }
    }
    rows
}

/// Existence-level (Added/Removed, not field-changed) columns/indexes/
/// constraints under a `Changed` table — `(added labels, removed labels)`.
fn table_existence_rows(t: &TableDiff) -> (Vec<String>, Vec<String>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for c in &t.columns {
        match c {
            ObjectDiff::Added(x) => added.push(format!("sloupec {}", x.name)),
            ObjectDiff::Removed(x) => removed.push(format!("sloupec {}", x.name)),
            _ => {}
        }
    }
    for i in &t.indexes {
        match i {
            ObjectDiff::Added(x) => added.push(format!("index {}", x.name)),
            ObjectDiff::Removed(x) => removed.push(format!("index {}", x.name)),
            _ => {}
        }
    }
    for c in &t.constraints {
        match c {
            ObjectDiff::Added(x) => added.push(format!("omezení {}", x.name)),
            ObjectDiff::Removed(x) => removed.push(format!("omezení {}", x.name)),
            _ => {}
        }
    }
    (added, removed)
}

fn tint_for_table_status(status: TableStatus, theme: &Theme) -> Option<Hsla> {
    match status {
        TableStatus::Added => Some(theme.diff_inserted_bg),
        TableStatus::Removed => Some(theme.diff_deleted_bg),
        TableStatus::Changed => Some(theme.diff_staged_bg),
        TableStatus::Unchanged => None,
    }
}

fn tint_for_object<T>(o: &ObjectDiff<T>, theme: &Theme) -> Option<Hsla> {
    match o {
        ObjectDiff::Added(_) => Some(theme.diff_inserted_bg),
        ObjectDiff::Removed(_) => Some(theme.diff_deleted_bg),
        ObjectDiff::Changed { .. } => Some(theme.diff_staged_bg),
        ObjectDiff::Unchanged(_) => None,
    }
}

// ---------------------------------------------------------------------
// Pure logic — data half (T8).
// ---------------------------------------------------------------------

/// RESULT-column indices (into `result_columns`, in `result_columns`'
/// order) of `table`'s PK columns — mirrors `main.rs::detect_editable_pk`'s
/// name-matching technique (main.rs:242-247) without ANY of its
/// read-only/engine gating (data diff needs neither).
pub fn pk_result_cols(table: &TableInfo, result_columns: &[String]) -> Vec<usize> {
    table
        .columns
        .iter()
        .filter(|c| c.is_pk)
        .filter_map(|c| result_columns.iter().position(|h| h == &c.name))
        .collect()
}

/// design §4: the "Porovnat data" affordance is enabled only for a matched
/// (present on both sides), PK'd-on-both-sides table pair.
pub fn data_diff_available(t: &TableDiff) -> bool {
    match (&t.left, &t.right) {
        (Some(l), Some(r)) => table_has_pk(l) && table_has_pk(r),
        _ => false,
    }
}

// ---------------------------------------------------------------------
// Entity state.
// ---------------------------------------------------------------------

#[derive(Clone)]
pub enum CompareLoadState {
    Loading,
    Ready { diff: SchemaDiff, mode: CompareMode },
    /// design §3: either leg failing surfaces as an error banner — `Some(_)`
    /// = that leg failed. A per-leg "retry" affordance described in the
    /// design's prose was not wired (documented deviation, see this task's
    /// final report) — the error text itself is always shown in full,
    /// nothing is silently swallowed; re-opening Compare from the dialog
    /// re-runs both legs.
    Error { a: Option<QueryError>, b: Option<QueryError> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CompareSelection {
    None,
    Table(usize),   // index into diff.tables
    Routine(usize), // index into diff.routines
    Trigger(usize), // index into diff.triggers
}

/// Per-section "Zobrazit beze změn" toggle state. Deviation from the plan's
/// literal `show_unchanged: [bool; 5]` sketch (which reserved a slot for
/// "Pohledy" alongside "Tabulky") — views are folded into the SAME
/// `diff.tables` list as tables (design §3: no separate "Pohledy" section
/// exists to toggle), so a named 4-field struct is clearer than a
/// nominally-5-long array with one permanently-unused slot.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShowUnchanged {
    pub tables: bool,
    pub routines: bool,
    pub triggers: bool,
    pub sequences: bool,
}

/// T8: data-diff state for ONE selected matched table pair. `Idle` before
/// any "Porovnat data" click; reset to `Idle` whenever the LEFT-pane
/// selection changes to a different table so a stale outcome for table A
/// never lingers while table B is selected (see the table-row click
/// handler in `render_table_section`).
///
/// Review fix (MAJOR): `Ready` used to carry the raw `DataDiffOutcome`
/// (up to `DIFF_ROW_CAP` = 1,000,000 `RowDiff` entries) plus the two full
/// `ResultBuffer`s, and `render_changed_rows` called
/// `data_diff::build_changed_batch` INSIDE the render path — re-
/// materializing every changed cell's `"{old} → {new}"` string and a fresh
/// Arrow batch on EVERY repaint (selection change, WHERE-box edit, any
/// `cx.notify()`), a multi-second UI freeze on a large diff well under the
/// advertised cap. `Ready` now carries a `DataDiffSummary`, computed EXACTLY
/// ONCE by `apply_data_diff_result` — rendering only ever reads it.
pub enum DataDiffState {
    Idle,
    Loading,
    Ready { summary: DataDiffSummary, sql_a: String, sql_b: String },
    /// design §4: `DIFF_ROW_CAP` hit — a banner, not silent.
    RowCapExceeded { message: String },
    Error(QueryError),
}

/// Precomputed ONCE by `apply_data_diff_result` from a `DataDiffOutcome` +
/// the two fetched `ResultBuffer`s — review fix (MAJOR), see
/// `DataDiffState::Ready`'s doc comment. `render_data_diff_outcome`/
/// `render_changed_rows_display` do NOT touch `dbc_diff::data_diff` at all;
/// they only read fields off this struct. Every `Vec` here is ALREADY
/// capped to `DISPLAY_ROW_CAP` at construction time (see
/// `build_changed_rows_display`'s `cap` parameter) — the one-time build
/// itself is bounded, not just "no longer per-frame".
pub struct DataDiffSummary {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub total_left: usize,
    /// RIGHT-side row indices of the first `DISPLAY_ROW_CAP` Added rows.
    pub added_shown: Vec<usize>,
    /// LEFT-side row indices of the first `DISPLAY_ROW_CAP` Removed rows.
    pub removed_shown: Vec<usize>,
    pub changed_columns: Vec<String>,
    /// Cell text for the first `DISPLAY_ROW_CAP` Changed rows — one `Vec`
    /// per row, column order matching `changed_columns`.
    pub changed_rows_shown: Vec<Vec<String>>,
    /// `(row, col)` indices into `changed_rows_shown` whose cell actually
    /// differs (tinted `TINT_CHANGED` at render time).
    pub changed_tinted: HashSet<(usize, usize)>,
}

pub struct CompareView {
    pub label_a: String,
    pub label_b: String,
    pub conn_a: dbc_state::ConnectionConfig,
    pub secret_a: Option<String>,
    pub conn_b: dbc_state::ConnectionConfig,
    pub secret_b: Option<String>,
    pub state: CompareLoadState,
    pub selection: CompareSelection,
    pub show_unchanged: ShowUnchanged,
    pub show_ddl_diff: bool,
    // --- T8: data-diff ---
    /// design §4: ONE optional free-text field, shared identically by both
    /// sides' composed `SELECT` (CURATION §0.1(b) — "one optional text
    /// field per side-pair", not one box per side).
    pub data_where: String,
    pub data_diff: DataDiffState,
    /// Bumped on every "Porovnat data" dispatch; a `fetch_diff_side` pair's
    /// result only applies if the generation still matches — same
    /// last-dispatched-wins guard `AppView::start_schema_slot_fetch` uses.
    pub data_diff_generation: u64,
}

/// Emitted toward `AppView` (subscription wired in
/// `connections_ui::confirm_compare_dialog` at tab-open time). `CompareView`
/// does not own a `QueryRunner` (no tab-content entity in this codebase
/// does — see `MonitorView`'s `KillRequested` for the same shape), so the
/// actual `fetch_diff_side` dispatch happens in
/// `AppView::on_compare_view_event`.
#[derive(Debug, Clone, Copy)]
pub enum CompareViewEvent {
    DataDiffRequested,
}
impl EventEmitter<CompareViewEvent> for CompareView {}

impl CompareView {
    /// `(conn_a.engine, conn_b.engine)` — `AppView::on_compare_schema_pair_ready`
    /// reads this to pick `CompareMode::SameEngine`/`CrossEngine` without
    /// needing its own copy of either `ConnectionConfig`.
    pub fn engines(&self) -> (dbc_state::Engine, dbc_state::Engine) {
        (self.conn_a.engine, self.conn_b.engine)
    }

    /// The `TableDiff` currently backing a `DataDiffState` other than
    /// `Idle` — `None` unless the left-pane selection is a table.
    fn selected_table<'a>(&self, diff: &'a SchemaDiff) -> Option<&'a TableDiff> {
        match self.selection {
            CompareSelection::Table(ix) => diff.tables.get(ix),
            _ => None,
        }
    }

    /// "Porovnat data" — dispatches `fetch_diff_side` for BOTH sides of the
    /// currently-selected matched table pair (design §4). Fire-and-forget,
    /// generation-guarded exactly like `start_schema_slot_fetch`/
    /// `confirm_compare_dialog`. Called from `AppView::on_compare_view_event`
    /// (which owns `runner`), not from this entity's own click handler
    /// directly. A no-op if the current selection isn't a data-diffable
    /// table pair (belt-and-braces — the button is already hidden/disabled
    /// in that state).
    pub fn start_data_diff(&mut self, diff: &SchemaDiff, runner: &QueryRunner, cx: &mut Context<Self>) {
        let Some(t) = self.selected_table(diff) else { return };
        if !data_diff_available(t) {
            return;
        }
        let (Some(left_tbl), Some(right_tbl)) = (t.left.clone(), t.right.clone()) else { return };
        let schema = t.schema.clone();
        let table_name = t.name.clone();
        let where_text = self.data_where.trim();
        let where_a = if where_text.is_empty() { None } else { Some(where_text.to_string()) };
        let where_b = where_a.clone();

        self.data_diff_generation += 1;
        let my_generation = self.data_diff_generation;
        self.data_diff = DataDiffState::Loading;

        let spec_a = ConnectSpec::Config { cfg: Box::new(self.conn_a.clone()), secret: self.secret_a.clone() };
        let spec_b = ConnectSpec::Config { cfg: Box::new(self.conn_b.clone()), secret: self.secret_b.clone() };
        let rx_a = runner.fetch_diff_side(spec_a, schema.clone(), table_name.clone(), where_a);
        let rx_b = runner.fetch_diff_side(spec_b, schema, table_name, where_b);

        cx.spawn(async move |this, cx| {
            let (result_a, result_b) = (rx_a.await, rx_b.await);
            let _ = this.update(cx, |view, cx| {
                if view.data_diff_generation != my_generation {
                    return; // superseded by a newer dispatch — last-dispatched wins
                }
                view.apply_data_diff_result(&left_tbl, &right_tbl, result_a, result_b, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_data_diff_result(
        &mut self,
        left_tbl: &TableInfo,
        right_tbl: &TableInfo,
        result_a: Result<Result<(String, SchemaRef, dbc_buffer::ResultBuffer), QueryError>, tokio::sync::oneshot::error::RecvError>,
        result_b: Result<Result<(String, SchemaRef, dbc_buffer::ResultBuffer), QueryError>, tokio::sync::oneshot::error::RecvError>,
        cx: &mut Context<Self>,
    ) {
        let a = result_a.unwrap_or_else(|_| Err(QueryError::msg("fetch zrušen".to_string())));
        let b = result_b.unwrap_or_else(|_| Err(QueryError::msg("fetch zrušen".to_string())));
        let (sql_a, schema_a, mut buf_a, sql_b, schema_b, mut buf_b) = match (a, b) {
            (Ok((sql_a, schema_a, buf_a)), Ok((sql_b, schema_b, buf_b))) => (sql_a, schema_a, buf_a, sql_b, schema_b, buf_b),
            (Err(e), _) | (_, Err(e)) => {
                self.data_diff = DataDiffState::Error(e);
                cx.notify();
                return;
            }
        };
        let left_names: Vec<String> = schema_a.fields().iter().map(|f| f.name().to_string()).collect();
        let right_names: Vec<String> = schema_b.fields().iter().map(|f| f.name().to_string()).collect();
        let left_pk = pk_result_cols(left_tbl, &left_names);
        let right_pk = pk_result_cols(right_tbl, &right_names);

        if left_pk.is_empty() || right_pk.is_empty() {
            self.data_diff = DataDiffState::Error(QueryError::msg(
                "primární klíč nebyl nalezen mezi sloupci výsledku — porovnání dat přerušeno".to_string(),
            ));
            cx.notify();
            return;
        }

        // review fix (MAJOR): the summary — including the "Změněné řádky"
        // display batch — is built HERE, exactly once, never again at
        // render time. `buf_a`/`buf_b` (and the full `outcome.rows`, up to
        // `DIFF_ROW_CAP`) are dropped at the end of this function; nothing
        // downstream keeps a reference to them.
        match data_diff::diff_data(&mut buf_a, &left_names, &left_pk, &mut buf_b, &right_names, &right_pk) {
            Ok(outcome) => {
                let (changed_rows_shown, changed_tinted) = build_changed_rows_display(
                    &mut buf_a,
                    &mut buf_b,
                    &outcome.intersection_columns,
                    &left_names,
                    &right_names,
                    &outcome.rows,
                    DISPLAY_ROW_CAP,
                );
                let summary = DataDiffSummary {
                    added: outcome.rows.iter().filter(|r| matches!(r, RowDiff::Added { .. })).count(),
                    removed: outcome.rows.iter().filter(|r| matches!(r, RowDiff::Removed { .. })).count(),
                    changed: outcome.rows.iter().filter(|r| matches!(r, RowDiff::Changed { .. })).count(),
                    total_left: outcome.rows.iter().filter(|r| !matches!(r, RowDiff::Added { .. })).count(),
                    added_shown: outcome
                        .rows
                        .iter()
                        .filter_map(|r| match r {
                            RowDiff::Added { right_row } => Some(*right_row),
                            _ => None,
                        })
                        .take(DISPLAY_ROW_CAP)
                        .collect(),
                    removed_shown: outcome
                        .rows
                        .iter()
                        .filter_map(|r| match r {
                            RowDiff::Removed { left_row } => Some(*left_row),
                            _ => None,
                        })
                        .take(DISPLAY_ROW_CAP)
                        .collect(),
                    changed_columns: outcome.intersection_columns,
                    changed_rows_shown,
                    changed_tinted,
                };
                self.data_diff = DataDiffState::Ready { summary, sql_a, sql_b };
            }
            Err(msg) => {
                self.data_diff = DataDiffState::RowCapExceeded { message: msg };
            }
        }
        cx.notify();
    }
}

/// Pure "prepare the Changed-rows display cache" step — review fix (MAJOR):
/// extracted specifically so a test can assert (a) this, not any render
/// function, is what calls `data_diff::build_changed_batch`, and (b) the
/// INPUT to that call is already bounded to `cap` `RowDiff::Changed`
/// entries — i.e. the one-time build itself is O(cap), never O(total
/// changed rows). Called exactly once, from `apply_data_diff_result`.
fn build_changed_rows_display(
    left: &mut dbc_buffer::ResultBuffer,
    right: &mut dbc_buffer::ResultBuffer,
    intersection_columns: &[String],
    left_names: &[String],
    right_names: &[String],
    rows: &[RowDiff],
    cap: usize,
) -> (Vec<Vec<String>>, HashSet<(usize, usize)>) {
    let capped_changed: Vec<RowDiff> =
        rows.iter().filter(|r| matches!(r, RowDiff::Changed { .. })).take(cap).cloned().collect();
    let (batch, tinted) =
        data_diff::build_changed_batch(left, right, intersection_columns, left_names, right_names, &capped_changed);
    (batch_to_text_rows(&batch, intersection_columns.len()), tinted)
}

fn batch_to_text_rows(batch: &RecordBatch, ncols: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let mut cells = Vec::with_capacity(ncols);
        for col in 0..ncols {
            let text = batch
                .column(col)
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|a| a.value(row).to_string())
                .unwrap_or_default();
            cells.push(text);
        }
        rows.push(cells);
    }
    rows
}

// ---------------------------------------------------------------------
// Rendering. Contract-specified per this phase's plan Self-Review note 2
// (GPUI render bodies aren't unit-tested anywhere in this codebase; every
// LOGIC-bearing helper above IS unit-tested).
// ---------------------------------------------------------------------

impl Render for CompareView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Review fix (MINOR 1): this used to be `self.state.clone()`, deep-
        // cloning the ENTIRE `SchemaDiff` (every `TableDiff`, both-side
        // `TableInfo` — columns/indexes/constraints/DDL text) on EVERY
        // render frame (selection change, WHERE-box keystroke, any
        // `cx.notify()`). The `render_modal_overlay`-style "clone is cheap"
        // precedent this cited does NOT hold for a large schema diff — a
        // modal's state is a handful of fields, a `SchemaDiff` can be
        // thousands of tables. None of the `render_*` helpers below
        // actually need `&mut self` (they only read fields; `cx.listener`
        // closures capture `&mut Self` themselves, independent of whether
        // the OUTER render method borrows `self` mutably) — so this reads
        // `&self.state` by reference instead, and every helper down the
        // chain takes `&self`.
        let theme = *cx.theme();
        let body: AnyElement = match &self.state {
            CompareLoadState::Loading => div()
                .flex_1()
                .p_4()
                .text_color(theme.text_primary)
                .child("Načítám schéma…")
                .into_any_element(),
            CompareLoadState::Error { a, b } => render_error_banner(a, b, &theme).into_any_element(),
            CompareLoadState::Ready { diff, mode } => self.render_ready(diff, *mode, cx),
        };
        div().id("compare-view").flex().flex_col().flex_1().bg(theme.bg_panel).child(body)
    }
}

fn render_error_banner(a: &Option<QueryError>, b: &Option<QueryError>, theme: &Theme) -> impl IntoElement {
    let mut banner = div().flex().flex_col().gap_1().p_4().text_color(theme.danger);
    if let Some(e) = a {
        banner = banner.child(format!("Databáze A: error: {e}"));
    }
    if let Some(e) = b {
        banner = banner.child(format!("Databáze B: error: {e}"));
    }
    banner
}

impl CompareView {
    fn render_ready(&self, diff: &SchemaDiff, mode: CompareMode, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let counts = count_table_statuses(&diff.tables);
        let mut root = div().id("compare-root").flex().flex_col().flex_1();

        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .p_2()
                .bg(theme.bg_app)
                .text_color(theme.text_primary)
                .child(format!("{} ↔ {}", self.label_a, self.label_b))
                .child(
                    div()
                        .ml_auto()
                        .child(format!("+{} -{} ~{}", counts.added, counts.removed, counts.changed)),
                ),
        );
        if mode == CompareMode::CrossEngine {
            root = root.child(
                div().p_1().bg(theme.bg_warn_banner).text_color(theme.warn).child(
                    "porovnání mezi různými databázovými systémy: typy a výchozí hodnoty sloupců se neporovnávají",
                ),
            );
        }

        let body = div()
            .id("compare-body")
            .flex()
            .flex_row()
            .flex_1()
            .child(self.render_left_pane(diff, cx))
            .child(self.render_right_pane(diff, cx));
        root = root.child(body);
        root.into_any_element()
    }

    fn render_left_pane(&self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        div()
            .id("compare-left")
            .w(px(320.))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .overflow_hidden()
            .border_r_1()
            .border_color(theme.border_subtle)
            .text_color(theme.text_primary)
            .child(self.render_table_section(diff, cx))
            .child(self.render_routine_section(diff, cx))
            .child(self.render_trigger_section(diff, cx))
            .child(self.render_sequence_section(diff, cx))
    }

    fn render_table_section(&self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let show_unchanged = self.show_unchanged.tables;
        let unchanged = diff.tables.iter().filter(|t| t.status == TableStatus::Unchanged).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Tabulky",
            unchanged,
            show_unchanged,
            &theme,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.tables = !v.show_unchanged.tables;
                cx.notify();
            }),
        ));
        // review fix (MINOR 2): visible rows capped at DISPLAY_ROW_CAP — a
        // schema with thousands of tables must not emit thousands of `div`s
        // per repaint.
        let visible: Vec<(usize, &TableDiff)> =
            diff.tables.iter().enumerate().filter(|(_, t)| show_unchanged || t.status != TableStatus::Unchanged).collect();
        let shown = visible.len().min(DISPLAY_ROW_CAP);
        for &(ix, t) in &visible[..shown] {
            let is_selected = self.selection == CompareSelection::Table(ix);
            let label = match &t.schema {
                Some(s) => format!("{s}.{}", t.name),
                None => t.name.clone(),
            };
            let tint = tint_for_table_status(t.status, &theme);
            section = section.child(
                compare_row(SharedString::from(format!("compare-table-row-{ix}")), label, tint, is_selected, &theme)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selection = CompareSelection::Table(ix);
                        v.data_diff = DataDiffState::Idle; // new selection invalidates any prior data diff
                        cx.notify();
                    })),
            );
        }
        if visible.len() > shown {
            section = section.child(
                div().text_color(theme.text_disabled).child(format!("… a {} dalších", visible.len() - shown)),
            );
        }
        section
    }

    fn render_routine_section(&self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let show_unchanged = self.show_unchanged.routines;
        let unchanged = diff.routines.iter().filter(|r| matches!(r, ObjectDiff::Unchanged(_))).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Funkce/procedury",
            unchanged,
            show_unchanged,
            &theme,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.routines = !v.show_unchanged.routines;
                cx.notify();
            }),
        ));
        let visible: Vec<(usize, &ObjectDiff<RoutineInfo>)> = diff
            .routines
            .iter()
            .enumerate()
            .filter(|(_, r)| show_unchanged || !matches!(r, ObjectDiff::Unchanged(_)))
            .collect();
        let shown = visible.len().min(DISPLAY_ROW_CAP);
        for &(ix, r) in &visible[..shown] {
            let is_selected = self.selection == CompareSelection::Routine(ix);
            let name = match r {
                ObjectDiff::Added(x) | ObjectDiff::Removed(x) | ObjectDiff::Unchanged(x) => x.name.clone(),
                ObjectDiff::Changed { left, .. } => left.name.clone(),
            };
            let tint = tint_for_object(r, &theme);
            section = section.child(
                compare_row(SharedString::from(format!("compare-routine-row-{ix}")), name, tint, is_selected, &theme)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selection = CompareSelection::Routine(ix);
                        cx.notify();
                    })),
            );
        }
        if visible.len() > shown {
            section = section.child(
                div().text_color(theme.text_disabled).child(format!("… a {} dalších", visible.len() - shown)),
            );
        }
        section
    }

    fn render_trigger_section(&self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let show_unchanged = self.show_unchanged.triggers;
        let unchanged = diff.triggers.iter().filter(|t| matches!(t, ObjectDiff::Unchanged(_))).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Triggery",
            unchanged,
            show_unchanged,
            &theme,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.triggers = !v.show_unchanged.triggers;
                cx.notify();
            }),
        ));
        let visible: Vec<(usize, &ObjectDiff<TriggerInfo>)> = diff
            .triggers
            .iter()
            .enumerate()
            .filter(|(_, t)| show_unchanged || !matches!(t, ObjectDiff::Unchanged(_)))
            .collect();
        let shown = visible.len().min(DISPLAY_ROW_CAP);
        for &(ix, t) in &visible[..shown] {
            let is_selected = self.selection == CompareSelection::Trigger(ix);
            let name = match t {
                ObjectDiff::Added(x) | ObjectDiff::Removed(x) | ObjectDiff::Unchanged(x) => x.name.clone(),
                ObjectDiff::Changed { left, .. } => left.name.clone(),
            };
            let tint = tint_for_object(t, &theme);
            section = section.child(
                compare_row(SharedString::from(format!("compare-trigger-row-{ix}")), name, tint, is_selected, &theme)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selection = CompareSelection::Trigger(ix);
                        cx.notify();
                    })),
            );
        }
        if visible.len() > shown {
            section = section.child(
                div().text_color(theme.text_disabled).child(format!("… a {} dalších", visible.len() - shown)),
            );
        }
        section
    }

    /// Sequences never carry a `Changed` variant and have nothing structured
    /// to drill into (design §1) — the section renders (with its own
    /// unchanged toggle, for consistency) but its rows are inert (no
    /// `CompareSelection` variant exists for them, matching this task's
    /// Interfaces block).
    fn render_sequence_section(&self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let show_unchanged = self.show_unchanged.sequences;
        let unchanged = diff.sequences.iter().filter(|s| matches!(s, ObjectDiff::Unchanged(_))).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Sekvence",
            unchanged,
            show_unchanged,
            &theme,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.sequences = !v.show_unchanged.sequences;
                cx.notify();
            }),
        ));
        let visible: Vec<_> =
            diff.sequences.iter().filter(|s| show_unchanged || !matches!(s, ObjectDiff::Unchanged(_))).collect();
        let shown = visible.len().min(DISPLAY_ROW_CAP);
        for s in &visible[..shown] {
            let name = match s {
                ObjectDiff::Added(x) | ObjectDiff::Removed(x) | ObjectDiff::Unchanged(x) => x.name.clone(),
                ObjectDiff::Changed { left, .. } => left.name.clone(),
            };
            let tint = tint_for_object(*s, &theme);
            let mut row = div().px_1().rounded_md().child(name);
            if let Some(t) = tint {
                row = row.bg(t);
            }
            section = section.child(row);
        }
        if visible.len() > shown {
            section = section.child(
                div().text_color(theme.text_disabled).child(format!("… a {} dalších", visible.len() - shown)),
            );
        }
        section
    }

    fn render_right_pane(&self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let mut pane = div()
            .id("compare-right")
            .flex()
            .flex_col()
            .flex_1()
            .p_2()
            .gap_2()
            .overflow_hidden()
            .text_color(theme.text_primary);
        match self.selection {
            CompareSelection::None => {
                pane = pane.child(div().text_color(theme.text_disabled).child("Vyber objekt vlevo."));
            }
            CompareSelection::Table(ix) => {
                if let Some(t) = diff.tables.get(ix).cloned() {
                    pane = pane.child(self.render_table_detail(&t, cx));
                }
            }
            CompareSelection::Routine(ix) => {
                if let Some(r) = diff.routines.get(ix) {
                    let (l, r_) = match r {
                        ObjectDiff::Added(x) => (None, Some(routine_ddl_text(x))),
                        ObjectDiff::Removed(x) => (Some(routine_ddl_text(x)), None),
                        ObjectDiff::Unchanged(x) => (Some(routine_ddl_text(x)), None),
                        ObjectDiff::Changed { left, right, .. } => {
                            (Some(routine_ddl_text(left)), Some(routine_ddl_text(right)))
                        }
                    };
                    pane = pane.child(render_ddl_or_diff(l.as_deref(), r_.as_deref(), &theme));
                }
            }
            CompareSelection::Trigger(ix) => {
                if let Some(t) = diff.triggers.get(ix) {
                    let (l, r) = match t {
                        ObjectDiff::Added(x) => (None, Some(trigger_ddl_text(x))),
                        ObjectDiff::Removed(x) => (Some(trigger_ddl_text(x)), None),
                        ObjectDiff::Unchanged(x) => (Some(trigger_ddl_text(x)), None),
                        ObjectDiff::Changed { left, right, .. } => {
                            (Some(trigger_ddl_text(left)), Some(trigger_ddl_text(right)))
                        }
                    };
                    pane = pane.child(render_ddl_or_diff(l.as_deref(), r.as_deref(), &theme));
                }
            }
        }
        pane
    }

    fn render_table_detail(&self, t: &TableDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let title = match &t.schema {
            Some(s) => format!("{s}.{}", t.name),
            None => t.name.clone(),
        };
        let mut detail = div().flex().flex_col().gap_2().child(div().text_size(px(15.)).child(title));

        match t.status {
            TableStatus::Added => {
                if let Some(ti) = &t.right {
                    detail = detail.child(ddl_block(&table_ddl_text(ti), &theme));
                }
            }
            TableStatus::Removed => {
                if let Some(ti) = &t.left {
                    detail = detail.child(ddl_block(&table_ddl_text(ti), &theme));
                }
            }
            TableStatus::Changed | TableStatus::Unchanged => {
                let (added, removed) = table_existence_rows(t);
                if !added.is_empty() || !removed.is_empty() {
                    detail = detail.child(existence_list(&added, &removed, &theme));
                }
                let field_rows = table_field_rows(t);
                if !field_rows.is_empty() {
                    detail = detail.child(field_change_table(&field_rows, &theme));
                }
                if let (Some(l), Some(r)) = (&t.left, &t.right) {
                    let toggle_label = if self.show_ddl_diff { "Skrýt DDL diff" } else { "Zobrazit DDL diff" };
                    detail = detail.child(
                        div()
                            .id("compare-ddl-diff-toggle")
                            .cursor_pointer()
                            .text_color(theme.accent)
                            .child(toggle_label)
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.show_ddl_diff = !v.show_ddl_diff;
                                cx.notify();
                            })),
                    );
                    if self.show_ddl_diff {
                        detail = detail.child(ddl_diff_block(&table_ddl_diff(l, r), &theme));
                    }
                }
            }
        }

        // T8: "Porovnat data" affordance — only for a matched, PK'd-on-both-
        // sides table pair (design §4).
        if data_diff_available(t) {
            detail = detail.child(self.render_data_diff_section(cx));
        } else if matches!(t.status, TableStatus::Changed | TableStatus::Unchanged) {
            detail = detail.child(
                div().text_color(theme.text_disabled).child("Porovnání dat: tabulka nemá primární klíč na obou stranách"),
            );
        }

        detail
    }

    fn render_data_diff_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let mut section =
            div().flex().flex_col().gap_2().mt_2().p_2().border_1().border_color(theme.border_subtle).rounded_md();
        section = section.child(div().text_color(theme.accent).child("Porovnání dat"));

        let where_text = self.data_where.clone();
        section = section.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .child(div().text_color(theme.text_disabled).child("WHERE"))
                .child(
                    div()
                        .id("compare-data-where")
                        .flex_1()
                        .px_1()
                        .bg(theme.bg_app)
                        .rounded_md()
                        .text_color(theme.text_primary)
                        .child(if where_text.is_empty() { "(bez omezení)".to_string() } else { where_text }),
                ),
        );

        section = section.child(
            compare_button("compare-run-data-diff", "Porovnat data", &theme).on_click(cx.listener(
                |_v, _, _, cx| {
                    cx.emit(CompareViewEvent::DataDiffRequested);
                },
            )),
        );

        match &self.data_diff {
            DataDiffState::Idle => {}
            DataDiffState::Loading => {
                section = section.child(div().text_color(theme.warn).child("Načítám data…"));
            }
            DataDiffState::Error(e) => {
                section = section.child(div().text_color(theme.danger).child(format!("error: {e}")));
            }
            DataDiffState::RowCapExceeded { message } => {
                section = section.child(div().text_color(theme.danger).child(message.clone()));
            }
            DataDiffState::Ready { summary, sql_a, sql_b } => {
                section = section.child(render_data_diff_outcome(summary, sql_a, sql_b, &theme));
            }
        }
        section
    }
}

/// Review fix (MAJOR): reads ONLY from the already-computed
/// `DataDiffSummary` (built once by `apply_data_diff_result`) — no
/// `dbc_diff::data_diff` call, no `ResultBuffer` access, no per-cell string
/// formatting happens here. Every list this renders was already capped to
/// `DISPLAY_ROW_CAP` at construction time; the "… zobrazeno prvních N z M"
/// footers use `summary`'s own totals, which ARE the full (uncapped)
/// counts — cheap `usize` fields, not a re-scan of the row list.
fn render_data_diff_outcome(summary: &DataDiffSummary, sql_a: &str, sql_b: &str, theme: &Theme) -> impl IntoElement {
    let mut block = div().flex().flex_col().gap_2();
    block = block.child(div().text_color(theme.text_muted).child(format!(
        "{} přidáno, {} odebráno, {} změněno (z {} řádků na levé straně)",
        summary.added, summary.removed, summary.changed, summary.total_left
    )));
    block = block.child(ddl_block(&format!("A: {sql_a}"), theme));
    block = block.child(ddl_block(&format!("B: {sql_b}"), theme));

    block = block.child(row_id_list("Přidané řádky", theme.diff_inserted_bg, &summary.added_shown, summary.added, theme));
    block = block.child(row_id_list(
        "Odebrané řádky",
        theme.diff_deleted_bg,
        &summary.removed_shown,
        summary.removed,
        theme,
    ));
    block = block.child(render_changed_rows_display(summary, theme));
    block
}

/// `DataDiffSummary::changed_rows_shown`, rendered as a text table with the
/// changed cells tinted `theme.diff_staged_bg` — review fix (MAJOR): purely
/// a read of the precomputed cache, no `build_changed_batch` call here.
fn render_changed_rows_display(summary: &DataDiffSummary, theme: &Theme) -> impl IntoElement {
    let mut block = div().flex().flex_col().gap_1();
    block = block.child(div().text_color(theme.diff_staged_bg).child("Změněné řádky"));
    if summary.changed_rows_shown.is_empty() {
        return block;
    }

    let mut header = div().flex().flex_row().gap_2().text_color(theme.text_disabled);
    for name in &summary.changed_columns {
        header = header.child(div().w(px(160.)).child(name.clone()));
    }
    block = block.child(header);

    for (row, cells) in summary.changed_rows_shown.iter().enumerate() {
        let mut r = div().flex().flex_row().gap_2();
        for (col, text) in cells.iter().enumerate() {
            let mut cell = div().w(px(160.)).child(text.clone());
            if summary.changed_tinted.contains(&(row, col)) {
                cell = cell.bg(theme.diff_staged_bg);
            }
            r = r.child(cell);
        }
        block = block.child(r);
    }
    let shown = summary.changed_rows_shown.len();
    if summary.changed > shown {
        block = block.child(
            div()
                .text_color(theme.text_disabled)
                .child(format!("… zobrazeno prvních {shown} z {} změněných řádků", summary.changed)),
        );
    }
    block
}

/// Renders an already-capped (`DISPLAY_ROW_CAP`, see `DataDiffSummary`)
/// list of row indices — used for the "Přidané řádky"/"Odebrané řádky"
/// sections (design §4). `total` is the FULL (uncapped) count, for the "…
/// zobrazeno prvních N z M" footer. Row values are NOT re-read from any
/// buffer here (only the row index is shown) — the composed SQL shown
/// above the sections is the source of truth for what each side actually
/// returned; a user who needs the full row contents can re-run that exact
/// SQL in a normal query tab.
fn row_id_list(title: &str, tint: Hsla, shown_indices: &[usize], total: usize, theme: &Theme) -> impl IntoElement {
    let mut block = div().flex().flex_col().gap_1();
    block = block.child(div().text_color(tint).child(title.to_string()));
    for &ix in shown_indices {
        block = block.child(div().text_color(theme.text_muted).child(format!("řádek {ix}")));
    }
    if total > shown_indices.len() {
        block = block.child(
            div()
                .text_color(theme.text_disabled)
                .child(format!("… zobrazeno prvních {} z {total} řádků", shown_indices.len())),
        );
    }
    block
}

fn section_header(
    label: &str,
    unchanged_count: usize,
    show_unchanged: bool,
    theme: &Theme,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let toggle_label = format!("Zobrazit beze změn ({unchanged_count})");
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().text_color(theme.accent).child(label.to_string()))
        .child(
            div()
                .id(SharedString::from(format!("compare-toggle-{label}")))
                .cursor_pointer()
                .text_color(if show_unchanged { theme.success } else { theme.text_disabled })
                .child(toggle_label)
                .on_click(on_toggle),
        )
}

fn compare_row(id: SharedString, label: String, tint: Option<Hsla>, selected: bool, theme: &Theme) -> Stateful<Div> {
    let mut row = div().id(id).px_1().cursor_pointer().rounded_md().hover(|s| s.bg(theme.bg_hover)).child(label);
    if let Some(t) = tint {
        row = row.bg(t);
    }
    if selected {
        row = row.text_color(theme.warn);
    }
    row
}

fn compare_button(id: &'static str, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .w(px(140.))
        .bg(theme.bg_hover)
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_selected))
        .child(label)
}

fn ddl_block(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .font_family("Consolas")
        .p_1()
        .bg(theme.bg_app)
        .rounded_md()
        .text_color(theme.text_muted)
        .whitespace_normal()
        .child(text.to_string())
}

fn ddl_diff_block(lines: &[DiffLine], theme: &Theme) -> impl IntoElement {
    let mut block = div().flex().flex_col().font_family("Consolas").p_1().bg(theme.bg_app).rounded_md();
    for l in lines {
        let (prefix, color) = match l.tag {
            DiffTag::Equal => ("  ", theme.text_muted),
            DiffTag::Insert => ("+ ", theme.success),
            DiffTag::Delete => ("- ", theme.danger),
        };
        block = block.child(div().text_color(color).child(format!("{prefix}{}", l.text)));
    }
    block
}

fn render_ddl_or_diff(left_text: Option<&str>, right_text: Option<&str>, theme: &Theme) -> AnyElement {
    match (left_text, right_text) {
        (Some(l), Some(r)) => ddl_diff_block(&diff_lines(l, r), theme).into_any_element(),
        (Some(l), None) => ddl_block(l, theme).into_any_element(),
        (None, Some(r)) => ddl_block(r, theme).into_any_element(),
        (None, None) => div().text_color(theme.text_disabled).child("(bez DDL)").into_any_element(),
    }
}

fn field_change_table(rows: &[(String, FieldChange)], theme: &Theme) -> impl IntoElement {
    let mut table = div().flex().flex_col().gap_1();
    table = table.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .text_color(theme.text_disabled)
            .child(div().w(px(180.)).child("pole"))
            .child(div().w(px(200.)).child("A"))
            .child(div().flex_1().child("B")),
    );
    for (ctx, fc) in rows {
        table = table.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(div().w(px(180.)).child(format!("{ctx}.{}", fc.field)))
                .child(div().w(px(200.)).bg(theme.diff_deleted_bg).child(fc.left.clone()))
                .child(div().flex_1().bg(theme.diff_inserted_bg).child(fc.right.clone())),
        );
    }
    table
}

fn existence_list(added: &[String], removed: &[String], theme: &Theme) -> impl IntoElement {
    let mut block = div().flex().flex_col().gap_1();
    for a in added {
        block = block.child(div().text_color(theme.success).child(format!("+ {a}")));
    }
    for r in removed {
        block = block.child(div().text_color(theme.danger).child(format!("- {r}")));
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::ColumnInfo;

    fn table_diff(status: TableStatus) -> TableDiff {
        TableDiff {
            schema: None,
            name: "t".into(),
            status,
            table_fields: vec![],
            columns: vec![],
            indexes: vec![],
            constraints: vec![],
            left: None,
            right: None,
        }
    }

    #[test]
    fn counts_added_removed_changed_ignores_unchanged() {
        let tables = vec![
            table_diff(TableStatus::Added),
            table_diff(TableStatus::Added),
            table_diff(TableStatus::Removed),
            table_diff(TableStatus::Changed),
            table_diff(TableStatus::Unchanged),
        ];
        let c = count_table_statuses(&tables);
        assert_eq!((c.added, c.removed, c.changed), (2, 1, 1));
    }

    #[test]
    fn table_has_pk_requires_a_real_pk_column_on_a_base_table() {
        let mut t = TableInfo { kind: dbc_core::TableKind::Table, ..Default::default() };
        assert!(!table_has_pk(&t));
        t.columns.push(ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() });
        assert!(table_has_pk(&t));
        t.kind = dbc_core::TableKind::View;
        assert!(!table_has_pk(&t), "a view is never PK-diffable regardless of a reported is_pk column");
    }

    #[test]
    fn table_ddl_text_falls_back_to_synthesis_when_engine_gave_none() {
        let t = TableInfo {
            name: "t".into(),
            kind: dbc_core::TableKind::Table,
            columns: vec![ColumnInfo { name: "id".into(), data_type: "integer".into(), is_pk: true, ..Default::default() }],
            ddl: None,
            ..Default::default()
        };
        assert!(table_ddl_text(&t).starts_with("CREATE TABLE"));
        let with_ddl = TableInfo { ddl: Some("CUSTOM DDL".into()), ..t };
        assert_eq!(table_ddl_text(&with_ddl), "CUSTOM DDL");
    }

    #[test]
    fn table_ddl_diff_over_two_synthesized_tables() {
        let mk = |ty: &str| TableInfo {
            name: "t".into(),
            kind: dbc_core::TableKind::Table,
            columns: vec![ColumnInfo { name: "id".into(), data_type: ty.into(), is_pk: true, ..Default::default() }],
            ..Default::default()
        };
        let lines = table_ddl_diff(&mk("int4"), &mk("int8"));
        assert!(!lines.is_empty());
    }

    #[test]
    fn table_field_rows_flattens_table_column_index_constraint_changes_with_context() {
        let mut t = table_diff(TableStatus::Changed);
        t.table_fields = vec![FieldChange { field: "kind".into(), left: "Table".into(), right: "View".into() }];
        t.columns = vec![ObjectDiff::Changed {
            left: ColumnInfo { name: "id".into(), ..Default::default() },
            right: ColumnInfo { name: "id".into(), ..Default::default() },
            fields: vec![FieldChange { field: "data_type".into(), left: "int4".into(), right: "int8".into() }],
        }];
        let rows = table_field_rows(&t);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "tabulka");
        assert_eq!(rows[1].0, "sloupec id");
        assert_eq!(rows[1].1.field, "data_type");
    }

    #[test]
    fn table_existence_rows_separates_added_from_removed_across_object_kinds() {
        let mut t = table_diff(TableStatus::Changed);
        t.columns = vec![
            ObjectDiff::Added(ColumnInfo { name: "new_col".into(), ..Default::default() }),
            ObjectDiff::Removed(ColumnInfo { name: "gone".into(), ..Default::default() }),
        ];
        let (added, removed) = table_existence_rows(&t);
        assert_eq!(added, vec!["sloupec new_col".to_string()]);
        assert_eq!(removed, vec!["sloupec gone".to_string()]);
    }

    #[test]
    fn pk_result_cols_maps_by_name_ignoring_gating() {
        let table = TableInfo {
            columns: vec![
                ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                ColumnInfo { name: "tenant".into(), is_pk: true, ..Default::default() },
                ColumnInfo { name: "note".into(), is_pk: false, ..Default::default() },
            ],
            ..Default::default()
        };
        let result_cols = vec!["note".to_string(), "id".to_string(), "tenant".to_string()];
        assert_eq!(pk_result_cols(&table, &result_cols), vec![1, 2]);
    }

    #[test]
    fn pk_result_cols_missing_pk_column_in_the_result_is_silently_skipped_not_a_panic() {
        let table = TableInfo {
            columns: vec![ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(pk_result_cols(&table, &["other".to_string()]), Vec::<usize>::new());
    }

    #[test]
    fn data_diff_available_requires_pk_on_both_matched_sides() {
        let mut t = table_diff(TableStatus::Changed);
        let with_pk = || TableInfo {
            kind: dbc_core::TableKind::Table,
            columns: vec![ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() }],
            ..Default::default()
        };
        let without_pk = || TableInfo { kind: dbc_core::TableKind::Table, ..Default::default() };

        assert!(!data_diff_available(&t), "no left/right at all");
        t.left = Some(with_pk());
        assert!(!data_diff_available(&t), "right side missing");
        t.right = Some(without_pk());
        assert!(!data_diff_available(&t), "right side has no PK");
        t.right = Some(with_pk());
        assert!(data_diff_available(&t));
    }

    // --- review fix (MAJOR): `build_changed_rows_display` is the ONLY
    // place `data_diff::build_changed_batch` is ever called from this file
    // (see `apply_data_diff_result`'s single call site) — `render_*`
    // functions only ever read the `DataDiffSummary` it produces. These
    // tests prove (a) it actually works over real `ResultBuffer`s the same
    // way `dbc_diff::data_diff`'s own tests do, and (b) — the crux of the
    // MAJOR finding — the `cap` parameter bounds the INPUT to
    // `build_changed_batch`, not just the rendered output: passing `cap=2`
    // over 3 genuinely-changed rows must never materialize the 3rd row at
    // all, proving the one-time build itself is O(cap), not O(total
    // changed rows). ---

    fn test_buf(names: &[&str], rows: Vec<Vec<Option<&str>>>) -> (dbc_buffer::ResultBuffer, Vec<String>) {
        use dbc_core::arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let fields: Vec<Field> = names.iter().map(|n| Field::new(*n, DataType::Utf8, true)).collect();
        let schema = Arc::new(Schema::new(fields));
        let ncols = names.len();
        let mut arrays: Vec<Arc<dyn Array>> = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let col: Vec<Option<&str>> = rows.iter().map(|r| r[c]).collect();
            arrays.push(Arc::new(StringArray::from(col)));
        }
        let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let mut rb = dbc_buffer::ResultBuffer::new(schema);
        rb.push(batch).unwrap();
        (rb, names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn build_changed_rows_display_respects_the_cap_never_materializing_beyond_it() {
        let (mut left, ln) = test_buf(
            &["id", "val"],
            vec![vec![Some("1"), Some("a")], vec![Some("2"), Some("b")], vec![Some("3"), Some("c")]],
        );
        let (mut right, rn) = test_buf(
            &["id", "val"],
            vec![vec![Some("1"), Some("A")], vec![Some("2"), Some("B")], vec![Some("3"), Some("C")]],
        );
        let outcome = data_diff::diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
        assert_eq!(
            outcome.rows.iter().filter(|r| matches!(r, RowDiff::Changed { .. })).count(),
            3,
            "fixture sanity check: all 3 rows must be Changed"
        );

        let (rows_shown, _tinted) =
            build_changed_rows_display(&mut left, &mut right, &outcome.intersection_columns, &ln, &rn, &outcome.rows, 2);
        assert_eq!(rows_shown.len(), 2, "cap=2 must yield exactly 2 display rows, never all 3");
    }

    #[test]
    fn build_changed_rows_display_marks_only_the_differing_cells() {
        let (mut left, ln) = test_buf(&["id", "val"], vec![vec![Some("1"), Some("a")]]);
        let (mut right, rn) = test_buf(&["id", "val"], vec![vec![Some("1"), Some("b")]]);
        let outcome = data_diff::diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();

        let (rows_shown, tinted) = build_changed_rows_display(
            &mut left,
            &mut right,
            &outcome.intersection_columns,
            &ln,
            &rn,
            &outcome.rows,
            DISPLAY_ROW_CAP,
        );
        assert_eq!(rows_shown.len(), 1);
        let val_col = outcome.intersection_columns.iter().position(|c| c == "val").unwrap();
        let id_col = outcome.intersection_columns.iter().position(|c| c == "id").unwrap();
        assert!(tinted.contains(&(0, val_col)), "val differs on both sides — must be tinted");
        assert!(!tinted.contains(&(0, id_col)), "id is the (unchanged) PK — must not be tinted");
        assert_eq!(rows_shown[0][val_col], "a → b");
    }
}
