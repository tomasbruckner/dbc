//! Editor tab bookkeeping — which tabs exist, in what order, which one is
//! active. Generic over the payload so this half is testable without
//! GPUI; `main.rs` instantiates it with `EditorTab` (the `Entity<SqlInput>`,
//! context, results and everything else that used to assume one editor).
//!
//! Ids are handed out once and never recycled: every async completion in
//! `main.rs` finds its target by id (spec §2), and a recycled id would let a
//! result meant for a closed tab land in a new one.
//!
//! There is always at least one tab (spec §1). `close` refuses the last one
//! and the caller replaces its contents instead.

pub const MAX_EDITOR_TABS: usize = 32;
const TITLE_MAX_CHARS: usize = 32;

pub struct Slot<P> {
    pub id: u64,
    /// Creation ordinal, for the „Dotaz N" fallback title. Unlike `id` it
    /// is what the user sees, so it counts from 1 in creation order.
    pub ordinal: u64,
    pub payload: P,
}

pub struct EditorTabs<P> {
    tabs: Vec<Slot<P>>,
    active: usize,
    next_id: u64,
}

impl<P> EditorTabs<P> {
    pub fn new(first: P) -> Self {
        Self { tabs: vec![Slot { id: 1, ordinal: 1, payload: first }], active: 0, next_id: 2 }
    }

    /// Inserts right after the active tab (where a browser puts a new tab
    /// opened from the current one) and activates it. Beyond the cap the
    /// open is refused and the payload dropped — the caller checks `len()`
    /// first when it wants to say so.
    pub fn open(&mut self, payload: P) -> u64 {
        if self.tabs.len() >= MAX_EDITOR_TABS {
            return self.active_id();
        }
        let id = self.next_id;
        self.next_id += 1;
        let ordinal = id;
        let at = self.active + 1;
        self.tabs.insert(at, Slot { id, ordinal, payload });
        self.active = at;
        id
    }

    pub fn close(&mut self, id: u64) -> Option<P> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let ix = self.tabs.iter().position(|t| t.id == id)?;
        let removed = self.tabs.remove(ix);
        if self.active > ix || self.active == self.tabs.len() {
            self.active -= 1;
        }
        Some(removed.payload)
    }

    pub fn activate(&mut self, id: u64) -> bool {
        match self.tabs.iter().position(|t| t.id == id) {
            Some(ix) => {
                self.active = ix;
                true
            }
            None => false,
        }
    }

    pub fn next(&mut self) -> u64 {
        self.active = (self.active + 1) % self.tabs.len();
        self.active_id()
    }

    pub fn prev(&mut self) -> u64 {
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        self.active_id()
    }

    pub fn active(&self) -> &Slot<P> {
        &self.tabs[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Slot<P> {
        &mut self.tabs[self.active]
    }

    pub fn active_id(&self) -> u64 {
        self.tabs[self.active].id
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn by_id(&self, id: u64) -> Option<&Slot<P>> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn by_id_mut(&mut self, id: u64) -> Option<&mut Slot<P>> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Slot<P>> {
        self.tabs.iter()
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }
}

/// What the tab strip shows (spec §4): the bound script's file name, else
/// the first non-empty line of the buffer trimmed to 32 chars, else
/// „Dotaz N". Derived every render, never stored.
pub fn derive_title(bound_file: Option<&str>, sql: &str, ordinal: u64) -> String {
    if let Some(name) = bound_file {
        return name.to_string();
    }
    let line = sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.is_empty() {
        return format!("Dotaz {ordinal}");
    }
    let mut out: String = line.chars().take(TITLE_MAX_CHARS).collect();
    if line.chars().count() > TITLE_MAX_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_one_tab_that_is_active() {
        let t = EditorTabs::new("a");
        assert_eq!(t.len(), 1);
        assert_eq!(t.active_id(), 1);
        assert_eq!(t.active().payload, "a");
    }

    #[test]
    fn open_inserts_after_the_active_tab_and_activates_it() {
        let mut t = EditorTabs::new("a");
        let b = t.open("b");
        t.activate(1);
        let c = t.open("c");
        let order: Vec<&str> = t.iter().map(|s| s.payload).collect();
        assert_eq!(order, vec!["a", "c", "b"]);
        assert_eq!(t.active_id(), c);
        assert_ne!(b, c);
    }

    #[test]
    fn ids_are_never_recycled() {
        let mut t = EditorTabs::new("a");
        let b = t.open("b");
        t.close(b);
        let c = t.open("c");
        assert!(c > b);
    }

    #[test]
    fn closing_the_active_tab_activates_its_left_neighbour_or_the_new_first() {
        let mut t = EditorTabs::new("a");
        let b = t.open("b");
        let c = t.open("c");
        assert_eq!(t.close(c).unwrap(), "c");
        assert_eq!(t.active_id(), b);
        t.activate(1);
        assert_eq!(t.close(1).unwrap(), "a");
        assert_eq!(t.active().payload, "b");
    }

    #[test]
    fn the_last_tab_cannot_be_closed() {
        let mut t = EditorTabs::new("a");
        assert!(t.close(1).is_none());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn next_and_prev_wrap() {
        let mut t = EditorTabs::new("a");
        let b = t.open("b");
        let c = t.open("c");
        assert_eq!(t.next(), 1);
        assert_eq!(t.prev(), c);
        assert_eq!(t.prev(), b);
    }

    #[test]
    fn by_id_is_none_after_close() {
        let mut t = EditorTabs::new("a");
        let b = t.open("b");
        assert!(t.by_id(b).is_some());
        t.close(b);
        assert!(t.by_id(b).is_none());
        assert!(t.by_id_mut(b).is_none());
    }

    #[test]
    fn open_refuses_beyond_the_cap() {
        let mut t = EditorTabs::new(0);
        for i in 1..MAX_EDITOR_TABS {
            t.open(i);
        }
        let before = t.len();
        t.open(99);
        assert_eq!(t.len(), before, "cap holds");
        assert_eq!(t.active().payload, MAX_EDITOR_TABS - 1, "active is unchanged by a refused open");
    }

    #[test]
    fn title_prefers_the_bound_file_then_the_first_line_then_the_ordinal() {
        assert_eq!(derive_title(Some("report.sql"), "select 1", 3), "report.sql");
        assert_eq!(derive_title(None, "\n\n  select * from app_user  \nwhere 1", 3), "select * from app_user");
        assert_eq!(derive_title(None, "   \n", 3), "Dotaz 3");
        let long = "select a_very_long_column_name, another_one from somewhere";
        assert_eq!(derive_title(None, long, 1).chars().count(), 32 + 1, "trimmed to 32 + ellipsis");
        assert!(derive_title(None, long, 1).ends_with('…'));
    }
}
