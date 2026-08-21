mod grid;
mod runner;

use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::CancelToken;
use dbc_driver_sqlite::SqliteConnection;
use gpui::{
    div, prelude::*, px, rgb, size, App, Bounds, Context, Entity, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use grid::ResultGrid;
use runner::{QueryEvent, QueryRunner};

struct AppView {
    grid: Entity<ResultGrid>,
    status: String,
}

impl AppView {
    fn run_startup_query(&mut self, db_path: String, cx: &mut Context<Self>) {
        let runner = QueryRunner::new();
        let conn = Box::new(SqliteConnection::new(db_path));
        let mut rx = runner.run(conn, "SELECT name, type FROM sqlite_master".into(), CancelToken::new());
        // Keep the runtime alive for the app's lifetime.
        std::mem::forget(runner); // phase 1 only; task 8 moves ownership into AppView
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
                            view.status = "running…".into();
                        }
                        QueryEvent::Batch(b) => {
                            if let Some(buf) = &buffer { buf.borrow_mut().push(b); }
                        }
                        QueryEvent::Finished { elapsed } => {
                            let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                            view.status = format!("{rows} rows in {elapsed:.2?}");
                        }
                        QueryEvent::Failed(e) => { view.status = format!("error: {e}"); }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
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
    let db_path = std::env::args().nth(1).expect("usage: dbc-ui <sqlite-file>");
    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    let grid = cx.new(|_| ResultGrid::new());
                    let mut view = AppView { grid, status: "connecting…".into() };
                    view.run_startup_query(db_path, cx);
                    view
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
