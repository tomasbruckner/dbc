use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use gpui::{
    actions, div, prelude::*, px, rgb, uniform_list, ClipboardItem, Context, FocusHandle,
    Focusable, KeyBinding, Window,
};

use crate::row_view::RowView;

pub const ROW_HEIGHT: f32 = 24.0;
pub const DEFAULT_COL_WIDTH: f32 = 160.0;
/// G4 Task 2: above this many rows, a sort click sets `status_note` to
/// "řadím…" before `rebuild_view` runs. `rebuild` is synchronous today, so
/// this note is a retroactive "that sort was over a big set" marker rather
/// than a live in-progress spinner — see `status_note`'s doc comment.
const LARGE_SORT_ROWS: usize = 100_000;

actions!(grid, [CopySelection]);

/// Bind ResultGrid's own keys. Scoped to the `"ResultGrid"` key context so
/// ctrl-c only fires `CopySelection` while the grid (not `SqlInput`) is
/// focused — SqlInput binds its own `Copy` action under context `None`, and
/// since the grid isn't in SqlInput's dispatch path (and vice versa), the two
/// never contend even without scoping, but the explicit context makes the
/// intent unambiguous and future-proof.
pub fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([KeyBinding::new("ctrl-c", CopySelection, Some("ResultGrid"))]);
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
        }
    }

    pub fn set_buffer(&mut self, buffer: Rc<RefCell<ResultBuffer>>) {
        let ncols = buffer.borrow().column_count();
        let nrows = buffer.borrow().row_count();
        self.col_widths = vec![DEFAULT_COL_WIDTH; ncols];
        self.buffer = Some(buffer);
        self.selection = None;
        self.view = RowView::identity(nrows);
        self.hidden_cols = vec![false; ncols];
        self.dirty = false;
        self.status_note = None;
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
        // Row count/order goes through `view` (G4 Task 2: local sort), not
        // the buffer's raw row count directly — `row_ix` below is a
        // DISPLAY index, mapped to the buffer's actual row via
        // `this.view.source_row` before every read/selection use.
        let row_count = self.view.len();
        let buffer = self.buffer.clone();
        let widths = self.col_widths.clone();
        let is_resizing = self.resizing.is_some();

        let mut root = div()
            .key_context("ResultGrid")
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .on_action(cx.listener(Self::on_copy))
            .child(self.header(cx))
            .child(
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
                                    if selected {
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

        root
    }
}
