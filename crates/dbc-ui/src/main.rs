mod connect;
mod connections_ui;
mod grid;
mod history_panel;
mod palette;
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
use dbc_core::{apply_auto_limit, is_read_statement, quote_qualified, CancelToken, QueryError};
use dbc_state::{AppConfig, HistoryDb, HistoryEntry, Vault};
use gpui::{
    actions, div, prelude::*, px, rgb, rgba, size, AnyElement, App, Bounds, ClipboardItem,
    Context, Entity, Focusable, KeyBinding, ScrollDelta, ScrollWheelEvent, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use grid::ResultGrid;
use palette::{PaletteAction, PaletteItem};
use runner::{ConnectSpec, QueryEvent, QueryRunner};
use schema_tree::{SchemaTree, TreeEvent};
use sql_input::SqlInput;
use tabs::{collapse_title, ResultTab, TabContent, Tabs};

actions!(dbc, [RunQuery, RunQueryUnlimited, CancelQuery, ToggleTree, ToggleHistory, OpenPalette]);

/// G3 Task 5: Ctrl+K command palette state — created on `OpenPalette`,
/// dropped on close/execute. `items`/`selected` are recomputed from
/// `input`'s text (see `AppView::refresh_palette_items`), polled lazily at
/// render time the same way `history_search`/`last_history_query` are (see
/// history_panel.rs's module doc comment) rather than via an on-change hook
/// `connections_ui::TextField` doesn't have.
struct PaletteState {
    input: Entity<connections_ui::TextField>,
    items: Vec<PaletteItem>,
    selected: usize,
    /// The text `items` was last computed from — compared against `input`'s
    /// live text each render to detect an edit.
    last_query: String,
}

/// G2 Task 7: SQL builder for `TreeEvent::OpenPreview`. Pure — no GPUI, no
/// I/O — so quoting can be unit-tested directly. `quote_qualified` (shared
/// with `synthesize_create_table`'s DDL quoting) is what makes this safe
/// against a table literally named `we"ird`: the embedded quote is doubled,
/// not smuggled into the query as SQL syntax.
fn preview_sql(schema: Option<&str>, table: &str) -> String {
    format!("SELECT * FROM {} LIMIT 1000", quote_qualified(schema, table))
}

/// Set by `TreeEvent::OpenPreview` and threaded through `run_query_with` so
/// a preview runs through the exact same guarded pipeline as an
/// editor-typed query, without ever touching `self.sql`'s text: `title`
/// overrides the tab's title (`collapse_title(sql)` is used otherwise), and
/// `key` is the tab's `preview_key` — matched by `Tabs::close_by_preview_key`
/// so re-previewing the same (schema, table) replaces rather than stacks
/// (brief contract #1).
struct PreviewTarget {
    title: String,
    key: String,
}

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
    // --- G3 Task 3: history panel + query recording ---
    /// Opened from `default_history_path()` at startup; `None` when the open
    /// failed (surfaced once in the startup status — see `main`), in which
    /// case the app stays fully functional, just without recording/search
    /// (`record_history` and the panel's search both no-op gracefully).
    history: Option<HistoryDb>,
    /// Ctrl+H (`ToggleHistory`, app action, binding context `None`) — same
    /// "not rendered at all when hidden" convention as `tree_visible`.
    history_visible: bool,
    /// Search box for the history panel (unmasked `TextField`, reused from
    /// connections_ui.rs). Its text is polled (cheap string compare) at the
    /// start of every `render_history_panel` call against
    /// `last_history_query` to detect an edit — see history_panel.rs's
    /// module doc comment for the full caching strategy.
    history_search: Entity<connections_ui::TextField>,
    /// Cached result of the last `HistoryDb::search`, recomputed only by
    /// `AppView::refresh_history_cache` (startup, after a recorded run, star
    /// toggle, ToggleHistory-on, and search-text change detected in
    /// `render_history_panel`) rather than on every render frame — same
    /// precedent as `grouped_cache`. Post-review fix for Task 3 review
    /// Issue 1 (unindexed full-table sort on every window repaint).
    history_cache: Vec<HistoryEntry>,
    /// The search text `history_cache` was last computed from, compared
    /// against `history_search`'s live text each render to decide whether a
    /// refresh is needed (see `history_search`'s doc comment).
    last_history_query: String,
    // --- G3 Task 5: Ctrl+K command palette ---
    /// `None` when the palette isn't open — same "not rendered at all"
    /// convention as `modal`, and mutually exclusive with it (see
    /// `on_open_palette`/`render_palette_overlay`).
    palette: Option<PaletteState>,
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
    ///
    /// Reads the SQL straight from the editor and delegates to
    /// `run_query_with` — the editor-typed-query path, as opposed to a
    /// preview's `run_query_with` call (see `on_tree_event`'s
    /// `TreeEvent::OpenPreview` arm), which supplies its own SQL/title and
    /// never touches `self.sql`.
    fn run_query(&mut self, bypass_auto_limit: bool, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).text();
        if sql.trim().is_empty() {
            return;
        }
        self.run_query_with(sql, None, bypass_auto_limit, cx);
    }

    /// The actual guarded run pipeline (guard order per the doc comment on
    /// `run_query`), shared by an editor-typed query (`run_query`, `preview
    /// == None`) and a schema-tree preview (`TreeEvent::OpenPreview`,
    /// `preview == Some(..)`). `sql` is whatever the caller wants executed —
    /// for a preview this is `preview_sql`'s output, never the editor's
    /// text. `bypass_auto_limit` is still the caller's choice (a preview
    /// always passes `true` since it already carries its own `LIMIT`).
    fn run_query_with(
        &mut self,
        sql: String,
        preview: Option<PreviewTarget>,
        bypass_auto_limit: bool,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            return; // don't run queries under a modal
        }
        if self.cancel.is_some() {
            return; // one query at a time in v1
        }
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
        // post-auto-limit-rewrite. Unused when `preview` overrides the title
        // (still harmless to compute — the collapse is cheap).
        let sql_for_title = sql.clone();
        // G3 Task 3: captured at dispatch (not resolution) for
        // `record_history` — the unix time the run started, and the active
        // connection's name (or "cli" for the CLI-arg path), both fixed for
        // the lifetime of this run regardless of what the user does
        // meanwhile (e.g. switching connections while this query runs).
        let history_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let history_conn_name = self.active_connection_name_for_history();
        let mut rx = self.runner.connect_and_run(spec, sql, cancel, timeout_secs);
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            // Set (to the buffer-push error text) once a buffer push fails;
            // suppresses further batch processing for this run while the
            // cancel we just fired propagates through the driver. The
            // captured text is what actually gets recorded to history when
            // the run's terminal event (`Finished` or `Failed` — the driver
            // sends exactly one) eventually arrives — review Issue 2: a
            // spill failure must still produce a history entry.
            let mut errored: Option<String> = None;
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
                                let title = preview
                                    .as_ref()
                                    .map(|p| p.title.clone())
                                    .unwrap_or_else(|| collapse_title(&sql_for_title));
                                // Brief contract #1: re-preview of the same
                                // (schema, table) replaces its existing
                                // preview tab rather than stacking a
                                // duplicate — must happen before `open` so
                                // the closed tab never overlaps the new one.
                                if let Some(p) = &preview {
                                    view.tabs.close_by_preview_key(&p.key);
                                }
                                let id = view.tabs.open(ResultTab {
                                    id: 0,
                                    title,
                                    pinned: false,
                                    preview_key: preview.as_ref().map(|p| p.key.clone()),
                                    content: TabContent::Grid { grid, buffer: buf },
                                });
                                tab_id = Some(id);
                                view.status = format!("running…{limit_suffix}");
                            }
                            QueryEvent::Batch(b) => {
                                if errored.is_some() {
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
                                    let err_text = e.to_string();
                                    view.status = format!("error: {err_text}");
                                    errored = Some(err_text);
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
                            // The driver sends exactly one terminal event
                            // per run (`Finished` xor `Failed` —
                            // `runner::stream_query`), so exactly one of
                            // these two arms fires, and each records
                            // history exactly once (review Issue 2): when a
                            // buffer-push spill error already latched
                            // (`errored`), record that as the failed entry
                            // (its text is the real root cause; a queued
                            // `Finished`'s fake success or `Failed`'s
                            // redundant "cancelled" text would be wrong)
                            // and leave the status bar alone — it already
                            // shows the spill error from the `Batch` arm
                            // above (bb2dd7c: never clobber it with a stale
                            // status). Otherwise record the terminal
                            // event's own outcome and update the status bar
                            // as before.
                            QueryEvent::Finished { elapsed } => {
                                match &errored {
                                    None => {
                                        let rows =
                                            buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                                        view.status =
                                            format!("{rows} rows in {elapsed:.2?}{limit_suffix}");
                                        // G3 Task 3: record the run (previews
                                        // included — they run real SQL too).
                                        // Fire-and-forget; a write failure never
                                        // surfaces here.
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            Some(elapsed.as_millis() as i64),
                                            Some(rows as i64),
                                            None,
                                            cx,
                                        );
                                    }
                                    Some(err_text) => {
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            None,
                                            None,
                                            Some(err_text),
                                            cx,
                                        );
                                    }
                                }
                                view.cancel = None;
                            }
                            QueryEvent::Failed(e) => {
                                match &errored {
                                    None => {
                                        view.status = format!("error: {e}");
                                        let err_text = e.to_string();
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            None,
                                            None,
                                            Some(&err_text),
                                            cx,
                                        );
                                    }
                                    Some(err_text) => {
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            None,
                                            None,
                                            Some(err_text),
                                            cx,
                                        );
                                    }
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
        // G3 Task 5: this check is THE mechanism that makes Esc close the
        // palette — do not remove it as "redundant". The palette's scoped
        // "escape" binding (context "Palette", palette.rs `bind_keys`) does
        // NOT win GPUI's keymap resolution: focus sits on the palette's
        // nested TextField, so "Palette" is an ancestor context, and per the
        // pinned gpui's `keymap.rs::bindings_for_input` an unscoped binding
        // (this `escape → CancelQuery`) outranks ancestor-scoped ones.
        // Verified against the vendored source in the Task 5 review.
        if self.palette.is_some() {
            self.palette = None;
            cx.notify();
            return;
        }
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

    fn on_toggle_history(&mut self, _: &ToggleHistory, _window: &mut Window, cx: &mut Context<Self>) {
        self.history_visible = !self.history_visible;
        if self.history_visible {
            // The cache may be stale relative to runs recorded while the
            // panel was hidden (record_history's own refresh still ran, but
            // this is cheap insurance and matches the review's explicit
            // "ToggleHistory-on" trigger list).
            self.refresh_history_cache(cx);
        }
        cx.notify();
    }

    /// Ctrl+K (brief contract #1). Guarded against another modal being up
    /// (contract #5) — the reverse (a modal opening while the palette is up)
    /// is prevented for free by the palette overlay's `.occlude()` blocking
    /// the clicks that would open one (top-bar/dropdown), same as the
    /// existing modal overlay does for the dropdown. Also closes the
    /// connection dropdown if it happened to be open, so the two overlays
    /// never stack. Sources are assembled fresh on every open (contract #2).
    fn on_open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.palette.is_some() {
            return;
        }
        self.dropdown_open = false;
        let input = cx.new(|cx| connections_ui::TextField::new(cx, "Ctrl+K – tabulky, historie, spojení, akce…", false));
        let focus = input.focus_handle(cx);
        let items = self.build_palette_items("", cx);
        self.palette = Some(PaletteState { input, items, selected: 0, last_query: String::new() });
        // G1 lesson (binding per the brief): focus must move to the
        // palette's own input in the SAME update the overlay appears in, or
        // a stray keystroke lands on whatever had focus before Ctrl+K.
        window.focus(&focus, cx);
        cx.notify();
    }

    fn on_palette_up(&mut self, _: &palette::PaletteUp, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.palette {
            p.selected = p.selected.saturating_sub(1);
        }
        cx.notify();
    }

    fn on_palette_down(&mut self, _: &palette::PaletteDown, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.palette {
            if p.selected + 1 < p.items.len() {
                p.selected += 1;
            }
        }
        cx.notify();
    }

    fn on_palette_confirm(&mut self, _: &palette::PaletteConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.palette.as_ref().and_then(|p| p.items.get(p.selected).cloned()) else { return };
        self.execute_palette_item(item, window, cx);
    }

    fn on_palette_close(&mut self, _: &palette::PaletteClose, _window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        cx.notify();
    }

    /// Recomputes `palette.items` from `palette.input`'s current text — same
    /// lazy "compare against last-computed text at render time" trigger as
    /// `history_panel`'s `refresh_history_cache` (see `render_palette_overlay`).
    /// Resets `selected` to 0 since a re-ranked list makes the previous
    /// index meaningless.
    fn refresh_palette_items(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.palette.as_ref().map(|p| p.input.read(cx).text()) else { return };
        let items = self.build_palette_items(&query, cx);
        if let Some(p) = &mut self.palette {
            p.items = items;
            p.selected = 0;
            p.last_query = query;
        }
    }

    /// Assembles + ranks every palette source (brief contract #2): tables/
    /// views from the tree's current snapshot (with the favourite bonus —
    /// matched against `config.favourite_objects` filtered to the active
    /// connection, kind "table"|"view"), history top-20 for `query` (via
    /// `HistoryDb::search`, same call `history_panel` makes), every saved
    /// connection, and the 5 fixed actions — delegated to `palette::rank_items`,
    /// the pure scoring/assembly function.
    fn build_palette_items(&self, query: &str, cx: &Context<Self>) -> Vec<PaletteItem> {
        let is_favourite_table = |schema: &Option<String>, name: &str| {
            self.active_connection_id.as_deref().is_some_and(|conn_id| {
                self.config.favourite_objects.iter().any(|f| {
                    f.connection_id == conn_id
                        && &f.schema == schema
                        && f.name == name
                        && (f.kind == "table" || f.kind == "view")
                })
            })
        };
        let tables: Vec<palette::TableSource> = self
            .tree
            .read(cx)
            .snapshot()
            .map(|s| {
                s.tables
                    .iter()
                    .map(|t| palette::TableSource {
                        schema: t.schema.clone(),
                        name: t.name.clone(),
                        favourite: is_favourite_table(&t.schema, &t.name),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let history: Vec<palette::HistorySource> = self
            .history
            .as_ref()
            .and_then(|h| h.search(query, 20).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| palette::HistorySource { id: e.id, sql: e.sql })
            .collect();

        let connections: Vec<palette::ConnectionSource> = self
            .config
            .connections
            .iter()
            .map(|c| palette::ConnectionSource { id: c.id.clone(), name: c.name.clone(), favourite: c.favourite })
            .collect();

        palette::rank_items(query, &tables, &history, &connections, 30)
    }

    /// Brief contract #4: execution routes through EXISTING paths only —
    /// no new execution logic here, just dispatch to the same
    /// methods/pipeline the tree/history-panel/dropdown/actions already use.
    fn execute_palette_item(&mut self, item: PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        match item {
            PaletteItem::Table { schema, name } => {
                // Exactly `on_tree_event`'s `TreeEvent::OpenPreview` arm.
                let sql = preview_sql(schema.as_deref(), &name);
                let preview = PreviewTarget {
                    title: format!("Náhled: {name}"),
                    key: format!("{}.{name}", schema.unwrap_or_default()),
                };
                self.run_query_with(sql, Some(preview), true, cx);
            }
            PaletteItem::HistoryEntry { sql, .. } => {
                // Exactly the history panel's row click: load into the
                // editor and focus it, never run it.
                self.sql.update(cx, |s, cx| s.set_text(&sql, cx));
                let editor_focus = self.sql.focus_handle(cx);
                window.focus(&editor_focus, cx);
            }
            PaletteItem::Connection { id, .. } => {
                self.switch_to_connection(&id, cx);
            }
            PaletteItem::Action { action, .. } => match action {
                PaletteAction::RunQuery => self.run_query(false, cx),
                PaletteAction::ToggleTree => {
                    self.tree_visible = !self.tree_visible;
                }
                PaletteAction::ToggleHistory => {
                    self.history_visible = !self.history_visible;
                    if self.history_visible {
                        self.refresh_history_cache(cx);
                    }
                }
                PaletteAction::NewConnection => {
                    // Exactly the dropdown's "Nové spojení…" click — sets
                    // its own focus, which must win over anything below.
                    self.open_connection_dialog(None, window, cx);
                }
                PaletteAction::RefreshSchema => {
                    // Exactly `on_tree_event`'s `TreeEvent::RefreshRequested` arm.
                    if let Some(spec) = self.active_conn_spec() {
                        self.trigger_schema_fetch(spec, cx);
                    } else {
                        self.schema_tree_connection_key = None;
                        self.tree.update(cx, |t, cx| t.clear(cx));
                    }
                }
            },
        }
        cx.notify();
    }

    /// Centered overlay (brief contract #1), same full-screen-backdrop +
    /// `.occlude()` shape as `connections_ui::render_modal_overlay` — key
    /// context "Palette" on the panel wraps the input so Up/Down/Enter/Esc
    /// (`palette::bind_keys`) resolve even though focus sits on the input's
    /// own nested "TextField" context. `None` (renders nothing) both when
    /// the palette is closed and — belt and suspenders alongside the guard
    /// in `on_open_palette` — while a modal is up.
    fn render_palette_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.palette.is_none() || self.modal.is_some() {
            return None;
        }
        let current_query = self.palette.as_ref().unwrap().input.read(cx).text();
        if current_query != self.palette.as_ref().unwrap().last_query {
            self.refresh_palette_items(cx);
        }
        let p = self.palette.as_ref()?;
        let items = p.items.clone();
        let selected = p.selected;
        let input = p.input.clone();

        let mut list = div().id("palette-list").flex().flex_col().flex_1().overflow_hidden();
        for (ix, item) in items.into_iter().enumerate() {
            let label = palette::display_label(&item);
            let is_selected = ix == selected;
            let bg = if is_selected { rgb(0x45475a) } else { rgb(0x1e1e2e) };
            list = list.child(
                div()
                    .id(("palette-item", ix))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .bg(bg)
                    .text_color(rgb(0xcdd6f4))
                    .hover(|s| s.bg(rgb(0x313244)))
                    .child(label)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.execute_palette_item(item.clone(), window, cx);
                    })),
            );
        }

        let panel = div()
            .id("palette-panel")
            .key_context("Palette")
            .w(px(560.))
            .max_h(px(420.))
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded_md()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_palette_up))
            .on_action(cx.listener(Self::on_palette_down))
            .on_action(cx.listener(Self::on_palette_confirm))
            .on_action(cx.listener(Self::on_palette_close))
            .child(div().px_2().py_2().border_b_1().border_color(rgb(0x45475a)).child(input))
            .child(list);

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
                // `same_connection` is decided at APPLY time against the key
                // of the snapshot actually shown in the tree — deciding it at
                // dispatch time let a superseded switch-fetch leave the key
                // pointing at the new target before any reset ever applied,
                // so a same-target refresh would "preserve" the previous
                // connection's expand/filter state (re-review residual race).
                match result {
                    Ok(Ok(snapshot)) => {
                        let same_connection =
                            view.schema_tree_connection_key.as_deref() == Some(key.as_str());
                        view.schema_tree_connection_key = Some(key.clone());
                        // G3 Task 4: (re-)apply the favourite set alongside
                        // every snapshot — a fresh connection switch needs it
                        // for its "Oblíbené" section to show anything at all,
                        // and a same-connection refresh needs it re-applied
                        // too since `set_snapshot` doesn't touch it.
                        let favourites = view.config.favourite_objects.clone();
                        let active_id = view.active_connection_id.clone();
                        view.tree.update(cx, |t, cx| {
                            t.set_snapshot(snapshot, same_connection, cx);
                            t.set_favourites(favourites, active_id, cx);
                        });
                    }
                    Ok(Err(e)) => {
                        view.tree.update(cx, |t, cx| t.set_error(e.to_string(), cx));
                    }
                    Err(_) => {
                        view.tree
                            .update(cx, |t, cx| t.set_error("fetch zrušen".to_string(), cx));
                    }
                }
            });
        })
        .detach();
    }

    /// `SchemaTree`'s `TreeEvent` subscription (wired in `main`). G2 Task 7:
    /// `OpenPreview` builds the SQL via `preview_sql` and runs it through the
    /// normal guarded pipeline (`run_query_with`, `bypass_auto_limit = true`
    /// — the SQL already carries its own `LIMIT 1000`) without touching the
    /// editor's text; `OpenDdl` (double-click on a routine/trigger, or the
    /// tree header's "DDL" button via `SchemaTree::handle_generate_ddl`)
    /// just opens a read-only `Text` tab — no DB round-trip either way.
    fn on_tree_event(&mut self, _emitter: Entity<SchemaTree>, event: &TreeEvent, cx: &mut Context<Self>) {
        match event {
            TreeEvent::OpenPreview { schema, table } => {
                let sql = preview_sql(schema.as_deref(), table);
                let preview = PreviewTarget {
                    title: format!("Náhled: {table}"),
                    key: format!("{}.{table}", schema.clone().unwrap_or_default()),
                };
                self.run_query_with(sql, Some(preview), true, cx);
            }
            TreeEvent::OpenDdl { title, ddl } => {
                self.tabs.open(ResultTab {
                    id: 0,
                    title: format!("DDL: {title}"),
                    pinned: false,
                    preview_key: None,
                    content: TabContent::Text { text: ddl.clone(), scroll_lines: 0 },
                });
                self.status = format!("DDL otevřeno: {title}");
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
            // G3 Task 4: a row's ★/☆ toggle (a table/view/routine/trigger/
            // sequence in the schema tree proper, or an item already listed
            // under the "Oblíbené" section) — mirrors
            // `connections_ui::AppView::toggle_connection_favourite`'s
            // guarded-save shape for the dropdown's connection stars.
            TreeEvent::ToggleFavourite(fav) => {
                if !self.guard_corrupt_config(cx) {
                    return;
                }
                self.config.toggle_favourite(fav.clone());
                self.status = match self.config.save(&self.config_path) {
                    Ok(()) => "Uloženo".to_string(),
                    Err(e) => format!("error saving config: {}", e.message),
                };
                let favourites = self.config.favourite_objects.clone();
                let active_id = self.active_connection_id.clone();
                self.tree.update(cx, |t, cx| t.set_favourites(favourites, active_id, cx));
                cx.notify();
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

        // G3 Task 3: the history panel sits RIGHT of `column`, fixed 280 px,
        // collapsible via Ctrl+H (`ToggleHistory`) — same collapse-to-0px
        // convention as the schema tree panel above.
        if self.history_visible {
            body = body.child(self.render_history_panel(cx));
        }

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
            .on_action(cx.listener(Self::on_toggle_history))
            .on_action(cx.listener(Self::on_open_palette))
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
        if let Some(overlay) = self.render_palette_overlay(cx) {
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
    // G3 Task 3: opened once at startup; a failure (e.g. an unwritable
    // config dir) is surfaced in the status bar below but never blocks the
    // rest of the app — `record_history`/the panel's search both treat
    // `history: None` as "no history available" rather than panicking.
    let (history, history_open_error) = match HistoryDb::open(&dbc_state::default_history_path()) {
        Ok(h) => (Some(h), None),
        Err(e) => (None, Some(e.to_string())),
    };

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("ctrl-shift-enter", RunQueryUnlimited, None),
            KeyBinding::new("escape", CancelQuery, None),
            KeyBinding::new("ctrl-b", ToggleTree, None),
            KeyBinding::new("ctrl-h", ToggleHistory, None),
            KeyBinding::new("ctrl-k", OpenPalette, None),
        ]);
        sql_input::bind_keys(cx);
        grid::bind_keys(cx);
        connections_ui::bind_keys(cx);
        schema_tree::bind_keys(cx);
        palette::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let window_handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(format!("dbc v{}", env!("CARGO_PKG_VERSION")).into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
                        window.focus(&sql.focus_handle(cx), cx);
                        let grouped_cache = connections_ui::group_connections(&config.connections);
                        // config.toml corruption takes priority (it blocks
                        // saving/editing connections outright); a history
                        // open failure is a lesser, non-blocking notice.
                        let status = match (&config_load_error, &history_open_error) {
                            (Some(detail), _) => {
                                format!("error: config.toml je poškozený – oprav nebo smaž soubor ({detail})")
                            }
                            (None, Some(detail)) => format!("error: historie nedostupná ({detail})"),
                            (None, None) => "ready".into(),
                        };
                        let editor_focus = sql.focus_handle(cx);
                        let tree = cx.new(|cx| SchemaTree::new(cx, editor_focus));
                        cx.subscribe(&tree, AppView::on_tree_event).detach();
                        let history_search = cx.new(|cx| connections_ui::TextField::new(cx, "Hledat…", false));
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
                            history,
                            history_visible: true,
                            history_search,
                            history_cache: Vec::new(),
                            last_history_query: String::new(),
                            palette: None,
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
            // G3 Task 3 review fix: populate `history_cache` once at
            // startup (history panel defaults to visible) instead of
            // leaving it empty until the first recorded run/search edit.
            view.refresh_history_cache(cx);
        });
    });
}

#[cfg(test)]
mod preview_sql_tests {
    use super::*;

    #[test]
    fn quotes_schema_and_table_with_limit_1000() {
        assert_eq!(preview_sql(Some("public"), "orders"), "SELECT * FROM \"public\".\"orders\" LIMIT 1000");
    }

    #[test]
    fn omits_schema_qualifier_when_none() {
        assert_eq!(preview_sql(None, "orders"), "SELECT * FROM \"orders\" LIMIT 1000");
    }

    /// Brief contract #4: a table literally named `we"ird` must not break
    /// out of the query or inject anything — `quote_qualified` doubles the
    /// embedded quote.
    #[test]
    fn survives_a_table_name_with_an_embedded_quote() {
        assert_eq!(preview_sql(None, "we\"ird"), "SELECT * FROM \"we\"\"ird\" LIMIT 1000");
        assert_eq!(
            preview_sql(Some("we\"ird"), "t"),
            "SELECT * FROM \"we\"\"ird\".\"t\" LIMIT 1000"
        );
    }
}
