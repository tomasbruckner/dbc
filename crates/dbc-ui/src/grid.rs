use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use gpui::{
    actions, div, prelude::*, px, rgb, uniform_list, ClipboardItem, Context, FocusHandle,
    Focusable, KeyBinding, Window,
};

pub const ROW_HEIGHT: f32 = 24.0;
pub const DEFAULT_COL_WIDTH: f32 = 160.0;

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
    /// (anchor cell, focus cell) as (row, col). Not normalized until copy.
    selection: Option<((usize, usize), (usize, usize))>,
    /// (col index, mouse-down start x, start width) while a resize drag is active.
    resizing: Option<(usize, f32, f32)>,
}

impl ResultGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            buffer: None,
            col_widths: Vec::new(),
            focus_handle: cx.focus_handle(),
            selection: None,
            resizing: None,
        }
    }

    pub fn set_buffer(&mut self, buffer: Rc<RefCell<ResultBuffer>>) {
        let ncols = buffer.borrow().column_count();
        self.col_widths = vec![DEFAULT_COL_WIDTH; ncols];
        self.buffer = Some(buffer);
        self.selection = None;
    }

    fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(((r0, c0), (r1, c1))) = self.selection else {
            return false;
        };
        let (rmin, rmax) = (r0.min(r1), r0.max(r1));
        let (cmin, cmax) = (c0.min(c1), c0.max(c1));
        row >= rmin && row <= rmax && col >= cmin && col <= cmax
    }

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
            for c in cmin..=cmax {
                if c > cmin {
                    out.push('\t');
                }
                out.push_str(&buf.cell_text(r, c));
            }
            out.push('\n');
        }
        cx.write_to_clipboard(ClipboardItem::new_string(out));
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().flex_row().bg(rgb(0x313244)).text_color(rgb(0xf9e2af));
        if let Some(buf) = &self.buffer {
            let buf = buf.borrow();
            for (i, field) in buf.schema().fields().iter().enumerate() {
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
                                .px_2()
                                .h(px(ROW_HEIGHT))
                                .overflow_hidden()
                                .child(field.name().clone()),
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
        let row_count = self.buffer.as_ref().map_or(0, |b| b.borrow().row_count());
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
                                let mut row = div()
                                    .id(row_ix)
                                    .flex()
                                    .flex_row()
                                    .h(px(ROW_HEIGHT))
                                    .bg(if row_ix % 2 == 0 { rgb(0x1e1e2e) } else { rgb(0x232334) });
                                for col in 0..ncols {
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
                                        .child(buf.cell_text(row_ix, col));
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
