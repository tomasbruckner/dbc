//! G7 T7: `CompareView` — the per-tab GPUI entity behind
//! `TabContent::Compare`. Renders the schema diff (`dbc_diff::schema_diff`)
//! computed by `AppView::on_compare_schema_pair_ready` (main.rs) from a
//! `fetch_schema_pair` result: a left-pane status-tinted object list
//! (Tabulky/Funkce/Triggery/Sekvence) and a right-pane detail view
//! (Added/Removed DDL, Changed field table + optional DDL-diff drill-down)
//! — design §3. T8 extends this same entity with an in-process PK-based
//! data diff for one selected matched table pair.
//!
//! READ-ONLY end to end: nothing in this file ever calls `.execute(` — the
//! only SQL text this feature ever shows the user (T8, `fetch_diff_side`'s
//! composed `SELECT`) is display/copy only, never re-run from here (design
//! CURATION §0.1(d)/Global Constraints — no sync-script generation of any
//! kind).

use dbc_core::{synthesize_create_table, QueryError, RoutineInfo, TableInfo, TriggerInfo};
use dbc_diff::schema_diff::{CompareMode, FieldChange, ObjectDiff, SchemaDiff, TableDiff, TableStatus};
use dbc_diff::text_diff::{diff_lines, DiffLine, DiffTag};
use gpui::{div, prelude::*, px, rgb, AnyElement, ClickEvent, Context, Div, SharedString, Stateful, Window};

// Mirrors grid.rs's sandbox diff tints (grid.rs:26-28) — same convention,
// different module (those constants are private to grid.rs).
const TINT_ADDED: u32 = 0x2e5d3a; // green
const TINT_REMOVED: u32 = 0x5d2e2e; // red
const TINT_CHANGED: u32 = 0x6b5d2e; // amber/yellow

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
///
/// `#[allow(dead_code)]`: no caller exists yet in this worktree — G7 T8
/// (`data_diff_available`'s "Porovnat data" gating) is the first call site,
/// same posture `runner.rs`'s `fetch_schema_pair`/`fetch_diff_side` allows
/// took through T5/T6. `table_has_pk_requires_a_real_pk_column_on_a_base_table`
/// exercises it directly. Remove this allow once T8 wires a real call site.
#[allow(dead_code)]
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

fn tint_for_table_status(status: TableStatus) -> Option<u32> {
    match status {
        TableStatus::Added => Some(TINT_ADDED),
        TableStatus::Removed => Some(TINT_REMOVED),
        TableStatus::Changed => Some(TINT_CHANGED),
        TableStatus::Unchanged => None,
    }
}

fn tint_for_object<T>(o: &ObjectDiff<T>) -> Option<u32> {
    match o {
        ObjectDiff::Added(_) => Some(TINT_ADDED),
        ObjectDiff::Removed(_) => Some(TINT_REMOVED),
        ObjectDiff::Changed { .. } => Some(TINT_CHANGED),
        ObjectDiff::Unchanged(_) => None,
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

pub struct CompareView {
    pub label_a: String,
    pub label_b: String,
    pub conn_a: dbc_state::ConnectionConfig,
    /// `#[allow(dead_code)]`: not read anywhere in T7 — T8's
    /// `start_data_diff` (`fetch_diff_side`'s per-side vault secret) is the
    /// first reader. Removed once T8 wires that call site.
    #[allow(dead_code)]
    pub secret_a: Option<String>,
    pub conn_b: dbc_state::ConnectionConfig,
    #[allow(dead_code)]
    pub secret_b: Option<String>,
    pub state: CompareLoadState,
    pub selection: CompareSelection,
    pub show_unchanged: ShowUnchanged,
    pub show_ddl_diff: bool,
}

impl CompareView {
    /// `(conn_a.engine, conn_b.engine)` — `AppView::on_compare_schema_pair_ready`
    /// reads this to pick `CompareMode::SameEngine`/`CrossEngine` without
    /// needing its own copy of either `ConnectionConfig`.
    pub fn engines(&self) -> (dbc_state::Engine, dbc_state::Engine) {
        (self.conn_a.engine, self.conn_b.engine)
    }
}

// ---------------------------------------------------------------------
// Rendering. Contract-specified per this phase's plan Self-Review note 2
// (GPUI render bodies aren't unit-tested anywhere in this codebase; every
// LOGIC-bearing helper above IS unit-tested).
// ---------------------------------------------------------------------

impl Render for CompareView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Cloning the load state per render frame mirrors this codebase's
        // own established precedent for modal/tab state
        // (`connections_ui::render_modal_overlay`'s `self.modal.clone()`) —
        // not a hot path, simplicity over a borrow-splitting rewrite.
        let state = self.state.clone();
        let body: AnyElement = match state {
            CompareLoadState::Loading => div()
                .flex_1()
                .p_4()
                .text_color(rgb(0xcdd6f4))
                .child("Načítám schéma…")
                .into_any_element(),
            CompareLoadState::Error { a, b } => render_error_banner(&a, &b).into_any_element(),
            CompareLoadState::Ready { diff, mode } => self.render_ready(&diff, mode, cx),
        };
        div().id("compare-view").flex().flex_col().flex_1().bg(rgb(0x1e1e2e)).child(body)
    }
}

fn render_error_banner(a: &Option<QueryError>, b: &Option<QueryError>) -> impl IntoElement {
    let mut banner = div().flex().flex_col().gap_1().p_4().text_color(rgb(0xf38ba8));
    if let Some(e) = a {
        banner = banner.child(format!("Databáze A: error: {e}"));
    }
    if let Some(e) = b {
        banner = banner.child(format!("Databáze B: error: {e}"));
    }
    banner
}

impl CompareView {
    fn render_ready(&mut self, diff: &SchemaDiff, mode: CompareMode, cx: &mut Context<Self>) -> AnyElement {
        let counts = count_table_statuses(&diff.tables);
        let mut root = div().id("compare-root").flex().flex_col().flex_1();

        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .p_2()
                .bg(rgb(0x181825))
                .text_color(rgb(0xcdd6f4))
                .child(format!("{} ↔ {}", self.label_a, self.label_b))
                .child(
                    div()
                        .ml_auto()
                        .child(format!("+{} -{} ~{}", counts.added, counts.removed, counts.changed)),
                ),
        );
        if mode == CompareMode::CrossEngine {
            root = root.child(
                div().p_1().bg(rgb(0x3a3a1e)).text_color(rgb(0xf9e2af)).child(
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

    fn render_left_pane(&mut self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("compare-left")
            .w(px(320.))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .overflow_hidden()
            .border_r_1()
            .border_color(rgb(0x313244))
            .text_color(rgb(0xcdd6f4))
            .child(self.render_table_section(diff, cx))
            .child(self.render_routine_section(diff, cx))
            .child(self.render_trigger_section(diff, cx))
            .child(self.render_sequence_section(diff, cx))
    }

    fn render_table_section(&mut self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let show_unchanged = self.show_unchanged.tables;
        let unchanged = diff.tables.iter().filter(|t| t.status == TableStatus::Unchanged).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Tabulky",
            unchanged,
            show_unchanged,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.tables = !v.show_unchanged.tables;
                cx.notify();
            }),
        ));
        for (ix, t) in diff.tables.iter().enumerate() {
            if t.status == TableStatus::Unchanged && !show_unchanged {
                continue;
            }
            let is_selected = self.selection == CompareSelection::Table(ix);
            let label = match &t.schema {
                Some(s) => format!("{s}.{}", t.name),
                None => t.name.clone(),
            };
            let tint = tint_for_table_status(t.status);
            section = section.child(
                compare_row(SharedString::from(format!("compare-table-row-{ix}")), label, tint, is_selected)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selection = CompareSelection::Table(ix);
                        cx.notify();
                    })),
            );
        }
        section
    }

    fn render_routine_section(&mut self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let show_unchanged = self.show_unchanged.routines;
        let unchanged = diff.routines.iter().filter(|r| matches!(r, ObjectDiff::Unchanged(_))).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Funkce/procedury",
            unchanged,
            show_unchanged,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.routines = !v.show_unchanged.routines;
                cx.notify();
            }),
        ));
        for (ix, r) in diff.routines.iter().enumerate() {
            if matches!(r, ObjectDiff::Unchanged(_)) && !show_unchanged {
                continue;
            }
            let is_selected = self.selection == CompareSelection::Routine(ix);
            let name = match r {
                ObjectDiff::Added(x) | ObjectDiff::Removed(x) | ObjectDiff::Unchanged(x) => x.name.clone(),
                ObjectDiff::Changed { left, .. } => left.name.clone(),
            };
            let tint = tint_for_object(r);
            section = section.child(
                compare_row(SharedString::from(format!("compare-routine-row-{ix}")), name, tint, is_selected)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selection = CompareSelection::Routine(ix);
                        cx.notify();
                    })),
            );
        }
        section
    }

    fn render_trigger_section(&mut self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let show_unchanged = self.show_unchanged.triggers;
        let unchanged = diff.triggers.iter().filter(|t| matches!(t, ObjectDiff::Unchanged(_))).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Triggery",
            unchanged,
            show_unchanged,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.triggers = !v.show_unchanged.triggers;
                cx.notify();
            }),
        ));
        for (ix, t) in diff.triggers.iter().enumerate() {
            if matches!(t, ObjectDiff::Unchanged(_)) && !show_unchanged {
                continue;
            }
            let is_selected = self.selection == CompareSelection::Trigger(ix);
            let name = match t {
                ObjectDiff::Added(x) | ObjectDiff::Removed(x) | ObjectDiff::Unchanged(x) => x.name.clone(),
                ObjectDiff::Changed { left, .. } => left.name.clone(),
            };
            let tint = tint_for_object(t);
            section = section.child(
                compare_row(SharedString::from(format!("compare-trigger-row-{ix}")), name, tint, is_selected)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.selection = CompareSelection::Trigger(ix);
                        cx.notify();
                    })),
            );
        }
        section
    }

    /// Sequences never carry a `Changed` variant and have nothing structured
    /// to drill into (design §1) — the section renders (with its own
    /// unchanged toggle, for consistency) but its rows are inert (no
    /// `CompareSelection` variant exists for them, matching this task's
    /// Interfaces block).
    fn render_sequence_section(&mut self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let show_unchanged = self.show_unchanged.sequences;
        let unchanged = diff.sequences.iter().filter(|s| matches!(s, ObjectDiff::Unchanged(_))).count();
        let mut section = div().flex().flex_col().gap_1().child(section_header(
            "Sekvence",
            unchanged,
            show_unchanged,
            cx.listener(|v, _, _, cx| {
                v.show_unchanged.sequences = !v.show_unchanged.sequences;
                cx.notify();
            }),
        ));
        for s in diff.sequences.iter() {
            if matches!(s, ObjectDiff::Unchanged(_)) && !show_unchanged {
                continue;
            }
            let name = match s {
                ObjectDiff::Added(x) | ObjectDiff::Removed(x) | ObjectDiff::Unchanged(x) => x.name.clone(),
                ObjectDiff::Changed { left, .. } => left.name.clone(),
            };
            let tint = tint_for_object(s);
            let mut row = div().px_1().rounded_md().child(name);
            if let Some(t) = tint {
                row = row.bg(rgb(t));
            }
            section = section.child(row);
        }
        section
    }

    fn render_right_pane(&mut self, diff: &SchemaDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = div()
            .id("compare-right")
            .flex()
            .flex_col()
            .flex_1()
            .p_2()
            .gap_2()
            .overflow_hidden()
            .text_color(rgb(0xcdd6f4));
        match self.selection {
            CompareSelection::None => {
                pane = pane.child(div().text_color(rgb(0x6c7086)).child("Vyber objekt vlevo."));
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
                    pane = pane.child(render_ddl_or_diff(l.as_deref(), r_.as_deref()));
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
                    pane = pane.child(render_ddl_or_diff(l.as_deref(), r.as_deref()));
                }
            }
        }
        pane
    }

    fn render_table_detail(&mut self, t: &TableDiff, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &t.schema {
            Some(s) => format!("{s}.{}", t.name),
            None => t.name.clone(),
        };
        let mut detail = div().flex().flex_col().gap_2().child(div().text_size(px(15.)).child(title));

        match t.status {
            TableStatus::Added => {
                if let Some(ti) = &t.right {
                    detail = detail.child(ddl_block(&table_ddl_text(ti)));
                }
            }
            TableStatus::Removed => {
                if let Some(ti) = &t.left {
                    detail = detail.child(ddl_block(&table_ddl_text(ti)));
                }
            }
            TableStatus::Changed | TableStatus::Unchanged => {
                let (added, removed) = table_existence_rows(t);
                if !added.is_empty() || !removed.is_empty() {
                    detail = detail.child(existence_list(&added, &removed));
                }
                let field_rows = table_field_rows(t);
                if !field_rows.is_empty() {
                    detail = detail.child(field_change_table(&field_rows));
                }
                if let (Some(l), Some(r)) = (&t.left, &t.right) {
                    let toggle_label = if self.show_ddl_diff { "Skrýt DDL diff" } else { "Zobrazit DDL diff" };
                    detail = detail.child(
                        div()
                            .id("compare-ddl-diff-toggle")
                            .cursor_pointer()
                            .text_color(rgb(0x89b4fa))
                            .child(toggle_label)
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.show_ddl_diff = !v.show_ddl_diff;
                                cx.notify();
                            })),
                    );
                    if self.show_ddl_diff {
                        detail = detail.child(ddl_diff_block(&table_ddl_diff(l, r)));
                    }
                }
            }
        }

        detail
    }
}

fn section_header(
    label: &str,
    unchanged_count: usize,
    show_unchanged: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let toggle_label = format!("Zobrazit beze změn ({unchanged_count})");
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().text_color(rgb(0x89b4fa)).child(label.to_string()))
        .child(
            div()
                .id(SharedString::from(format!("compare-toggle-{label}")))
                .cursor_pointer()
                .text_color(if show_unchanged { rgb(0xa6e3a1) } else { rgb(0x6c7086) })
                .child(toggle_label)
                .on_click(on_toggle),
        )
}

fn compare_row(id: SharedString, label: String, tint: Option<u32>, selected: bool) -> Stateful<Div> {
    let mut row = div().id(id).px_1().cursor_pointer().rounded_md().hover(|s| s.bg(rgb(0x313244))).child(label);
    if let Some(t) = tint {
        row = row.bg(rgb(t));
    }
    if selected {
        row = row.text_color(rgb(0xf9e2af));
    }
    row
}

fn ddl_block(text: &str) -> impl IntoElement {
    div()
        .font_family("Consolas")
        .p_1()
        .bg(rgb(0x181825))
        .rounded_md()
        .text_color(rgb(0xa6adc8))
        .whitespace_normal()
        .child(text.to_string())
}

fn ddl_diff_block(lines: &[DiffLine]) -> impl IntoElement {
    let mut block = div().flex().flex_col().font_family("Consolas").p_1().bg(rgb(0x181825)).rounded_md();
    for l in lines {
        let (prefix, color) = match l.tag {
            DiffTag::Equal => ("  ", rgb(0xa6adc8)),
            DiffTag::Insert => ("+ ", rgb(0xa6e3a1)),
            DiffTag::Delete => ("- ", rgb(0xf38ba8)),
        };
        block = block.child(div().text_color(color).child(format!("{prefix}{}", l.text)));
    }
    block
}

fn render_ddl_or_diff(left_text: Option<&str>, right_text: Option<&str>) -> AnyElement {
    match (left_text, right_text) {
        (Some(l), Some(r)) => ddl_diff_block(&diff_lines(l, r)).into_any_element(),
        (Some(l), None) => ddl_block(l).into_any_element(),
        (None, Some(r)) => ddl_block(r).into_any_element(),
        (None, None) => div().text_color(rgb(0x6c7086)).child("(bez DDL)").into_any_element(),
    }
}

fn field_change_table(rows: &[(String, FieldChange)]) -> impl IntoElement {
    let mut table = div().flex().flex_col().gap_1();
    table = table.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .text_color(rgb(0x6c7086))
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
                .child(div().w(px(200.)).bg(rgb(TINT_REMOVED)).child(fc.left.clone()))
                .child(div().flex_1().bg(rgb(TINT_ADDED)).child(fc.right.clone())),
        );
    }
    table
}

fn existence_list(added: &[String], removed: &[String]) -> impl IntoElement {
    let mut block = div().flex().flex_col().gap_1();
    for a in added {
        block = block.child(div().text_color(rgb(0xa6e3a1)).child(format!("+ {a}")));
    }
    for r in removed {
        block = block.child(div().text_color(rgb(0xf38ba8)).child(format!("- {r}")));
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
}
