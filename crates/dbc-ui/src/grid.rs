use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::FkRef;
use gpui::{
    actions, div, prelude::*, px, uniform_list, AnyElement, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, Focusable, KeyBinding, ScrollDelta, ScrollStrategy,
    ScrollWheelEvent, UniformListScrollHandle, Window,
};

use crate::connections_ui::TextField;
use crate::export::{self, ExportFormat};
use crate::fk_join::{self, JoinSpec, VirtualCol};
use crate::row_view::{self, RowView};
use crate::sandbox::{self, EditState, Editable};
use crate::theme::ActiveTheme;

pub const ROW_HEIGHT: f32 = 24.0;
pub const DEFAULT_COL_WIDTH: f32 = 160.0;
/// G5 Task 3: leftmost per-row affordance column on an editable tab — "✕"
/// (toggle delete) per real row, "␡" (remove) per inserted row. Narrow by
/// design (brief: "~24 px") since it holds a single glyph, not text.
const GUTTER_WIDTH: f32 = 24.0;
/// G4 Task 2: above this many rows, a sort click sets `status_note` to
/// "řadím…" before `rebuild_view` runs. `rebuild` is synchronous today, so
/// this note is a retroactive "that sort was over a big set" marker rather
/// than a live in-progress spinner — see `status_note`'s doc comment.
const LARGE_SORT_ROWS: usize = 100_000;
/// G4 Task 3 review issue 2: the Ctrl+F scan runs synchronously on the UI
/// thread on every keystroke — bound both the scanned display rows and the
/// collected matches; the "i z n" indicator shows "n+" when capped.
const FIND_MAX_ROWS: usize = 100_000;
const FIND_MAX_MATCHES: usize = 1_000;
/// G4 Task 4: above this row count, export's formatting+write pass is
/// offloaded to `cx.background_executor()` instead of running inline in the
/// same UI-thread task that read the buffer (see `start_export`'s doc
/// comment for why only that half can move — `ResultBuffer` itself is
/// `Rc<RefCell<_>>`, not `Send`).
const LARGE_EXPORT_ROWS: usize = 50_000;
/// Review fix (Task 4 round 1, Issue 1): `start_export`'s row/column
/// snapshot reads this many display rows per iteration of a `cx.spawn` loop
/// on the FOREGROUND executor, yielding (a real, non-zero timer await — see
/// that loop's comment for why zero-duration doesn't actually yield) between
/// chunks so the window keeps painting/handling input even for a full
/// spilled 500k-row export, instead of one unbounded synchronous pass.
const EXPORT_SNAPSHOT_CHUNK_ROWS: usize = 25_000;

actions!(grid, [CopySelection, FindInResult, FindNext, FindPrev]);

/// G4 Task 5: what a `ResultGrid` asks `main.rs` to DO on its behalf when the
/// ☰ FK menu's checkboxes change — the grid itself has no connection/runner
/// access (same reasoning `TreeEvent` and the export-dialog seam already
/// follow: I/O stays in `main.rs`/`runner.rs`, the widget only decides
/// intent). Carries everything `main.rs` needs to act without having to
/// search `self.tabs` for "which tab is this" — for `RerunPreviewJoins` that
/// means the whole preview identity (schema/table/key/title) travels with
/// the event even though `main.rs` already "knows" it in principle, because
/// the emitting grid is about to be REPLACED by a brand new grid entity
/// (`Tabs::close_by_preview_key` + a fresh `cx.new(ResultGrid::new)` — see
/// `QueryEvent::Started`), so there is no "this tab" to look up by the time
/// the event is handled. `RunLookup` instead updates the SAME (never
/// replaced) ad-hoc grid entity — `main.rs`'s subscribe closure captures the
/// `emitter` handle for that.
pub enum GridEvent {
    RerunPreviewJoins {
        schema: Option<String>,
        table: String,
        key: String,
        title: String,
        joins: Vec<JoinSpec>,
        /// Review fix (Task 5 round 1, Issue 2): the SOURCE column/ref-column
        /// this specific checkbox click toggled — `main.rs::on_grid_event`
        /// needs this to undo exactly that flip via
        /// `ResultGrid::revert_fk_toggle` if the re-run can't actually start
        /// (one-query-at-a-time guard already busy). `toggle_fk_column`
        /// already mutated `fk_checked` and `cx.notify()`'d before emitting,
        /// so without this the checkbox would stay visibly checked/unchecked
        /// while the SQL behind it never changed.
        col: usize,
        ref_col: String,
    },
    /// Review fix (Task 5 round 1, Issue 5, informational): unlike
    /// `RerunPreviewJoins` (routed through `run_query_with`'s
    /// one-query-at-a-time guard), `main.rs::start_lookup` dispatches this
    /// regardless of whether a query is already running — deliberately safe,
    /// since `QueryRunner::fetch_lookup` opens its own independent
    /// connection rather than sharing one with an in-flight run (see
    /// `start_lookup`'s own doc comment for the full reasoning).
    RunLookup {
        sql: String,
        ref_table: String,
        wanted_cols: Vec<String>,
        src_col: usize,
        /// Review fix (Task 5 round 1, Issue 1): this `src_col`'s
        /// `ResultGrid::lookup_generation` value AT THE MOMENT this event
        /// was emitted — threaded through `main.rs::start_lookup` and back
        /// into `ResultGrid::accept_lookup_result` when the response
        /// arrives, so a response can tell whether a newer request for the
        /// same column has been dispatched since (last-dispatched wins, not
        /// last-arrived).
        generation: u64,
    },
    /// G4 Task 6: emitted by a PREVIEW tab whenever its local view state
    /// changes in a way `main.rs` should persist via `ViewPrefsStore` — sort
    /// (`on_header_click`), column visibility (`toggle_column_visibility`),
    /// or a width-drag END (the `on_mouse_up`/`on_mouse_up_out` handlers in
    /// `Render::render`, never mid-drag — see those call sites). Ad-hoc tabs
    /// never emit this (every call site is guarded by `self.is_preview`).
    /// Carries nothing: `main.rs`'s handler reads everything it needs
    /// (identity + current sort/hidden/widths/fk-joins) straight off
    /// `emitter` via `ResultGrid::{preview_identity,column_names,view_state,
    /// col_widths,active_fk_join_names}` rather than duplicating that state
    /// into the event payload. fk-join changes are NOT saved through this
    /// event — a ☰ toggle re-runs the query (`RerunPreviewJoins`) and the
    /// resulting new grid's state is saved once its `Started` event lands
    /// instead (see `main.rs::apply_view_prefs_to_grid`'s doc comment).
    ViewChanged,
}

/// Bind ResultGrid's own keys. Scoped to the `"ResultGrid"` key context so
/// ctrl-c only fires `CopySelection` while the grid (not `SqlInput`) is
/// focused — SqlInput binds its own `Copy` action under context `None`, and
/// since the grid isn't in SqlInput's dispatch path (and vice versa), the two
/// never contend even without scoping, but the explicit context makes the
/// intent unambiguous and future-proof.
///
/// G4 Task 3: `enter`/`shift-enter` are scoped to `"ResultGrid"`. Precision
/// note (Task 3 review issue 4): per the pinned gpui's depth-based keymap
/// precedence, SqlInput's UNSCOPED `enter → Newline` binding actually
/// outranks these scoped ones and is tried FIRST — it just has no handler
/// anywhere in the find bar's dispatch path (SqlInput is a sibling subtree),
/// so dispatch falls through to the next binding, which is our scoped
/// `FindNext`/`FindPrev` with a handler on the grid root. It works by
/// binding-fallthrough, NOT by "ancestor scope wins". Harmless no-ops via
/// `on_find_next`/`on_find_prev` when the find bar isn't open.
pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-c", CopySelection, Some("ResultGrid")),
        KeyBinding::new("ctrl-f", FindInResult, Some("ResultGrid")),
        KeyBinding::new("enter", FindNext, Some("ResultGrid")),
        KeyBinding::new("shift-enter", FindPrev, Some("ResultGrid")),
    ]);
}

/// G4 Task 3: Ctrl+F find-bar state — created on `FindInResult`, dropped on
/// close (Esc, via `close_overlay_if_open`, or the find bar's own "✕").
/// `matches`/`current` are in DISPLAY coordinates: `(display_row,
/// source_col)`, i.e. already mapped through `RowView::source_row` for the
/// row half but NOT for the column half (`source_col` is what
/// `buf.cell_text` and `col_widths` index by, same convention as
/// `hidden_cols`/`filters`).
struct FindState {
    input: Entity<TextField>,
    matches: Vec<(usize, usize)>,
    /// True when the last scan hit `FIND_MAX_ROWS`/`FIND_MAX_MATCHES` —
    /// the indicator shows "n+" instead of an exact total.
    capped: bool,
    current: Option<usize>,
    /// The text `matches` was last computed from — compared against
    /// `input`'s live text each render, same lazy-recompute trigger as
    /// `AppView::last_history_query`/`PaletteState::last_query`.
    last_query: String,
    /// The `ResultGrid::view_generation` `matches` was last computed
    /// against — a sort/filter rebuild changes which display row each
    /// match lives at (or removes it) even when the query text hasn't
    /// changed, so text-equality alone isn't enough to know `matches` is
    /// still valid.
    computed_generation: u64,
}

/// G4 Task 3: full-text popup for a double-clicked cell (the grid clips
/// long values). `scroll_lines` is mutated by mouse wheel exactly like
/// `main.rs`'s `TabContent::Text` body scrolling.
#[derive(Clone)]
struct CellDetail {
    text: String,
    scroll_lines: usize,
}

/// G5 Task 3: which cell a `CellEditor` overlay is staging into — a real
/// row's cell (keyed by SOURCE row, brief contract #6) or one column of a
/// not-yet-applied inserted row (`sandbox::EditState::inserted_rows`
/// index).
#[derive(Clone, Copy)]
enum EditTarget {
    Cell { source_row: usize, col: usize },
    Insert { ins_ix: usize, col: usize },
}

/// G5 Task 3: the cell-editor overlay a double-click opens on an editable
/// tab (brief contract #2) — column name + the full ORIGINAL (committed)
/// text shown for reference, an editable `TextField` prefilled with the
/// CURRENT display value (staged value if this cell is already staged, else
/// the original), and Uložit/NULL/Zrušit. `original_text` is fixed at open
/// time (rendered once); `input`'s live text is read only when Uložit is
/// clicked.
struct CellEditor {
    target: EditTarget,
    column_name: String,
    original_text: String,
    /// G5 Task 4 (folded T3 review issue 4 — NULL cue): true when, AT OPEN
    /// TIME, `target`'s CURRENT staged state (not the ORIGINAL committed
    /// value `original_text` holds) is an explicit staged SQL NULL —
    /// `Some(None)` in `EditState::cells`/`inserted_rows` terms. Distinct
    /// from "untouched" (nothing staged, `original_text` is what shows) and
    /// from a staged EMPTY STRING (`Some(Some(String::new()))`) — both of
    /// those prefill the `TextField` with an indistinguishable-looking
    /// empty box, same as a staged NULL does (`open_cell_editor`'s prefill
    /// is `""` in all three cases). Without this cue, reopening a
    /// NULL-staged cell and clicking "Uložit" without retyping anything
    /// silently restages `''` instead of leaving the NULL alone — surfaced
    /// as an "aktuálně: (NULL)" line in `render_cell_editor_overlay` (the
    /// least invasive fix: it doesn't change what "Uložit" does, it just
    /// makes the already-staged NULL visible before the user acts).
    currently_staged_null: bool,
    input: Entity<TextField>,
}

pub struct ResultGrid {
    pub buffer: Option<Rc<RefCell<ResultBuffer>>>,
    pub col_widths: Vec<f32>,
    focus_handle: FocusHandle,
    /// (anchor cell, focus cell) as (row, col) — DISPLAY-order coordinates
    /// (post `view`, not source rows); normalized (and mapped through
    /// `view.source_row`) only at copy time, see `on_copy`.
    selection: Option<((usize, usize), (usize, usize))>,
    /// (col index, mouse-down start x, start width) while a resize drag is active.
    resizing: Option<(usize, f32, f32)>,
    /// G4 Task 2: local sort (+ Task 3's filters) view over `buffer`.
    /// `uniform_list`'s row count is `view.len()`, and every row index used
    /// to read/select a cell is first mapped through `view.source_row`.
    pub view: RowView,
    /// By SOURCE column index — `true` hides that column from the header,
    /// cells, and `col_widths` iteration. Sized to `column_count()` in
    /// `set_buffer`, all `false` initially.
    pub hidden_cols: Vec<bool>,
    /// Set when a sort/filter is active and a streamed `Batch` arrived
    /// while it was — resorting per-batch would be wasted work, so
    /// `on_batch_grown` just marks this and `on_stream_finished` does the
    /// one deferred rebuild (see `main.rs`'s `QueryEvent::Batch`/`Finished`
    /// handling).
    dirty: bool,
    /// Set by `rebuild_view` right before a rebuild over more than
    /// `LARGE_SORT_ROWS` rows, read (and cleared) by `main.rs`'s
    /// `render_tab_content` into `AppView::status` — `ResultGrid` doesn't
    /// own a status bar itself, so this is the minimal one-field seam
    /// rather than a full event/callback plumbing just for this note.
    /// Since `rebuild` is synchronous, this ends up describing the sort
    /// that JUST happened rather than one still in flight; documented here
    /// because the brief's "in-progress" framing doesn't quite hold for a
    /// synchronous rebuild.
    pub status_note: Option<String>,
    /// G4 Task 3: bumped every `rebuild_view` call — lets `FindState` (and
    /// anything else that caches derived-from-`view` positions) tell
    /// whether its cache is stale without comparing the whole `RowView`.
    view_generation: u64,
    /// "Filtr" toggle (brief contract #1) — whether the filter row (one
    /// `TextField` per VISIBLE column, below the toolbar) is shown.
    /// Toggling this OFF also clears every active filter (see
    /// `toggle_filters`): once hidden there's no way to see/edit which
    /// columns were filtered, so leaving stale filters silently applied
    /// would be a footgun.
    filters_open: bool,
    /// By SOURCE column index (sized to `column_count()` in `set_buffer`,
    /// same convention as `hidden_cols`/`col_widths`) — one persistent
    /// `TextField` per column, created once so its text/cursor survive
    /// across renders (recreating it every frame would drop focus and any
    /// in-progress edit). Hidden columns still have an entry here (`view`
    /// state re: Task 6 may re-show a column later) but never get rendered
    /// or polled while hidden.
    filter_inputs: Vec<Entity<TextField>>,
    /// Last text pulled from each `filter_inputs[i]`, compared against its
    /// live text every render (`toolbar`) to detect an edit — same
    /// history_cache-style polling `AppView::last_history_query` uses,
    /// since `TextField` has no on-change hook. `view.filters` is rebuilt
    /// from the non-empty entries of this whenever any of them differs.
    filter_cache: Vec<String>,
    /// Ctrl+F find-bar state; `None` when closed (brief: search is opt-in,
    /// not always-on chrome).
    find: Option<FindState>,
    /// Double-click-a-cell popup; `None` when closed. Owned here (not
    /// `AppView::modal`) since it's grid-local state tied to a specific
    /// tab's `ResultGrid`, same reasoning `find`/`filters_open` follow.
    cell_detail: Option<CellDetail>,
    /// Bound to the result `uniform_list` via `.track_scroll` — lets
    /// `find_step`/`toolbar`'s auto-jump-to-first-match scroll a matched
    /// row into view (`UniformListScrollHandle::scroll_to_item`).
    scroll_handle: UniformListScrollHandle,
    /// G4 Task 4: source table name for `INSERT` exports. `"export"` is the
    /// placeholder for an ad-hoc SQL-editor result (no single source table
    /// is known); a schema-tree/palette preview tab overrides this via
    /// `set_table_name` right after `set_buffer` since it DOES know its
    /// table (`main.rs`'s `QueryEvent::Started` handling).
    pub table_name: String,
    /// "Export ▾" dropdown open/closed.
    export_open: bool,
    /// "Sloupce ▾" dropdown open/closed.
    columns_open: bool,
    // --- G4 Task 5: FK joined columns ---
    /// By SOURCE column index (sized to `column_count()` in `set_buffer`,
    /// same convention as `hidden_cols`) — `Some(fk)` marks a column as an
    /// FK the ☰ menu can offer to join, populated by `main.rs` via
    /// `set_fk_info` right after `set_buffer`/`set_table_name` (a preview
    /// tab looks up the previewed `TableInfo`; an ad-hoc tab uses the
    /// "exactly one snapshot table has all these column names" heuristic —
    /// see `set_fk_info`'s doc comment).
    fk_info: Vec<Option<FkRef>>,
    /// Parallel to `fk_info`: the referenced table's column names (from the
    /// same schema snapshot main.rs already has), precomputed at the same
    /// time as `fk_info` rather than making the grid hold the whole
    /// snapshot itself — the simplest of the two shapes the brief allows
    /// (precompute vs. grid-emits-event-main-answers).
    fk_ref_columns: Vec<Option<Vec<String>>>,
    /// Which SOURCE column's ☰ dropdown is open, if any. Mutually exclusive
    /// with `export_open`/`columns_open` (same "only one popover" rule).
    fk_menu_open: Option<usize>,
    /// By SOURCE column index (sized to `fk_info.len()`) — the ref-table
    /// columns currently checked in that column's ☰ menu. Preview tabs: a
    /// change here is what `build_active_joins` turns into the
    /// `RerunPreviewJoins` event's `joins`. Ad-hoc tabs: a change here
    /// drives `RunLookup`/`virtual_cols` for that one FK column only.
    fk_checked: Vec<HashSet<String>>,
    /// `true` once `set_preview_context` has been called — distinguishes
    /// the PREVIEW re-run path (☰ toggle → `RerunPreviewJoins`) from the
    /// AD-HOC lookup path (☰ toggle → `RunLookup` + `virtual_cols`) in
    /// `toggle_fk_column`. `false` (the `set_buffer` default) for every
    /// SQL-editor result.
    is_preview: bool,
    /// Preview-tab identity, set together by `set_preview_context` — `None`
    /// on an ad-hoc tab. Threaded back out on `RerunPreviewJoins` since the
    /// re-run replaces this very grid entity (see `GridEvent`'s doc
    /// comment).
    preview_schema: Option<String>,
    preview_key: Option<String>,
    preview_title: Option<String>,
    /// PREVIEW tabs only: by SOURCE column index (sized to `fk_info.len()`
    /// — i.e. to the CURRENT, post-join result's column count), `true` when
    /// that column's header name is one of the active joins' `"{ref_table}.
    /// {col}"` aliases — computed once by `apply_active_joins` right after
    /// a (re-)run rather than re-derived every render. Drives the tinted
    /// header/cell background (brief: bg 0x2a2a3d).
    joined_cols: Vec<bool>,
    /// AD-HOC tabs only: looked-up columns rendered AFTER the source
    /// columns — see `fk_join::VirtualCol` and the module's
    /// "effective-column" indexing doc comment (sources `0..n`, virtuals
    /// `n..n+m`). Populated/replaced per-FK-column by `set_virtual_cols_for_src`.
    virtual_cols: Vec<VirtualCol>,
    /// AD-HOC tabs only, review fix (Task 5 round 1, Issue 1): by SOURCE
    /// column index (sized to `fk_info.len()`, same convention as
    /// `fk_checked`) — bumped by `toggle_fk_column` on EVERY ad-hoc state
    /// transition for that column, whether it fires a fresh `RunLookup` or
    /// just clears `virtual_cols` locally (an uncheck-to-zero never
    /// dispatches a request, but still needs to invalidate any earlier
    /// still-in-flight one). `GridEvent::RunLookup::generation` carries the
    /// value captured at dispatch time; `accept_lookup_result` compares it
    /// against the CURRENT value here when the response arrives — a
    /// mismatch means a newer request (or a local clear) has superseded it,
    /// so the stale response is dropped (last-dispatched wins, not
    /// last-arrived).
    lookup_generation: Vec<u64>,
    // --- G5 Task 3: grid edit mode (staged diff over `sandbox::EditState`) ---
    /// `Some` only on a PREVIEW tab that passed every `detect_editable_pk`
    /// check (brief contract #1) — set once by `main.rs` via
    /// `set_editable`, right after `set_buffer`/`set_fk_info`. Drives every
    /// edit affordance below: `None` means no gutter, no cell editor (a
    /// double-click falls back to the existing read-only `cell_detail`
    /// popup), no "+ řádek" button, exactly the brief's "PK-less table or
    /// read-only or ad-hoc" case.
    pub editable: Option<Editable>,
    /// Staged, not-yet-applied edits for this tab — the Apply dialog (a
    /// later task) will turn this into `sandbox::generate_statements`
    /// input; here it only drives staged/deleted/inserted diff rendering
    /// and the tab-strip dirty indicator (`main.rs` reads
    /// `edit_state.is_dirty()`).
    pub edit_state: EditState,
    /// Double-click-a-cell editor overlay (brief contract #2); `None` when
    /// closed. Distinct from `cell_detail` (the non-editable popup) —
    /// mutually exclusive in practice since a double-click opens exactly
    /// one of the two depending on `editable`/the clicked column, but kept
    /// as separate fields rather than one enum since their contents differ
    /// enough (this one owns a live `TextField`) that a shared type would
    /// need its own internal `Option` anyway.
    cell_editor: Option<CellEditor>,
}

impl ResultGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            buffer: None,
            col_widths: Vec::new(),
            focus_handle: cx.focus_handle(),
            selection: None,
            resizing: None,
            view: RowView::identity(0),
            hidden_cols: Vec::new(),
            dirty: false,
            status_note: None,
            view_generation: 0,
            filters_open: false,
            filter_inputs: Vec::new(),
            filter_cache: Vec::new(),
            find: None,
            cell_detail: None,
            scroll_handle: UniformListScrollHandle::new(),
            table_name: "export".to_string(),
            export_open: false,
            columns_open: false,
            fk_info: Vec::new(),
            fk_ref_columns: Vec::new(),
            fk_menu_open: None,
            fk_checked: Vec::new(),
            is_preview: false,
            preview_schema: None,
            preview_key: None,
            preview_title: None,
            joined_cols: Vec::new(),
            virtual_cols: Vec::new(),
            lookup_generation: Vec::new(),
            editable: None,
            edit_state: EditState::default(),
            cell_editor: None,
        }
    }

    /// `cx` (new in Task 3) is needed to create one `TextField` entity per
    /// column for the filter row — see `filter_inputs`'s doc comment.
    pub fn set_buffer(&mut self, buffer: Rc<RefCell<ResultBuffer>>, cx: &mut Context<Self>) {
        let ncols = buffer.borrow().column_count();
        let nrows = buffer.borrow().row_count();
        self.col_widths = vec![DEFAULT_COL_WIDTH; ncols];
        self.buffer = Some(buffer);
        self.selection = None;
        self.view = RowView::identity(nrows);
        self.hidden_cols = vec![false; ncols];
        self.dirty = false;
        self.status_note = None;
        self.view_generation += 1;
        self.filters_open = false;
        self.filter_inputs =
            (0..ncols).map(|_| cx.new(|cx| TextField::new(cx, "filtr…", false))).collect();
        self.filter_cache = vec![String::new(); ncols];
        self.find = None;
        self.cell_detail = None;
        self.table_name = "export".to_string();
        self.export_open = false;
        self.columns_open = false;
        self.fk_info = vec![None; ncols];
        self.fk_ref_columns = vec![None; ncols];
        self.fk_menu_open = None;
        self.fk_checked = vec![HashSet::new(); ncols];
        self.is_preview = false;
        self.preview_schema = None;
        self.preview_key = None;
        self.preview_title = None;
        self.joined_cols = vec![false; ncols];
        self.virtual_cols = Vec::new();
        self.lookup_generation = vec![0; ncols];
        self.editable = None;
        self.edit_state = EditState::default();
        self.cell_editor = None;
    }

    /// G5 Task 3 public seam: called by `main.rs` right after `set_buffer`
    /// (and `set_fk_info`) for a PREVIEW tab that passed `detect_editable_pk`
    /// — `None` (already `set_buffer`'s default) for every ad-hoc tab or a
    /// preview that failed one of that function's checks. Doesn't touch
    /// `edit_state` — a fresh `set_buffer` already reset it, and a re-run of
    /// the SAME preview (a ☰ toggle, a sort/filter never re-runs SQL at all)
    /// intentionally starts with a clean slate rather than trying to carry
    /// staged edits across a brand-new `ResultBuffer`/row set.
    pub fn set_editable(&mut self, editable: Option<Editable>) {
        self.editable = editable;
    }

    /// Public seam (Task 4): called by `main.rs` right after `set_buffer`
    /// for a tab whose source table IS known (a schema-tree/palette
    /// preview) — overrides the `"export"` placeholder default (see
    /// `table_name`'s doc comment) used for ad-hoc SQL-editor results.
    pub fn set_table_name(&mut self, name: String) {
        self.table_name = name;
    }

    /// G4 Task 5: marks this grid as a PREVIEW tab and records its identity
    /// — called by `main.rs` right after `set_buffer`/`set_table_name` for a
    /// preview (schema-tree/palette) tab, mirroring `set_table_name`'s "own
    /// public seam" shape. `key`/`title` are exactly `PreviewTarget`'s
    /// fields (see main.rs) — carried here only so `toggle_fk_column` can
    /// hand them back out on `GridEvent::RerunPreviewJoins` (this grid
    /// entity is about to be replaced by the re-run, see `GridEvent`'s doc
    /// comment, so `main.rs` can't just "look up the active tab" when the
    /// event arrives).
    pub fn set_preview_context(&mut self, schema: Option<String>, key: String, title: String) {
        self.is_preview = true;
        self.preview_schema = schema;
        self.preview_key = Some(key);
        self.preview_title = Some(title);
    }

    /// G4 Task 5: FK metadata for the ☰ menu — `fk_info`/`ref_columns` are
    /// parallel to the CURRENT result's columns (sized/ordered the same as
    /// `buf.schema().fields()`), computed by `main.rs`:
    /// - PREVIEW tabs: `fk_info[i]` is the previewed table's `ColumnInfo::fk`
    ///   for the column named `fields[i].name()` (or `None` if the column
    ///   has no such name in the base table — e.g. an already-joined
    ///   `"ref.col"` alias from a previous re-run).
    /// - AD-HOC tabs: matched against ALL snapshot tables that contain every
    ///   result column name; if exactly one such table exists, its FK data
    ///   is used the same way, otherwise `fk_info` stays all-`None` (brief:
    ///   ambiguous match = no ☰, documented heuristic — a result that could
    ///   plausibly come from two different tables with overlapping column
    ///   sets doesn't get FK-menu treatment rather than guessing wrong).
    ///
    /// Resets `fk_checked` to empty (no columns joined yet) — callers that
    /// need to restore a previous selection (a preview re-run) call
    /// `apply_active_joins` right after this.
    pub fn set_fk_info(&mut self, fk_info: Vec<Option<FkRef>>, ref_columns: Vec<Option<Vec<String>>>) {
        self.fk_checked = vec![HashSet::new(); fk_info.len()];
        self.lookup_generation = vec![0; fk_info.len()];
        self.fk_info = fk_info;
        self.fk_ref_columns = ref_columns;
    }

    /// G4 Task 5, PREVIEW tabs only: restores `fk_checked` (so the ☰ menu
    /// shows the right checkmarks after a re-run) and computes `joined_cols`
    /// (tinting) from `joins` — the SAME join list that was just used to
    /// build this result's SQL (`fk_join::build_join_sql`), matched back
    /// against THIS result's actual columns: `fk_checked` by finding each
    /// join's `fk_col` name among the base columns, `joined_cols` by
    /// checking which header names equal one of the joins' `"{ref_table}.
    /// {col}"` aliases (exactly the aliases `build_join_sql` emitted — a
    /// column named that way is unambiguously "this session's join
    /// output", not a coincidentally-dotted real column name, since none of
    /// the drivers' catalog columns contain a literal `.`).
    pub fn apply_active_joins(&mut self, joins: &[JoinSpec]) {
        let Some(buf) = &self.buffer else { return };
        let buf = buf.borrow();
        let names: Vec<String> =
            buf.schema().fields().iter().map(|f| f.name().to_string()).collect();
        drop(buf);
        for j in joins {
            if let Some(ix) = names.iter().position(|n| n == &j.fk_col) {
                if let Some(set) = self.fk_checked.get_mut(ix) {
                    *set = j.cols.iter().cloned().collect();
                }
            }
        }
        let mut expected: HashSet<String> = HashSet::new();
        for j in joins {
            for c in &j.cols {
                expected.insert(format!("{}.{}", j.ref_table, c));
            }
        }
        self.joined_cols = names.iter().map(|n| expected.contains(n)).collect();
    }

    /// "☰" click (header, FK columns only). Mutually exclusive with
    /// `export_open`/`columns_open`, same "only one popover" convention
    /// `toggle_export_menu`/`toggle_columns_menu` already follow.
    fn toggle_fk_menu(&mut self, col: usize, cx: &mut Context<Self>) {
        self.fk_menu_open = if self.fk_menu_open == Some(col) { None } else { Some(col) };
        self.export_open = false;
        self.columns_open = false;
        cx.notify();
    }

    /// Sum of the visible SOURCE column widths strictly before `col` — used
    /// to roughly anchor the ☰ dropdown under its own column instead of
    /// always in one fixed spot like `export`/`columns` menus (whose
    /// triggers live in the fixed toolbar, not a column that can be
    /// anywhere in a wide, scrolled header).
    fn fk_menu_left_offset(&self, col: usize) -> f32 {
        let mut x = 0.0;
        for i in 0..col.min(self.hidden_cols.len()) {
            if !self.hidden_cols.get(i).copied().unwrap_or(false) {
                x += self.col_widths.get(i).copied().unwrap_or(DEFAULT_COL_WIDTH);
            }
        }
        x
    }

    /// A ☰-menu checkbox click for `col`'s `ref_col`. Toggles membership in
    /// `fk_checked[col]`, then dispatches per tab kind (brief contract #2 vs
    /// #3):
    /// - PREVIEW: rebuilds the FULL active-join list across every FK column
    ///   (not just `col` — `build_join_sql` needs the complete set for one
    ///   SQL rewrite) and emits `RerunPreviewJoins`; `main.rs` re-runs
    ///   through the normal preview pipeline, which replaces this tab.
    /// - AD-HOC: unchecking down to zero columns for `col` just clears its
    ///   `virtual_cols` locally (no query needed — nothing left to show).
    ///   Otherwise collects the CURRENT VIEW's distinct `col` values (capped
    ///   at 1000, brief contract #3) and emits `RunLookup`; an over-cap
    ///   result aborts locally with the brief's exact status text instead of
    ///   firing a query at all.
    fn toggle_fk_column(&mut self, col: usize, ref_col: String, cx: &mut Context<Self>) {
        let Some(set) = self.fk_checked.get_mut(col) else { return };
        // Clones into `insert` (rather than moving `ref_col`) so the PREVIEW
        // branch below can still hand `ref_col` to `GridEvent::RerunPreviewJoins`
        // (Task 5 round 1, Issue 2's revert path needs it).
        if !set.remove(&ref_col) {
            set.insert(ref_col.clone());
        }
        let checked_ordered: Vec<String> = self
            .fk_ref_columns
            .get(col)
            .and_then(|o| o.as_ref())
            .map(|all| {
                all.iter().filter(|c| self.fk_checked[col].contains(*c)).cloned().collect()
            })
            .unwrap_or_default();

        if self.is_preview {
            let joins = self.build_active_joins();
            cx.emit(GridEvent::RerunPreviewJoins {
                schema: self.preview_schema.clone(),
                table: self.table_name.clone(),
                key: self.preview_key.clone().unwrap_or_default(),
                title: self.preview_title.clone().unwrap_or_default(),
                joins,
                col,
                ref_col,
            });
            cx.notify();
            return;
        }

        // Review fix (Task 5 round 1, Issue 1): EVERY ad-hoc state
        // transition for this column bumps its lookup generation first —
        // whether it goes on to fire a fresh `RunLookup` below or just
        // clears `virtual_cols` locally (the empty/over-cap/no-distinct-
        // values paths). Bumping on the local-clear paths too is what lets
        // `accept_lookup_result` catch "check, then quickly uncheck again"
        // (uncheck never dispatches a new request — without this the
        // earlier still-in-flight response would have nothing newer to lose
        // to) as well as the "check A, then check B before A resolves" race
        // (both dispatch, only B's generation is current). See
        // `lookup_generation`'s doc comment.
        if let Some(g) = self.lookup_generation.get_mut(col) {
            *g += 1;
        }
        let generation = self.lookup_generation.get(col).copied().unwrap_or(0);

        if checked_ordered.is_empty() {
            self.virtual_cols.retain(|v| v.src_col != col);
            self.sync_virtual_aux(cx);
            cx.notify();
            return;
        }
        let Some(fk) = self.fk_info.get(col).cloned().flatten() else { return };
        let Some(distinct) = self.collect_distinct_fk_values(col) else {
            self.status_note = Some("příliš mnoho hodnot pro dočasný join".to_string());
            cx.notify();
            return;
        };
        if distinct.is_empty() {
            // Nothing to look up (every visible value is NULL, or the view
            // is empty) — clear rather than firing a pointless `IN ()`.
            self.virtual_cols.retain(|v| v.src_col != col);
            self.sync_virtual_aux(cx);
            cx.notify();
            return;
        }
        let sql = fk_join::build_lookup_sql(
            fk.schema.as_deref(),
            &fk.table,
            &fk.column,
            &checked_ordered,
            &distinct,
        );
        cx.emit(GridEvent::RunLookup {
            sql,
            ref_table: fk.table.clone(),
            wanted_cols: checked_ordered,
            src_col: col,
            generation,
        });
        cx.notify();
    }

    /// Review fix (Task 5 round 1, Issue 1): decides whether a `RunLookup`
    /// response for `col` should still be applied, given the generation
    /// `main.rs::start_lookup` captured at dispatch time and the ref-columns
    /// that response was fetched for (`wanted_cols`). Two independent
    /// staleness checks, either one alone is enough to reject:
    /// - `generation` no longer matches `lookup_generation[col]` — a newer
    ///   ad-hoc state transition for this column (another toggle, in either
    ///   direction) has happened since this request was dispatched.
    /// - any of `wanted_cols` is no longer in `fk_checked[col]` — the user
    ///   has since unchecked at least one column this response covers.
    ///   Redundant with the generation check for every path THIS file
    ///   drives (every transition bumps the generation), but kept as an
    ///   explicit, independently-correct second condition per the review
    ///   recommendation rather than relying solely on every future call site
    ///   remembering to bump — cheap defense in depth for a HashSet compare
    ///   this small.
    pub fn accept_lookup_result(&self, col: usize, generation: u64, wanted_cols: &[String]) -> bool {
        let current = self.lookup_generation.get(col).copied().unwrap_or(0);
        let Some(checked) = self.fk_checked.get(col) else { return false };
        should_apply_lookup(generation, current, checked, wanted_cols)
    }

    /// Review fix (Task 5 round 1, Issue 2), PREVIEW tabs only: undoes
    /// exactly the checkbox flip `toggle_fk_column` just applied to
    /// `fk_checked[col]` — called by `main.rs::on_grid_event` when the
    /// `RerunPreviewJoins` this same toggle emitted arrives while a query is
    /// already running (`run_query_with`'s one-query-at-a-time guard).
    /// `toggle_fk_column` already mutated `fk_checked` and `cx.notify()`'d
    /// before emitting the event that got dropped, so without this the
    /// checkbox would stay visibly flipped while the SQL behind it never
    /// actually changed — a silent desync between what the ☰ menu shows and
    /// what the tab's data actually reflects. Toggling membership is its own
    /// inverse (remove-if-present else insert), so this is the exact same
    /// operation `toggle_fk_column` opened with, just without emitting a new
    /// event.
    pub fn revert_fk_toggle(&mut self, col: usize, ref_col: &str, cx: &mut Context<Self>) {
        if let Some(set) = self.fk_checked.get_mut(col) {
            if !set.remove(ref_col) {
                set.insert(ref_col.to_string());
            }
        }
        cx.notify();
    }

    /// PREVIEW tabs: rebuilds the complete `JoinSpec` list from `fk_info` +
    /// `fk_checked` across EVERY fk column (not just the one that was just
    /// toggled) — `build_join_sql` needs the whole set to emit one SQL
    /// rewrite with every active join, since a re-run replaces the entire
    /// query rather than adding one `LEFT JOIN` incrementally.
    fn build_active_joins(&self) -> Vec<JoinSpec> {
        let Some(buf) = &self.buffer else { return Vec::new() };
        let buf = buf.borrow();
        let mut joins = Vec::new();
        for (i, fk) in self.fk_info.iter().enumerate() {
            let Some(fk) = fk else { continue };
            let Some(checked) = self.fk_checked.get(i) else { continue };
            if checked.is_empty() {
                continue;
            }
            let cols: Vec<String> = self
                .fk_ref_columns
                .get(i)
                .and_then(|o| o.as_ref())
                .map(|all| all.iter().filter(|c| checked.contains(*c)).cloned().collect())
                .unwrap_or_default();
            if cols.is_empty() {
                continue;
            }
            let Some(field) = buf.schema().fields().get(i) else { continue };
            joins.push(JoinSpec {
                fk_col: field.name().to_string(),
                ref_schema: fk.schema.clone(),
                ref_table: fk.table.clone(),
                ref_key: fk.column.clone(),
                cols,
            });
        }
        joins
    }

    /// AD-HOC tabs: distinct values of SOURCE column `col` over the CURRENT
    /// VIEW (brief contract #3 — filtered/sorted rows the user is actually
    /// looking at, not the whole underlying result), capped at 1000 via
    /// `fk_join::collect_distinct_capped`. `None` = over cap.
    fn collect_distinct_fk_values(&self, col: usize) -> Option<Vec<String>> {
        let buf = self.buffer.clone()?;
        let mut buf = buf.borrow_mut();
        let n = self.view.len();
        let values: Vec<Option<String>> = (0..n)
            .map(|r| {
                let source_row = self.view.source_row(r);
                if buf.cell_is_null(source_row, col) {
                    None
                } else {
                    Some(buf.cell_text(source_row, col))
                }
            })
            .collect();
        fk_join::collect_distinct_capped(values, 1000)
    }

    /// AD-HOC tabs: replaces every `virtual_cols` entry whose `src_col ==
    /// col` with `new_cols` (a fresh lookup always supersedes — the
    /// checkbox set it was built from IS the full desired state for that FK
    /// column, not an incremental add) and re-syncs the width/filter
    /// plumbing that's sized to the effective (source + virtual) column
    /// count (see `sync_virtual_aux`).
    pub fn set_virtual_cols_for_src(&mut self, col: usize, new_cols: Vec<VirtualCol>, cx: &mut Context<Self>) {
        self.virtual_cols.retain(|v| v.src_col != col);
        self.virtual_cols.extend(new_cols);
        self.sync_virtual_aux(cx);
    }

    /// Keeps `col_widths`/`filter_inputs`/`filter_cache` sized to the
    /// EFFECTIVE column count (`ncols + virtual_cols.len()`, see the
    /// `fk_join` module doc comment) after `virtual_cols` changes. Source
    /// portion (`0..ncols`) is left untouched; the virtual portion is
    /// dropped and rebuilt from scratch each time — simpler than diffing,
    /// and cheap since `virtual_cols` is small (one entry per checked
    /// ref-column) and this only runs on a checkbox click, never per-frame.
    /// Rebuilding loses any per-virtual-column width a user had dragged,
    /// which is an accepted, documented trade-off (Task 5 scope) rather
    /// than an oversight.
    fn sync_virtual_aux(&mut self, cx: &mut Context<Self>) {
        let ncols = self.buffer.as_ref().map_or(0, |b| b.borrow().column_count());
        let total = ncols + self.virtual_cols.len();
        self.col_widths.truncate(ncols);
        self.col_widths.resize(total, DEFAULT_COL_WIDTH);
        self.filter_inputs.truncate(ncols);
        self.filter_cache.truncate(ncols);
        while self.filter_inputs.len() < total {
            self.filter_inputs.push(cx.new(|cx| TextField::new(cx, "filtr…", false)));
            self.filter_cache.push(String::new());
        }
        // A source-column filter/sort surviving a virtual-column add/remove
        // is unaffected (indices `< ncols` never move); but a filter/sort
        // that was targeting a NOW-DROPPED virtual index would silently
        // point at a different (or no) column — clear anything targeting
        // the virtual range so a stale index can't linger. Only a genuine
        // clear triggers a full `rebuild_view` (which can change row
        // order/count); otherwise this is a column-shape-only change with
        // the same "don't flash řadím… for a mere UI toggle" reasoning
        // `toggle_column_visibility` already documents — a plain
        // `view_generation` bump is enough to invalidate `find`'s cache.
        let had_filter = self.view.filters.iter().any(|(c, _)| *c >= ncols);
        self.view.filters.retain(|(c, _)| *c < ncols);
        let had_sort = self.view.sort.is_some_and(|(c, _)| c >= ncols);
        if had_sort {
            self.view.sort = None;
        }
        if had_filter || had_sort {
            self.rebuild_view();
        } else {
            self.view_generation += 1;
        }
    }

    /// Re-derives `view.order` from the current buffer contents. Sets
    /// `status_note` first when the source is large (see its doc comment)
    /// so a caller that immediately re-renders after this has a chance to
    /// surface it.
    fn rebuild_view(&mut self) {
        let Some(buf) = self.buffer.clone() else { return };
        let rows = buf.borrow().row_count();
        self.status_note =
            if rows > LARGE_SORT_ROWS { Some("řadím…".to_string()) } else { None };
        let ncols = buf.borrow().column_count();
        // G4 Task 5: `virtual_cols` borrowed as its OWN field (disjoint from
        // `self.view`, which `self.view.rebuild(..)` below borrows
        // mutably) — see `effective_text`'s doc comment for why this has to
        // be a free function taking both pieces separately.
        let virtual_cols = &self.virtual_cols;
        let mut buf = buf.borrow_mut();
        self.view.rebuild(rows, &mut |r, c| effective_text(ncols, virtual_cols, &mut buf, r, c));
        drop(buf);
        // G4 Task 3: any active find's `matches` were computed against the
        // PREVIOUS display order — bump so `toolbar`'s staleness check
        // recomputes them (see `FindState::computed_generation`).
        self.view_generation += 1;
        // Final review fix #1 (HIGH — stale selection vs. shrunken view):
        // `selection` is stored in DISPLAY coordinates and deliberately
        // survives a call here in the common no-sort/no-filter `Identity`
        // case (`on_batch_grown` routes every streamed batch through this
        // function; the view only ever grows there, so an in-progress
        // selection must not be wiped every batch). But a sort/filter
        // change (header click, a typed column filter, a virtual-column
        // add/remove that drops a stale sort/filter) can genuinely shrink
        // the view out from under an existing selection, leaving its row
        // range pointing past `view.len()` — the scenario that panicked
        // `on_copy` via `view.source_row`. Drop the selection as soon as
        // that's detected, rather than leaving it dangling until the next
        // copy/click; `on_copy` and the double-click cell-detail handler
        // (grid.rs, mouse-down listener) each ALSO carry their own
        // defensive bounds check, since a click can still land in the
        // narrow window between the view shrinking and this rebuild
        // running (e.g. the click-then-Ctrl+C race, or a click whose
        // `row_ix` was captured at a since-stale render).
        if let Some(((r0, _), (r1, _))) = self.selection {
            if r0.max(r1) >= self.view.len() {
                self.selection = None;
            }
        }
    }

    /// Called from `main.rs`'s `QueryEvent::Batch` handling for the tab
    /// that just grew. When no sort/filter is active this is the cheap
    /// `RowView::rebuild` early-return (just refreshes the identity count);
    /// otherwise resorting the whole set on every batch would be wasted
    /// work, so it's deferred — see `dirty` and `on_stream_finished`.
    pub fn on_batch_grown(&mut self) {
        if self.view.sort.is_some() || !self.view.filters.is_empty() {
            self.dirty = true;
        } else {
            self.rebuild_view();
        }
    }

    /// Called from `main.rs`'s `QueryEvent::Finished` handling. If a sort/
    /// filter was deferred during streaming (`dirty`), does the one
    /// rebuild now and returns a status note to append; `None` when there
    /// was nothing deferred (identity view — already up to date).
    pub fn on_stream_finished(&mut self) -> Option<String> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.rebuild_view();
        Some("seřazeno po dokončení".to_string())
    }

    /// Header click (outside the resize-handle strip): cycles this
    /// column's sort none → asc → desc → none, replacing any previous
    /// sort column (only one sort column at a time), then rebuilds.
    fn on_header_click(&mut self, col: usize, cx: &mut Context<Self>) {
        self.view.sort = match self.view.sort {
            Some((c, true)) if c == col => Some((col, false)),
            Some((c, false)) if c == col => None,
            _ => Some((col, true)),
        };
        self.rebuild_view();
        // G4 Task 6: persist the new sort for a PREVIEW tab (see
        // `GridEvent::ViewChanged`'s doc comment) — a no-op emit target for
        // an ad-hoc tab, so guard here rather than relying on the (absent)
        // subscriber to ignore it.
        if self.is_preview {
            cx.emit(GridEvent::ViewChanged);
        }
        cx.notify();
    }

    /// Public seam for Task 6 persistence (view_prefs): applies a saved
    /// sort + hidden-column set and rebuilds. `hidden` is by SOURCE column
    /// index, same convention as `hidden_cols`. Called by
    /// `main.rs::apply_view_prefs_to_grid` on a PREVIEW tab's `Started`
    /// event, once `dbc-state::ViewPrefsStore` prefs have been mapped from
    /// saved names to this result's current indices.
    pub fn set_view_state(&mut self, sort: Option<(usize, bool)>, hidden: Vec<bool>) {
        self.view.sort = sort;
        self.hidden_cols = hidden;
        self.rebuild_view();
    }

    /// Public seam for Task 6 persistence: current sort + hidden-column
    /// state, by SOURCE column index — read by
    /// `main.rs::save_view_prefs_for_grid`.
    pub fn view_state(&self) -> (Option<(usize, bool)>, Vec<bool>) {
        (self.view.sort, self.hidden_cols.clone())
    }

    /// G4 Task 6: this PREVIEW tab's identity for `ViewPrefsStore::get`/
    /// `set` — `None` for an ad-hoc tab (never persisted, brief contract:
    /// "ad-hoc tabs: per-tab only, nothing persisted"). `(schema, table)`,
    /// exactly `PreviewTarget`'s own fields — `main.rs` still supplies the
    /// connection id itself (the grid doesn't know it).
    pub fn preview_identity(&self) -> Option<(Option<String>, String)> {
        if self.is_preview {
            Some((self.preview_schema.clone(), self.table_name.clone()))
        } else {
            None
        }
    }

    /// G4 Task 6: the CURRENT result's column names, in source-column order
    /// — what `main.rs` maps saved/current view state through (name↔ix), so
    /// prefs survive a column reorder in the underlying table and silently
    /// drop a renamed/removed one (brief contract #4) rather than trusting a
    /// stale index.
    pub fn column_names(&self) -> Vec<String> {
        self.buffer
            .as_ref()
            .map(|b| b.borrow().schema().fields().iter().map(|f| f.name().to_string()).collect())
            .unwrap_or_default()
    }

    /// G4 Task 6: SOURCE column names with at least one ref-column currently
    /// checked in their ☰ menu — i.e. this PREVIEW tab's active fk-joins by
    /// name, exactly `TableViewPrefs::fk_joins`'s shape. Read by
    /// `main.rs::save_view_prefs_for_grid`; note this only ever has entries
    /// on a PREVIEW tab (`fk_checked` on an ad-hoc tab drives `RunLookup`/
    /// `virtual_cols` instead, never persisted).
    pub fn active_fk_join_names(&self) -> Vec<String> {
        let names = self.column_names();
        self.fk_checked
            .iter()
            .enumerate()
            .filter(|(_, set)| !set.is_empty())
            .filter_map(|(i, _)| names.get(i).cloned())
            .collect()
    }

    /// "Filtr" button click. Turning the row OFF also clears every active
    /// filter (see `filters_open`'s doc comment for why); turning it ON is
    /// just a visibility flip — no filters are active yet at that point
    /// unless the row is being re-shown after having been hidden without
    /// clearing (not currently reachable, but harmless either way).
    fn toggle_filters(&mut self, cx: &mut Context<Self>) {
        self.filters_open = !self.filters_open;
        if !self.filters_open && !self.view.filters.is_empty() {
            self.view.filters.clear();
            for t in &mut self.filter_cache {
                t.clear();
            }
            for input in self.filter_inputs.clone() {
                input.update(cx, |f, cx| f.set_text("", cx));
            }
            self.rebuild_view();
        }
        cx.notify();
    }

    /// Polled once per render (`toolbar`, called from `render`) while
    /// `filters_open`: compares each visible column's `TextField` text
    /// against `filter_cache`, and — on any difference — rebuilds
    /// `view.filters` from the non-empty entries and reruns `rebuild_view`.
    /// Same "compare live text to a cached last-value" trigger as
    /// `AppView::refresh_history_cache`'s polling, since `TextField` has no
    /// on-change hook to drive this from directly.
    fn poll_filters(&mut self, cx: &mut Context<Self>) {
        if !self.filters_open {
            return;
        }
        let mut changed = false;
        for i in 0..self.filter_inputs.len() {
            if self.hidden_cols.get(i).copied().unwrap_or(false) {
                continue;
            }
            let text = self.filter_inputs[i].read(cx).text();
            if text != self.filter_cache[i] {
                self.filter_cache[i] = text;
                changed = true;
            }
        }
        if changed {
            self.view.filters = self
                .filter_cache
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.is_empty())
                .map(|(i, t)| (i, t.clone()))
                .collect();
            self.rebuild_view();
        }
    }

    /// Ctrl+F. Re-focuses the existing find bar's input if it's already
    /// open (so a second Ctrl+F is a no-op rather than losing the current
    /// search), otherwise opens a fresh one — same "create on open, drop on
    /// close" shape as `AppView::on_open_palette`.
    fn on_find_in_result(&mut self, _: &FindInResult, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(f) = &self.find {
            window.focus(&f.input.focus_handle(cx), cx);
            return;
        }
        let input = cx.new(|cx| TextField::new(cx, "Hledat…", false));
        let focus = input.focus_handle(cx);
        self.find = Some(FindState {
            input,
            matches: Vec::new(),
            capped: false,
            current: None,
            last_query: String::new(),
            computed_generation: self.view_generation,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn on_find_next(&mut self, _: &FindNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.find_step(true, cx);
    }

    fn on_find_prev(&mut self, _: &FindPrev, _window: &mut Window, cx: &mut Context<Self>) {
        self.find_step(false, cx);
    }

    /// Moves `find.current` to the next/prev match (wrap-around, via
    /// `row_view::wrapped_index`) and scrolls it into view. A no-op when
    /// the find bar isn't open or has no matches.
    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(f) = &self.find else { return };
        let next = row_view::wrapped_index(f.current, f.matches.len(), forward);
        let Some(f) = &mut self.find else { return };
        f.current = next;
        if let Some(ix) = next {
            let (row, _col) = f.matches[ix];
            self.scroll_handle.scroll_to_item(row, ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// Polled once per render (`toolbar`) while `find.is_some()`: compares
    /// the find bar's live text (and `view_generation`, see
    /// `FindState::computed_generation`'s doc comment) against what
    /// `matches` was last computed from, and recomputes — over the CURRENT
    /// view's VISIBLE cells only (brief contract #2) — on any difference.
    /// Auto-selects (and scrolls to) the first match on every recompute,
    /// same "typing narrows/changes the result, jump to it" UX as a
    /// browser's find bar.
    fn poll_find(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.find else { return };
        let query = f.input.read(cx).text();
        if query == f.last_query && f.computed_generation == self.view_generation {
            return;
        }
        let ncols = self.hidden_cols.len();
        // G4 Task 5: visible SOURCE columns, same as before, PLUS every
        // virtual column — those have no `hidden_cols` entry (never
        // hideable, see `virtual_cols`' doc comment) so they're always
        // included, matching the brief's "find must see virtual columns".
        let mut visible_cols: Vec<usize> =
            (0..ncols).filter(|&c| !self.hidden_cols.get(c).copied().unwrap_or(false)).collect();
        visible_cols.extend(ncols..ncols + self.virtual_cols.len());
        let rows = self.view.len();
        let gen = self.view_generation;
        // Capped scan (Task 3 review issue 2): synchronous per-keystroke
        // work on the UI thread must be bounded for huge/spilled results.
        let (matches, capped) = if let Some(buf) = self.buffer.clone() {
            let view = &self.view;
            let virtual_cols = &self.virtual_cols;
            let mut buf = buf.borrow_mut();
            row_view::find_matches_capped(
                rows,
                &visible_cols,
                &query,
                FIND_MAX_ROWS,
                FIND_MAX_MATCHES,
                &mut |r, c| effective_text(ncols, virtual_cols, &mut buf, view.source_row(r), c),
            )
        } else {
            (Vec::new(), false)
        };
        let first = if matches.is_empty() { None } else { Some(0) };
        if let Some(row) = first.map(|ix| matches[ix].0) {
            self.scroll_handle.scroll_to_item(row, ScrollStrategy::Center);
        }
        if let Some(f) = &mut self.find {
            f.matches = matches;
            f.capped = capped;
            f.current = first;
            f.last_query = query;
            f.computed_generation = gen;
        }
    }

    /// G4 Task 3: called from `main.rs`'s `on_cancel_query` — the actual
    /// mechanism that makes Esc close the cell-detail popup / find bar (a
    /// scoped `"escape"` binding on `"ResultGrid"` would lose to the
    /// unscoped `escape → CancelQuery` global binding, same precedent as
    /// the palette's Esc documented on `AppView::on_cancel_query`). Closes
    /// at most one layer per call (cell detail first, since it visually
    /// sits on top), and reports whether it closed anything so the caller
    /// can stop there instead of also cancelling a running query.
    pub fn close_overlay_if_open(&mut self) -> bool {
        // G5 Task 3: the cell-editor overlay is the same kind of top-layer
        // popup as `cell_detail` — closes without staging anything (same as
        // its own "Zrušit" button).
        if self.cell_editor.is_some() {
            self.cell_editor = None;
            return true;
        }
        if self.cell_detail.is_some() {
            self.cell_detail = None;
            return true;
        }
        if self.find.is_some() {
            self.find = None;
            return true;
        }
        false
    }

    /// "Export ▾" button click — opens/closes the format menu; opening it
    /// closes "Sloupce ▾" first (the two menus never show at once, same
    /// "only one popover open" convention the connection dropdown/palette
    /// already follow implicitly by being singletons).
    fn toggle_export_menu(&mut self, cx: &mut Context<Self>) {
        self.export_open = !self.export_open;
        if self.export_open {
            self.columns_open = false;
            self.fk_menu_open = None;
        }
        cx.notify();
    }

    /// "Sloupce ▾" button click — see `toggle_export_menu`.
    fn toggle_columns_menu(&mut self, cx: &mut Context<Self>) {
        self.columns_open = !self.columns_open;
        if self.columns_open {
            self.export_open = false;
            self.fk_menu_open = None;
        }
        cx.notify();
    }

    /// "Sloupce ▾" checkbox click. Refuses to hide the LAST visible column
    /// (brief: "at least one column must stay visible") — a no-op rather
    /// than leaving `header`/the row list with nothing to render. Hiding a
    /// column ALSO clears its filter (a decision, not forced by the brief):
    /// once hidden, the filter row skips it entirely (see `filter_row`), so
    /// a stale filter would keep silently narrowing `view.filters` with no
    /// UI left to see or clear it — same "toggle-off-clears" rationale
    /// `toggle_filters` already applies to the whole filter row. Re-showing
    /// a column needs no special handling: hiding always cleared whatever
    /// filter it had, so there's nothing to restore.
    fn toggle_column_visibility(&mut self, col: usize, cx: &mut Context<Self>) {
        let Some(&was_hidden) = self.hidden_cols.get(col) else { return };
        if !was_hidden {
            let visible_count = self.hidden_cols.iter().filter(|&&h| !h).count();
            if visible_count <= 1 {
                return; // refuse to hide the last visible column
            }
        }
        self.hidden_cols[col] = !was_hidden;
        if self.hidden_cols[col] {
            if let Some(input) = self.filter_inputs.get(col).cloned() {
                input.update(cx, |f, cx| f.set_text("", cx));
            }
            if let Some(c) = self.filter_cache.get_mut(col) {
                c.clear();
            }
            let had_filter = self.view.filters.iter().any(|(c, _)| *c == col);
            self.view.filters.retain(|(c, _)| *c != col);
            if had_filter {
                self.rebuild_view();
            }
        }
        // Review fix (Task 4 round 1, Issue 2): bump `view_generation`
        // UNCONDITIONALLY on every visibility flip, not only when a filter
        // was also cleared above. `poll_find` only recomputes `find.matches`
        // when `view_generation` changed (or the query text did) — without
        // this, hiding a column that has no active filter (the common case)
        // left stale matches pointing at a column the row-rendering loop no
        // longer draws, overcounting the "i z n" indicator and letting
        // next/prev jump to a row with no visible highlight. A plain bump
        // (rather than a full `rebuild_view()`) is deliberate: visibility
        // never changes row order/count, so re-deriving `view.order` would
        // be pure waste and would incorrectly flash the "řadím…" status note
        // for a large result on a mere show/hide click.
        self.view_generation += 1;
        // G4 Task 6: persist the new visibility for a PREVIEW tab — same
        // guard as `on_header_click`.
        if self.is_preview {
            cx.emit(GridEvent::ViewChanged);
        }
        cx.notify();
    }

    /// Visible-only headers (display order == source order; hiding doesn't
    /// reorder columns, only sort does that at the ROW level) plus the
    /// SOURCE column index each one came from — used both by the header/row
    /// renderers (already inline) and by `start_export`'s accessor.
    fn export_headers_and_cols(&self) -> (Vec<String>, Vec<usize>) {
        let Some(buf) = &self.buffer else { return (Vec::new(), Vec::new()) };
        let buf = buf.borrow();
        let mut headers = Vec::new();
        let mut cols = Vec::new();
        for (i, field) in buf.schema().fields().iter().enumerate() {
            if self.hidden_cols.get(i).copied().unwrap_or(false) {
                continue;
            }
            headers.push(field.name().clone());
            cols.push(i);
        }
        // G4 Task 5: virtual columns are never hideable — always included
        // in exports, same "find/copy see them too" treatment.
        let ncols = buf.column_count();
        for (vi, vcol) in self.virtual_cols.iter().enumerate() {
            headers.push(vcol.name.clone());
            cols.push(ncols + vi);
        }
        (headers, cols)
    }

    /// "Export ▾" format click. Exports the CURRENT VIEW: rows in display
    /// order (`view.source_row`), hidden columns excluded (brief contract).
    ///
    /// Review fix (Task 4 round 1, Issue 1/4): restructured from the
    /// original "snapshot everything synchronously, THEN show the dialog"
    /// shape into dialog-first, chunked-snapshot-second:
    ///
    /// 1. The save-destination dialog is awaited FIRST, before any row data
    ///    is touched — a cancel now costs nothing instead of throwing away a
    ///    full synchronous read.
    /// 2. Once a destination is resolved, `status_note` is set to
    ///    "exportuji…" and `cx.notify()`d so it actually PAINTS before the
    ///    heavy work starts (previously the note was set only after the
    ///    snapshot had already completed).
    /// 3. The row/column snapshot into owned `Option<String>` data then runs
    ///    in `EXPORT_SNAPSHOT_CHUNK_ROWS`-sized chunks, each a separate
    ///    `this.update` on the FOREGROUND executor (so it can safely touch
    ///    `self.view`/`self.buffer`), with a real (non-zero-duration —
    ///    `BackgroundExecutor::timer` short-circuits `Duration::ZERO` to an
    ///    already-`Ready` task that never actually yields control back to
    ///    the platform run loop, so it wouldn't let the window repaint)
    ///    timer await between chunks. This keeps the window responsive
    ///    (repaint, input, even closing the tab) for the entire snapshot,
    ///    including a full spilled 500k-row export, instead of one
    ///    unbounded synchronous UI-thread pass.
    /// 4. Each chunk re-checks that this grid entity and its buffer are
    ///    still the ones we started with (`Rc::ptr_eq` + `view_generation`)
    ///    — if the tab was closed or a new query/sort/filter/visibility
    ///    change replaced the view mid-export, the export aborts with a
    ///    status note instead of silently mixing rows from two different
    ///    views or writing into a torn-down entity.
    /// 5. Formatting+write is unchanged: still offloaded to
    ///    `cx.background_executor()` above `LARGE_EXPORT_ROWS` (the
    ///    completed `rows_data` — plain `String`s — IS `Send`, unlike
    ///    `ResultBuffer` itself which is `Rc<RefCell<_>>`), inline
    ///    otherwise, fed by the now-completed snapshot.
    fn start_export(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let Some(buf) = self.buffer.clone() else { return };
        let (headers, cols) = self.export_headers_and_cols();
        if headers.is_empty() {
            self.status_note = Some("error: žádné viditelné sloupce k exportu".to_string());
            cx.notify();
            return;
        }
        let table_name = self.table_name.clone();
        let ext = format.extension();
        let suggested_name = format!("{table_name}.{ext}");
        let n = self.view.len();
        let view_generation = self.view_generation;

        // Fix 1a: resolve the destination FIRST — cancelling the dialog now
        // costs nothing since no snapshot work has happened yet.
        self.status_note = Some("volím cíl exportu…".to_string());
        cx.notify();
        let dialog = cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name));
        cx.spawn(async move |this, cx| {
            // Fix 3 (Issue 3): keep a genuine dialog error's text separate
            // from a plain cancelled/unavailable dialog so it isn't silently
            // discarded — surfaced in the final status note if the Downloads
            // fallback is used.
            let mut dialog_error: Option<String> = None;
            let path = match dialog.await {
                Ok(Ok(Some(p))) => p,
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |g, cx| {
                        g.status_note = Some("export zrušen".to_string());
                        cx.notify();
                    });
                    return;
                }
                // A genuine platform error from the dialog itself (invalid
                // initial directory, permission issue, ...) — fall back to
                // Downloads like the "unavailable" case below, but keep the
                // error text to surface once the export finishes.
                Ok(Err(e)) => {
                    dialog_error = Some(e.to_string());
                    match dirs::download_dir() {
                        Some(dir) => dir.join(format!("dbc-export-{}.{ext}", export_timestamp())),
                        None => {
                            let _ = this.update(cx, |g, cx| {
                                g.status_note = Some(format!(
                                    "error: dialog pro uložení selhal ({e}) a složka Stažené není dostupná"
                                ));
                                cx.notify();
                            });
                            return;
                        }
                    }
                }
                // Dropped/cancelled oneshot channel — dialog unavailable
                // (brief's documented fallback for a platform picker that
                // isn't usable) — write a timestamped file into the user's
                // Downloads instead.
                Err(_canceled) => match dirs::download_dir() {
                    Some(dir) => dir.join(format!("dbc-export-{}.{ext}", export_timestamp())),
                    None => {
                        let _ = this.update(cx, |g, cx| {
                            g.status_note =
                                Some("error: dialog pro uložení i složka Stažené nejsou dostupné".to_string());
                            cx.notify();
                        });
                        return;
                    }
                },
            };

            // Fix 1b: paint "exporting…" BEFORE the heavy snapshot work
            // starts, not after (the previous version set this note *after*
            // the synchronous snapshot loop had already completed).
            if this
                .update(cx, |g, cx| {
                    g.status_note = Some(format!("exportuji… ({n} řádků)"));
                    cx.notify();
                })
                .is_err()
            {
                return; // grid entity gone already
            }

            // Fix 1c: snapshot in chunks, yielding between them so the UI
            // thread keeps painting/handling input.
            let mut rows_data: Vec<Vec<Option<String>>> = Vec::with_capacity(n);
            let mut row = 0usize;
            while row < n {
                let chunk_end = (row + EXPORT_SNAPSHOT_CHUNK_ROWS).min(n);
                let chunk = this.update(cx, |g, _cx| {
                    // Fix 1d: abort if the buffer/view changed out from
                    // under us mid-export (tab closed and entity gone is
                    // handled by `this.update`'s own `Err` below; this
                    // covers "entity alive but pointing at a different
                    // result now" — new query, or a sort/filter/visibility
                    // change, since all of those bump `view_generation`).
                    let current_buf = g.buffer.clone()?;
                    if !Rc::ptr_eq(&current_buf, &buf) || g.view_generation != view_generation {
                        return None;
                    }
                    // G4 Task 5: `cols` (from `export_headers_and_cols`) may
                    // include virtual-column indices (`>= ncols`) — route
                    // every read through `effective_is_null`/`effective_text`
                    // so those export the looked-up value (and a real NULL
                    // when unmatched) instead of panicking/reading garbage
                    // past the real buffer's column count.
                    let ncols = g.buffer.as_ref().map_or(0, |b| b.borrow().column_count());
                    let virtual_cols = &g.virtual_cols;
                    let mut buf_mut = buf.borrow_mut();
                    let mut chunk_rows = Vec::with_capacity(chunk_end - row);
                    for r in row..chunk_end {
                        let source_row = g.view.source_row(r);
                        let mut vals = Vec::with_capacity(cols.len());
                        for &c in &cols {
                            let val = if effective_is_null(ncols, virtual_cols, &mut buf_mut, source_row, c) {
                                None
                            } else {
                                Some(effective_text(ncols, virtual_cols, &mut buf_mut, source_row, c))
                            };
                            vals.push(val);
                        }
                        chunk_rows.push(vals);
                    }
                    Some(chunk_rows)
                });
                match chunk {
                    Ok(Some(mut chunk_rows)) => rows_data.append(&mut chunk_rows),
                    Ok(None) => {
                        let _ = this.update(cx, |g, cx| {
                            g.status_note =
                                Some("export přerušen: data se mezitím změnila".to_string());
                            cx.notify();
                        });
                        return;
                    }
                    Err(_) => return, // grid entity dropped mid-export
                }
                row = chunk_end;
                if row < n {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1))
                        .await;
                }
            }

            let large = n > LARGE_EXPORT_ROWS;
            let result: Result<(), String> = if large {
                let write_path = path.clone();
                cx.background_executor()
                    .spawn(async move {
                        write_export_file(&write_path, format, &headers, &table_name, n, &rows_data)
                    })
                    .await
            } else {
                write_export_file(&path, format, &headers, &table_name, n, &rows_data)
            };

            let _ = this.update(cx, |g, cx| {
                g.status_note = Some(match result {
                    Ok(()) => match &dialog_error {
                        Some(err) => format!(
                            "exportováno (dialog pro uložení selhal: {err}; použita složka Stažené): {} ({n} řádků)",
                            path.display()
                        ),
                        None => format!("exportováno: {} ({n} řádků)", path.display()),
                    },
                    Err(e) => format!("error: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(((r0, c0), (r1, c1))) = self.selection else {
            return false;
        };
        let (rmin, rmax) = (r0.min(r1), r0.max(r1));
        let (cmin, cmax) = (c0.min(c1), c0.max(c1));
        row >= rmin && row <= rmax && col >= cmin && col <= cmax
    }

    /// `self.selection` is in DISPLAY coordinates (row = position in
    /// `view`, same as `is_selected`/the click handler below) — each row is
    /// mapped through `view.source_row` before reading it from the buffer,
    /// otherwise a sorted/filtered grid would copy the wrong rows (G4 Task
    /// 2 fix). Hidden columns within the selected column range are skipped
    /// rather than copied blank.
    fn on_copy(&mut self, _: &CopySelection, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(((r0, c0), (r1, c1))) = self.selection else {
            return;
        };
        let (rmin, rmax) = (r0.min(r1), r0.max(r1));
        let (cmin, cmax) = (c0.min(c1), c0.max(c1));
        // Defensive clamp (final review fix #1): `rebuild_view` already
        // drops a selection it detects has fallen out of range, but a
        // click can still land after the last rebuild and before this
        // handler runs (the click-then-Ctrl+C race) — bail rather than
        // let `view.source_row` panic on an out-of-range display index.
        if rmax >= self.view.len() {
            return;
        }
        let Some(buf) = self.buffer.clone() else {
            return;
        };
        let ncols = buf.borrow().column_count();
        let virtual_cols = &self.virtual_cols;
        let mut buf = buf.borrow_mut();
        let mut out = String::new();
        for r in rmin..=rmax {
            if r > rmin {
                out.push('\n');
            }
            let source_row = self.view.source_row(r);
            let mut first_col = true;
            for c in cmin..=cmax {
                // G4 Task 5: hidden_cols only covers SOURCE columns
                // (`c < ncols`) — virtual columns (`c >= ncols`) are never
                // hideable, so they're always copied, same as `find`.
                if c < ncols && self.hidden_cols.get(c).copied().unwrap_or(false) {
                    continue;
                }
                if !first_col {
                    out.push('\t');
                }
                first_col = false;
                out.push_str(&effective_text(ncols, virtual_cols, &mut buf, source_row, c));
            }
        }
        cx.write_to_clipboard(ClipboardItem::new_string(out));
    }

    /// G4 Task 3: the toolbar row above the header (brief contract #1) —
    /// polls filters/find (see `poll_filters`/`poll_find`'s doc comments)
    /// before laying out, since either poll can change `view.filters` and
    /// hence `view.len()`, which the "{shown} / {total}" status reads.
    /// Only ever called when `self.buffer.is_some()` (see `render`).
    fn toolbar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.poll_filters(cx);
        self.poll_find(cx);

        let theme = *cx.theme();
        let shown = self.view.len();
        let total = self.buffer.as_ref().map_or(0, |b| b.borrow().row_count());
        let filtered = !self.view.filters.is_empty();
        let filters_open = self.filters_open;

        let mut row = div()
            .id("grid-toolbar")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(ROW_HEIGHT))
            .px_2()
            .bg(theme.bg_app)
            .text_color(theme.text_primary)
            .child(
                div()
                    .id("toggle-filters")
                    .cursor_pointer()
                    .px_2()
                    .rounded_md()
                    .bg(if filters_open { theme.bg_selected } else { theme.bg_hover })
                    .child("Filtr")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_filters(cx);
                    })),
            )
            .child(
                div()
                    .id("toggle-export-menu")
                    .cursor_pointer()
                    .px_2()
                    .rounded_md()
                    .bg(if self.export_open { theme.bg_selected } else { theme.bg_hover })
                    .child("Export ▾")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_export_menu(cx);
                    })),
            )
            .child(
                div()
                    .id("toggle-columns-menu")
                    .cursor_pointer()
                    .px_2()
                    .rounded_md()
                    .bg(if self.columns_open { theme.bg_selected } else { theme.bg_hover })
                    .child("Sloupce ▾")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_columns_menu(cx);
                    })),
            );

        // G5 Task 3, brief contract #4: "+ řádek" only on editable tabs.
        if self.editable.is_some() {
            row = row.child(
                div()
                    .id("add-insert-row")
                    .cursor_pointer()
                    .px_2()
                    .rounded_md()
                    .bg(theme.bg_hover)
                    .child("+ řádek")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_insert_row(cx);
                    })),
            );
        }

        if filtered {
            row = row.child(format!("{shown} / {total} řádků"));
        }

        row = row.child(div().flex_1()); // spacer, pushes the find bar to the right

        if let Some(f) = &self.find {
            let count_label = match f.current {
                Some(ix) => format!(
                    "{} z {}{}",
                    ix + 1,
                    f.matches.len(),
                    if f.capped { "+" } else { "" }
                ),
                None => format!("0 z {}", f.matches.len()),
            };
            row = row
                .child(div().w(px(180.)).child(f.input.clone()))
                .child(count_label)
                .child(
                    div()
                        .id("find-prev")
                        .cursor_pointer()
                        .px_1()
                        .child("◀")
                        .on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx))),
                )
                .child(
                    div()
                        .id("find-next")
                        .cursor_pointer()
                        .px_1()
                        .child("▶")
                        .on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx))),
                )
                .child(
                    div()
                        .id("find-close")
                        .cursor_pointer()
                        .px_1()
                        .child("✕")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.find = None;
                            cx.notify();
                        })),
                );
        }

        row.into_any_element()
    }

    /// Filter row (brief contract #1/#2): one `TextField` per VISIBLE
    /// column, same width as its header/data cells so the inputs line up.
    /// Hidden columns are skipped entirely (brief: "hidden columns keep no
    /// filter input") even though `filter_inputs`/`filter_cache` still hold
    /// an entry for them.
    fn filter_row(&self, theme: &crate::theme::Theme) -> impl IntoElement {
        let mut row = div().flex().flex_row().bg(theme.bg_app);
        // G5 Task 3: same gutter-width alignment spacer as `header`.
        if self.editable.is_some() {
            row = row.child(div().w(px(GUTTER_WIDTH)).h(px(ROW_HEIGHT)));
        }
        if self.buffer.is_some() {
            // G4 Task 5: `filter_inputs`/`filter_cache` are kept sized to
            // the EFFECTIVE column count by `sync_virtual_aux` — iterating
            // the whole thing (rather than just `buf.column_count()`)
            // naturally picks up virtual-column filter inputs too. Hidden
            // only applies to the source range (`hidden_cols.get(i)` is
            // `None`, i.e. "not hidden", past it).
            for i in 0..self.filter_inputs.len() {
                if self.hidden_cols.get(i).copied().unwrap_or(false) {
                    continue;
                }
                if let Some(input) = self.filter_inputs.get(i) {
                    row = row.child(
                        div()
                            .w(px(self.col_widths.get(i).copied().unwrap_or(DEFAULT_COL_WIDTH)))
                            .h(px(ROW_HEIGHT))
                            .px_1()
                            .child(input.clone()),
                    );
                }
            }
        }
        row
    }

    /// "Export ▾" dropdown (brief contract): a flat list of the four
    /// formats, anchored under the toolbar. Same overlay shape as
    /// `connections_ui::render_dropdown_overlay` — `.occlude()` +
    /// `on_mouse_down_out` closes it on an outside click, positioned
    /// `.absolute()` under the (`.relative()`) root `render` builds.
    fn render_export_menu_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let mut panel = div()
            .id("export-menu")
            .absolute()
            .top(px(ROW_HEIGHT))
            .left(px(70.))
            .w(px(140.))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .text_color(theme.text_primary)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.export_open = false;
                cx.notify();
            }));
        for (label, format) in [
            ("CSV", ExportFormat::Csv),
            ("TSV", ExportFormat::Tsv),
            ("JSON", ExportFormat::Json),
            ("INSERT", ExportFormat::Insert),
        ] {
            panel = panel.child(
                div()
                    .id(gpui::SharedString::from(format!("export-item-{label}")))
                    .cursor_pointer()
                    .px_2()
                    .rounded_md()
                    .hover(|s| s.bg(theme.bg_hover))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.export_open = false;
                        this.start_export(format, cx);
                    })),
            );
        }
        panel.into_any_element()
    }

    /// "Sloupce ▾" dropdown (brief contract): a checkbox per SOURCE column
    /// (checked = visible), toggling `hidden_cols` via
    /// `toggle_column_visibility`. Iterates ALL source columns, not just
    /// visible ones (unlike `filter_row`/`header`) — this is the one place
    /// a hidden column is ever shown again, so it can be re-checked.
    fn render_columns_menu_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let mut panel = div()
            .id("columns-menu")
            .absolute()
            .top(px(ROW_HEIGHT))
            .left(px(190.))
            .w(px(220.))
            .max_h(px(320.))
            .overflow_hidden()
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .text_color(theme.text_primary)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.columns_open = false;
                cx.notify();
            }));
        if let Some(buf) = &self.buffer {
            let ncols = buf.borrow().column_count();
            let visible_count = self.hidden_cols.iter().filter(|&&h| !h).count();
            for i in 0..ncols {
                let name = buf.borrow().schema().fields()[i].name().clone();
                let hidden = self.hidden_cols.get(i).copied().unwrap_or(false);
                // Disabled (can't uncheck) when this is the LAST visible
                // column — see `toggle_column_visibility`'s doc comment.
                let disabled = !hidden && visible_count <= 1;
                panel = panel.child(
                    div()
                        .id(("columns-item", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .rounded_md()
                        .when(!disabled, |d| d.cursor_pointer().hover(|s| s.bg(theme.bg_hover)))
                        .text_color(if disabled { theme.text_disabled } else { theme.text_primary })
                        .child(if hidden { "☐" } else { "☑" })
                        .child(name)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_column_visibility(i, cx);
                        })),
                );
            }
        }
        panel.into_any_element()
    }

    /// Cell-detail popup (brief contract #3): same centered-overlay shape
    /// as `connections_ui::render_modal_overlay`/`AppView::render_palette_overlay`
    /// (`.occlude()` blocks clicks reaching the grid underneath), owned
    /// on `ResultGrid` rather than `AppView::modal` since it's tied to this
    /// specific tab's grid (see `cell_detail`'s doc comment). Text is
    /// rendered line-by-line with wheel-driven `scroll_lines`, exactly like
    /// `main.rs`'s `TabContent::Text` body — reused rather than reinvented.
    fn render_cell_detail_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let detail = self.cell_detail.clone()?;
        let text_for_copy = detail.text.clone();
        let lines: Vec<&str> = detail.text.lines().collect();
        let scroll = detail.scroll_lines.min(lines.len());

        let mut body = div()
            .id("cell-detail-body")
            .font_family("Consolas")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .p_2()
            .text_color(theme.text_primary)
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                let delta_lines = match e.delta {
                    ScrollDelta::Lines(p) => p.y,
                    ScrollDelta::Pixels(p) => p.y.as_f32() / 20.0,
                };
                if let Some(cd) = &mut this.cell_detail {
                    let max_scroll = cd.text.lines().count().saturating_sub(1);
                    let current = cd.scroll_lines as f32;
                    let new_scroll = (current - delta_lines).round();
                    cd.scroll_lines = new_scroll.max(0.0).min(max_scroll as f32) as usize;
                }
                cx.notify();
            }));
        for line in &lines[scroll..] {
            body = body.child(div().whitespace_normal().child(line.to_string()));
        }

        let panel = div()
            .id("cell-detail-panel")
            .w(px(560.))
            .max_h(px(420.))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .flex()
            .flex_col()
            .child(body)
            .child(
                div().flex().flex_row().justify_end().gap_2().p_2().child(
                    div()
                        .id("cell-detail-copy")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .text_color(theme.text_primary)
                        .px_2()
                        .rounded_md()
                        .child("Kopírovat")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text_for_copy.clone()));
                        })),
                ).child(
                    div()
                        .id("cell-detail-close")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .text_color(theme.text_primary)
                        .px_2()
                        .rounded_md()
                        .child("Zavřít")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cell_detail = None;
                            cx.notify();
                        })),
                ),
            );

        Some(
            div()
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
                .child(panel)
                .into_any_element(),
        )
    }

    /// G5 Task 3: "✕" gutter click on a real row — toggles that row's
    /// delete flag, keyed by SOURCE row (brief contract #6: staged/deleted
    /// state survives a sort/filter). The gutter cell's own `on_click`
    /// calls `cx.stop_propagation()` first (brief contract #4) so this
    /// never also lands on the row's selection click handler underneath.
    fn toggle_row_delete(&mut self, source_row: usize, cx: &mut Context<Self>) {
        self.edit_state.toggle_delete(source_row);
        cx.notify();
    }

    /// G5 Task 3: "␡" gutter click on an inserted row — removes it
    /// entirely (`EditState::remove_insert_row`). Unlike a real row's
    /// delete (a reversible flag — the row still exists in the table until
    /// Apply runs), an insert row has no underlying identity to preserve,
    /// so removing it here is the only way to un-stage it.
    fn remove_insert_row(&mut self, ins_ix: usize, cx: &mut Context<Self>) {
        self.edit_state.remove_insert_row(ins_ix);
        cx.notify();
    }

    /// "+ řádek" toolbar click (editable tabs only, brief contract #4) —
    /// appends one blank insert row sized to the CURRENT result's column
    /// count (every column starts untouched, i.e. "(výchozí)" — see
    /// `sandbox::insert_cell_display`).
    fn add_insert_row(&mut self, cx: &mut Context<Self>) {
        let ncols = self.buffer.as_ref().map_or(0, |b| b.borrow().column_count());
        self.edit_state.add_insert_row(ncols);
        cx.notify();
    }

    /// G5 Task 4: "Zahodit" on the apply bar, and the terminal step of a
    /// successful Apply (`main.rs::on_confirm_apply`) — drops every staged
    /// cell/delete/insert. Deliberately does NOT touch `cell_editor`/
    /// `find`/`cell_detail` or any other overlay state; a caller that also
    /// wants those closed (none currently do — "Zahodit" only appears on the
    /// apply bar, which isn't reachable while an overlay is open since
    /// double-clicking a cell for the editor and clicking "Zahodit" in the
    /// toolbar-adjacent bar are mutually exclusive click targets) can close
    /// them separately.
    pub fn clear_edits(&mut self, cx: &mut Context<Self>) {
        self.edit_state.clear();
        cx.notify();
    }

    /// G5 Task 3: opens the cell-editor overlay (brief contract #2) for
    /// `target`. `column_name`/`original_text` are snapshotted once at open
    /// time (same "capture at click time" convention `cell_detail` already
    /// follows) — `original_text` is the ORIGINAL committed value (empty
    /// string for a real NULL cell, same convention `ResultBuffer::cell_text`
    /// uses), shown for reference alongside the editable field so staging
    /// over a value doesn't lose sight of what it used to be (this is what
    /// lets the editor also cover the old cell-detail "see the full text"
    /// use case). The `TextField` itself is prefilled with the CURRENT
    /// display value: this cell's staged text if it's already staged (a
    /// staged NULL prefills empty — re-clicking "NULL" re-stages it), else
    /// `original_text`/the insert row's untouched default (also empty).
    fn open_cell_editor(
        &mut self,
        target: EditTarget,
        column_name: String,
        original_text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = match target {
            EditTarget::Cell { source_row, col } => self
                .edit_state
                .cells
                .get(&(source_row, col))
                .map(|staged| staged.clone().unwrap_or_default())
                .unwrap_or_else(|| original_text.clone()),
            EditTarget::Insert { ins_ix, col } => self
                .edit_state
                .inserted_rows
                .get(ins_ix)
                .and_then(|row| row.get(col))
                .cloned()
                .flatten()
                .flatten()
                .unwrap_or_default(),
        };
        // G5 Task 4 (folded T3 review issue 4): see `CellEditor::
        // currently_staged_null`'s doc comment.
        let currently_staged_null = match target {
            EditTarget::Cell { source_row, col } => {
                matches!(self.edit_state.cells.get(&(source_row, col)), Some(None))
            }
            EditTarget::Insert { ins_ix, col } => matches!(
                self.edit_state.inserted_rows.get(ins_ix).and_then(|row| row.get(col)),
                Some(Some(None))
            ),
        };
        let input = cx.new(|cx| TextField::new(cx, "hodnota…", false));
        input.update(cx, |f, cx| f.set_text(&prefill, cx));
        let focus = input.focus_handle(cx);
        self.cell_editor =
            Some(CellEditor { target, column_name, original_text, currently_staged_null, input });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Routes a staged value to the right `EditState` method for `target` —
    /// shared by "Uložit" (`Some(text)`) and "NULL" (`None`).
    fn stage_from_editor(&mut self, target: EditTarget, v: Option<String>) {
        match target {
            EditTarget::Cell { source_row, col } => self.edit_state.stage_cell(source_row, col, v),
            EditTarget::Insert { ins_ix, col } => self.edit_state.stage_insert_cell(ins_ix, col, v),
        }
    }

    /// "Uložit" click — stages the editor's live `TextField` text (brief
    /// contract #2), then closes the overlay.
    fn commit_cell_editor(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = &self.cell_editor else { return };
        let text = ed.input.read(cx).text();
        let target = ed.target;
        self.stage_from_editor(target, Some(text));
        self.cell_editor = None;
        cx.notify();
    }

    /// "NULL" click — stages a SQL NULL for the editor's target, then
    /// closes the overlay.
    fn commit_cell_editor_null(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = &self.cell_editor else { return };
        let target = ed.target;
        self.stage_from_editor(target, None);
        self.cell_editor = None;
        cx.notify();
    }

    /// G5 Task 3: cell-editor overlay (brief contract #2) — same centered-
    /// modal shape as `render_cell_detail_overlay`, but with an editable
    /// `TextField` plus Uložit/NULL/Zrušit instead of a read-only scrolled
    /// body + Kopírovat/Zavřít. The original value is still shown (a plain
    /// wrapped text block, not scrolled — v1, per the brief's "centered
    /// modal is acceptable") so this covers the old cell-detail "see the
    /// full text" use case too.
    fn render_cell_editor_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let ed = self.cell_editor.as_ref()?;
        let column_name = ed.column_name.clone();
        let original_text = ed.original_text.clone();
        let currently_staged_null = ed.currently_staged_null;
        let input = ed.input.clone();

        let panel = div()
            .id("cell-editor-panel")
            .w(px(480.))
            .max_h(px(360.))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .text_color(theme.text_primary)
            .child(div().text_color(theme.warn).child(column_name))
            .child(
                div()
                    .id("cell-editor-original")
                    .max_h(px(120.))
                    .overflow_hidden()
                    .p_1()
                    .bg(theme.bg_app)
                    .rounded_md()
                    .text_color(theme.text_muted)
                    .whitespace_normal()
                    .child(if original_text.is_empty() {
                        "(prázdné/NULL)".to_string()
                    } else {
                        original_text
                    }),
            )
            // G5 Task 4 (folded T3 review issue 4): only shown when the
            // CURRENT staged state (as opposed to the original block above)
            // is an explicit NULL — see `CellEditor::currently_staged_null`.
            .when(currently_staged_null, |d| {
                d.child(
                    div()
                        .text_color(theme.danger)
                        .child("aktuálně: (NULL) — prázdné Uložit přepíše na '', ne NULL"),
                )
            })
            .child(div().w_full().child(input))
            .child(
                div().flex().flex_row().justify_end().gap_2().child(
                    div()
                        .id("cell-editor-save")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .text_color(theme.text_primary)
                        .px_2()
                        .rounded_md()
                        .child("Uložit")
                        .on_click(cx.listener(|this, _, _, cx| this.commit_cell_editor(cx))),
                ).child(
                    div()
                        .id("cell-editor-null")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .text_color(theme.text_primary)
                        .px_2()
                        .rounded_md()
                        .child("NULL")
                        .on_click(cx.listener(|this, _, _, cx| this.commit_cell_editor_null(cx))),
                ).child(
                    div()
                        .id("cell-editor-cancel")
                        .cursor_pointer()
                        .bg(theme.bg_hover)
                        .text_color(theme.text_primary)
                        .px_2()
                        .rounded_md()
                        .child("Zrušit")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.cell_editor = None;
                            cx.notify();
                        })),
                ),
            );

        Some(
            div()
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
                .child(panel)
                .into_any_element(),
        )
    }

    /// Skips hidden source columns entirely (brief: "header + cells +
    /// widths skip hidden"). The label area (everything except the 5px
    /// resize-handle strip) is clickable and cycles that column's sort
    /// none → asc → desc → none via `on_header_click`, showing a ▲/▼
    /// indicator next to the name when it's the active sort column — the
    /// resize handle is a separate sibling element so a resize drag never
    /// also fires a sort toggle.
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let mut row = div().flex().flex_row().bg(theme.bg_hover).text_color(theme.warn);
        // G5 Task 3: blank gutter-width spacer so the header stays aligned
        // with each row's own leading "✕"/"␡" gutter cell (brief contract
        // #4) — editable tabs only.
        if self.editable.is_some() {
            row = row.child(div().w(px(GUTTER_WIDTH)).h(px(ROW_HEIGHT)));
        }
        let Some(buf) = &self.buffer else { return row };
        let buf = buf.borrow();
        let ncols = buf.column_count();
        for (i, field) in buf.schema().fields().iter().enumerate() {
            if self.hidden_cols.get(i).copied().unwrap_or(false) {
                continue;
            }
            let mut label = field.name().clone();
            match self.view.sort {
                Some((c, true)) if c == i => label.push_str(" \u{25B2}"), // ▲
                Some((c, false)) if c == i => label.push_str(" \u{25BC}"), // ▼
                _ => {}
            }
            // G4 Task 5: joined columns (preview re-run output) render
            // tinted (brief: bg 0x2a2a3d).
            let joined = self.joined_cols.get(i).copied().unwrap_or(false);
            let bg = if joined { theme.bg_joined_col } else { theme.bg_hover };
            // ☰ only on columns the ☰-menu can actually do something with —
            // `fk_info[i].is_some()`.
            let has_fk = matches!(self.fk_info.get(i), Some(Some(_)));
            // Resize handle overlays the last 5px of the column (absolute,
            // anchored to the right edge) instead of adding extra width
            // after the cell — that would push header columns out of
            // alignment with the (handle-less) data columns below as the
            // column index grows.
            let mut cell = div()
                .relative()
                .w(px(self.col_widths[i]))
                .h(px(ROW_HEIGHT))
                .bg(bg)
                .child(
                    div()
                        .id(("header-label", i))
                        .px_2()
                        .h(px(ROW_HEIGHT))
                        .overflow_hidden()
                        .cursor_pointer()
                        .child(label)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.on_header_click(i, cx);
                        })),
                )
                .child(
                    div()
                        .id(("resize", i))
                        .absolute()
                        .top_0()
                        .right_0()
                        .w(px(5.))
                        .h(px(ROW_HEIGHT))
                        // Blocks the label's hitbox underneath —
                        // without this a resize drag ALSO fires the
                        // label's on_click and toggles the sort
                        // (Task 2 review issue 1).
                        .occlude()
                        .cursor_col_resize()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, e: &gpui::MouseDownEvent, _w, cx| {
                                this.resizing =
                                    Some((i, f32::from(e.position.x), this.col_widths[i]));
                                cx.notify();
                            }),
                        ),
                );
            if has_fk {
                cell = cell.child(
                    div()
                        .id(("fk-menu-btn", i))
                        .absolute()
                        .top_0()
                        .right(px(7.))
                        .w(px(12.))
                        .h(px(ROW_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        // Blocks the label's hitbox underneath, same
                        // reasoning as the resize handle above — otherwise
                        // clicking ☰ would ALSO toggle the column's sort.
                        .occlude()
                        .child("☰")
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.toggle_fk_menu(i, cx);
                        })),
                );
            }
            row = row.child(cell);
        }
        // G4 Task 5: virtual columns render AFTER every source column
        // (brief), always tinted (there's no "un-joined" virtual column),
        // sortable/clickable exactly like a source column via the same
        // effective column index (`ncols + vi`) — `on_header_click`/
        // `rebuild_view` don't distinguish source vs. virtual at all.
        for (vi, vcol) in self.virtual_cols.iter().enumerate() {
            let col_ix = ncols + vi;
            let mut label = vcol.name.clone();
            match self.view.sort {
                Some((c, true)) if c == col_ix => label.push_str(" \u{25B2}"),
                Some((c, false)) if c == col_ix => label.push_str(" \u{25BC}"),
                _ => {}
            }
            row = row.child(
                div()
                    .w(px(self.col_widths.get(col_ix).copied().unwrap_or(DEFAULT_COL_WIDTH)))
                    .h(px(ROW_HEIGHT))
                    .bg(theme.bg_joined_col)
                    .child(
                        div()
                            .id(("header-label-v", vi))
                            .px_2()
                            .h(px(ROW_HEIGHT))
                            .overflow_hidden()
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.on_header_click(col_ix, cx);
                            })),
                    ),
            );
        }
        row
    }

    /// ☰-menu dropdown (brief contract #1): "Přidat sloupce z {ref_table}"
    /// listing the referenced table's columns (`fk_ref_columns`, from the
    /// schema snapshot `main.rs` already has — see `set_fk_info`'s doc
    /// comment) with checkboxes reflecting `fk_checked[col]`. Same overlay
    /// shape/`.occlude()`+`on_mouse_down_out` convention as
    /// `render_export_menu_overlay`/`render_columns_menu_overlay`, anchored
    /// under its own column via `fk_menu_left_offset` rather than a fixed
    /// toolbar position (this menu's trigger can be anywhere in a wide,
    /// scrolled header, unlike Export/Sloupce which live in the fixed
    /// toolbar).
    fn render_fk_menu_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let col = self.fk_menu_open?;
        let fk = self.fk_info.get(col)?.clone()?;
        let ref_cols = self.fk_ref_columns.get(col).cloned().flatten().unwrap_or_default();
        let checked = self.fk_checked.get(col).cloned().unwrap_or_default();
        let left = self.fk_menu_left_offset(col);
        let top = if self.filters_open { ROW_HEIGHT * 2.0 } else { ROW_HEIGHT };
        let mut panel = div()
            .id("fk-menu")
            .absolute()
            .top(px(top))
            .left(px(left))
            .w(px(220.))
            .max_h(px(320.))
            .overflow_hidden()
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .text_color(theme.text_primary)
            .occlude()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.fk_menu_open = None;
                cx.notify();
            }))
            .child(
                div()
                    .px_2()
                    .text_color(theme.text_muted)
                    .child(format!("Přidat sloupce z {}", fk.table)),
            );
        for c in ref_cols {
            let is_checked = checked.contains(&c);
            let c_for_click = c.clone();
            panel = panel.child(
                div()
                    .id(gpui::SharedString::from(format!("fk-item-{col}-{c}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_hover))
                    .child(if is_checked { "☑" } else { "☐" })
                    .child(c)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_fk_column(col, c_for_click.clone(), cx);
                    })),
            );
        }
        Some(panel.into_any_element())
    }
}

/// Review fix (Task 5 round 1, Issue 1): pure decision logic behind
/// `ResultGrid::accept_lookup_result` — pulled out as a free function over
/// plain values (no `ResultGrid`/entity needed) so it's directly unit
/// testable. See `ResultGrid::lookup_generation`'s doc comment for the two
/// races this guards against.
fn should_apply_lookup(
    requested_generation: u64,
    current_generation: u64,
    currently_checked: &HashSet<String>,
    wanted_cols: &[String],
) -> bool {
    requested_generation == current_generation
        && wanted_cols.iter().all(|c| currently_checked.contains(c))
}

/// G4 Task 5: effective-column (`fk_join`'s "sources 0..n, virtuals
/// n..n+m") text accessor over a REAL `ResultBuffer` — every non-render
/// consumer (`rebuild_view`'s sort/filter closure, `poll_find`, `on_copy`,
/// export) reads cells through this instead of `buf.cell_text` directly,
/// which is the entire mechanism by which they "see" virtual columns for
/// free (per `fk_join::effective_cell_text`'s doc comment: swap the
/// closure's inner call). Free function (not a `ResultGrid` method) so
/// callers can pass `&self.virtual_cols` (one field, immutably) alongside a
/// SEPARATELY held `&mut ResultBuffer` (either `self.buffer`'s own
/// `RefCell` borrow, or — inside `rebuild_view` — while `self.view` is ALSO
/// borrowed mutably via `self.view.rebuild(..)`); a method taking `&mut
/// self` would conflict with that partial borrow, a free function taking
/// disjoint pieces does not.
fn effective_text(
    ncols: usize,
    virtual_cols: &[VirtualCol],
    buf: &mut ResultBuffer,
    source_row: usize,
    col: usize,
) -> String {
    fk_join::effective_cell_text(ncols, virtual_cols, &mut |r, c| buf.cell_text(r, c), source_row, col)
}

/// Null-aware companion to `effective_text` (same "None = real SQL NULL"
/// convention `ResultBuffer::cell_is_null` uses for source columns) — used
/// only by `start_export`'s snapshot loop, which needs to tell a genuine
/// NULL apart from an empty string for JSON/`INSERT` export. A virtual
/// cell is NULL when its fk value has no matching ref row at all OR the
/// matched row's joined value is itself NULL — both cases `fk_join::
/// VirtualCol::map` represents as "no `Some(Some(_))` entry".
fn effective_is_null(
    ncols: usize,
    virtual_cols: &[VirtualCol],
    buf: &mut ResultBuffer,
    source_row: usize,
    col: usize,
) -> bool {
    if col < ncols {
        return buf.cell_is_null(source_row, col);
    }
    let Some(vcol) = virtual_cols.get(col - ncols) else { return true };
    let fk_val = buf.cell_text(source_row, vcol.src_col);
    !matches!(vcol.map.get(&fk_val), Some(Some(_)))
}

/// Unix-seconds timestamp used to name a `start_export` Downloads fallback
/// file (`dbc-export-{ts}.{ext}`) — shared by both fallback arms (a real
/// dialog error and a dropped/cancelled dialog channel) so they don't
/// duplicate the `SystemTime` dance.
fn export_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Formats `data` (already-snapshotted visible cells, display order — see
/// `ResultGrid::start_export`) via `export::export` and writes it to
/// `path`, going through a `.tmp` sibling file + rename (brief contract:
/// never leave a half-written file at the final name if the write is
/// interrupted midway).
fn write_export_file(
    path: &std::path::Path,
    format: ExportFormat,
    headers: &[String],
    table_name: &str,
    rows: usize,
    data: &[Vec<Option<String>>],
) -> Result<(), String> {
    let tmp_path = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    // On any failure past this point, remove the partial .tmp so a
    // disk-full/AV-locked run doesn't leave orphans (Task 4 re-review
    // issue 5).
    let write = || -> Result<(), String> {
        let file = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
        let mut w = std::io::BufWriter::new(file);
        export::export(&mut w, format, headers, table_name, rows, &mut |r, c| {
            data.get(r).and_then(|row| row.get(c)).cloned().flatten()
        })?;
        std::io::Write::flush(&mut w).map_err(|e| e.to_string())?;
        Ok(())
    };
    let result = write().and_then(|()| std::fs::rename(&tmp_path, path).map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

impl Focusable for ResultGrid {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// G4 Task 5: lets `main.rs` `cx.subscribe(&grid, AppView::on_grid_event)`
/// per grid entity — see `GridEvent`'s doc comment.
impl EventEmitter<GridEvent> for ResultGrid {}

impl Render for ResultGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let buffer = self.buffer.clone();
        let widths = self.col_widths.clone();
        let is_resizing = self.resizing.is_some();
        let has_buffer = self.buffer.is_some();

        // G4 Task 3: `.relative()` so the cell-detail popup can be an
        // absolutely-positioned overlay child, same as `AppView`'s own root
        // (`main.rs`) for its modal/palette overlays.
        let mut root = div()
            .relative()
            .key_context("ResultGrid")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_find_in_result))
            .on_action(cx.listener(Self::on_find_next))
            .on_action(cx.listener(Self::on_find_prev));

        // Toolbar (+ filter row) only when a buffer is actually set (brief
        // contract #1) — `toolbar` also polls filters/find as a side
        // effect (see its doc comment), so it must run before `header`/the
        // row list read `view`/`find` below.
        if has_buffer {
            root = root.child(self.toolbar(cx));
            if self.filters_open {
                let theme = *cx.theme();
                root = root.child(self.filter_row(&theme));
            }
        }

        // Row count/order goes through `view` (G4 Task 2: local sort), not
        // the buffer's raw row count directly — `row_ix` below is a DISPLAY
        // index, mapped via `this.view.source_row` before every read.
        // CAPTURED ONLY AFTER `toolbar(cx)` ran above: `poll_filters` inside
        // it can shrink `self.view` mid-render, and a stale larger count fed
        // to `uniform_list` panics `source_row` out-of-bounds (Task 3 review
        // issue 1).
        let real_row_count = self.view.len();
        // G5 Task 3, brief contract #3: inserted rows render AFTER every
        // real row (regardless of sort — `RowView`/`view.source_row` never
        // sees them at all), so the total `uniform_list` count on an
        // editable tab is `view.len() + inserted.len()`.
        let is_editable = self.editable.is_some();
        let insert_row_count = if is_editable { self.edit_state.inserted_rows.len() } else { 0 };
        let row_count = real_row_count + insert_row_count;
        root = root.child(self.header(cx)).child(
            uniform_list(
                "result-rows",
                row_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let theme = *cx.theme();
                    let mut items = Vec::with_capacity(range.len());
                    if let Some(buf) = &buffer {
                        let mut buf = buf.borrow_mut();
                        let ncols = buf.column_count();
                        // G4 Task 5: effective column range — source columns
                        // `0..ncols` unchanged, then every virtual column
                        // (`ncols..ncols+virtual_cols.len()`), never
                        // hideable (see `virtual_cols`' doc comment).
                        let effective_ncols = ncols + this.virtual_cols.len();
                        let editable = this.editable.is_some();
                        let real_row_count = this.view.len();
                        for row_ix in range {
                            // G5 Task 3: rows past `real_row_count` are
                            // staged INSERTs (brief contract #3) — a
                            // completely separate rendering path (no
                            // `view`/`source_row`, no selection/find, cells
                            // read from `edit_state.inserted_rows` instead
                            // of the buffer).
                            if editable && row_ix >= real_row_count {
                                let ins_ix = row_ix - real_row_count;
                                let mut row = div()
                                    .id(row_ix)
                                    .flex()
                                    .flex_row()
                                    .h(px(ROW_HEIGHT))
                                    .bg(theme.diff_inserted_bg);
                                row = row.child(
                                    div()
                                        .id(("gutter-ins", ins_ix))
                                        .w(px(GUTTER_WIDTH))
                                        .h(px(ROW_HEIGHT))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .text_color(theme.text_disabled)
                                        .child("␡")
                                        .on_click(cx.listener(move |this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            this.remove_insert_row(ins_ix, cx);
                                        })),
                                );
                                for col in 0..ncols {
                                    if this.hidden_cols.get(col).copied().unwrap_or(false) {
                                        continue;
                                    }
                                    let cell_val = this
                                        .edit_state
                                        .inserted_rows
                                        .get(ins_ix)
                                        .and_then(|r| r.get(col))
                                        .cloned()
                                        .unwrap_or(None);
                                    let text = sandbox::insert_cell_display(&cell_val);
                                    let column_name = buf
                                        .schema()
                                        .fields()
                                        .get(col)
                                        .map(|f| f.name().clone())
                                        .unwrap_or_default();
                                    let cell = div()
                                        .id(("cell-ins", ins_ix * 10_000 + col))
                                        .w(px(widths.get(col).copied().unwrap_or(DEFAULT_COL_WIDTH)))
                                        .px_2()
                                        .overflow_hidden()
                                        .text_color(theme.text_primary)
                                        .bg(theme.diff_inserted_bg)
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                                                window.focus(&this.focus_handle, cx);
                                                if e.click_count >= 2 {
                                                    // T3 review issue 1: `ins_ix`
                                                    // was captured at render time.
                                                    // A concurrent "␡" removal of
                                                    // an earlier insert row shifts
                                                    // later rows down (Vec::remove),
                                                    // so a stale `ins_ix` can now
                                                    // point past the vec or at a
                                                    // different row. Re-validate
                                                    // before opening the editor —
                                                    // mirror the real-row
                                                    // `row_ix >= view.len()` clamp.
                                                    // (stage_insert_cell is also a
                                                    // no-op on OOB as a backstop.)
                                                    if ins_ix >= this.edit_state.inserted_rows.len() {
                                                        return;
                                                    }
                                                    this.open_cell_editor(
                                                        EditTarget::Insert { ins_ix, col },
                                                        column_name.clone(),
                                                        "(nový řádek)".to_string(),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            }),
                                        )
                                        .child(text);
                                    row = row.child(cell);
                                }
                                items.push(row);
                                continue;
                            }

                            let source_row = this.view.source_row(row_ix);
                            let is_deleted =
                                editable && this.edit_state.deleted_rows.contains(&source_row);
                            let mut row = div()
                                .id(row_ix)
                                .flex()
                                .flex_row()
                                .h(px(ROW_HEIGHT))
                                .bg(if is_deleted {
                                    theme.diff_deleted_bg
                                } else if row_ix % 2 == 0 {
                                    theme.bg_panel
                                } else {
                                    // G14 Task 2: bg_panel_alt zebra confirmed intentional.
                                    theme.bg_panel_alt
                                });
                            // G5 Task 3, brief contract #4: "✕" toggles this
                            // SOURCE row's delete flag; `stop_propagation`
                            // keeps it from also landing on a cell's own
                            // mouse-down (selection/double-click) below.
                            if editable {
                                row = row.child(
                                    div()
                                        .id(("gutter", row_ix))
                                        .w(px(GUTTER_WIDTH))
                                        .h(px(ROW_HEIGHT))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .text_color(if is_deleted {
                                            theme.danger
                                        } else {
                                            theme.text_disabled
                                        })
                                        .child("✕")
                                        .on_click(cx.listener(move |this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            if row_ix < this.view.len() {
                                                let source_row = this.view.source_row(row_ix);
                                                this.toggle_row_delete(source_row, cx);
                                            }
                                        })),
                                );
                            }
                            for col in 0..effective_ncols {
                                if col < ncols && this.hidden_cols.get(col).copied().unwrap_or(false) {
                                    continue;
                                }
                                let is_virtual = col >= ncols;
                                let mut text = if !is_virtual {
                                    buf.cell_text(source_row, col)
                                } else {
                                    let vcol = &this.virtual_cols[col - ncols];
                                    let fk_val = buf.cell_text(source_row, vcol.src_col);
                                    fk_join::virtual_cell_text(&fk_val, &vcol.map)
                                };
                                // G5 Task 3, brief contract #3: a staged
                                // edit shows the STAGED value/"(NULL)"
                                // instead of the committed one — keyed by
                                // SOURCE row+col, never applies to a virtual
                                // (ad-hoc lookup) column.
                                let staged_display = if editable && !is_virtual {
                                    sandbox::staged_cell_display(
                                        this.edit_state.cells.get(&(source_row, col)),
                                    )
                                } else {
                                    None
                                };
                                let is_staged = staged_display.is_some();
                                if let Some(d) = staged_display {
                                    text = d;
                                }
                                // G4 Task 5: joined (preview) / virtual
                                // (ad-hoc) columns render tinted (brief: bg
                                // 0x2a2a3d) — checked before selection/find
                                // so those still take visual priority below.
                                let joined = is_virtual
                                    || this.joined_cols.get(col).copied().unwrap_or(false);
                                let selected = this.is_selected(row_ix, col);
                                // G4 Task 3: the current find match gets its
                                // own distinct bg, taking priority over the
                                // (unrelated) selection highlight.
                                let is_find_match = this
                                    .find
                                    .as_ref()
                                    .and_then(|f| f.current.map(|ix| f.matches[ix]))
                                    == Some((row_ix, col));
                                let mut cell = div()
                                    .id(("cell", row_ix * 10_000 + col))
                                    .w(px(widths.get(col).copied().unwrap_or(DEFAULT_COL_WIDTH)))
                                    .px_2()
                                    .overflow_hidden()
                                    .text_color(theme.text_primary)
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                                            window.focus(&this.focus_handle, cx);
                                            // Defensive clamp (final review
                                            // fix #1): `row_ix` was captured
                                            // at render time; if the view
                                            // shrank (a filter typed, a
                                            // sort/virtual-col change) in
                                            // the gap between that render
                                            // and this click landing, it can
                                            // now point past `view.len()`.
                                            // Ignore the click rather than
                                            // open a stale cell-detail popup
                                            // or set a selection anchor
                                            // `on_copy` would later see.
                                            if row_ix >= this.view.len() {
                                                return;
                                            }
                                            // G4 Task 3: double-click (or
                                            // more) opens a popup instead of
                                            // touching selection — re-reads
                                            // the SOURCE row from the
                                            // CURRENT `view` rather than
                                            // capturing the render-time
                                            // `source_row`, in case sort/
                                            // filter changed between render
                                            // and click.
                                            //
                                            // G5 Task 3, brief contract #2:
                                            // on an EDITABLE tab, a real
                                            // (non-virtual, non-joined)
                                            // column opens the staging
                                            // editor instead of the
                                            // read-only detail popup —
                                            // joined columns aren't part of
                                            // the writable table, so they
                                            // keep the old read-only
                                            // behaviour.
                                            if e.click_count >= 2 {
                                                if let Some(buf) = this.buffer.clone() {
                                                    let source_row = this.view.source_row(row_ix);
                                                    let ncols = buf.borrow().column_count();
                                                    let is_virtual = col >= ncols;
                                                    let joined = !is_virtual
                                                        && this
                                                            .joined_cols
                                                            .get(col)
                                                            .copied()
                                                            .unwrap_or(false);
                                                    if this.editable.is_some() && !is_virtual && !joined {
                                                        let column_name = buf
                                                            .borrow()
                                                            .schema()
                                                            .fields()
                                                            .get(col)
                                                            .map(|f| f.name().clone())
                                                            .unwrap_or_default();
                                                        let original_text =
                                                            buf.borrow_mut().cell_text(source_row, col);
                                                        this.open_cell_editor(
                                                            EditTarget::Cell { source_row, col },
                                                            column_name,
                                                            original_text,
                                                            window,
                                                            cx,
                                                        );
                                                    } else {
                                                        let text = if col < ncols {
                                                            buf.borrow_mut().cell_text(source_row, col)
                                                        } else if let Some(vcol) =
                                                            this.virtual_cols.get(col - ncols).cloned()
                                                        {
                                                            let fk_val = buf
                                                                .borrow_mut()
                                                                .cell_text(source_row, vcol.src_col);
                                                            fk_join::virtual_cell_text(&fk_val, &vcol.map)
                                                        } else {
                                                            String::new()
                                                        };
                                                        this.cell_detail =
                                                            Some(CellDetail { text, scroll_lines: 0 });
                                                    }
                                                }
                                                cx.notify();
                                                return;
                                            }
                                            if e.modifiers.shift {
                                                if let Some((anchor, _)) = this.selection {
                                                    this.selection = Some((anchor, (row_ix, col)));
                                                } else {
                                                    this.selection =
                                                        Some(((row_ix, col), (row_ix, col)));
                                                }
                                            } else {
                                                this.selection =
                                                    Some(((row_ix, col), (row_ix, col)));
                                            }
                                            cx.notify();
                                        }),
                                    )
                                    .child(text);
                                if is_deleted {
                                    cell = cell.bg(theme.diff_deleted_bg);
                                } else if is_find_match {
                                    cell = cell.bg(theme.bg_find_match);
                                } else if selected {
                                    cell = cell.bg(theme.bg_selected);
                                } else if is_staged {
                                    cell = cell.bg(theme.diff_staged_bg);
                                } else if joined {
                                    cell = cell.bg(theme.bg_joined_col);
                                }
                                row = row.child(cell);
                            }
                            items.push(row);
                        }
                    }
                    items
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1(),
        );

        if is_resizing {
            root = root
                .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _w, cx| {
                    if let Some((col, start_x, start_w)) = this.resizing {
                        let dx: f32 = f32::from(e.position.x) - start_x;
                        if let Some(w) = this.col_widths.get_mut(col) {
                            *w = (start_w + dx).max(40.0);
                        }
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| {
                        this.resizing = None;
                        // G4 Task 6: width-drag END (never mid-drag, brief
                        // contract #3) — same PREVIEW-only guard as
                        // `on_header_click`/`toggle_column_visibility`.
                        if this.is_preview {
                            cx.emit(GridEvent::ViewChanged);
                        }
                        cx.notify();
                    }),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| {
                        this.resizing = None;
                        if this.is_preview {
                            cx.emit(GridEvent::ViewChanged);
                        }
                        cx.notify();
                    }),
                );
        }

        if has_buffer && self.export_open {
            root = root.child(self.render_export_menu_overlay(cx));
        }
        if has_buffer && self.columns_open {
            root = root.child(self.render_columns_menu_overlay(cx));
        }
        if has_buffer && self.fk_menu_open.is_some() {
            if let Some(overlay) = self.render_fk_menu_overlay(cx) {
                root = root.child(overlay);
            }
        }

        if let Some(overlay) = self.render_cell_detail_overlay(cx) {
            root = root.child(overlay);
        }
        if let Some(overlay) = self.render_cell_editor_overlay(cx) {
            root = root.child(overlay);
        }

        root
    }
}

/// Review fix (Task 5 round 1, Issue 1): pure unit tests for
/// `should_apply_lookup` — the extractable decision logic behind
/// `ResultGrid::accept_lookup_result`. Doesn't need a `Context`/`App` at
/// all, unlike the rest of `ResultGrid`'s behaviour, so it's tested
/// directly rather than through a GPUI test harness.
#[cfg(test)]
mod lookup_generation_tests {
    use super::should_apply_lookup;
    use std::collections::HashSet;

    fn set(cols: &[&str]) -> HashSet<String> {
        cols.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn accepts_when_generation_matches_and_all_wanted_cols_still_checked() {
        let checked = set(&["name", "email"]);
        assert!(should_apply_lookup(3, 3, &checked, &["name".to_string()]));
        assert!(should_apply_lookup(
            3,
            3,
            &checked,
            &["name".to_string(), "email".to_string()]
        ));
    }

    #[test]
    fn rejects_when_a_newer_request_has_since_been_dispatched() {
        // Simulates: lookup A dispatched at generation 1 (wanted=[name]),
        // then lookup B dispatched at generation 2 (wanted=[name,email]) —
        // A's response arrives after B was dispatched (reordering).
        let checked = set(&["name", "email"]);
        assert!(!should_apply_lookup(1, 2, &checked, &["name".to_string()]));
    }

    #[test]
    fn rejects_when_checked_uncheck_happened_with_no_new_dispatch() {
        // Simulates: lookup A dispatched at generation 1 (wanted=[name]),
        // then the user unchecks "name" — `checked_ordered` goes empty, the
        // local-clear path bumps the generation to 2 with NO new
        // `RunLookup` dispatched. A's response arrives late.
        let checked: HashSet<String> = HashSet::new();
        assert!(!should_apply_lookup(1, 2, &checked, &["name".to_string()]));
    }

    #[test]
    fn rejects_when_generation_matches_but_a_wanted_col_was_unchecked() {
        // Defense-in-depth: even if the generation happened to match, a
        // response is rejected if any column it covers is no longer in the
        // checked set.
        let checked = set(&["email"]);
        assert!(!should_apply_lookup(5, 5, &checked, &["name".to_string()]));
    }

    #[test]
    fn rejects_when_wanted_cols_is_a_strict_subset_of_checked() {
        // Response for an OLDER, smaller wanted-set arriving while a newer
        // dispatch (different generation) is in flight for a larger set —
        // generation alone already rejects this, checked here for the
        // "checked cols is a superset" combination specifically.
        let checked = set(&["name", "email", "phone"]);
        assert!(!should_apply_lookup(
            1,
            2,
            &checked,
            &["name".to_string(), "email".to_string()]
        ));
    }

    #[test]
    fn accepts_empty_wanted_cols_vacuously_when_generation_matches() {
        let checked: HashSet<String> = HashSet::new();
        assert!(should_apply_lookup(0, 0, &checked, &[]));
    }
}
