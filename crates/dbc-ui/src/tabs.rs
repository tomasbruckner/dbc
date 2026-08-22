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
        ResultTab { id: 0, title: "t".into(), pinned, content: TabContent::Text { text: String::new(), scroll_lines: 0 } }
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
}
