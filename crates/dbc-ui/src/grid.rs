use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use gpui::{div, prelude::*, px, rgb, uniform_list, Context, Window};

pub const ROW_HEIGHT: f32 = 24.0;
pub const DEFAULT_COL_WIDTH: f32 = 160.0;

pub struct ResultGrid {
    pub buffer: Option<Rc<RefCell<ResultBuffer>>>,
    pub col_widths: Vec<f32>,
}

impl ResultGrid {
    pub fn new() -> Self {
        Self { buffer: None, col_widths: Vec::new() }
    }

    pub fn set_buffer(&mut self, buffer: Rc<RefCell<ResultBuffer>>) {
        let ncols = buffer.borrow().column_count();
        self.col_widths = vec![DEFAULT_COL_WIDTH; ncols];
        self.buffer = Some(buffer);
    }

    fn header(&self) -> impl IntoElement {
        let mut row = div().flex().flex_row().bg(rgb(0x313244)).text_color(rgb(0xf9e2af));
        if let Some(buf) = &self.buffer {
            let buf = buf.borrow();
            for (i, field) in buf.schema().fields().iter().enumerate() {
                row = row.child(
                    div()
                        .w(px(self.col_widths[i]))
                        .px_2()
                        .h(px(ROW_HEIGHT))
                        .overflow_hidden()
                        .child(field.name().clone()),
                );
            }
        }
        row
    }
}

impl Render for ResultGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row_count = self.buffer.as_ref().map_or(0, |b| b.borrow().row_count());
        let buffer = self.buffer.clone();
        let widths = self.col_widths.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.header())
            .child(
                uniform_list(
                    "result-rows",
                    row_count,
                    cx.processor(move |_this, range: std::ops::Range<usize>, _window, _cx| {
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
                                    row = row.child(
                                        div()
                                            .w(px(widths[col]))
                                            .px_2()
                                            .overflow_hidden()
                                            .text_color(rgb(0xcdd6f4))
                                            .child(buf.cell_text(row_ix, col)),
                                    );
                                }
                                items.push(row);
                            }
                        }
                        items
                    }),
                )
                .flex_1(),
            )
    }
}
