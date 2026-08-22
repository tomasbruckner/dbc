mod connect;
mod connections_ui;
mod grid;
mod runner;
mod schema_tree;
mod sql_input;
mod tabs;
mod text_model;
mod tunnel;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::{apply_auto_limit, is_read_statement, CancelToken, QueryError};
use dbc_state::{AppConfig, Vault};
use gpui::{
    actions, div, prelude::*, px, rgb, size, AnyElement, App, Bounds, ClipboardItem, Context,
    Entity, Focusable, KeyBinding, ScrollDelta, ScrollWheelEvent, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use grid::ResultGrid;
use runner::{ConnectSpec, QueryEvent, QueryRunner};
use schema_tree::{SchemaTree, TreeEvent};
use sql_input::SqlInput;
use tabs::{collapse_title, ResultTab, TabContent, Tabs};

actions!(dbc, [RunQuery, RunQueryUnlimited, CancelQuery, ToggleTree]);

struct AppView {
    tabs: Tabs,
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
    // --- G2 Task 6: schema tree panel ---
    /// Loading/error/snapshot state lives on the entity itself, driven by
    /// direct mutation from `trigger_schema_fetch` (see schema_tree.rs's
    /// header comment for why this isn't done via `TreeEvent` instead).
    tree: Entity<SchemaTree>,
    /// Ctrl+B (`ToggleTree`, app action, binding context `None`). `false`
    /// means the panel isn't rendered at all (0 px), not just visually
    /// hidden.
    tree_visible: bool,
    /// Bumped on every `trigger_schema_fetch` dispatch; a fetch result only
    /// applies if the generation still matches (last-dispatched wins — same
    /// pattern as `switch_generation`). Fixes review Issue 1: without this,
    /// a slow fetch for a connection the user has since switched away from
    /// can resolve after a faster fetch for the new connection and silently
    /// overwrite the tree with the wrong connection's schema.
    schema_fetch_generation: u64,
    /// Identity (see `conn_spec_key`) of the connection whose schema is
    /// currently being fetched/shown in `tree`, so `trigger_schema_fetch`
    /// can tell `SchemaTree::set_snapshot` whether an incoming snapshot is a
    /// same-connection refresh (preserve expand/filter/selection) or a
    /// switch to a different connection (reset them) — review Issue 3.
    schema_tree_connection_key: Option<String>,
}

/// Stable identity for a `ConnectSpec`, used only to decide whether two
/// `trigger_schema_fetch` dispatches target the "same connection" (see
/// `schema_tree_connection_key`) — not used for anything security-sensitive,
/// so the secret on `ConnectSpec::Config` is deliberately not part of it.
fn conn_spec_key(spec: &ConnectSpec) -> String {
    match spec {
        ConnectSpec::Config { cfg, .. } => format!("cfg:{}", cfg.id),
        ConnectSpec::Url(u) => format!("url:{u}"),
    }
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

        // Captured for the new tab's title (single-line-collapsed SQL, see
        // `tabs::collapse_title`) — the actual SQL text being run, i.e.
        // post-auto-limit-rewrite.
        let sql_for_title = sql.clone();
        let mut rx = self.runner.connect_and_run(spec, sql, cancel, timeout_secs);
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            // Set once a buffer push fails; suppresses further batch
            // processing for this run while the cancel we just fired
            // propagates through the driver.
            let mut errored = false;
            // This run's own tab id, set once `Started` opens it. `Batch`
            // events target this tab specifically (by id, not "the active
            // tab") — if the tab was closed mid-stream, the run cancels
            // itself and stops consuming further events.
            let mut tab_id: Option<u64> = None;
            while let Some(ev) = rx.recv().await {
                let stop = this
                    .update(cx, |view, cx| {
                        let mut stop = false;
                        match ev {
                            QueryEvent::Started { columns } => {
                                let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
                                buffer = Some(buf.clone());
                                let grid = cx.new(ResultGrid::new);
                                grid.update(cx, |g, _| g.set_buffer(buf.clone()));
                                let title = collapse_title(&sql_for_title);
                                let id = view.tabs.open(ResultTab {
                                    id: 0,
                                    title,
                                    pinned: false,
                                    content: TabContent::Grid { grid, buffer: buf },
                                });
                                tab_id = Some(id);
                                view.status = format!("running…{limit_suffix}");
                            }
                            QueryEvent::Batch(b) => {
                                if errored {
                                    // Already failed and cancelled this run —
                                    // drop any further in-flight batches.
                                } else if tab_id.is_some_and(|id| view.tabs.iter().all(|t| t.id != id)) {
                                    // This run's tab was closed mid-stream —
                                    // cancel and stop consuming; nothing left
                                    // to render the remaining batches into.
                                    stop = true;
                                    if let Some(token) = view.cancel.take() {
                                        token.cancel();
                                    }
                                    view.status = "zrušeno (tab zavřen)".into();
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
                        stop
                    })
                    .unwrap_or(false);
                if stop {
                    break;
                }
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

    fn on_toggle_tree(&mut self, _: &ToggleTree, _window: &mut Window, cx: &mut Context<Self>) {
        self.tree_visible = !self.tree_visible;
        cx.notify();
    }

    /// Builds the `ConnectSpec` for the *currently active* connection (saved
    /// config or CLI-arg URL) — used by the schema tree's initial fetch and
    /// its `RefreshRequested` handler. Unlike `run_query`'s spec, callers
    /// here don't need `read_only`/`auto_limit`/`timeout_secs`, so this just
    /// returns the spec. `None` means there's nothing to fetch a schema for
    /// (tree shows "Bez připojení").
    fn active_conn_spec(&self) -> Option<ConnectSpec> {
        if let Some(id) = self.active_connection_id.clone() {
            let cfg = self.config.connections.iter().find(|c| c.id == id)?.clone();
            let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
            Some(ConnectSpec::Config { cfg: Box::new(cfg), secret })
        } else {
            self.conn_url.clone().map(ConnectSpec::Url)
        }
    }

    /// Dispatches `runner.fetch_schema(spec)` off the UI thread and updates
    /// `self.tree`'s loading/snapshot/error state as it resolves — same
    /// "UI thread only ever awaits a channel via `cx.spawn`" shape as
    /// `run_query`/`switch_to_connection`. Called from the
    /// `switch_to_connection` success arm, `TreeEvent::RefreshRequested`,
    /// and once at CLI-arg startup (see `main`).
    ///
    /// Guarded by `schema_fetch_generation` (review Issue 1, mirroring
    /// `switch_generation`): every dispatch bumps the counter and captures
    /// it, and the `cx.spawn` completion drops its result if the generation
    /// has since moved on — so a slow fetch for a connection the user has
    /// already switched away from can never overwrite a newer one
    /// (last-dispatched wins, not last-resolved).
    fn trigger_schema_fetch(&mut self, spec: ConnectSpec, cx: &mut Context<Self>) {
        self.tree.update(cx, |t, cx| t.set_loading(cx));
        let key = conn_spec_key(&spec);
        let same_connection = self.schema_tree_connection_key.as_deref() == Some(key.as_str());
        self.schema_tree_connection_key = Some(key);
        self.schema_fetch_generation += 1;
        let my_generation = self.schema_fetch_generation;
        let rx = self.runner.fetch_schema(spec);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                // A newer fetch was dispatched meanwhile — this result is
                // stale, drop it (last-dispatched wins).
                if view.schema_fetch_generation != my_generation {
                    return;
                }
                view.tree.update(cx, |t, cx| match result {
                    Ok(Ok(snapshot)) => t.set_snapshot(snapshot, same_connection, cx),
                    Ok(Err(e)) => t.set_error(e.to_string(), cx),
                    Err(_) => t.set_error("fetch zrušen".to_string(), cx),
                });
            });
        })
        .detach();
    }

    /// `SchemaTree`'s `TreeEvent` subscription (wired in `main`).
    /// `OpenPreview`/`OpenDdl` are stubbed here — Task 7 wires them to
    /// actually open a preview/DDL tab; this task just needs the
    /// subscription seam to exist and be exercised end-to-end.
    fn on_tree_event(&mut self, _emitter: Entity<SchemaTree>, event: &TreeEvent, cx: &mut Context<Self>) {
        match event {
            TreeEvent::OpenPreview { schema, table } => {
                let qualified = match schema {
                    Some(s) => format!("{s}.{table}"),
                    None => table.clone(),
                };
                self.status = format!("Náhled {qualified} přijde v T7");
                cx.notify();
            }
            TreeEvent::OpenDdl { title, ddl } => {
                self.status = format!("DDL {title} ({} znaků) přijde v T7", ddl.len());
                cx.notify();
            }
            TreeEvent::RefreshRequested => {
                if let Some(spec) = self.active_conn_spec() {
                    self.trigger_schema_fetch(spec, cx);
                } else {
                    self.schema_tree_connection_key = None;
                    self.tree.update(cx, |t, cx| t.clear(cx));
                }
            }
        }
    }

    /// Tab strip between the SQL editor and result content: title +
    /// row-count badge (`Grid` tabs read `buffer.row_count()` fresh at
    /// render time rather than caching it on the tab) + pin toggle + close.
    /// Click activates. Active tab bg 0x313244, inactive 0x181825. Only
    /// called when there's at least one open tab (see `Render::render`).
    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_id = self.tabs.active().map(|t| t.id);
        let rows: Vec<(u64, String, bool, usize)> = self
            .tabs
            .iter()
            .map(|t| {
                let row_count = match &t.content {
                    TabContent::Grid { buffer, .. } => buffer.borrow().row_count(),
                    TabContent::Text { .. } => 0,
                };
                (t.id, t.title.clone(), t.pinned, row_count)
            })
            .collect();

        let mut strip = div().id("tab-strip").flex().flex_row().h(px(28.)).bg(rgb(0x181825));
        for (id, title, pinned, row_count) in rows {
            let is_active = Some(id) == active_id;
            let bg = if is_active { rgb(0x313244) } else { rgb(0x181825) };
            let pin_color = if pinned { rgb(0xf9e2af) } else { rgb(0x6c7086) };
            strip = strip.child(
                div()
                    .id(("tab", id as usize))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h_full()
                    .bg(bg)
                    .text_color(rgb(0xcdd6f4))
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.tabs.activate(id);
                        cx.notify();
                    }))
                    .child(format!("{title} ({row_count})"))
                    .child(
                        div()
                            .id(("tab-pin", id as usize))
                            .px_1()
                            .cursor_pointer()
                            .text_color(pin_color)
                            .child("📌")
                            .on_click(cx.listener(move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.tabs.toggle_pin(id);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(("tab-close", id as usize))
                            .px_1()
                            .cursor_pointer()
                            .child("✕")
                            .on_click(cx.listener(move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.tabs.close(id);
                                cx.notify();
                            })),
                    ),
            );
        }
        strip
    }

    /// Only the active tab's content renders. `Grid` tabs render their own
    /// `Entity<ResultGrid>`; `Text` tabs render read-only monospace lines
    /// (scrolled via `scroll_lines`, mutated by mouse wheel) plus a
    /// "Kopírovat" button that copies the whole text to the clipboard. With
    /// no tabs open at all, renders a neutral placeholder.
    fn render_tab_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(active) = self.tabs.active() else {
            return div().flex_1().bg(rgb(0x1e1e2e)).into_any_element();
        };

        match &active.content {
            TabContent::Grid { grid, .. } => grid.clone().into_any_element(),
            TabContent::Text { text, scroll_lines } => {
                let lines: Vec<&str> = text.lines().collect();
                let scroll = (*scroll_lines).min(lines.len());
                let text_for_copy = text.clone();

                let mut body = div()
                    .id("tab-text-body")
                    .font_family("Consolas")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .p_2()
                    .text_color(rgb(0xcdd6f4))
                    .on_scroll_wheel(cx.listener(|view, e: &ScrollWheelEvent, _, cx| {
                        let delta_lines = match e.delta {
                            ScrollDelta::Lines(p) => p.y,
                            ScrollDelta::Pixels(p) => p.y.as_f32() / 20.0,
                        };
                        if let Some(TabContent::Text { text, scroll_lines }) =
                            view.tabs.active_mut().map(|t| &mut t.content)
                        {
                            let max_scroll = text.lines().count().saturating_sub(1);
                            let current = *scroll_lines as f32;
                            let new_scroll = (current - delta_lines).round();
                            *scroll_lines = new_scroll.max(0.0).min(max_scroll as f32) as usize;
                        }
                        cx.notify();
                    }));
                for line in &lines[scroll..] {
                    body = body.child(div().child(line.to_string()));
                }

                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .bg(rgb(0x1e1e2e))
                    .child(
                        div().flex().flex_row().justify_end().p_1().child(
                            div()
                                .id("tab-copy")
                                .cursor_pointer()
                                .bg(rgb(0x313244))
                                .text_color(rgb(0xcdd6f4))
                                .px_2()
                                .rounded_md()
                                .child("Kopírovat")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text_for_copy.clone()));
                                })),
                        ),
                    )
                    .child(body)
                    .into_any_element()
            }
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The SQL editor + tab strip + tab content column, unchanged from
        // pre-Task-6 except that it's now one column in a horizontal row
        // rather than filling the whole window body.
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(
                // Fixed height of 8 lines (SqlInput's own line_height is
                // px(20.), see sql_input.rs render()); the input scrolls
                // internally once the buffer grows past that.
                div()
                    .h(px(20. * 8. + 4. * 2.))
                    .px_2()
                    .bg(rgb(0x181825))
                    .child(self.sql.clone()),
            );

        // Tab strip only renders when there's at least one open tab (brief
        // contract #2); with none, `render_tab_content` fills the area with
        // a neutral placeholder instead.
        if self.tabs.iter().next().is_some() {
            column = column.child(self.render_tab_strip(cx));
        }
        column = column.child(self.render_tab_content(cx));

        // G2 Task 6: the schema tree panel sits LEFT of `column`, fixed
        // 260 px, collapsible via Ctrl+B (`ToggleTree`) — collapsed means
        // not rendered at all (width 0), not just visually hidden.
        let mut body = div().flex().flex_row().flex_1().min_h_0();
        if self.tree_visible {
            body = body.child(
                div()
                    .w(px(260.))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(rgb(0x45475a))
                    .child(self.tree.clone()),
            );
        }
        body = body.child(column);

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .on_action(cx.listener(Self::on_run_query))
            .on_action(cx.listener(Self::on_run_query_unlimited))
            .on_action(cx.listener(Self::on_cancel_query))
            .on_action(cx.listener(Self::on_toggle_tree))
            .child(self.render_top_bar(cx))
            .child(body)
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
            KeyBinding::new("ctrl-b", ToggleTree, None),
        ]);
        sql_input::bind_keys(cx);
        grid::bind_keys(cx);
        connections_ui::bind_keys(cx);
        schema_tree::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let window_handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
                        window.focus(&sql.focus_handle(cx), cx);
                        let grouped_cache = connections_ui::group_connections(&config.connections);
                        let status = match &config_load_error {
                            Some(detail) => {
                                format!("error: config.toml je poškozený – oprav nebo smaž soubor ({detail})")
                            }
                            None => "ready".into(),
                        };
                        let editor_focus = sql.focus_handle(cx);
                        let tree = cx.new(|cx| SchemaTree::new(cx, editor_focus));
                        cx.subscribe(&tree, AppView::on_tree_event).detach();
                        AppView {
                            tabs: Tabs::new(),
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
                            tree,
                            tree_visible: true,
                            schema_fetch_generation: 0,
                            schema_tree_connection_key: None,
                        }
                    })
                },
            )
            .unwrap();
        cx.activate(true);

        // CLI-arg back-compat startup path (brief contract #6): also fires
        // the initial schema fetch, exactly like a dropdown connection
        // switch does — `active_conn_spec` reads `conn_url` when no saved
        // connection is active yet, which is always true this early.
        let _ = window_handle.update(cx, |view, _window, cx| {
            if let Some(spec) = view.active_conn_spec() {
                view.trigger_schema_fetch(spec, cx);
            }
        });
    });
}
