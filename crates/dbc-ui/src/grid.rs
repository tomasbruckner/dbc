use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use gpui::{
    actions, div, prelude::*, px, rgb, rgba, uniform_list, AnyElement, ClipboardItem, Context,
    Entity, FocusHandle, Focusable, KeyBinding, ScrollDelta, ScrollStrategy, ScrollWheelEvent,
    UniformListScrollHandle, Window,
};

use crate::connections_ui::TextField;
use crate::row_view::{self, RowView};

pub const ROW_HEIGHT: f32 = 24.0;
pub const DEFAULT_COL_WIDTH: f32 = 160.0;
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

actions!(grid, [CopySelection, FindInResult, FindNext, FindPrev]);

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
        let mut buf = buf.borrow_mut();
        self.view.rebuild(rows, &mut |r, c| buf.cell_text(r, c));
        drop(buf);
        // G4 Task 3: any active find's `matches` were computed against the
        // PREVIOUS display order — bump so `toolbar`'s staleness check
        // recomputes them (see `FindState::computed_generation`).
        self.view_generation += 1;
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
        cx.notify();
    }

    /// Public seam for Task 6 persistence (view_prefs): applies a saved
    /// sort + hidden-column set and rebuilds. `hidden` is by SOURCE column
    /// index, same convention as `hidden_cols`. Not called anywhere yet —
    /// Task 6 wires it up once `dbc-state::ViewPrefsStore` (Task 1) is
    /// loaded and a preview tab's schema is known.
    #[allow(dead_code)]
    pub fn set_view_state(&mut self, sort: Option<(usize, bool)>, hidden: Vec<bool>) {
        self.view.sort = sort;
        self.hidden_cols = hidden;
        self.rebuild_view();
    }

    /// Public seam for Task 6 persistence: current sort + hidden-column
    /// state, by SOURCE column index. Not called anywhere yet — see
    /// `set_view_state`.
    #[allow(dead_code)]
    pub fn view_state(&self) -> (Option<(usize, bool)>, Vec<bool>) {
        (self.view.sort, self.hidden_cols.clone())
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
        let visible_cols: Vec<usize> =
            (0..self.hidden_cols.len()).filter(|&c| !self.hidden_cols.get(c).copied().unwrap_or(false)).collect();
        let rows = self.view.len();
        let gen = self.view_generation;
        // Capped scan (Task 3 review issue 2): synchronous per-keystroke
        // work on the UI thread must be bounded for huge/spilled results.
        let (matches, capped) = if let Some(buf) = self.buffer.clone() {
            let view = &self.view;
            let mut buf = buf.borrow_mut();
            row_view::find_matches_capped(
                rows,
                &visible_cols,
                &query,
                FIND_MAX_ROWS,
                FIND_MAX_MATCHES,
                &mut |r, c| buf.cell_text(view.source_row(r), c),
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
        let Some(buf) = &self.buffer else {
            return;
        };
        let mut buf = buf.borrow_mut();
        let mut out = String::new();
        for r in rmin..=rmax {
            if r > rmin {
                out.push('\n');
            }
            let source_row = self.view.source_row(r);
            let mut first_col = true;
            for c in cmin..=cmax {
                if self.hidden_cols.get(c).copied().unwrap_or(false) {
                    continue;
                }
                if !first_col {
                    out.push('\t');
                }
                first_col = false;
                out.push_str(&buf.cell_text(source_row, c));
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
            .bg(rgb(0x181825))
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .id("toggle-filters")
                    .cursor_pointer()
                    .px_2()
                    .rounded_md()
                    .bg(if filters_open { rgb(0x45475a) } else { rgb(0x313244) })
                    .child("Filtr")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_filters(cx);
                    })),
            );

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
    fn filter_row(&self) -> impl IntoElement {
        let mut row = div().flex().flex_row().bg(rgb(0x181825));
        if let Some(buf) = &self.buffer {
            let ncols = buf.borrow().column_count();
            for i in 0..ncols {
                if self.hidden_cols.get(i).copied().unwrap_or(false) {
                    continue;
                }
                if let Some(input) = self.filter_inputs.get(i) {
                    row = row.child(
                        div()
                            .w(px(self.col_widths[i]))
                            .h(px(ROW_HEIGHT))
                            .px_1()
                            .child(input.clone()),
                    );
                }
            }
        }
        row
    }

    /// Cell-detail popup (brief contract #3): same centered-overlay shape
    /// as `connections_ui::render_modal_overlay`/`AppView::render_palette_overlay`
    /// (`.occlude()` blocks clicks reaching the grid underneath), owned
    /// on `ResultGrid` rather than `AppView::modal` since it's tied to this
    /// specific tab's grid (see `cell_detail`'s doc comment). Text is
    /// rendered line-by-line with wheel-driven `scroll_lines`, exactly like
    /// `main.rs`'s `TabContent::Text` body — reused rather than reinvented.
    fn render_cell_detail_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
            .text_color(rgb(0xcdd6f4))
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
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded_md()
            .flex()
            .flex_col()
            .child(body)
            .child(
                div().flex().flex_row().justify_end().gap_2().p_2().child(
                    div()
                        .id("cell-detail-copy")
                        .cursor_pointer()
                        .bg(rgb(0x313244))
                        .text_color(rgb(0xcdd6f4))
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
                        .bg(rgb(0x313244))
                        .text_color(rgb(0xcdd6f4))
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
                .bg(rgba(0x00000099))
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
        let mut row = div().flex().flex_row().bg(rgb(0x313244)).text_color(rgb(0xf9e2af));
        if let Some(buf) = &self.buffer {
            let buf = buf.borrow();
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
                // Resize handle overlays the last 5px of the column (absolute,
                // anchored to the right edge) instead of adding extra width
                // after the cell — that would push header columns out of
                // alignment with the (handle-less) data columns below as the
                // column index grows.
                row = row.child(
                    div()
                        .relative()
                        .w(px(self.col_widths[i]))
                        .h(px(ROW_HEIGHT))
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
                        ),
                );
            }
        }
        row
    }
}

impl Focusable for ResultGrid {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

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
                root = root.child(self.filter_row());
            }
        }

        // Row count/order goes through `view` (G4 Task 2: local sort), not
        // the buffer's raw row count directly — `row_ix` below is a DISPLAY
        // index, mapped via `this.view.source_row` before every read.
        // CAPTURED ONLY AFTER `toolbar(cx)` ran above: `poll_filters` inside
        // it can shrink `self.view` mid-render, and a stale larger count fed
        // to `uniform_list` panics `source_row` out-of-bounds (Task 3 review
        // issue 1).
        let row_count = self.view.len();
        root = root.child(self.header(cx)).child(
            uniform_list(
                "result-rows",
                row_count,
                cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                    let mut items = Vec::with_capacity(range.len());
                    if let Some(buf) = &buffer {
                        let mut buf = buf.borrow_mut();
                        let ncols = buf.column_count();
                        for row_ix in range {
                            let source_row = this.view.source_row(row_ix);
                            let mut row = div()
                                .id(row_ix)
                                .flex()
                                .flex_row()
                                .h(px(ROW_HEIGHT))
                                .bg(if row_ix % 2 == 0 { rgb(0x1e1e2e) } else { rgb(0x232334) });
                            for col in 0..ncols {
                                if this.hidden_cols.get(col).copied().unwrap_or(false) {
                                    continue;
                                }
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
                                    .w(px(widths[col]))
                                    .px_2()
                                    .overflow_hidden()
                                    .text_color(rgb(0xcdd6f4))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                                            window.focus(&this.focus_handle, cx);
                                            // G4 Task 3: double-click (or
                                            // more) opens the cell-detail
                                            // popup instead of touching
                                            // selection — re-reads the
                                            // SOURCE row from the CURRENT
                                            // `view` rather than capturing
                                            // the render-time `source_row`,
                                            // in case sort/filter changed
                                            // between render and click.
                                            if e.click_count >= 2 {
                                                if let Some(buf) = this.buffer.clone() {
                                                    let source_row = this.view.source_row(row_ix);
                                                    let text = buf.borrow_mut().cell_text(source_row, col);
                                                    this.cell_detail =
                                                        Some(CellDetail { text, scroll_lines: 0 });
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
                                    .child(buf.cell_text(source_row, col));
                                if is_find_match {
                                    cell = cell.bg(rgb(0x585b70));
                                } else if selected {
                                    cell = cell.bg(rgb(0x45475a));
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
                        cx.notify();
                    }),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _e, _w, cx| {
                        this.resizing = None;
                        cx.notify();
                    }),
                );
        }

        if let Some(overlay) = self.render_cell_detail_overlay(cx) {
            root = root.child(overlay);
        }

        root
    }
}
