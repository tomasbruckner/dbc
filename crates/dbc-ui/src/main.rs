mod connect;
mod grid;
mod runner;
mod sql_input;
mod text_model;
mod tunnel;

use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::CancelToken;
use gpui::{
    actions, div, prelude::*, px, rgb, size, App, Bounds, Context, Entity, Focusable, KeyBinding,
    Window, WindowBounds, WindowOptions,
};
use gpui_platform::application;
use grid::ResultGrid;
use runner::{QueryEvent, QueryRunner};
use sql_input::SqlInput;

actions!(dbc, [RunQuery, CancelQuery]);

struct AppView {
    grid: Entity<ResultGrid>,
    status: String,
    runner: QueryRunner,
    conn_url: String,
    sql: Entity<SqlInput>,
    cancel: Option<CancelToken>,
    started_at: Option<std::time::Instant>,
}

impl AppView {
    fn on_run_query(&mut self, _: &RunQuery, _window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel.is_some() {
            return; // one query at a time in v1
        }
        let sql = self.sql.read(cx).text();
        if sql.trim().is_empty() {
            return;
        }
        let conn = match connect::open(&self.conn_url, &self.runner.handle()) {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("error: {e}");
                cx.notify();
                return;
            }
        };
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.started_at = Some(std::time::Instant::now());
        self.status = "running…".into();
        let mut rx = self.runner.run(conn, sql, cancel);
        let grid = self.grid.clone();
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |view, cx| {
                    match ev {
                        QueryEvent::Started { columns } => {
                            let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
                            buffer = Some(buf.clone());
                            grid.update(cx, |g, _| g.set_buffer(buf));
                        }
                        QueryEvent::Batch(b) => {
                            if let Some(buf) = &buffer {
                                buf.borrow_mut().push(b);
                            }
                            let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                            let secs = view.started_at.map_or(0.0, |t| t.elapsed().as_secs_f32());
                            view.status = format!("{rows} rows… {secs:.1}s");
                        }
                        QueryEvent::Finished { elapsed } => {
                            let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                            view.status = format!("{rows} rows in {elapsed:.2?}");
                            view.cancel = None;
                        }
                        QueryEvent::Failed(e) => {
                            view.status = format!("error: {e}");
                            view.cancel = None;
                        }
                    }
                    cx.notify();
                });
            }
            let _ = this.update(cx, |view, cx| {
                view.cancel = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn on_cancel_query(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
            self.status = "cancelling…".into();
            cx.notify();
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .on_action(cx.listener(Self::on_run_query))
            .on_action(cx.listener(Self::on_cancel_query))
            .child(
                // Fixed height of 8 lines (SqlInput's own line_height is
                // px(20.), see sql_input.rs render()); the input scrolls
                // internally once the buffer grows past that.
                div()
                    .h(px(20. * 8. + 4. * 2.))
                    .px_2()
                    .bg(rgb(0x181825))
                    .child(self.sql.clone()),
            )
            .child(self.grid.clone())
            .child(
                div()
                    .h(px(28.))
                    .px_2()
                    .bg(rgb(0x313244))
                    .text_color(rgb(0xa6adc8))
                    .child(self.status.clone()),
            )
    }
}

fn main() {
    let conn_url = std::env::args()
        .nth(1)
        .expect("usage: dbc-ui <connection-string>");
    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("escape", CancelQuery, None),
        ]);
        sql_input::bind_keys(cx);
        grid::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let grid = cx.new(ResultGrid::new);
                    let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
                    window.focus(&sql.focus_handle(cx), cx);
                    AppView {
                        grid,
                        status: "ready".into(),
                        runner: QueryRunner::new(),
                        conn_url,
                        sql,
                        cancel: None,
                        started_at: None,
                    }
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
