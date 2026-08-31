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
use gpui::{div, prelude::*, px, uniform_list, AnyElement, Context, Focusable};

use crate::theme::ActiveTheme;
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

/// Badge prefix (including trailing space) for a history entry's first line,
/// keyed off `HistoryEntry::kind` (brief contract, G10 N2 final review):
/// `"query"` (the implicit default `add`/`record_history` write) gets no
/// badge; `"backup"`/`"restore"` (G11 T7) keep the existing 🗄 badge;
/// `"admin"` (G10 N2, `record_history_with_kind` from the admin Apply path)
/// gets its own 🛡 badge so an admin DDL/grant write is visually
/// distinguishable from a backup/restore run in the same list. Any OTHER
/// kind — an old row from before a kind existed, or a future kind this
/// build doesn't know about yet — falls back to the generic 🗄 badge rather
/// than panicking or rendering blank, same back-compat guarantee the
/// pre-G10-N2 `if kind == "query" {..} else { 🗄 }` shape already had.
fn badge_for_kind(kind: &str) -> &'static str {
    match kind {
        "query" => "",
        "admin" => "🛡 ",
        KIND_CLI => "❯ ",
        _ => "🗄 ", // "backup" | "restore" | any unrecognized kind
    }
}

/// Second line of a history entry: `"{connection} · {rows} řádků · {duration} ms"`
/// for a successful run, or the raw error text for a failed one (brief
/// contract #3). Returns `(text, is_error)` — `is_error` drives the caller's
/// red-vs-muted text color.
/// Design §5 row 8: „{name}/{db}" when the active db ≠ default, plain
/// name otherwise. Display text only — history keeps recording names,
/// never URLs/credentials (design §4.6); dedup (`sql + connection +
/// window`) naturally scopes per db. The known name-collision lossiness
/// (rename/delete → "cli") is unchanged and out of scope.
pub(crate) use dbc_state::conn_label as history_conn_label;

/// Written by `dbc-cli`, grouped on here — defined beside the column it
/// fills so the two sides cannot drift.
pub use dbc_state::KIND_CLI;

/// The panel's section headings.
pub const SECTION_APP: &str = "V aplikaci";
pub const SECTION_CLI: &str = "Z příkazové řádky";

/// One rendered row: either a heading or an index into the entry cache.
///
/// An INDEX rather than a borrowed entry, because `uniform_list`'s
/// processor reads `AppView::history_cache` itself and a second borrow of
/// the same data would be a second thing to keep in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRow {
    Section(&'static str),
    Entry(usize),
}

/// Split the cache into sections: the app's own runs, then the CLI's.
///
/// Headings appear ONLY when both kinds are present. Anyone who never
/// touches `dbc` therefore sees exactly the panel they saw before — a
/// lone „V aplikaci" heading over every row is decoration, not
/// information — and so does anyone whose history is all CLI.
///
/// Within each section the incoming order is preserved, which is the
/// order `HistoryDb::search` chose: starred first, then newest.
pub fn history_rows(entries: &[HistoryEntry]) -> Vec<HistoryRow> {
    let (cli, app): (Vec<usize>, Vec<usize>) =
        (0..entries.len()).partition(|&i| entries[i].kind == KIND_CLI);
    if cli.is_empty() || app.is_empty() {
        return (0..entries.len()).map(HistoryRow::Entry).collect();
    }
    let mut out = Vec::with_capacity(entries.len() + 2);
    out.push(HistoryRow::Section(SECTION_APP));
    out.extend(app.into_iter().map(HistoryRow::Entry));
    out.push(HistoryRow::Section(SECTION_CLI));
    out.extend(cli.into_iter().map(HistoryRow::Entry));
    out
}

pub fn format_meta_line(entry: &HistoryEntry) -> (String, bool) {
    if let Some(err) = &entry.error {
        (err.clone(), true)
    } else {
        let rows = entry.row_count.map(|r| r.to_string()).unwrap_or_else(|| "?".into());
        let dur = entry.duration_ms.map(|d| d.to_string()).unwrap_or_else(|| "?".into());
        (format!("{} · {rows} řádků · {dur} ms", entry.connection), false)
    }
}

/// Both history recorders funnel through here, so every run that reaches
/// history reaches the log too — and with the same information minus the
/// SQL text.
fn log_run(kind: &str, duration_ms: Option<i64>, row_count: Option<i64>, error: Option<&str>) {
    use dbc_state::applog::{log, Event};
    match error {
        Some(e) => log(Event::QueryFailed { kind: kind.to_string(), error: e.to_string() }),
        None => log(Event::QueryOk {
            kind: kind.to_string(),
            rows: row_count.unwrap_or(0).max(0) as usize,
            ms: duration_ms.unwrap_or(0).max(0) as u64,
        }),
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
        // The log entry is written whether or not history is available —
        // a failed history open is exactly when you want the log — and it
        // carries the statement KIND, never the statement (`statement_kind`
        // returns a `&'static str` from a closed set for that reason).
        log_run(dbc_core::format::statement_kind(sql), duration_ms, row_count, error);
        if let Some(h) = self.history.as_mut() {
            if h.add(sql, connection, started_at, duration_ms, row_count, error).is_ok() {
                self.refresh_history_cache(cx);
            }
        }
    }

    /// G11 T7 (G10 N2: also used for admin writes): same shape as
    /// `record_history`, but records a `kind` other than the implicit
    /// `"query"` — the ONLY way a run shows up in the History panel with a
    /// badge instead of plain text (see `badge_for_kind` and
    /// `render_history_panel`'s row-building closure below). Called from
    /// `main.rs`'s `finish_backup_restore`/`record_backup_restore_history`
    /// (kind `"backup"`/`"restore"`) and `on_confirm_apply`'s
    /// `ApplyTarget::Admin` arm (kind `"admin"`) in place of
    /// `record_history`.
    pub(crate) fn record_history_with_kind(
        &mut self,
        sql: &str,
        connection: &str,
        started_at: i64,
        duration_ms: Option<i64>,
        row_count: Option<i64>,
        error: Option<&str>,
        kind: &str,
        cx: &mut Context<Self>,
    ) {
        // `kind` here is one of this codebase's own literals („admin",
        // „backup", „restore"), so it is safe to log as-is.
        log_run(kind, duration_ms, row_count, error);
        if let Some(h) = self.history.as_mut() {
            if h.add_with_kind(sql, connection, started_at, duration_ms, row_count, error, kind).is_ok() {
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
        self.history_rows = history_rows(&self.history_cache);
        self.last_history_query = query;
    }

    /// Connection name to record with a run (brief contract #2): the active
    /// saved connection's `name` — „{name}/{db}" when the active database
    /// ≠ default (sidebar rework, design §5 row 8) — or `"cli"` for the
    /// CLI-arg back-compat path. Only meaningful once `run_query_with` has
    /// already resolved a `ConnectSpec` successfully (a query can't run
    /// with neither set).
    pub(crate) fn active_connection_name_for_history(&self) -> String {
        if let Some(id) = &self.active_connection_id {
            if let Some(c) = self.config.connections.iter().find(|c| &c.id == id) {
                return history_conn_label(&c.name, self.active_database.as_deref());
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
        let row_count = self.history_rows.len();
        let list = uniform_list(
            "history-list",
            row_count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let mut items = Vec::with_capacity(range.len());
                for row_ix in range {
                    // A heading takes a whole row: `uniform_list` gives
                    // every row the same height, and buying a tighter
                    // divider would mean giving up the virtualization that
                    // lets all 100 entries scroll.
                    let ix = match this.history_rows[row_ix] {
                        HistoryRow::Section(label) => {
                            items.push(
                                div()
                                    .h(px(HISTORY_ROW_HEIGHT))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .border_t_1()
                                    .border_color(cx.theme().border_subtle)
                                    .text_size(px(11.))
                                    .text_color(cx.theme().text_muted)
                                    .child(label)
                                    .into_any_element(),
                            );
                            continue;
                        }
                        HistoryRow::Entry(ix) => ix,
                    };
                    let entry = &this.history_cache[ix];
                    let id = entry.id;
                    let sql_for_click = entry.sql.clone();
                    // G11 T7 (G10 N2: generalized to `badge_for_kind`) — a
                    // small badge prefix for non-"query" runs, the only
                    // rendering change the `kind` column drives;
                    // `format_meta_line`/`collapse_sql` themselves stay
                    // unchanged (the badge is decided once, here, at
                    // render time from `entry.kind`, not a new "meta line"
                    // variant).
                    let raw_line1 = collapse_sql(&entry.sql, SQL_COLLAPSE_MAX_CHARS);
                    let line1 = format!("{}{raw_line1}", badge_for_kind(&entry.kind));
                    let (line2, is_error) = format_meta_line(entry);
                    let line2_color = if is_error { cx.theme().danger } else { cx.theme().text_muted };
                    let starred = entry.starred;
                    let star = if starred { "★" } else { "☆" };
                    let star_color = if starred { cx.theme().warn } else { cx.theme().text_disabled };

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
                            .hover(|s| s.bg(cx.theme().bg_hover))
                            // Brief contract #4: click loads the SQL into
                            // the editor and focuses it, but NEVER runs it.
                            //
                            // Workspace T8: LEGACY CLOBBER SITE 2 of 2 —
                            // this used to overwrite the editor with no
                            // guard at all, destroying a bound script's
                            // unsaved changes. It now routes through THE
                            // guard (Part S §5.5); the focus call is
                            // conditional for the same reason as the
                            // palette's history arm (see it in main.rs).
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.editor_load_guarded(
                                    crate::PendingScriptAction::LoadText {
                                        sql: sql_for_click.clone(),
                                    },
                                    cx,
                                );
                                if view.discard_confirm.is_none() {
                                    let editor_focus = view.sql.focus_handle(cx);
                                    window.focus(&editor_focus, cx);
                                }
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
                                    .child(div().text_color(cx.theme().text_primary).child(line1))
                                    .child(div().text_size(px(11.)).text_color(line2_color).child(line2)),
                            )
                            .into_any_element(),
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
            .border_color(cx.theme().border)
            .bg(cx.theme().bg_app)
            .text_color(cx.theme().text_primary)
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
    fn badge_for_kind_query_has_no_badge() {
        assert_eq!(badge_for_kind("query"), "");
    }

    #[test]
    fn badge_for_kind_admin_gets_its_own_badge() {
        assert_eq!(badge_for_kind("admin"), "🛡 ");
    }

    #[test]
    fn badge_for_kind_backup_and_restore_get_the_g11_badge() {
        assert_eq!(badge_for_kind("backup"), "🗄 ");
        assert_eq!(badge_for_kind("restore"), "🗄 ");
    }

    #[test]
    fn badge_for_kind_unknown_kind_falls_back_gracefully() {
        // Back-compat: a kind this build doesn't recognize (e.g. an older
        // row, or a future kind) must still render something sane, not
        // panic or render blank.
        assert_eq!(badge_for_kind("something-new"), "🗄 ");
        assert_eq!(badge_for_kind(""), "🗄 ");
    }

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
            kind: "query".into(),
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

    // Sidebar rework T8 (design §5 row 8): the recorded connection label.
    #[test]
    fn history_conn_label_appends_db_only_when_non_default() {
        assert_eq!(history_conn_label("prod", None), "prod");
        assert_eq!(history_conn_label("prod", Some("inventory")), "prod/inventory");
    }
}

#[cfg(test)]
mod section_tests {
    use super::*;

    fn entry(id: i64, kind: &str) -> HistoryEntry {
        HistoryEntry {
            id,
            sql: "select 1".into(),
            connection: "c".into(),
            started_at: 0,
            duration_ms: None,
            row_count: None,
            error: None,
            starred: false,
            kind: kind.into(),
        }
    }

    fn labels(rows: &[HistoryRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                HistoryRow::Section(l) => format!("[{l}]"),
                HistoryRow::Entry(i) => i.to_string(),
            })
            .collect()
    }

    /// The common case, and the one that must not regress: somebody who
    /// never runs `dbc` sees the list exactly as before, with no headings
    /// at all.
    #[test]
    fn app_only_history_gets_no_headings() {
        let e = [entry(1, "query"), entry(2, "admin"), entry(3, "backup")];
        assert_eq!(labels(&history_rows(&e)), ["0", "1", "2"]);
    }

    /// And the mirror: an all-CLI history is a list too. „Z příkazové
    /// řádky" over every row would be a heading that distinguishes
    /// nothing.
    #[test]
    fn cli_only_history_gets_no_headings_either() {
        let e = [entry(1, KIND_CLI), entry(2, KIND_CLI)];
        assert_eq!(labels(&history_rows(&e)), ["0", "1"]);
    }

    #[test]
    fn a_mixed_history_splits_into_two_sections_app_first() {
        let e = [entry(1, "query"), entry(2, KIND_CLI), entry(3, "admin"), entry(4, KIND_CLI)];
        assert_eq!(
            labels(&history_rows(&e)),
            ["[V aplikaci]", "0", "2", "[Z příkazové řádky]", "1", "3"]
        );
    }

    /// The order inside a section is the order `search` returned (starred
    /// first, then newest) — sectioning must not resort anything.
    #[test]
    fn sectioning_preserves_the_incoming_order_within_each_group() {
        let e = [entry(9, KIND_CLI), entry(8, "query"), entry(7, KIND_CLI), entry(6, "query")];
        let rows = history_rows(&e);
        let entries: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                HistoryRow::Entry(i) => Some(*i),
                HistoryRow::Section(_) => None,
            })
            .collect();
        assert_eq!(entries, [1, 3, 0, 2]);
    }

    #[test]
    fn an_empty_history_is_an_empty_list() {
        assert!(history_rows(&[]).is_empty());
    }

    /// Every entry must survive sectioning — a heading that swallowed a
    /// row would hide history rather than organise it.
    #[test]
    fn no_entry_is_lost_or_duplicated() {
        let e = [entry(1, "query"), entry(2, KIND_CLI), entry(3, "query")];
        let mut seen: Vec<usize> = history_rows(&e)
            .iter()
            .filter_map(|r| match r {
                HistoryRow::Entry(i) => Some(*i),
                HistoryRow::Section(_) => None,
            })
            .collect();
        seen.sort();
        assert_eq!(seen, [0, 1, 2]);
    }

    #[test]
    fn a_cli_run_carries_its_own_badge() {
        assert_eq!(badge_for_kind(KIND_CLI), "❯ ");
        assert_ne!(badge_for_kind(KIND_CLI), badge_for_kind("query"));
        assert_ne!(badge_for_kind(KIND_CLI), badge_for_kind("backup"));
    }
}
