// G3 Task 3: history panel + query recording.
//
// Layout of this file:
//   1. Pure formatting helpers (GPUI-free) for a single history entry's two
//      display lines — unit-tested directly below, same "pull the pure
//      logic out so it doesn't need a window" approach as
//      `tabs::collapse_title` / `schema_tree::flatten`.
//   2. `impl AppView` — recording (`record_history`, called from
//      `run_query_with`'s `Finished`/`Failed` arms in main.rs) and the
//      panel's render helper.
//
// Caching (post-review fix — Task 3 review Issue 1): `AppView::history_cache`
// holds the last-fetched `Vec<HistoryEntry>`, mirroring the
// `grouped_cache`/`refresh_grouped_cache` precedent in `main.rs` /
// `connections_ui.rs` — a derived list recomputed only at the handful of
// events that actually change what should be displayed, not on every GPUI
// window repaint (`cx.notify()` anywhere in the window re-runs
// `render_history_panel`, which includes every SQL-editor keystroke and
// every streamed result batch — see the review for the full call-site
// list). `refresh_history_cache` is called from: startup once `history` has
// opened (`main`), `record_history` after a successful `add`, the star
// toggle's click handler, and `on_toggle_history` when flipping to visible.
//
// The search `TextField` (`connections_ui::TextField`) has no on-change
// hook to drive the refresh from directly — it's a plain get/set-text
// buffer with no event emission, reused as-is from the connection dialog
// rather than growing a new capability just for this. So search-text-change
// is instead detected lazily, inside `render_history_panel` itself: a
// stored `last_history_query` is compared against the field's current text
// on every render (a `String` equality check — cheap, unlike the sqlite
// query) and `refresh_history_cache` only runs when they differ.

use dbc_state::HistoryEntry;
use gpui::{div, prelude::*, px, rgb, uniform_list, AnyElement, Context, Focusable};

use crate::AppView;

/// Right-panel fixed width (brief contract #3), mirroring the schema tree
/// panel's `w(px(260.))` in `main.rs`'s `render`.
pub const PANEL_WIDTH: f32 = 280.;

/// Fixed per-row height (brief: G3 final-review fix F4) — `uniform_list`
/// (the same mechanism `grid.rs`/`schema_tree.rs` use for their scrollable
/// rows) requires every row the same height; a two-line entry (SQL +
/// meta/error line) needs more than the tree's single-line 22px, so this is
/// taller.
const HISTORY_ROW_HEIGHT: f32 = 44.;

/// First line of a history entry: SQL collapsed to one line, truncated to
/// this many chars (brief contract #3). Same algorithm as
/// `tabs::collapse_title`, kept as a local `max_chars`-parameterized copy
/// rather than changing that function's signature (Task 3's work discipline
/// scopes `tabs.rs` edits to "only if you parameterize `collapse_title`").
const SQL_COLLAPSE_MAX_CHARS: usize = 48;

/// Collapses `sql` onto a single line (any run of whitespace, including
/// newlines, becomes one space) and truncates to `max_chars` characters,
/// appending '…' when truncation happened.
pub fn collapse_sql(sql: &str, max_chars: usize) -> String {
    let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Second line of a history entry: `"{connection} · {rows} řádků · {duration} ms"`
/// for a successful run, or the raw error text for a failed one (brief
/// contract #3). Returns `(text, is_error)` — `is_error` drives the caller's
/// red-vs-muted text color.
pub fn format_meta_line(entry: &HistoryEntry) -> (String, bool) {
    if let Some(err) = &entry.error {
        (err.clone(), true)
    } else {
        let rows = entry.row_count.map(|r| r.to_string()).unwrap_or_else(|| "?".into());
        let dur = entry.duration_ms.map(|d| d.to_string()).unwrap_or_else(|| "?".into());
        (format!("{} · {rows} řádků · {dur} ms", entry.connection), false)
    }
}

impl AppView {
    /// Fire-and-forget history record, called from `run_query_with`'s
    /// `Finished`/`Failed` arms (main.rs) after every recorded run,
    /// including previews (brief contract #2), and — post-review — for a
    /// spill-errored run too (see main.rs's `run_query_with`). A
    /// missing/unopenable `history` (startup open error — see `main`) or a
    /// write failure is silently ignored: history is a convenience, never a
    /// reason to disrupt the run pipeline. Refreshes `history_cache` after a
    /// successful `add` so the panel reflects the new entry without waiting
    /// for the next search-text-change poll.
    pub(crate) fn record_history(
        &mut self,
        sql: &str,
        connection: &str,
        started_at: i64,
        duration_ms: Option<i64>,
        row_count: Option<i64>,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if let Some(h) = self.history.as_mut() {
            if h.add(sql, connection, started_at, duration_ms, row_count, error).is_ok() {
                self.refresh_history_cache(cx);
            }
        }
    }

    /// Recomputes `history_cache` from the search box's current text — the
    /// module doc comment above explains why this is a separate step rather
    /// than querying inline in `render_history_panel` on every call. `None`
    /// `history` (open failed at startup) yields an empty cache.
    pub(crate) fn refresh_history_cache(&mut self, cx: &mut Context<Self>) {
        let query = self.history_search.read(cx).text();
        self.history_cache =
            self.history.as_ref().and_then(|h| h.search(&query, 100).ok()).unwrap_or_default();
        self.last_history_query = query;
    }

    /// Connection name to record with a run (brief contract #2): the active
    /// saved connection's `name`, or `"cli"` for the CLI-arg back-compat
    /// path. Only meaningful once `run_query_with` has already resolved a
    /// `ConnectSpec` successfully (a query can't run with neither set).
    pub(crate) fn active_connection_name_for_history(&self) -> String {
        if let Some(id) = &self.active_connection_id {
            if let Some(c) = self.config.connections.iter().find(|c| &c.id == id) {
                return c.name.clone();
            }
        }
        "cli".to_string()
    }

    /// Header "Historie" + search field + newest/starred-first entry list,
    /// rendered from `history_cache` — see the module doc comment for the
    /// caching strategy. The one thing this method still does per-call is
    /// the cheap search-text-vs-`last_history_query` string compare that
    /// detects a search edit and triggers the (not-cheap) cache refresh.
    /// `None` `history` (open failed at startup) renders an empty list
    /// rather than nothing, so the panel's layout doesn't jump around.
    pub(crate) fn render_history_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current_query = self.history_search.read(cx).text();
        if current_query != self.last_history_query {
            self.refresh_history_cache(cx);
        }

        // G3 final-review fix (F4): `uniform_list` — same mechanism as
        // `grid.rs`'s result rows and `schema_tree.rs`'s tree rows — instead
        // of a plain clipped `div`, so all fetched entries (up to 100) are
        // reachable by scrolling, not just the ~15-20 that fit one
        // screenful. Reads `this.history_cache[ix]` directly inside the
        // processor rather than capturing a separate clone, since the cache
        // already lives on `AppView`.
        let entry_count = self.history_cache.len();
        let list = uniform_list(
            "history-list",
            entry_count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let mut items = Vec::with_capacity(range.len());
                for ix in range {
                    let entry = &this.history_cache[ix];
                    let id = entry.id;
                    let sql_for_click = entry.sql.clone();
                    let line1 = collapse_sql(&entry.sql, SQL_COLLAPSE_MAX_CHARS);
                    let (line2, is_error) = format_meta_line(entry);
                    let line2_color = if is_error { rgb(0xf38ba8) } else { rgb(0xa6adc8) };
                    let starred = entry.starred;
                    let star = if starred { "★" } else { "☆" };
                    let star_color = if starred { rgb(0xf9e2af) } else { rgb(0x6c7086) };

                    items.push(
                        div()
                            .id(("history-entry", id as usize))
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap_1()
                            .h(px(HISTORY_ROW_HEIGHT))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x313244)))
                            // Brief contract #4: click loads the SQL into
                            // the editor and focuses it, but NEVER runs it.
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.sql.update(cx, |sql, cx| sql.set_text(&sql_for_click, cx));
                                let editor_focus = view.sql.focus_handle(cx);
                                window.focus(&editor_focus, cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .id(("history-star", id as usize))
                                    .cursor_pointer()
                                    .text_color(star_color)
                                    .child(star)
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        cx.stop_propagation();
                                        if let Some(h) = view.history.as_mut() {
                                            let _ = h.set_starred(id, !starred);
                                        }
                                        // Review Issue 1: the star order
                                        // (starred entries sort first)
                                        // changed, so the cache must be
                                        // refreshed, not just the window
                                        // re-notified.
                                        view.refresh_history_cache(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_color(rgb(0xcdd6f4)).child(line1))
                                    .child(div().text_size(px(11.)).text_color(line2_color).child(line2)),
                            ),
                    );
                }
                items
            }),
        )
        .flex_1();

        div()
            .id("history-panel")
            .w(px(PANEL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(0x45475a))
            .bg(rgb(0x181825))
            .text_color(rgb(0xcdd6f4))
            .child(div().px_2().py_1().child("Historie"))
            .child(div().px_2().pb_1().child(self.history_search.clone()))
            .child(list)
            .into_any_element()
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn collapse_sql_collapses_whitespace_and_leaves_short_text_alone() {
        assert_eq!(collapse_sql("select 1", 48), "select 1");
        assert_eq!(collapse_sql("select\n  *\nfrom\tt", 48), "select * from t");
    }

    #[test]
    fn collapse_sql_truncates_at_the_given_width_with_ellipsis() {
        let long = "a".repeat(60);
        let title = collapse_sql(&long, 48);
        assert_eq!(title.chars().count(), 49);
        assert!(title.ends_with('…'));
        assert!(title.starts_with(&"a".repeat(48)));
    }

    fn entry(error: Option<&str>, duration_ms: Option<i64>, row_count: Option<i64>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            sql: "select 1".into(),
            connection: "demo".into(),
            started_at: 1000,
            duration_ms,
            row_count,
            error: error.map(|s| s.to_string()),
            starred: false,
        }
    }

    #[test]
    fn meta_line_success_formats_connection_rows_and_duration() {
        let (line, is_error) = format_meta_line(&entry(None, Some(12), Some(34)));
        assert_eq!(line, "demo · 34 řádků · 12 ms");
        assert!(!is_error);
    }

    #[test]
    fn meta_line_success_falls_back_to_question_marks_when_missing() {
        let (line, is_error) = format_meta_line(&entry(None, None, None));
        assert_eq!(line, "demo · ? řádků · ? ms");
        assert!(!is_error);
    }

    #[test]
    fn meta_line_error_variant_shows_the_error_text_instead() {
        let (line, is_error) = format_meta_line(&entry(Some("syntax error"), None, None));
        assert_eq!(line, "syntax error");
        assert!(is_error);
    }
}
