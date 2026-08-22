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
use dbc_core::{apply_auto_limit, is_read_statement, CancelToken, QueryError};
use dbc_state::{AppConfig, Vault};
use gpui::{
    actions, div, prelude::*, px, rgb, size, App, Bounds, Context, Entity, Focusable, KeyBinding,
    Window, WindowBounds, WindowOptions,
};
use gpui_platform::application;
use grid::ResultGrid;
use runner::{ConnectSpec, QueryEvent, QueryRunner};
use sql_input::SqlInput;

actions!(dbc, [RunQuery, RunQueryUnlimited, CancelQuery]);

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
    /// Set when `AppConfig::load` failed to parse an existing config.toml at
    /// startup (surfaced in the status bar; see `main`). Cleared by
    /// `finish_save` once the corrupt file has been safely moved aside to
    /// `config.toml.corrupt-bak` — never overwritten silently (final-review
    /// must-fix #2).
    config_load_error: Option<String>,
    vault_path: PathBuf,
    /// Unlocked vault, kept for the session once the user has entered the
    /// master password once (brief: prompt on first use, not at startup).
    vault: Option<Vault>,
    active_connection_id: Option<String>,
    /// Bumped on every dropdown connection switch; a switch result only
    /// applies if the generation still matches (last-dispatched wins, not
    /// last-resolved).
    switch_generation: u64,
    dropdown_open: bool,
    modal: Option<connections_ui::ModalState>,
    /// Cached folder/favourite grouping of `config.connections`, recomputed
    /// on dropdown-open and after config mutations (see
    /// `AppView::refresh_grouped_cache`) rather than on every render frame.
    grouped_cache: connections_ui::GroupedConnections,
}

impl AppView {
    fn on_run_query(&mut self, _: &RunQuery, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_query(false, cx);
    }

    /// `Ctrl+Shift+Enter`: bypasses ONLY the auto-limit guard. Read-only
    /// enforcement is not a "per-run convenience" the way auto-limit is —
    /// it stays enforced regardless of how the query was launched.
    fn on_run_query_unlimited(
        &mut self,
        _: &RunQueryUnlimited,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_query(true, cx);
    }

    /// Guard order (brief, Task 8): (1) read-only — rejected without ever
    /// connecting; (2) auto-limit — rewrites the SQL text, unless bypassed;
    /// (3) timeout — enforced inside `QueryRunner::connect_and_run`, since it
    /// must race the whole connect+query sequence, not just this call.
    fn run_query(&mut self, bypass_auto_limit: bool, cx: &mut Context<Self>) {
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

        let spec = if let Some(id) = self.active_connection_id.clone() {
            let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return;
            };
            let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
            (cfg.read_only, cfg.auto_limit, cfg.timeout_secs, ConnectSpec::Config { cfg: Box::new(cfg), secret })
        } else if let Some(url) = self.conn_url.clone() {
            // CLI-arg back-compat path: no read-only/auto-limit/timeout
            // config exists for it (no ConnectionConfig backs it).
            (false, None, None, ConnectSpec::Url(url))
        } else {
            self.status = "Bez připojení — vyberte připojení nahoře.".into();
            cx.notify();
            return;
        };
        let (read_only, auto_limit, timeout_secs, spec) = spec;

        // Guard 1: read-only — rejected client-side without connecting.
        // (Server-side enforcement lives in connect::open_config: Postgres
        // `default_transaction_read_only=on`, SQLite `SQLITE_OPEN_READ_ONLY`
        // — this check is the fast, no-connection-needed first line, not the
        // only line.)
        if read_only && !is_read_statement(&sql) {
            let err = QueryError::msg("connection is read-only");
            self.status = format!("error: {err}");
            cx.notify();
            return;
        }

        // Guard 2: auto-limit.
        let mut sql = sql;
        let mut limit_suffix = String::new();
        if !bypass_auto_limit {
            if let Some(n) = auto_limit {
                let (rewritten, changed) = apply_auto_limit(&sql, n);
                if changed {
                    sql = rewritten;
                    limit_suffix = format!(" · auto-LIMIT {n}");
                }
            }
        }

        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.started_at = Some(std::time::Instant::now());
        self.status = format!("connecting…{limit_suffix}");
        cx.notify();

        let mut rx = self.runner.connect_and_run(spec, sql, cancel, timeout_secs);
        let grid = self.grid.clone();
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            // Set once a buffer push fails; suppresses further batch
            // processing for this run while the cancel we just fired
            // propagates through the driver.
            let mut errored = false;
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |view, cx| {
                    match ev {
                        QueryEvent::Started { columns } => {
                            let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
                            buffer = Some(buf.clone());
                            grid.update(cx, |g, _| g.set_buffer(buf));
                            view.status = format!("running…{limit_suffix}");
                        }
                        QueryEvent::Batch(b) => {
                            if errored {
                                // Already failed and cancelled this run —
                                // drop any further in-flight batches.
                            } else if let Some(Err(e)) =
                                buffer.as_ref().map(|buf| buf.borrow_mut().push(b))
                            {
                                errored = true;
                                view.status = format!("error: {e}");
                                if let Some(token) = view.cancel.take() {
                                    token.cancel();
                                }
                            } else {
                                let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                                let secs =
                                    view.started_at.map_or(0.0, |t| t.elapsed().as_secs_f32());
                                view.status = format!("{rows} rows… {secs:.1}s{limit_suffix}");
                            }
                        }
                        QueryEvent::Finished { elapsed } => {
                            // A queued Finished must not clobber a spill
                            // error with a fake success status.
                            if !errored {
                                let rows =
                                    buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                                view.status =
                                    format!("{rows} rows in {elapsed:.2?}{limit_suffix}");
                            }
                            view.cancel = None;
                        }
                        QueryEvent::Failed(e) => {
                            // Same guard: the push error is the root cause;
                            // the driver's follow-up "cancelled" is noise.
                            if !errored {
                                view.status = format!("error: {e}");
                            }
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
        // M6: Escape closes the dropdown / a modal first, rather than
        // falling through to query-cancel underneath it. A modal holding
        // unsaved password state (a master-password prompt/creation modal,
        // or the connection dialog with a non-empty password field) is
        // deliberately NOT closed by Escape — same "no accidental dismissal
        // while a password is typed" reasoning as the overlay `.occlude()`
        // fix.
        if self.dropdown_open {
            self.dropdown_open = false;
            cx.notify();
            return;
        }
        if let Some(modal) = self.modal.clone() {
            let closable = match &modal {
                connections_ui::ModalState::ConnectionDialog(ui) => ui.password.read(cx).text().is_empty(),
                _ => false,
            };
            if closable {
                self.close_modal(cx);
            }
            return;
        }
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
            .on_action(cx.listener(Self::on_run_query_unlimited))
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
    // A parse error (as opposed to a missing file, which `AppConfig::load`
    // treats as an empty default) means an existing config.toml is
    // corrupt — surfaced in the status bar below rather than silently
    // discarded (final-review must-fix #2). `finish_save` refuses to
    // overwrite the file until it's been moved aside.
    let (config, config_load_error) = match AppConfig::load(&config_path) {
        Ok(cfg) => (cfg, None),
        Err(e) => (AppConfig::default(), Some(e.to_string())),
    };

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("ctrl-shift-enter", RunQueryUnlimited, None),
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
                    let grouped_cache = connections_ui::group_connections(&config.connections);
                    let status = match &config_load_error {
                        Some(detail) => {
                            format!("error: config.toml je poškozený – oprav nebo smaž soubor ({detail})")
                        }
                        None => "ready".into(),
                    };
                    AppView {
                        grid,
                        status,
                        runner: QueryRunner::new(),
                        conn_url,
                        sql,
                        cancel: None,
                        started_at: None,
                        config,
                        config_path,
                        config_load_error,
                        vault_path,
                        vault: None,
                        active_connection_id: None,
                        switch_generation: 0,
                        dropdown_open: false,
                        modal: None,
                        grouped_cache,
                    }
                })
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
