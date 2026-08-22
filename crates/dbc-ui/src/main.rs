mod connect;
mod connections_ui;
mod grid;
mod runner;
mod sql_input;
mod text_model;
mod tunnel;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::CancelToken;
use dbc_state::{AppConfig, Vault};
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
    /// Back-compat CLI-arg connection string (phase 0-2 path). `None` when
    /// the app was started with no argument (Task 7's new startup path) or
    /// once a saved connection has been switched to.
    conn_url: Option<String>,
    sql: Entity<SqlInput>,
    cancel: Option<CancelToken>,
    started_at: Option<std::time::Instant>,
    // --- Task 7: connection manager state ---
    config: AppConfig,
    config_path: PathBuf,
    vault_path: PathBuf,
    /// Unlocked vault, kept for the session once the user has entered the
    /// master password once (brief: prompt on first use, not at startup).
    vault: Option<Vault>,
    active_connection_id: Option<String>,
    dropdown_open: bool,
    modal: Option<connections_ui::ModalState>,
}

impl AppView {
    fn on_run_query(&mut self, _: &RunQuery, _window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return; // don't run queries under a modal
        }
        if self.cancel.is_some() {
            return; // one query at a time in v1
        }
        let sql = self.sql.read(cx).text();
        if sql.trim().is_empty() {
            return;
        }
        // Back-compat CLI-arg path only for now — connecting via a saved
        // ConnectionConfig (`active_connection_id`) is Task 8's seam; see
        // connections_ui::pending_connect's doc comment.
        let Some(url) = self.conn_url.clone() else {
            self.status = if self.active_connection_id.is_some() {
                "connect flow lands in Task 8".into()
            } else {
                "Bez připojení — vyberte připojení nahoře.".into()
            };
            cx.notify();
            return;
        };
        let conn = match connect::open(&url, &self.runner.handle()) {
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
        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .on_action(cx.listener(Self::on_run_query))
            .on_action(cx.listener(Self::on_cancel_query))
            .child(self.render_top_bar(cx))
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
            );

        if self.dropdown_open && self.modal.is_none() {
            root = root.child(self.render_dropdown_overlay(cx));
        }
        if let Some(overlay) = self.render_modal_overlay(cx) {
            root = root.child(overlay);
        }
        root
    }
}

fn main() {
    // CLI arg is now optional: back-compat direct-connect path (phase 0-2)
    // when present, otherwise the app starts with no active connection and
    // the user picks one from the top-bar switcher (Task 7).
    let conn_url = std::env::args().nth(1);
    let config_path = dbc_state::default_config_path();
    let vault_path = dbc_state::default_vault_path();
    let config = AppConfig::load(&config_path).unwrap_or_default();

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("escape", CancelQuery, None),
        ]);
        sql_input::bind_keys(cx);
        grid::bind_keys(cx);
        connections_ui::bind_keys(cx);

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
                        config,
                        config_path,
                        vault_path,
                        vault: None,
                        active_connection_id: None,
                        dropdown_open: false,
                        modal: None,
                    }
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
