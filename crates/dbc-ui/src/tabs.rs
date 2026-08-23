// G2 Task 5: result-tab infrastructure.
//
// `Tabs` is deliberately GPUI-free plain-data logic (no `Entity`/`Context`
// needed to open/close/activate/pin a tab) so its eviction and
// active-index-repair rules can be unit tested directly, without spinning up
// a window. The one GPUI-flavoured piece that lives inside a `ResultTab` is
// `TabContent::Grid`'s `Entity<ResultGrid>` — that's just a typed handle
// (constructing one requires a `Context`, but *holding* one in a plain
// struct doesn't), so `Tabs` itself stays free of any GPUI dependency beyond
// that type name. Rendering (`render_tab_strip`, `render_tab_content`) lives
// on `AppView` in main.rs, not here.

use std::cell::RefCell;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use gpui::Entity;

use crate::grid::ResultGrid;

/// Max number of open result tabs. `Tabs::open` evicts before growing past
/// this, so `iter().count()` never exceeds it.
pub const TAB_CAP: usize = 10;

/// Max character length of a tab title before it's truncated with a
/// trailing '…' (see `collapse_title`).
const TITLE_MAX_CHARS: usize = 30;

pub enum TabContent {
    Grid { grid: Entity<ResultGrid>, buffer: Rc<RefCell<ResultBuffer>> },
    /// Read-only DDL/source view (schema browser previews, G3+). Rendering
    /// support (`AppView::render_tab_content`) and scroll handling exist
    /// already; no producer constructs this variant yet outside unit tests
    /// — the schema browser that will is a later task.
    #[allow(dead_code)]
    Text { text: String, scroll_lines: usize },
    /// G9: server-monitor dashboard tab (one per connection at a time —
    /// keyed via `preview_key = "monitor:{conn_identity}"`, activated
    /// not re-stacked on reopen; see AppView::open_monitor_tab).
    ///
    Monitor { view: Entity<crate::monitor_view::MonitorView> },
    /// G13: an Explain/Analyze execution-plan tab (`AppView::dispatch_plan_query`/
    /// `AppView::on_confirm_analyze_write`) — one per run, stacked like a
    /// normal ad-hoc query tab (no preview-key dedup).
    Plan { view: Entity<crate::plan::PlanView> },
    /// G8 T6: ER diagram tab — one per `open_er_diagram` call (schema-tree
    /// icon or the "ER diagram" palette action), titled `"ER: {schema}"`.
    /// Read-only, never editable — see `crate::er_diagram_view::ErDiagramView`.
    Diagram { view: Entity<crate::er_diagram_view::ErDiagramView> },
    /// G7: schema/data compare tab — a typed `Entity` handle, same shape as
    /// `Grid`'s (tabs.rs stays GPUI-free beyond this type name, per the
    /// file's own module doc comment). Opened by
    /// `AppView::on_compare_schema_pair_ready`, stacked like an ad-hoc query
    /// tab (no preview-key dedup — repeated compares of the same pair just
    /// open more tabs, matching `Plan`'s own precedent).
    Compare { view: Entity<crate::compare::CompareView> },
    /// G12 T3: live progress tab for a script run (`AppView::confirm_script_run`)
    /// or a CSV import (T4, `AppView::confirm_csv_import` — reuses this same
    /// tab kind, see `ScriptRunState.progress_rows`). Plain data behind an
    /// `Rc<RefCell<_>>` (not an `Entity`) — the spawned event loop mutates it
    /// directly and calls `cx.notify()` itself, same "no GPUI in the state
    /// type" posture the rest of this file keeps; rendering lives on
    /// `AppView::render_tab_content` like every other variant.
    ScriptRun { state: Rc<RefCell<ScriptRunState>> },
    /// G10 T4: the "Správa serveru" admin panel — one per connection at a
    /// time (`preview_key = admin_panel::ADMIN_PREVIEW_KEY`, re-focused on
    /// same-connection reopen, closed+replaced on a stale-connection
    /// reopen; see `AppView::open_admin_tab`). Typed `Entity` handle, same
    /// "tabs.rs stays GPUI-free beyond this type name" posture as `Grid`/
    /// `Compare`.
    Admin { view: Entity<crate::admin_panel::AdminPanel> },
}

/// G12 T3: outcome of a script/CSV-import run, driving the progress tab's
/// summary line and its "Zrušit" button's visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptRunOutcome {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// G12 T3: per-file row status in the progress tab's file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptFileStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

pub struct ScriptFileRow {
    pub name: String,
    pub status: ScriptFileStatus,
    pub statements_run: usize,
    pub statements_failed: usize,
}

/// Cap on retained log lines in `ScriptRunState.log` — same fixed-cap
/// posture as `TAB_CAP`, not user-tunable.
pub const SCRIPT_LOG_CAP: usize = 1000;

/// G12 T3/T4: plain-data progress state behind `TabContent::ScriptRun`,
/// mutated in place by the spawned event loop (`AppView::confirm_script_run`
/// / `AppView::confirm_csv_import`) as `ScriptEvent`/`CsvImportEvent`s
/// arrive, then rendered fresh every `cx.notify()` — no separate "apply
/// events to a snapshot" step, the state IS what's rendered.
pub struct ScriptRunState {
    pub files: Vec<ScriptFileRow>,
    /// From the UI pre-scan (scripts) or the row pre-count (CSV, T4).
    pub total_statements: usize,
    pub statements_run: usize,
    pub statements_failed: usize,
    pub total_affected: u64,
    /// CSV import only (T4): `(rows done, rows total)` — drives an honest
    /// progress bar. `None` for script runs.
    pub progress_rows: Option<(u64, u64)>,
    pub log: std::collections::VecDeque<String>,
    pub outcome: ScriptRunOutcome,
    pub started_at: std::time::Instant,
    pub elapsed: Option<std::time::Duration>,
}

impl ScriptRunState {
    /// Appends `line`, evicting the oldest entries past `SCRIPT_LOG_CAP` —
    /// same eviction posture as `Tabs::open`'s `TAB_CAP`.
    pub fn push_log(&mut self, line: String) {
        self.log.push_back(line);
        while self.log.len() > SCRIPT_LOG_CAP {
            self.log.pop_front();
        }
    }
}

pub struct ResultTab {
    /// Monotonic, assigned by `Tabs::open` (any value passed in is
    /// overwritten) — callers never need to invent a unique id themselves.
    pub id: u64,
    /// ≤30 chars + '…' if truncated; see `collapse_title`. Convention for
    /// non-query tabs (schema browser, G3+): "Náhled: {table}" for a data
    /// preview, "DDL: {name}" for a DDL view.
    pub title: String,
    pub pinned: bool,
    /// (schema, table) identity for a data-preview tab opened via
    /// `TreeEvent::OpenPreview` (G2 Task 7) — `None` for every other kind of
    /// tab (a run from the SQL editor, a DDL text tab, ...). Lets
    /// `close_by_preview_key` find and replace an existing preview for the
    /// same table instead of stacking a duplicate (brief contract #1).
    pub preview_key: Option<String>,
    /// G5 Task 4 review fix (BLOCKER 1): the connection identity (`main.rs::
    /// AppView::current_conn_identity` — `active_connection_id`, or the
    /// CLI-arg sentinel) THIS tab's data was fetched from, stamped once at
    /// open time. The Apply flow refuses to touch a tab's staged edits once
    /// this no longer matches the CURRENTLY active identity (switching
    /// connections after staging edits on a preview must never let
    /// "Aplikovat" run those edits' PK-based statements against a
    /// DIFFERENT, currently-active connection/database) — see
    /// `AppView::conn_identity_matches` and its call sites
    /// (`on_open_apply_dialog`/`on_confirm_apply`/`render_apply_bar`).
    /// Meaningless (but always present, for a uniform `ResultTab` shape) on
    /// a non-editable/`Text` tab.
    pub conn_identity: String,
    pub content: TabContent,
}

/// Collapses `sql` to a single line (runs of whitespace, including
/// newlines, become a single space) and truncates to `TITLE_MAX_CHARS`
/// characters, appending '…' when truncation happened.
pub fn collapse_title(sql: &str) -> String {
    let collapsed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let truncated: String = chars.by_ref().take(TITLE_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Open result tabs, in insertion order, plus which one (if any) is active.
/// Plain data — see module doc comment for why this has no GPUI dependency.
pub struct Tabs {
    tabs: Vec<ResultTab>,
    active: Option<usize>,
    next_id: u64,
}

impl Tabs {
    pub fn new() -> Self {
        Self { tabs: Vec::new(), active: None, next_id: 1 }
    }

    /// Inserts `tab`, assigning it a fresh monotonic id (overwriting
    /// whatever `tab.id` was) and returning that id. If already at
    /// `TAB_CAP`, evicts one tab first: the oldest (lowest id) unpinned tab,
    /// or — if every open tab is pinned — the oldest pinned tab. The newly
    /// opened tab always becomes the active one.
    pub fn open(&mut self, mut tab: ResultTab) -> u64 {
        if self.tabs.len() >= TAB_CAP {
            let evict_ix = self
                .tabs
                .iter()
                .enumerate()
                .filter(|(_, t)| !t.pinned)
                .min_by_key(|(_, t)| t.id)
                .map(|(ix, _)| ix)
                .or_else(|| self.tabs.iter().enumerate().min_by_key(|(_, t)| t.id).map(|(ix, _)| ix));
            if let Some(ix) = evict_ix {
                self.tabs.remove(ix);
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        tab.id = id;
        self.tabs.push(tab);
        self.active = Some(self.tabs.len() - 1);
        id
    }

    /// Removes the tab with `id` (a no-op if it's not open). Repairs
    /// `active` so it keeps pointing at the same logical tab when a tab
    /// *before* the active one is closed, and — when the active tab itself
    /// is the one closed — prefers the right neighbour (which slides into
    /// the closed tab's index), falling back to the left neighbour, falling
    /// back to `None` if the last tab was closed.
    pub fn close(&mut self, id: u64) {
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let was_active = self.active == Some(ix);
        self.tabs.remove(ix);

        if self.tabs.is_empty() {
            self.active = None;
        } else if was_active {
            self.active = Some(if ix < self.tabs.len() { ix } else { ix - 1 });
        } else if let Some(a) = self.active {
            if a > ix {
                self.active = Some(a - 1);
            }
        }
    }

    /// Closes the currently-open tab whose `preview_key` equals `key`, if
    /// any (a no-op otherwise) — called by `AppView::run_query_with` right
    /// before opening a fresh preview tab for the same (schema, table), so
    /// re-previewing replaces rather than stacks (brief contract #1). Reuses
    /// `close`'s active-index-repair logic rather than duplicating it.
    pub fn close_by_preview_key(&mut self, key: &str) {
        if let Some(id) = self.tabs.iter().find(|t| t.preview_key.as_deref() == Some(key)).map(|t| t.id) {
            self.close(id);
        }
    }

    pub fn activate(&mut self, id: u64) {
        if let Some(ix) = self.tabs.iter().position(|t| t.id == id) {
            self.active = Some(ix);
        }
    }

    pub fn toggle_pin(&mut self, id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.pinned = !t.pinned;
        }
    }

    pub fn active(&self) -> Option<&ResultTab> {
        self.active.and_then(|ix| self.tabs.get(ix))
    }

    pub fn active_mut(&mut self) -> Option<&mut ResultTab> {
        self.active.and_then(move |ix| self.tabs.get_mut(ix))
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResultTab> {
        self.tabs.iter()
    }
}

impl Default for Tabs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_tab(pinned: bool) -> ResultTab {
        ResultTab {
            id: 0,
            title: "t".into(),
            pinned,
            preview_key: None,
            conn_identity: "conn-a".into(),
            content: TabContent::Text { text: String::new(), scroll_lines: 0 },
        }
    }

    fn preview_tab(key: &str) -> ResultTab {
        ResultTab {
            id: 0,
            title: format!("Náhled: {key}"),
            pinned: false,
            preview_key: Some(key.to_string()),
            conn_identity: "conn-a".into(),
            content: TabContent::Text { text: String::new(), scroll_lines: 0 },
        }
    }

    #[test]
    fn collapse_title_collapses_whitespace_and_leaves_short_text_alone() {
        assert_eq!(collapse_title("select 1"), "select 1");
        assert_eq!(collapse_title("select\n  *\nfrom\tt"), "select * from t");
    }

    #[test]
    fn collapse_title_truncates_long_text_with_ellipsis() {
        let long = "a".repeat(40);
        let title = collapse_title(&long);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));
        assert!(title.starts_with(&"a".repeat(TITLE_MAX_CHARS)));
    }

    #[test]
    fn cap_eviction_keeps_ten_and_drops_oldest_unpinned() {
        let mut tabs = Tabs::new();
        let mut ids = Vec::new();
        for _ in 0..11 {
            ids.push(tabs.open(text_tab(false)));
        }
        assert_eq!(tabs.iter().count(), 10);
        assert!(tabs.iter().all(|t| t.id != ids[0]), "oldest tab should have been evicted");
        assert!(tabs.iter().any(|t| t.id == ids[1]));
        assert!(tabs.iter().any(|t| t.id == *ids.last().unwrap()));
    }

    #[test]
    fn pinned_tab_survives_eviction() {
        let mut tabs = Tabs::new();
        let pinned_id = tabs.open(text_tab(true));
        for _ in 0..10 {
            tabs.open(text_tab(false));
        }
        assert_eq!(tabs.iter().count(), 10);
        assert!(tabs.iter().any(|t| t.id == pinned_id), "pinned tab must not be evicted while unpinned tabs remain");
    }

    #[test]
    fn eviction_falls_back_to_oldest_pinned_when_all_pinned() {
        let mut tabs = Tabs::new();
        let mut ids = Vec::new();
        for _ in 0..11 {
            ids.push(tabs.open(text_tab(true)));
        }
        assert_eq!(tabs.iter().count(), 10);
        assert!(tabs.iter().all(|t| t.id != ids[0]));
    }

    #[test]
    fn close_active_middle_selects_right_neighbour() {
        let mut tabs = Tabs::new();
        let a = tabs.open(text_tab(false));
        let b = tabs.open(text_tab(false));
        let c = tabs.open(text_tab(false));
        tabs.activate(b);
        tabs.close(b);
        assert_eq!(tabs.active().map(|t| t.id), Some(c));
        assert!(tabs.iter().any(|t| t.id == a));
    }

    #[test]
    fn close_active_last_selects_left_neighbour() {
        let mut tabs = Tabs::new();
        let a = tabs.open(text_tab(false));
        let b = tabs.open(text_tab(false));
        tabs.activate(b);
        tabs.close(b);
        assert_eq!(tabs.active().map(|t| t.id), Some(a));
    }

    #[test]
    fn close_last_remaining_tab_leaves_no_active_tab() {
        let mut tabs = Tabs::new();
        let a = tabs.open(text_tab(false));
        tabs.close(a);
        assert!(tabs.active().is_none());
        assert_eq!(tabs.iter().count(), 0);
    }

    #[test]
    fn closing_a_tab_before_active_shifts_active_index_down() {
        let mut tabs = Tabs::new();
        let a = tabs.open(text_tab(false));
        let b = tabs.open(text_tab(false));
        tabs.activate(b);
        tabs.close(a);
        assert_eq!(tabs.active().map(|t| t.id), Some(b));
    }

    #[test]
    fn activate_and_toggle_pin_round_trip() {
        let mut tabs = Tabs::new();
        let a = tabs.open(text_tab(false));
        let _b = tabs.open(text_tab(false));
        tabs.activate(a);
        assert_eq!(tabs.active().map(|t| t.id), Some(a));
        assert_eq!(tabs.active().map(|t| t.pinned), Some(false));

        tabs.toggle_pin(a);
        assert_eq!(tabs.active().map(|t| t.pinned), Some(true));
        tabs.toggle_pin(a);
        assert_eq!(tabs.active().map(|t| t.pinned), Some(false));
    }

    #[test]
    fn close_by_preview_key_replaces_existing_preview_for_same_key() {
        let mut tabs = Tabs::new();
        let a = tabs.open(preview_tab("public.users"));
        let b = tabs.open(text_tab(false));
        tabs.close_by_preview_key("public.users");
        assert!(tabs.iter().all(|t| t.id != a), "old preview tab should have been closed");
        assert!(tabs.iter().any(|t| t.id == b));
    }

    #[test]
    fn close_by_preview_key_is_noop_when_no_matching_tab_is_open() {
        let mut tabs = Tabs::new();
        let a = tabs.open(text_tab(false));
        tabs.close_by_preview_key("nope");
        assert!(tabs.iter().any(|t| t.id == a));
    }

    // G5 Task 4 review fix (BLOCKER 1): `Tabs::open` must not touch/drop
    // `conn_identity` — the Apply flow's connection-identity guard depends on
    // it surviving exactly as stamped, unmodified by tab-cap eviction, id
    // assignment, or activation bookkeeping.
    #[test]
    fn script_log_caps_at_limit() {
        let mut s = ScriptRunState {
            files: Vec::new(),
            total_statements: 0,
            statements_run: 0,
            statements_failed: 0,
            total_affected: 0,
            progress_rows: None,
            log: std::collections::VecDeque::new(),
            outcome: ScriptRunOutcome::Running,
            started_at: std::time::Instant::now(),
            elapsed: None,
        };
        for i in 0..(SCRIPT_LOG_CAP + 10) {
            s.push_log(format!("line {i}"));
        }
        assert_eq!(s.log.len(), SCRIPT_LOG_CAP);
        assert_eq!(s.log.front().map(String::as_str), Some("line 10"));
    }

    #[test]
    fn conn_identity_survives_open_and_is_readable_off_the_tab() {
        let mut tabs = Tabs::new();
        let mut tab = text_tab(false);
        tab.conn_identity = "conn-B-id".to_string();
        let id = tabs.open(tab);
        let found = tabs.iter().find(|t| t.id == id).expect("tab just opened");
        assert_eq!(found.conn_identity, "conn-B-id");
    }
}
