//! G9 T4: `MonitorView` — the per-tab GPUI entity behind
//! `TabContent::Monitor`. Drains `runner::MonitorEvent`s from the
//! background task (`runner::open_monitor`, T3), owns pause/backoff/
//! generation state, and renders the tile row / running-queries list /
//! blocking-chains tree / per-table sizes (design §5).

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, ClipboardItem, Context, Div, EventEmitter, Hsla, Window,
};

use crate::monitor;
use crate::monitor_sql;
use crate::runner;
use crate::theme::{ActiveTheme, Theme};
use crate::ui;

// Duration colour tiers (design §5 — constants live HERE, not monitor.rs).
pub const DURATION_WARN_SECS: f64 = 1.0;
pub const DURATION_CRIT_SECS: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Normal,
    Warn,
    Crit,
}

pub fn duration_tier(secs: f64) -> Tier {
    if secs >= DURATION_CRIT_SECS {
        Tier::Crit
    } else if secs >= DURATION_WARN_SECS {
        Tier::Warn
    } else {
        Tier::Normal
    }
}

fn tier_color(t: Tier, theme: &Theme) -> Hsla {
    match t {
        Tier::Normal => theme.text_primary,
        Tier::Warn => theme.warn, // design §5's literal warn colour
        Tier::Crit => theme.danger, // design §5's literal crit colour
    }
}

/// Events `MonitorView` emits toward `AppView` (subscription wired in T6's
/// `open_monitor_tab`).
#[derive(Debug, Clone)]
pub enum MonitorViewEvent {
    /// Kill icon clicked on a running-query row (never emitted when
    /// read_only). `label` = "{user} · {application} · běží {n}s" row
    /// facts; `sql` = the exact statement (monitor_sql::kill_sql) the
    /// confirm dialog will display and the background task will run.
    KillRequested { pid: i64, label: String, sql: String },
    /// MonitorEvent::KillResult relayed after the view processed it
    /// (Ok already triggered the out-of-cycle refresh).
    KillFinished { pid: i64, result: Result<(), String> },
}
impl EventEmitter<MonitorViewEvent> for MonitorView {}

pub struct MonitorView {
    cmd_tx: tokio::sync::mpsc::Sender<runner::MonitorCmd>,
    pub read_only: bool,
    engine: dbc_state::Engine,
    paused: bool,
    awaiting: bool,
    refresh_generation: u64,
    interval_secs: u64, // 5 -> 10 -> 20 -> 40 -> 60 backoff
    snapshot: Option<monitor::MonitorSnapshot>,
    /// (xact_total, at) from the previous accepted Data — the client-side
    /// TPS delta state (design §1).
    prev_xact: Option<(i64, std::time::Instant)>,
    last_error: Option<String>,
    last_refresh_at: Option<std::time::Instant>,
    /// Read-only query-text overlay (click a running row's query / a
    /// blocking node) — same local-state idiom as grid.rs's CellDetail
    /// (grid.rs:180-183, render at 1825), not AppView::modal.
    detail: Option<String>,
}

impl MonitorView {
    /// Spawns the event-drain loop and dispatches the initial
    /// Refresh{generation: 1} immediately (first paint must not wait 5s).
    pub fn new(
        cx: &mut Context<Self>,
        cmd_tx: tokio::sync::mpsc::Sender<runner::MonitorCmd>,
        mut event_rx: tokio::sync::mpsc::Receiver<runner::MonitorEvent>,
        read_only: bool,
        engine: dbc_state::Engine,
    ) -> Self {
        // Event drain — same cx.spawn + channel-recv shape main.rs's
        // QueryEvent loop uses; ends when the runner task drops event_tx
        // OR this entity is released (update() errs).
        cx.spawn(async move |this, cx| {
            while let Some(ev) = event_rx.recv().await {
                if this.update(cx, |view, cx| view.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut view = Self {
            cmd_tx,
            read_only,
            engine,
            paused: false,
            awaiting: false,
            refresh_generation: 0,
            interval_secs: 5,
            snapshot: None,
            prev_xact: None,
            last_error: None,
            last_refresh_at: None,
            detail: None,
        };
        view.dispatch_refresh(); // initial paint data, generation 1
        view
    }

    fn dispatch_refresh(&mut self) {
        self.refresh_generation += 1;
        // try_send: the loop drains promptly; a full channel just means a
        // dispatch is dropped and the next timer lap retries — never block
        // the UI thread on a channel.
        if self
            .cmd_tx
            .try_send(runner::MonitorCmd::Refresh { generation: self.refresh_generation })
            .is_ok()
        {
            self.awaiting = true;
        }
    }

    /// Timer-driven (AppView loop, T6): dispatches a Refresh unless
    /// paused or one is already in flight (design §4 overlap prevention —
    /// a skipped tick is skipped, never queued).
    pub fn tick_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.paused || self.awaiting {
            return;
        }
        self.dispatch_refresh();
        cx.notify();
    }

    /// Toolbar ↻: out-of-cycle refresh regardless of paused/backoff,
    /// still gated by awaiting (design §5).
    pub fn manual_refresh(&mut self, cx: &mut Context<Self>) {
        if self.awaiting {
            return;
        }
        self.dispatch_refresh();
        cx.notify();
    }

    /// Confirm-dialog path (T5): sends MonitorCmd::Kill. Does NOT check
    /// read_only itself — the UI never emits KillRequested when
    /// read_only, and the background task independently refuses (the two
    /// designated gates, design §6); a third check here would mask a
    /// regression in either.
    pub fn dispatch_kill(&mut self, pid: i64, cx: &mut Context<Self>) {
        let _ = self
            .cmd_tx
            .try_send(runner::MonitorCmd::Kill { generation: self.refresh_generation, pid });
        cx.notify();
    }

    /// Current tick interval (5s, doubling to 60s cap under errors) —
    /// read by AppView's timer loop each lap (T6).
    pub fn interval_secs(&self) -> u64 {
        self.interval_secs
    }

    fn on_event(&mut self, ev: runner::MonitorEvent, cx: &mut Context<Self>) {
        if let Some(event) = self.apply_event(ev) {
            cx.emit(event);
        }
        cx.notify();
    }

    /// Pure state transition for an incoming `MonitorEvent` — deliberately
    /// takes no `Context`; the only GPUI-flavoured bits (`cx.notify()` /
    /// `cx.emit()`) live in the thin `on_event` wrapper above. Factored out
    /// so the MAJOR review-mandated invariant below is unit-testable
    /// directly against a hand-built `MonitorView` (no entity/`cx.spawn`
    /// needed — `dispatch_refresh` itself never touches `cx` either).
    /// Returns the `MonitorViewEvent` to emit, if any.
    fn apply_event(&mut self, ev: runner::MonitorEvent) -> Option<MonitorViewEvent> {
        match ev {
            runner::MonitorEvent::Data { generation, mut snapshot } => {
                // Last-dispatched-wins, same convention as
                // AppView::switch_generation / sidebar_fetch_generation.
                if generation != self.refresh_generation {
                    return None;
                }
                self.awaiting = false;
                self.interval_secs = 5; // any Data resets backoff (design §4)
                self.last_error = None;
                if let Some(perf) = snapshot.perf.as_mut() {
                    if let Some(total) = perf.xact_total {
                        perf.tps = monitor::compute_rate(total, self.prev_xact, snapshot.fetched_at);
                        self.prev_xact = Some((total, snapshot.fetched_at));
                    }
                }
                self.snapshot = Some(snapshot);
                self.last_refresh_at = Some(std::time::Instant::now());
                None
            }
            runner::MonitorEvent::Error { generation, message } => {
                if generation != self.refresh_generation {
                    return None;
                }
                self.awaiting = false;
                self.interval_secs = (self.interval_secs * 2).min(60); // 5→10→20→40→60
                self.last_error = Some(message);
                None
            }
            runner::MonitorEvent::KillResult { pid, result, .. } => {
                // NOT generation-gated: a kill outcome is never superseded
                // by refresh generations (design §4 gates Data/Error only).
                let outcome = result.map(|_affected| ()).map_err(|e| e.message);
                // MAJOR review fix: ALWAYS dispatch a fresh refresh here —
                // ok AND err alike. `monitor_loop` cancels any in-flight
                // refresh before servicing a Kill (design §4's
                // `tokio::select!`) and sends NO terminal Data/Error for
                // that cancelled generation, so the generation this Kill
                // interrupted is stale regardless of whether the kill
                // itself succeeded. Gating this on `outcome.is_ok()` left
                // `awaiting` stuck `true` forever after a FAILED kill that
                // raced an in-flight refresh — auto-refresh silently dead
                // for the rest of the tab's life, since nothing else ever
                // clears `awaiting`.
                self.dispatch_refresh();
                Some(MonitorViewEvent::KillFinished { pid, result: outcome })
            }
        }
    }
}

/// Base card styling shared by the four tiles (design §5 — `bg_panel`/
/// `border`, matching `connections_ui.rs`'s panels).
fn card(theme: &Theme) -> Div {
    ui::popover(*theme)
        .w(px(220.))
}

fn card_title(label: &str) -> Div {
    div().font_weight(gpui::FontWeight::BOLD).child(label.to_string())
}

/// Query text shortened for a one-line row/label — plain char truncation
/// (rows already `.overflow_hidden()` for the running-queries list; this is
/// for the blocking-tree labels, which are plain text with no CSS clipping).
fn truncate_query(s: &str) -> String {
    const MAX: usize = 80;
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// One flattened row of the blocking-chain tree, ready for rendering —
/// pure data, no GPUI. Built by `flatten_blocking_tree`. Deliberately
/// flat (no nested `Vec<Self>`), so `#[derive(Debug)]` here is safe to
/// print/compare even for a large tree — unlike `monitor::BlockingNode`,
/// where the analogous derive recurses per depth (never `{:?}`-format a
/// `BlockingNode` tree directly; use this instead for test assertions).
#[derive(Debug, Clone, PartialEq)]
struct BlockingRow {
    depth: usize,
    pid: i64,
    wait_secs: Option<f64>,
    query: Option<String>,
    cycle: bool,
}

/// BLOCKER 1 review fix: explicit-stack pre-order flatten of the blocking
/// forest — no recursion, so depth is bounded by heap, not by the UI
/// thread's stack (mirrors `monitor.rs::build_tree_iterative`'s fix for
/// the same class of bug: the original `push_blocking_rows` recursed one
/// stack frame per tree depth and a chain deep enough — reachable given
/// `MONITOR_ROW_CAP = 10_000` in runner.rs — crashed with
/// `STATUS_STACK_OVERFLOW`, taking the whole app down, not just this tab).
/// Extracted as a pure function (no `Context`) so it's unit-testable
/// directly, per the same "pure logic + thin render consumption" split
/// used throughout this module. NEVER clones a `BlockingNode` — only
/// `node.query: Option<String>` per visited node is cloned into the row
/// (same constraint `build_blocking_tree`'s own `Drop` rewrite exists
/// for).
fn flatten_blocking_tree(roots: &[monitor::BlockingNode]) -> Vec<BlockingRow> {
    let mut out = Vec::new();
    // Push in reverse so popping visits children in original left-to-right
    // order (matches the old recursive walk's visible ordering).
    let mut stack: Vec<(usize, &monitor::BlockingNode)> = roots.iter().rev().map(|n| (0usize, n)).collect();
    while let Some((depth, node)) = stack.pop() {
        out.push(BlockingRow {
            depth,
            pid: node.pid,
            wait_secs: node.wait_secs,
            query: node.query.clone(),
            cycle: node.cycle,
        });
        for child in node.children.iter().rev() {
            stack.push((depth + 1, child));
        }
    }
    out
}

impl MonitorView {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let pause_icon = if self.paused { "▶" } else { "⏸" };
        let freshness = match self.last_refresh_at {
            Some(t) => format!("aktualizace před {} s", t.elapsed().as_secs()),
            None => "načítám…".to_string(),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap_2()
            .p_2()
            .text_color(theme.text_primary)
            .child(freshness)
            .child(
                div()
                    .id("mon-refresh")
                    .cursor_pointer()
                    .px_2()
                    .child("↻")
                    .on_click(cx.listener(|this, _, _, cx| this.manual_refresh(cx))),
            )
            .child(
                div()
                    .id("mon-pause")
                    .cursor_pointer()
                    .px_2()
                    .child(pause_icon)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.paused = !this.paused;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_tiles(&self, theme: &Theme) -> AnyElement {
        let snap = self.snapshot.as_ref();

        let conn_body = match snap.and_then(|s| s.connections.as_ref()) {
            Some(t) => format!(
                "{} aktivní · {} idle / max {}",
                t.active,
                t.idle,
                t.max.map(|m| m.to_string()).unwrap_or_else(|| "neomezeno".into())
            ),
            None => "n/a".to_string(),
        };

        let locks_body = match snap.and_then(|s| s.locks.as_ref()) {
            Some(l) => format!("{} čeká na zámek · {} deadlocků", l.waiting, l.deadlocks_since_reset),
            None => "n/a".to_string(),
        };

        let size = snap.map(|s| &s.size);
        let data_bytes = size.and_then(|s| s.data_bytes);
        let wal_bytes = size.and_then(|s| s.wal_or_log_bytes);
        let size_sum = data_bytes.unwrap_or(0) + wal_bytes.unwrap_or(0);
        let data_frac = monitor::bar_fraction(data_bytes.unwrap_or(0), size_sum);
        let wal_frac = monitor::bar_fraction(wal_bytes.unwrap_or(0), size_sum);
        let size_label = format!(
            "data {} · WAL {}",
            data_bytes.map(monitor::fmt_bytes).unwrap_or_else(|| "n/a".into()),
            wal_bytes.map(monitor::fmt_bytes).unwrap_or_else(|| "n/a".into()),
        );

        let perf_body = match snap.and_then(|s| s.perf.as_ref()) {
            Some(p) => format!(
                "{} % cache hit · uptime {} · TPS {}",
                p.cache_hit_pct.map(|v| format!("{v:.1}")).unwrap_or_else(|| "–".into()),
                monitor::fmt_uptime(p.uptime_secs),
                p.tps.map(|v| format!("{v:.1}")).unwrap_or_else(|| "–".into()),
            ),
            None => "n/a".to_string(),
        };

        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .p_2()
            .child(card(theme).child(card_title("Připojení")).child(conn_body))
            .child(
                card(theme)
                    .child(card_title("Zámky"))
                    .child(locks_body)
                    .child(div().text_color(theme.text_disabled).child("od posledního resetu statistik")),
            )
            .child(
                card(theme)
                    .child(card_title("Velikost DB"))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .h(px(8.))
                            .w(px(160.))
                            .child(div().h(px(8.)).w(px(160. * data_frac)).bg(theme.accent))
                            .child(div().h(px(8.)).w(px(160. * wal_frac)).bg(theme.warn)),
                    )
                    .child(size_label),
            )
            .child(card(theme).child(card_title("Výkon")).child(perf_body))
            .into_any_element()
    }

    fn render_running(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.snapshot.as_ref().and_then(|s| s.running.as_ref());
        let header = match running {
            Some(rows) => format!("Běžící dotazy ({})", rows.len()),
            None => "Běžící dotazy: n/a".to_string(),
        };
        let rows_len = running.map(|r| r.len()).unwrap_or(0);

        let list = uniform_list(
            "monitor-running",
            rows_len,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let theme = *cx.theme();
                let mut items = Vec::with_capacity(range.len());
                let Some(rows) = this.snapshot.as_ref().and_then(|s| s.running.as_ref()) else {
                    return items;
                };
                for ix in range {
                    let Some(row) = rows.get(ix) else { continue };
                    let color = tier_color(duration_tier(row.duration_secs), &theme);
                    let query_text = row.query.clone().unwrap_or_default();
                    let detail_text = row.query.clone();
                    let pid = row.pid;

                    let kill = if this.read_only {
                        // First of the two designated read-only gates
                        // (design §6): disabled, never emits. A real
                        // tooltip is unavailable on a plain `div` at the
                        // pinned GPUI rev (no `gpui::Tooltip` type exists
                        // there) — falls back to a static caption instead
                        // (plan's explicitly-sanctioned fallback).
                        div()
                            .id(("mon-kill", pid as usize))
                            .text_color(theme.text_disabled)
                            .child("✕ pouze pro čtení")
                            .into_any_element()
                    } else {
                        let label = format!(
                            "{} · {} · běží {:.0}s",
                            row.user.clone().unwrap_or_else(|| "?".into()),
                            row.application.clone().unwrap_or_else(|| "?".into()),
                            row.duration_secs
                        );
                        let sql = monitor_sql::kill_sql(this.engine, pid).unwrap_or_default();
                        div()
                            .id(("mon-kill", pid as usize))
                            .cursor_pointer()
                            .text_color(theme.danger)
                            .child("✕")
                            .on_click(cx.listener(move |_this, _, _, cx| {
                                cx.emit(MonitorViewEvent::KillRequested {
                                    pid,
                                    label: label.clone(),
                                    sql: sql.clone(),
                                });
                            }))
                            .into_any_element()
                    };

                    let row_el = div()
                        .id(("mon-row", ix))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .child(div().w(px(56.)).child(pid.to_string()))
                        .child(div().w(px(90.)).child(row.user.clone().unwrap_or_else(|| "?".into())))
                        .child(div().w(px(110.)).child(row.application.clone().unwrap_or_else(|| "?".into())))
                        .child(div().w(px(100.)).child(row.client.clone().unwrap_or_else(|| "?".into())))
                        .child(div().w(px(70.)).child(row.state.clone().unwrap_or_else(|| "?".into())))
                        .child(
                            div()
                                .w(px(60.))
                                .text_color(color)
                                .child(format!("{:.1}s", row.duration_secs)),
                        )
                        .child(
                            div()
                                .id(("mon-query", ix))
                                .flex_1()
                                .overflow_hidden()
                                .cursor_pointer()
                                .child(query_text)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.detail = detail_text.clone();
                                    cx.notify();
                                })),
                        )
                        .child(kill);
                    items.push(row_el.into_any_element());
                }
                items
            }),
        );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .child(div().px_2().py_1().font_weight(gpui::FontWeight::BOLD).child(header))
            .child(list.flex_1())
            .into_any_element()
    }

    fn render_blocking(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = *cx.theme();
        let mut body: Vec<AnyElement> = Vec::new();
        match self.snapshot.as_ref().and_then(|s| s.blocking.as_ref()) {
            None => body.push(div().px_2().child("n/a").into_any_element()),
            Some(roots) if roots.is_empty() => {
                body.push(div().px_2().child("žádné blokace").into_any_element())
            }
            Some(roots) => {
                for (ix, row) in flatten_blocking_tree(roots).into_iter().enumerate() {
                    let wait = row.wait_secs.map(|w| format!("{w:.1}")).unwrap_or_else(|| "–".into());
                    let query_text = row.query.clone().unwrap_or_default();
                    let mut label =
                        format!("{} · wait {}s · {}", row.pid, wait, truncate_query(&query_text));
                    if row.cycle {
                        label.push_str(" (cyklus — možný deadlock)");
                    }
                    let color = if row.cycle { tier_color(Tier::Crit, &theme) } else { theme.text_primary };
                    let detail_text = row.query;
                    let el = div()
                        .id(("mon-block", ix))
                        .pl(px(16. * row.depth as f32))
                        .cursor_pointer()
                        .text_color(color)
                        .child(label)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.detail = detail_text.clone();
                            cx.notify();
                        }));
                    body.push(el.into_any_element());
                }
            }
        }
        if self.snapshot.as_ref().is_some_and(|s| s.blocking_truncated) {
            // BLOCKER 2 review fix: `build_blocking_tree` hit
            // `monitor::MONITOR_TREE_NODE_CAP` and stopped expanding —
            // surface it the same way as the refresh error/backoff line,
            // never silently.
            body.push(
                div()
                    .px_2()
                    .text_color(theme.warn)
                    .child("strom blokací je příliš velký — zobrazena je jen jeho část")
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .p_2()
            .child(div().font_weight(gpui::FontWeight::BOLD).child("Blokace"))
            .children(body)
            .into_any_element()
    }

    fn render_tables(&self, theme: &Theme) -> AnyElement {
        let mut body: Vec<AnyElement> = Vec::new();
        match self.snapshot.as_ref().and_then(|s| s.tables.as_ref()) {
            None => body.push(div().px_2().child("n/a").into_any_element()),
            Some(rows) if rows.is_empty() => {
                body.push(div().px_2().child("žádné tabulky").into_any_element())
            }
            Some(rows) => {
                let max_in_set =
                    rows.iter().map(|r| r.data_bytes + r.index_bytes + r.toast_bytes).max().unwrap_or(0);
                for r in rows {
                    let total = r.data_bytes + r.index_bytes + r.toast_bytes;
                    let frac = monitor::bar_fraction(total, max_in_set);
                    let name = match &r.schema {
                        Some(s) => format!("{s}.{}", r.table),
                        None => r.table.clone(),
                    };
                    let row_el = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .child(div().w(px(220.)).child(name))
                        .child(div().w(px(80.)).child(monitor::fmt_bytes(r.data_bytes)))
                        .child(div().w(px(80.)).child(monitor::fmt_bytes(r.index_bytes)))
                        .child(div().w(px(80.)).child(monitor::fmt_bytes(r.toast_bytes)))
                        .child(div().w(px(110.)).child(format!("~{} řádků", r.row_estimate)))
                        .child(div().h(px(6.)).w(px(160. * frac)).bg(theme.accent));
                    body.push(row_el.into_any_element());
                }
            }
        }
        div()
            .flex()
            .flex_col()
            .p_2()
            .child(div().font_weight(gpui::FontWeight::BOLD).child("Velikosti tabulek"))
            .children(body)
            .into_any_element()
    }

    /// Read-only query-text overlay (design §5 point 7). Local to this
    /// entity, NOT `AppView::modal` (same reasoning as grid's `cell_detail`
    /// field comment, grid.rs:286-289). Mirrors
    /// `grid.rs::render_cell_detail_overlay` (grid.rs:1825): a centred
    /// `.occlude()`d panel with "Kopírovat" + an explicit "Zavřít" close —
    /// deviation from the plan's prose ("close on backdrop click"): the
    /// cited precedent it mirrors has no backdrop-click dismissal either
    /// (only the existing modal overlays in this codebase, e.g.
    /// `connections_ui::render_modal_overlay`, close via an explicit
    /// button), so this follows the ACTUAL precedent rather than the
    /// prose description.
    fn render_detail_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = *cx.theme();
        let text = self.detail.clone()?;
        let text_for_copy = text.clone();
        let panel = ui::surface(theme)
            .id("mon-detail-panel")
            .w(px(560.))
            .max_h(px(420.))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("mon-detail-body")
                    .font_family("Consolas")
                    .flex_1()
                    .overflow_hidden()
                    .p_2()
                    .text_color(theme.text_primary)
                    .child(text),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .p_2()
                    .child(
                        ui::button("mon-detail-copy", "Kopírovat", theme)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text_for_copy.clone()));
                            })),
                    )
                    .child(
                        ui::button("mon-detail-close", "Zavřít", theme)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.detail = None;
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
                .bg(theme.bg_backdrop)
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }
}

impl Render for MonitorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let mut root = div()
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .bg(theme.bg_deep)
            .text_color(theme.text_primary)
            .child(self.render_toolbar(cx))
            .child(self.render_tiles(&theme));

        if let Some(err) = self.last_error.clone() {
            root = root.child(
                div()
                    .p_2()
                    .text_color(theme.warn)
                    .child(format!(
                        "aktualizace selhala ({err}) · další pokus za {}s",
                        self.interval_secs
                    )),
            );
        }

        root = root
            .child(self.render_running(cx))
            .child(self.render_blocking(cx))
            .child(self.render_tables(&theme));

        if let Some(overlay) = self.render_detail_overlay(cx) {
            root = root.child(overlay);
        }

        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_tier_boundaries() {
        assert_eq!(duration_tier(0.0), Tier::Normal);
        assert_eq!(duration_tier(0.999), Tier::Normal);
        assert_eq!(duration_tier(1.0), Tier::Warn); // >= WARN is Warn
        assert_eq!(duration_tier(9.999), Tier::Warn);
        assert_eq!(duration_tier(10.0), Tier::Crit); // >= CRIT is Crit
        assert_eq!(duration_tier(120.0), Tier::Crit);
    }

    #[test]
    fn backoff_progression_caps_at_60() {
        // The exact 5→10→20→40→60→60 ladder (design §4), as the pure
        // arithmetic on_event applies.
        let mut interval: u64 = 5;
        let mut seen = Vec::new();
        for _ in 0..6 {
            interval = (interval * 2).min(60);
            seen.push(interval);
        }
        assert_eq!(seen, vec![10, 20, 40, 60, 60, 60]);
    }

    /// Regression test for review BLOCKER 1: the original `push_blocking_
    /// rows` recursed one stack frame per tree depth — a chain deep enough
    /// (reachable given `MONITOR_ROW_CAP = 10_000` edges in runner.rs)
    /// overflowed the UI thread's stack (`STATUS_STACK_OVERFLOW`, i.e. the
    /// whole app crashes, not just this tab). `flatten_blocking_tree` must
    /// walk a 10,000-deep chain via an explicit heap stack instead.
    #[test]
    fn flatten_blocking_tree_handles_a_10k_deep_chain_without_stack_overflow() {
        let edges: Vec<monitor::BlockingEdge> = (0..10_000)
            .map(|i| monitor::BlockingEdge {
                waiter_pid: i + 1,
                blocker_pid: i,
                wait_secs: 1.0,
                waiter_query: None,
                blocker_query: None,
            })
            .collect();
        let (tree, truncated) = monitor::build_blocking_tree(&edges);
        assert!(!truncated);

        let rows = flatten_blocking_tree(&tree);
        assert_eq!(rows.len(), 10_001);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].pid, 0);
        assert_eq!(rows[10_000].depth, 10_000);
        assert_eq!(rows[10_000].pid, 10_000);
    }

    /// Regression test for review MAJOR finding: `apply_event`'s
    /// `KillResult` arm must dispatch a fresh refresh regardless of the
    /// kill's own outcome — before the fix, a FAILED kill left `awaiting`
    /// stuck `true` forever whenever it raced an in-flight refresh (the
    /// cancelled refresh's generation never gets a terminal Data/Error —
    /// see `runner::monitor_loop`'s `tokio::select!` — so nothing else
    /// would ever clear it, silently killing auto-refresh for the rest of
    /// the tab's life). Constructs a `MonitorView` directly (no `cx.spawn`/
    /// entity needed — `apply_event` and `dispatch_refresh` never touch
    /// `Context`) with `awaiting: true`, simulating exactly that race.
    #[test]
    fn kill_result_always_dispatches_a_fresh_refresh_ok_and_err_alike() {
        for result in [Ok(1u64), Err(dbc_core::QueryError::msg("boom"))] {
            let is_err = result.is_err();
            let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(8);
            let mut view = MonitorView {
                cmd_tx,
                read_only: false,
                engine: dbc_state::Engine::Postgres,
                paused: false,
                awaiting: true, // a refresh was in flight when the Kill fired
                refresh_generation: 5,
                interval_secs: 5,
                snapshot: None,
                prev_xact: None,
                last_error: None,
                last_refresh_at: None,
                detail: None,
            };
            view.apply_event(runner::MonitorEvent::KillResult { generation: 5, pid: 42, result });
            assert_eq!(view.refresh_generation, 6, "a fresh generation must be dispatched (err = {is_err})");
            assert!(view.awaiting, "err = {is_err}");
            match cmd_rx.try_recv() {
                Ok(runner::MonitorCmd::Refresh { generation: 6 }) => {}
                other => panic!("expected a fresh Refresh{{generation: 6}}, got {other:?} (err = {is_err})"),
            }
        }
    }
}
