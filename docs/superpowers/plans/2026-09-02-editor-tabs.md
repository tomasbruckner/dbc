# Editor Tabs + Database Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single global SQL editor with editor tabs that each own their connection/database and their result strip, and add a Ctrl+D picker that switches the active tab's database across every saved connection.

**Architecture:** A generic, GPUI-free `EditorTabs<P>` ring owns ids/order/active index; `AppView` stores `EditorTabs<EditorTab>` where the payload carries everything that used to assume one editor (`sql`, context, results, script binding, autocomplete, running query, status). All ~260 call sites go through `editor()`/`editor_mut()`; every async completion resolves its target by editor id. The session file gains `editors`/`active_editor` while keeping the legacy fields readable and written for the active tab.

**Tech Stack:** Rust 2021, GPUI (zed rev 907ed09), serde/toml session file in `dbc-state`, existing `Tabs`/palette/keymap infrastructure in `crates/dbc-ui`.

**Spec:** `docs/superpowers/specs/2026-09-02-editor-tabs-design.md`

## Global Constraints

- GPUI cannot be rendered or driven headlessly here; every testable rule lives in a pure function or a source-scan audit (the file's own pattern — see `only_the_guarded_sites_may_replace_the_sql_editors_buffer`).
- `SqlInput::replace_buffer` may only be called from the three sanctioned sites (`bind_script`, `perform_script_action`, `rewrite_buffer_in_place`); a fresh tab gets its text via `insert_text`, exactly as startup does today (`main.rs:14730`).
- Keyboard chords are registered in THREE places or the keymap audit fails: `cx.bind_keys([...])` (`main.rs:14661`), `keymap::SHORTCUTS`, and the `actions!` block.
- Copy is Czech, comments English with the user's Czech quote where a decision came from a report; match the surrounding comment density.
- Session file: never drop the legacy top-level fields (`connection`, `database`, `editor`, `cursor`, `tabs`); a downgrade must still open the active tab.
- Never run cargo from two worktrees at once (`.cargo/config.toml`). Build with `cargo build -p dbc-ui`; the app exe is locked while running — close it (WM_CLOSE saves the session) before building.
- Do not commit unless the user has asked for commits in this session; each task's final step is a checkpoint (`cargo test -p dbc-ui --bin dbc-ui` green + `git diff --stat` reviewed), and a commit only on request.

---

### Task 1: `EditorTabs<P>` — the pure tab ring

**Files:**
- Create: `crates/dbc-ui/src/editor_tabs.rs`
- Modify: `crates/dbc-ui/src/main.rs:60-70` (add `mod editor_tabs;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct EditorTabs<P> { /* private */ }
  pub struct Slot<P> { pub id: u64, pub ordinal: u64, pub payload: P }
  impl<P> EditorTabs<P> {
      pub fn new(first: P) -> Self;                 // always ≥ 1 tab; first gets id 1, ordinal 1
      pub fn open(&mut self, payload: P) -> u64;    // pushes after the active tab, activates it
      pub fn close(&mut self, id: u64) -> Option<P>;// returns the closed payload; refuses (None) when it is the last tab
      pub fn activate(&mut self, id: u64) -> bool;
      pub fn next(&mut self) -> u64;                // wraps
      pub fn prev(&mut self) -> u64;                // wraps
      pub fn active(&self) -> &Slot<P>;
      pub fn active_mut(&mut self) -> &mut Slot<P>;
      pub fn active_id(&self) -> u64;
      pub fn by_id(&self, id: u64) -> Option<&Slot<P>>;
      pub fn by_id_mut(&mut self, id: u64) -> Option<&mut Slot<P>>;
      pub fn iter(&self) -> impl Iterator<Item = &Slot<P>>;
      pub fn len(&self) -> usize;
      pub fn active_index(&self) -> usize;
  }
  pub fn derive_title(bound_file: Option<&str>, sql: &str, ordinal: u64) -> String;
  pub const MAX_EDITOR_TABS: usize = 32;
  ```

- [ ] **Step 1: Write the failing tests**

```rust
// crates/dbc-ui/src/editor_tabs.rs (bottom)
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p dbc-ui --bin dbc-ui -- editor_tabs::`
Expected: compile error — `editor_tabs` module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
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
```

And in `main.rs` next to the other `mod` lines: `mod editor_tabs;`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p dbc-ui --bin dbc-ui -- editor_tabs::`
Expected: 9 passed.

- [ ] **Step 5: Checkpoint**

Run: `cargo test -p dbc-ui --bin dbc-ui` — all green. Commit only if asked: `git add crates/dbc-ui/src/editor_tabs.rs crates/dbc-ui/src/main.rs && git commit -m "feat: editor tab bookkeeping, generic and GPUI-free"`.

---

### Task 2: Session model — `editors`, `active_editor`, legacy migration

**Files:**
- Modify: `crates/dbc-state/src/session.rs:50-115` (structs, `clamped`) and its tests module
- Test: same file

**Interfaces:**
- Produces:
  ```rust
  pub struct SessionEditor {
      pub sql: String, pub cursor: usize,
      pub connection: Option<String>, pub database: Option<String>,
      pub script_path: Option<PathBuf>,
      pub tabs: Vec<SessionTab>,
  }
  // on SessionState:
  pub editors: Vec<SessionEditor>,   // serde(default, skip_serializing_if = "Vec::is_empty")
  pub active_editor: usize,          // serde(default)
  pub fn editors_or_legacy(&self) -> (Vec<SessionEditor>, usize);
  pub const MAX_EDITORS: usize = 32;
  ```

- [ ] **Step 1: Write the failing tests** (in `session.rs`'s existing `mod tests`)

```rust
    #[test]
    fn editors_round_trip_through_toml() {
        let s = SessionState {
            editors: vec![
                SessionEditor {
                    sql: "select 1".into(),
                    cursor: 3,
                    connection: Some("c1".into()),
                    database: Some("sales".into()),
                    script_path: Some(PathBuf::from("D:/x/report.sql")),
                    tabs: vec![SessionTab { title: "t".into(), sql: "select 1".into(), pinned: true }],
                },
                SessionEditor { sql: "select 2".into(), ..Default::default() },
            ],
            active_editor: 1,
            ..Default::default()
        };
        let text = toml::to_string(&s).unwrap();
        let back: SessionState = toml::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    /// A session written before editors existed becomes editor 0 — and
    /// nothing is lost: text, cursor, context and the result tabs all move.
    #[test]
    fn a_legacy_session_becomes_a_single_editor() {
        let legacy = SessionState {
            connection: Some("c1".into()),
            database: None,
            editor: "select 1".into(),
            cursor: 2,
            tabs: vec![SessionTab { title: "t".into(), sql: "select 1".into(), pinned: false }],
            ..Default::default()
        };
        let (editors, active) = legacy.editors_or_legacy();
        assert_eq!(active, 0);
        assert_eq!(editors.len(), 1);
        assert_eq!(editors[0].sql, "select 1");
        assert_eq!(editors[0].cursor, 2);
        assert_eq!(editors[0].connection.as_deref(), Some("c1"));
        assert_eq!(editors[0].tabs.len(), 1);
    }

    /// An EMPTY legacy session still yields one (empty) editor: the app
    /// always has a tab.
    #[test]
    fn an_empty_session_still_yields_one_editor() {
        let (editors, active) = SessionState::default().editors_or_legacy();
        assert_eq!((editors.len(), active), (1, 0));
        assert!(editors[0].sql.is_empty());
    }

    #[test]
    fn when_editors_exist_the_legacy_fields_are_ignored() {
        let s = SessionState {
            editor: "legacy".into(),
            editors: vec![SessionEditor { sql: "new".into(), ..Default::default() }],
            active_editor: 0,
            ..Default::default()
        };
        let (editors, _) = s.editors_or_legacy();
        assert_eq!(editors[0].sql, "new");
    }

    #[test]
    fn clamped_caps_editors_and_their_tabs_and_the_active_index() {
        let mut s = SessionState::default();
        for _ in 0..(MAX_EDITORS + 5) {
            s.editors.push(SessionEditor {
                sql: "x".into(),
                tabs: (0..(MAX_TABS + 3)).map(|_| SessionTab::default()).collect(),
                ..Default::default()
            });
        }
        s.active_editor = 999;
        let s = s.clamped();
        assert_eq!(s.editors.len(), MAX_EDITORS);
        assert!(s.editors.iter().all(|e| e.tabs.len() <= MAX_TABS));
        assert_eq!(s.active_editor, MAX_EDITORS - 1);
    }

    #[test]
    fn clamped_drops_an_oversized_editor_buffer_and_fixes_its_cursor() {
        let mut s = SessionState::default();
        s.editors.push(SessionEditor { sql: "a".repeat(MAX_SQL_BYTES + 1), cursor: 5, ..Default::default() });
        s.editors.push(SessionEditor { sql: "žluť".into(), cursor: 2, ..Default::default() }); // 2 is inside 'ž'
        let s = s.clamped();
        assert!(s.editors[0].sql.is_empty());
        assert_eq!(s.editors[0].cursor, 0);
        assert!(s.editors[1].sql.is_char_boundary(s.editors[1].cursor));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p dbc-state -- session::`
Expected: compile errors (`SessionEditor`, `editors`, `editors_or_legacy`, `MAX_EDITORS` unknown).

- [ ] **Step 3: Implement**

```rust
// session.rs — next to MAX_TABS
pub const MAX_EDITORS: usize = 32;

/// One editor tab (spec §3). `tabs` are THIS editor's result tabs — the
/// same shape the legacy top-level `tabs` had for the one editor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEditor {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sql: String,
    #[serde(default)]
    pub cursor: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<SessionTab>,
}

// on SessionState, after `tabs`:
    /// Editor tabs (2026-09-02). The legacy `connection`/`database`/
    /// `editor`/`cursor`/`tabs` above are STILL WRITTEN, for the active
    /// editor, so a build from before editor tabs opens with that one
    /// intact; on load they are only read when this is empty
    /// (`editors_or_legacy`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editors: Vec<SessionEditor>,
    #[serde(default)]
    pub active_editor: usize,

impl SessionState {
    /// The editors to open: `editors` when present, else the legacy fields
    /// as one editor — and never none, because the app always has a tab.
    pub fn editors_or_legacy(&self) -> (Vec<SessionEditor>, usize) {
        if !self.editors.is_empty() {
            return (self.editors.clone(), self.active_editor.min(self.editors.len() - 1));
        }
        let one = SessionEditor {
            sql: self.editor.clone(),
            cursor: self.cursor,
            connection: self.connection.clone(),
            database: self.database.clone(),
            script_path: None,
            tabs: self.tabs.clone(),
        };
        (vec![one], 0)
    }
}

// in clamped(), after the existing tabs/expanded handling:
        self.editors.truncate(MAX_EDITORS);
        for e in &mut self.editors {
            if e.sql.len() > MAX_SQL_BYTES {
                e.sql = String::new();
                e.cursor = 0;
            }
            e.cursor = e.cursor.min(e.sql.len());
            while !e.sql.is_char_boundary(e.cursor) {
                e.cursor -= 1;
            }
            e.tabs.retain(|t| t.sql.len() <= MAX_SQL_BYTES);
            e.tabs.truncate(MAX_TABS);
        }
        if !self.editors.is_empty() {
            self.active_editor = self.active_editor.min(self.editors.len() - 1);
        }
```

Add `use std::path::PathBuf;` if not already imported. `is_empty()` (used by `save` to delete an empty session) keeps comparing against `default()` — a state with one empty editor and nothing else must still count as empty: in `capture_session` (Task 3) write `editors` only when there is more than one editor OR any editor differs from the legacy fields; simplest honest rule, pinned by a test in Task 3: **a single empty tab writes no `editors` at all**.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p dbc-state -- session::`
Expected: all pass, including the pre-existing session tests.

- [ ] **Step 5: Checkpoint**

`cargo test -p dbc-state` green. Commit only if asked: `git commit -am "feat(session): editor tabs in the session file, legacy fields still written"`.

---

### Task 3: Move the single-editor state into `EditorTab` (mechanical)

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` — struct `AppView` (fields at 2051-2074, 2142-2400), constructor (`14727-14870`), `capture_session` (6843), `restore_session_context` (6880), `session_tabs`/`restored_tabs` (2622-2660), and every `self.sql` / `self.tabs` / `self.active_connection_id` / `self.active_database` / `self.script_*` / `self.editor_discard_grant` / `self.autocomplete` / `self.last_ac_*` / `self.cancel` / `self.run_generation` / `self.started_at` / `self.status` / `self.attempted_restore` site.
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`on_dropdown_item_click` and the other `impl AppView` blocks that touch those fields), `backup.rs`, `history_panel.rs`, `tabs.rs`, `grid.rs`, `prefetch.rs`, `tree_menu.rs`, `runner.rs`, `schema_tree.rs` only where they name `active_connection_id`/`active_database`/`tabs` directly (grep list in spec §1).

**Interfaces:**
- Produces (in `main.rs`):
  ```rust
  pub(crate) struct EditorTab {
      pub sql: Entity<SqlInput>,
      pub connection: Option<String>,
      pub database: Option<String>,
      /// Context the tab carries from the session but has not connected
      /// with yet (spec §3). Cleared by the first successful switch.
      pub unverified: bool,
      pub results: Tabs,
      pub script_binding: Option<ScriptBinding>,
      pub script_dirty_flag: bool,
      pub script_binding_generation: u64,
      pub editor_discard_grant: Option<u64>,
      pub script_save_in_flight: bool,
      pub autocomplete: Option<AutocompleteState>,
      pub last_ac_text: String,
      pub last_ac_cursor: usize,
      pub cancel: Option<CancelToken>,
      pub run_generation: u64,
      pub started_at: Option<std::time::Instant>,
      pub status: String,
      pub attempted_restore: Option<(String, Option<String>)>,
  }
  impl EditorTab { pub fn new(sql: Entity<SqlInput>, connection: Option<String>, database: Option<String>) -> Self; }
  // on AppView:
  editors: editor_tabs::EditorTabs<EditorTab>,
  fn editor(&self) -> &EditorTab;
  fn editor_mut(&mut self) -> &mut EditorTab;
  fn editor_id(&self) -> u64;
  fn editor_by_id_mut(&mut self, id: u64) -> Option<&mut EditorTab>;
  ```
  Old field names disappear from `AppView` (the compiler is the checklist).

- [ ] **Step 1: Write the failing tests** (in `main.rs`'s session tests, near `restored_tabs` tests at ~16382)

```rust
    /// A single, empty tab writes NO `editors` — so an untouched app still
    /// removes its session file (`save` deletes an empty state) exactly as
    /// before editor tabs.
    #[test]
    fn a_lone_empty_editor_writes_no_editors_block() {
        let editors = vec![EditorSnapshot { sql: String::new(), cursor: 0, connection: None, database: None, script_path: None, tabs: Vec::new() }];
        let (list, _) = session_editors(&editors, 0);
        assert!(list.is_empty());
    }

    /// Two tabs, or one with content, write the block — and the legacy
    /// fields mirror the ACTIVE tab.
    #[test]
    fn the_legacy_fields_mirror_the_active_editor() {
        let editors = vec![
            EditorSnapshot { sql: "a".into(), cursor: 1, connection: Some("c1".into()), database: None, script_path: None, tabs: Vec::new() },
            EditorSnapshot { sql: "b".into(), cursor: 1, connection: Some("c2".into()), database: Some("d".into()), script_path: None, tabs: Vec::new() },
        ];
        let (list, legacy) = session_editors(&editors, 1);
        assert_eq!(list.len(), 2);
        assert_eq!(legacy.editor, "b");
        assert_eq!(legacy.connection.as_deref(), Some("c2"));
        assert_eq!(legacy.database.as_deref(), Some("d"));
    }
```

`EditorSnapshot` is a plain struct (no GPUI) that `capture_session` builds from each `EditorTab`; `session_editors(&[EditorSnapshot], active) -> (Vec<SessionEditor>, LegacyFields)` is the pure core. Define both in Step 3.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p dbc-ui --bin dbc-ui -- a_lone_empty_editor the_legacy_fields_mirror`
Expected: compile error — `EditorSnapshot`/`session_editors` unknown.

- [ ] **Step 3: Add `EditorTab`, the accessors and the pure session core**

```rust
// main.rs, next to `struct ScriptBinding`
/// One editor tab's state — everything that used to sit on `AppView` and
/// assume there was exactly one editor (spec §1). Fields keep their old
/// names so the ~260 `self.x` sites became `self.editor().x` mechanically.
pub(crate) struct EditorTab { /* fields per Interfaces above */ }

impl EditorTab {
    pub fn new(sql: Entity<SqlInput>, connection: Option<String>, database: Option<String>) -> Self {
        Self {
            sql, connection, database,
            unverified: false,
            results: Tabs::new(),
            script_binding: None, script_dirty_flag: false, script_binding_generation: 0,
            editor_discard_grant: None, script_save_in_flight: false,
            autocomplete: None, last_ac_text: String::new(), last_ac_cursor: 0,
            cancel: None, run_generation: 0, started_at: None,
            status: "ready".into(),
            attempted_restore: None,
        }
    }
}

/// GPUI-free view of an `EditorTab` for the session writer.
pub(crate) struct EditorSnapshot {
    pub sql: String, pub cursor: usize,
    pub connection: Option<String>, pub database: Option<String>,
    pub script_path: Option<PathBuf>,
    pub tabs: Vec<dbc_state::session::SessionTab>,
}

pub(crate) struct LegacyFields {
    pub connection: Option<String>, pub database: Option<String>,
    pub editor: String, pub cursor: usize,
    pub tabs: Vec<dbc_state::session::SessionTab>,
}

/// Pure core of `capture_session` (spec §3). The legacy fields always
/// describe the ACTIVE tab; the `editors` block is written only when there
/// is something the legacy fields cannot carry (a second tab, or a bound
/// script), so an untouched app still produces an EMPTY state.
pub(crate) fn session_editors(editors: &[EditorSnapshot], active: usize) -> (Vec<dbc_state::session::SessionEditor>, LegacyFields) {
    let a = &editors[active.min(editors.len() - 1)];
    let legacy = LegacyFields {
        connection: a.connection.clone(), database: a.database.clone(),
        editor: a.sql.clone(), cursor: a.cursor, tabs: a.tabs.clone(),
    };
    let needs_block = editors.len() > 1 || editors.iter().any(|e| e.script_path.is_some());
    let list = if needs_block {
        editors.iter().map(|e| dbc_state::session::SessionEditor {
            sql: e.sql.clone(), cursor: e.cursor,
            connection: e.connection.clone(), database: e.database.clone(),
            script_path: e.script_path.clone(), tabs: e.tabs.clone(),
        }).collect()
    } else {
        Vec::new()
    };
    (list, legacy)
}

// on AppView
    fn editor(&self) -> &EditorTab { &self.editors.active().payload }
    fn editor_mut(&mut self) -> &mut EditorTab { &mut self.editors.active_mut().payload }
    fn editor_id(&self) -> u64 { self.editors.active_id() }
    fn editor_by_id_mut(&mut self, id: u64) -> Option<&mut EditorTab> {
        self.editors.by_id_mut(id).map(|s| &mut s.payload)
    }
```

- [ ] **Step 4: Move the fields and let the compiler drive**

1. Delete from `struct AppView` the fields listed in Interfaces; add `editors: editor_tabs::EditorTabs<EditorTab>`.
2. In the constructor (`main.rs:14727-14870`): build the editors from `session.editors_or_legacy()`:
   ```rust
   let (session_editors, active_ix) = session.editors_or_legacy();
   let mut editors: Option<editor_tabs::EditorTabs<EditorTab>> = None;
   for e in &session_editors {
       let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
       if !e.sql.is_empty() {
           let (text, at) = (e.sql.clone(), e.cursor);
           sql.update(cx, |s, cx| { s.insert_text(&text, cx); s.set_cursor_offset(at, cx); });
       }
       let mut tab = EditorTab::new(sql, e.connection.clone(), e.database.clone());
       tab.unverified = e.connection.is_some();
       tab.results = restored_tabs_from(&e.tabs);
       // script_path: re-bound lazily in Task 5 (needs the file's text) — store the intent:
       tab.pending_script_path = e.script_path.clone();   // add this Option<PathBuf> field to EditorTab
       match &mut editors {
           None => editors = Some(editor_tabs::EditorTabs::new(tab)),
           Some(ed) => { ed.open(tab); }
       }
   }
   let mut editors = editors.expect("editors_or_legacy never returns zero editors");
   let active_id = editors.iter().nth(active_ix).map(|s| s.id).unwrap_or(1);
   editors.activate(active_id);
   window.focus(&editors.active().payload.sql.focus_handle(cx), cx);
   ```
   `restored_tabs_from(&[SessionTab]) -> Tabs` is `restored_tabs` with its loop body unchanged and the parameter narrowed; keep `restored_tabs(&SessionState)` as a one-line wrapper over `session.tabs` for the existing tests.
3. `capture_session`: build `Vec<EditorSnapshot>` from `self.editors.iter()` (`sql.read(cx).text()`, `.cursor()`, `script_binding.as_ref().map(|b| b.path.clone())`, `session_tabs(&tab.results)`), call `session_editors`, fill both the legacy fields and `editors`/`active_editor`. The `attempted_restore` fallback for `connection` becomes per tab: `tab.connection.clone().or_else(|| tab.attempted_restore.as_ref().map(|(c, _)| c.clone()))`.
4. `restore_session_context`: unchanged signature; it now writes `self.editor_mut().attempted_restore` and dispatches the switch for the ACTIVE tab only. `restore_conn` at `main.rs:14597` becomes `session_editors[active_ix].connection.clone().map(|id| (id, session_editors[active_ix].database.clone()))`.
5. `cargo build -p dbc-ui 2>&1 | grep -c "^error"` — expect ~260. Fix every `no field` error by inserting `editor()`/`editor_mut()`; `self.tabs` → `self.editor_mut().results` (or `editor().results`). Where a closure inside `cx.spawn` writes `view.status`/`view.tabs`/`view.cancel`, write `view.editor_mut()` FOR NOW — Task 4 replaces those with `editor_by_id_mut`.
6. Sites outside `main.rs` (`connections_ui.rs` `impl AppView` blocks, `backup.rs`, …) get the same treatment; `history_panel.rs`/`schema_tree.rs`/`tabs.rs`/`prefetch.rs`/`tree_menu.rs` only mention the names in comments or take them as parameters — leave those.
7. Borrow-checker knots: a `let tab = self.editor_mut();` followed by a `self.other_method()` call will not compile. Resolve by reading what you need into locals first (`let sql = self.editor().sql.clone();`), never by cloning `Tabs`.

- [ ] **Step 5: Build and run the whole suite**

Run: `cargo build -p dbc-ui && cargo test -p dbc-ui --bin dbc-ui`
Expected: build clean (no `dead_code` on the accessors), all tests green including the two new ones and the existing `session_tabs`/`restored_tabs` tests.

- [ ] **Step 6: Live smoke**

Close the app (WM_CLOSE), `cargo build -p dbc-ui`, launch. Expected: the session restores exactly as before this task — same editor text, result tabs, active connection. Take a window screenshot (PrintWindow, never the screen) and look at it.

- [ ] **Step 7: Checkpoint**

Suite green + `git diff --stat`. Commit only if asked: `git commit -am "refactor: editor state lives in EditorTab; one tab, no behaviour change"`.

---

### Task 4: Async rail — completions land by editor id

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` — `run_query_with` (2917-3450), the script-save spawn in `on_save_script`/`save_script_to` (~8573+), `switch_to_database` success arm (7040-7095), `on_cancel_query` (5433).
- Test: `main.rs` audit tests module (near 19181).

**Interfaces:**
- Consumes: `editor_by_id_mut(id)`, `editor_id()` from Task 3.
- Produces: nothing new; the rule „a spawn that writes editor state resolves `editor_by_id_mut`" pinned by an audit.

- [ ] **Step 1: Write the failing audit test**

```rust
    /// Spec §2: every async completion that writes into an editor finds
    /// its tab BY ID, never through `editor_mut()` (which is whatever tab
    /// is active when the future lands). Source scan over the three
    /// functions that spawn such work.
    #[test]
    fn async_completions_resolve_their_editor_by_id() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).expect("own source");
        for f in ["fn run_query_with(", "fn save_script_to(", "fn switch_to_database("] {
            let start = src.find(f).unwrap_or_else(|| panic!("{f} must exist"));
            let body = &src[start..];
            let end = body.find("\n    }\n").map(|e| e + 7).unwrap_or(body.len());
            let body = &body[..end];
            let spawn_at = body.find("cx.spawn(").unwrap_or_else(|| panic!("{f} spawns"));
            let after = &body[spawn_at..];
            assert!(!after.contains("view.editor_mut()"), "{f}: a completion writes into the ACTIVE tab instead of its own");
            assert!(after.contains("view.editor_by_id_mut("), "{f}: completion does not resolve its tab by id");
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p dbc-ui --bin dbc-ui -- async_completions_resolve_their_editor_by_id`
Expected: FAIL on `run_query_with` (`view.editor_mut()` present after Task 3).

- [ ] **Step 3: Thread the id through the three spawns**

`run_query_with`: at the top, `let editor_id = self.editor_id();`. Inside the spawn closure, replace every `view.editor_mut()` with:

```rust
                let Some(tab) = view.editor_by_id_mut(editor_id) else {
                    return; // the tab was closed while the query ran — spec §2, drop
                };
```

once per `this.update` block, then use `tab.results`, `tab.status`, `tab.cancel`, `tab.run_generation`, `tab.started_at`. Where the block also calls a method on `view` (`record_history`, `apply_view_prefs_to_grid`, `fk_info_for_table`…), take what the method needs out of `tab` first, drop `tab`, call the method, then re-resolve `view.editor_by_id_mut(editor_id)` if more writes follow — the borrow checker will flag each one. The `run_generation` guard stays; it now lives on the tab.

`save_script_to` (the fn `on_save_script` dispatches into; if the spawn is inline in `on_save_script`, extract it into `fn save_script_to(&mut self, path: PathBuf, text: String, cx)` first so the audit has a name to find): capture `editor_id`, resolve by id, write `script_save_in_flight`, `script_binding.saved_text`, `script_dirty_flag`, `status`.

`switch_to_database`: capture `let editor_id = self.editor_id();` before the spawn. Success arm:

```rust
                    Ok(Ok(())) => {
                        let Some(tab) = view.editor_by_id_mut(editor_id) else { return };
                        tab.status = format!("Připojeno ({engine_lbl})");
                        tab.connection = Some(target_id.clone());
                        tab.database = db.clone();
                        tab.unverified = false;
                        view.conn_url = None;
                        // The tree, ●, dropdown and autocomplete follow the ACTIVE tab
                        // (spec §2) — a switch requested by a background tab must not
                        // repaint them for a context the user is not looking at.
                        if view.editor_id() == editor_id {
                            view.close_autocomplete(cx);
                            view.refresh_tree_context(cx);
                        }
                        view.start_schema_slot_fetch(target_id.clone(), effective.clone(), cx);
                        /* follow_up match, expand_connection, load_missing_db_lists — unchanged */
                    }
```

The failure arms write `status` the same way (`editor_by_id_mut`, drop when `None`).

`on_cancel_query`: cancels `self.editor_mut().cancel` — the ACTIVE tab's, on purpose (Escape acts on what you see).

- [ ] **Step 4: Run the audit and the suite**

Run: `cargo test -p dbc-ui --bin dbc-ui`
Expected: all green.

- [ ] **Step 5: Checkpoint**

Commit only if asked: `git commit -am "feat: query, save and switch completions land in the tab that started them"`.

---

### Task 5: Editor tab strip, actions, shortcuts, activation, close guard

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` — `actions!` block (118-146), `cx.bind_keys` (14661), `Render for AppView` (column at 14004; strip goes BEFORE the script caption row), `on_action` registrations (14180-14190), `PendingDiscard` (1954), new handlers.
- Modify: `crates/dbc-ui/src/keymap.rs:94-135` (`SHORTCUTS`).
- Test: `main.rs` (pure helpers) + `keymap.rs` existing audit picks the chords up automatically.

**Interfaces:**
- Consumes: `EditorTabs` API (Task 1), `derive_title` (Task 1), `EditorTab::new` (Task 3).
- Produces:
  ```rust
  actions: NewEditorTab, CloseEditorTab, NextEditorTab, PrevEditorTab
  fn new_editor_tab(&mut self, window: &mut Window, cx: &mut Context<Self>)
  fn close_editor_tab(&mut self, id: u64, cx: &mut Context<Self>)      // guarded
  fn activate_editor(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>)
  fn editor_tab_title(&self, slot: &Slot<EditorTab>, cx: &App) -> String
  pub(crate) fn close_editor_decision(is_last: bool, dirty_script: bool, running: bool) -> CloseEditorDecision
  enum CloseEditorDecision { Close, ConfirmDiscard, CancelThenClose, ReplaceWithEmpty }
  PendingDiscard::CloseEditorTab { id: u64 }
  ```

- [ ] **Step 1: Write the failing tests**

```rust
    /// Spec §4, Ctrl+W: what closing a tab has to do first. Pure, so every
    /// branch is pinned without a window.
    #[test]
    fn close_editor_decision_covers_every_branch() {
        use CloseEditorDecision::*;
        assert!(matches!(close_editor_decision(false, false, false), Close));
        assert!(matches!(close_editor_decision(false, true, false), ConfirmDiscard));
        assert!(matches!(close_editor_decision(false, false, true), CancelThenClose));
        assert!(matches!(close_editor_decision(false, true, true), ConfirmDiscard), "dirty wins: ask before touching anything");
        assert!(matches!(close_editor_decision(true, false, false), ReplaceWithEmpty));
        assert!(matches!(close_editor_decision(true, true, false), ConfirmDiscard));
    }

    /// The four chords exist in all three registries (the keymap audit
    /// checks bind_keys vs SHORTCUTS; this pins the actions block too).
    #[test]
    fn editor_tab_actions_are_bound_and_documented() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).expect("own source");
        for (chord, action) in [("ctrl-n", "NewEditorTab"), ("ctrl-w", "CloseEditorTab"), ("ctrl-tab", "NextEditorTab"), ("ctrl-shift-tab", "PrevEditorTab")] {
            assert!(src.contains(&format!("KeyBinding::new(\"{chord}\", {action}, None)")), "{chord} not bound");
            assert!(src.contains(&format!("Self::on_{}", snake(action))), "{action} handler not registered");
        }
        fn snake(s: &str) -> String {
            let mut out = String::new();
            for (i, c) in s.chars().enumerate() {
                if c.is_uppercase() { if i > 0 { out.push('_'); } out.push(c.to_ascii_lowercase()); } else { out.push(c); }
            }
            out
        }
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p dbc-ui --bin dbc-ui -- close_editor_decision editor_tab_actions_are_bound`
Expected: compile error / FAIL.

- [ ] **Step 3: Implement**

Actions and chords:

```rust
// actions! block
        /// Editor tabs (spec §4). Ctrl+N/W/Tab were all unbound before.
        NewEditorTab,
        CloseEditorTab,
        NextEditorTab,
        PrevEditorTab,
// bind_keys
            KeyBinding::new("ctrl-n", NewEditorTab, None),
            KeyBinding::new("ctrl-w", CloseEditorTab, None),
            KeyBinding::new("ctrl-tab", NextEditorTab, None),
            KeyBinding::new("ctrl-shift-tab", PrevEditorTab, None),
// on_action registrations, next to on_focus_results
            .on_action(cx.listener(Self::on_new_editor_tab))
            .on_action(cx.listener(Self::on_close_editor_tab))
            .on_action(cx.listener(Self::on_next_editor_tab))
            .on_action(cx.listener(Self::on_prev_editor_tab))
// keymap::SHORTCUTS, Global section
    s("ctrl-n", "Nový tab s dotazem", Scope::Global, true),
    s("ctrl-w", "Zavřít tab s dotazem", Scope::Global, false),
    s("ctrl-tab", "Další tab", Scope::Global, false),
    s("ctrl-shift-tab", "Předchozí tab", Scope::Global, false),
```

Handlers and helpers:

```rust
    pub(crate) enum CloseEditorDecision { Close, ConfirmDiscard, CancelThenClose, ReplaceWithEmpty }

    /// Spec §4. Dirty first — nothing is cancelled or replaced behind a
    /// question the user has not answered yet.
    pub(crate) fn close_editor_decision(is_last: bool, dirty_script: bool, running: bool) -> CloseEditorDecision {
        if dirty_script { return CloseEditorDecision::ConfirmDiscard; }
        if is_last { return CloseEditorDecision::ReplaceWithEmpty; }
        if running { return CloseEditorDecision::CancelThenClose; }
        CloseEditorDecision::Close
    }

    fn on_new_editor_tab(&mut self, _: &NewEditorTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() { return; }
        self.new_editor_tab(window, cx);
    }

    /// Ctrl+N: a fresh buffer that inherits the current tab's context
    /// (spec §4) — the common case is „same server, another query".
    fn new_editor_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editors.len() >= editor_tabs::MAX_EDITOR_TABS {
            self.editor_mut().status = format!("error: maximum je {} tabů", editor_tabs::MAX_EDITOR_TABS);
            cx.notify();
            return;
        }
        let (conn, db) = (self.editor().connection.clone(), self.editor().database.clone());
        let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
        let id = self.editors.open(EditorTab::new(sql, conn, db));
        self.activate_editor(id, window, cx);
    }

    fn on_close_editor_tab(&mut self, _: &CloseEditorTab, _w: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() { return; }
        let id = self.editor_id();
        self.close_editor_tab(id, cx);
    }

    fn close_editor_tab(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(slot) = self.editors.by_id(id) else { return };
        let dirty = slot.payload.script_binding.as_ref()
            .is_some_and(|b| script_text_is_dirty(&slot.payload.sql.read(cx).text(), &b.saved_text));
        let running = slot.payload.cancel.is_some();
        match close_editor_decision(self.editors.len() == 1, dirty, running) {
            CloseEditorDecision::ConfirmDiscard => {
                self.discard_confirm = Some(DiscardConfirmState { change_count: 1, action: PendingDiscard::CloseEditorTab { id } });
                self.modal_needs_focus = true;
            }
            CloseEditorDecision::CancelThenClose => {
                if let Some(tab) = self.editor_by_id_mut(id) { if let Some(c) = tab.cancel.take() { c.cancel(); } }
                self.editors.close(id);
                self.refresh_tree_context(cx);
            }
            CloseEditorDecision::Close => {
                self.editors.close(id);
                self.refresh_tree_context(cx);
            }
            CloseEditorDecision::ReplaceWithEmpty => {
                // Spec §1: never zero tabs. Same context, empty buffer.
                let (conn, db) = (self.editor().connection.clone(), self.editor().database.clone());
                let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
                let fresh = self.editors.open(EditorTab::new(sql, conn, db));
                self.editors.close(id);
                self.editors.activate(fresh);
                self.refresh_tree_context(cx);
            }
        }
        cx.notify();
    }
```

`PendingDiscard::CloseEditorTab { id }`'s Yes-arm (in the same `match` as `CloseTab`): clear the tab's binding (`tab.script_binding = None; tab.script_dirty_flag = false;`) then call `close_editor_tab(id, cx)` again — now not dirty, it proceeds.

```rust
    fn on_next_editor_tab(&mut self, _: &NextEditorTab, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.editors.next();
        self.activate_editor(id, window, cx);
    }
    fn on_prev_editor_tab(&mut self, _: &PrevEditorTab, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.editors.prev();
        self.activate_editor(id, window, cx);
    }

    /// Spec §4: the tree, ●, dropdown and status all follow the active tab.
    /// A tab restored with a context it never connected with (spec §3)
    /// connects now.
    fn activate_editor(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editors.activate(id) { return; }
        self.close_autocomplete(cx);
        let focus = self.editor().sql.focus_handle(cx);
        window.focus(&focus, cx);
        self.refresh_tree_context(cx);
        if self.editor().unverified {
            if let Some(conn) = self.editor().connection.clone() {
                let db = self.editor().database.clone();
                self.switch_to_database(&conn, db, None, cx);
            }
        }
        cx.notify();
    }

    fn editor_tab_title(&self, slot: &editor_tabs::Slot<EditorTab>, cx: &App) -> String {
        let bound = slot.payload.script_binding.as_ref().and_then(|b| b.path.file_name()).map(|n| n.to_string_lossy().into_owned());
        editor_tabs::derive_title(bound.as_deref(), &slot.payload.sql.read(cx).text(), slot.ordinal)
    }
```

Render (in `Render for AppView`, immediately after `let mut column = …` at 14004, before the script caption `if let Some(rel)`):

```rust
        {
            let active_id = self.editors.active_id();
            let rows: Vec<(u64, String, bool)> = self.editors.iter().map(|s| {
                let dirty = s.payload.script_binding.as_ref()
                    .is_some_and(|b| script_text_is_dirty(&s.payload.sql.read(cx).text(), &b.saved_text));
                (s.id, self.editor_tab_title(s, cx), dirty)
            }).collect();
            let mut strip = div().id("editor-tab-strip").flex().flex_row().h(px(28.)).bg(theme.bg_app);
            for (id, title, dirty) in rows {
                let is_active = id == active_id;
                strip = strip.child(
                    div()
                        .id(("editor-tab", id as usize))
                        .flex().flex_row().items_center().gap_1().px_2().h_full()
                        .bg(if is_active { theme.bg_hover } else { theme.bg_app })
                        .text_color(if is_active { theme.text_primary } else { theme.text_muted })
                        .cursor_pointer()
                        .on_click(cx.listener(move |view, _, window, cx| view.activate_editor(id, window, cx)))
                        .child(if dirty { format!("{title} •") } else { title })
                        .child(
                            div().id(("editor-tab-close", id as usize)).px_1().cursor_pointer().child("✕")
                                .on_click(cx.listener(move |view, _, _, cx| { cx.stop_propagation(); view.close_editor_tab(id, cx); })),
                        ),
                );
            }
            strip = strip.child(
                div().id("editor-tab-new").px_2().h_full().flex().items_center().cursor_pointer()
                    .text_color(theme.text_muted).child("+")
                    .on_click(cx.listener(|view, _, window, cx| view.new_editor_tab(window, cx))),
            );
            column = column.child(strip);
        }
```

The editor `div` at `14066` renders `self.editor().sql.clone()`; the results strip/content already read through `editor()` after Task 3. The status bar (`14332`) reads `self.editor().status`.

Restore of a bound script (`pending_script_path` from Task 3): in `activate_editor` and once for the initial active tab at startup, if `pending_script_path.take()` is `Some(path)` and the file reads, call `bind_script(path, text, cx)` — this is one of the three sanctioned `replace_buffer` sites, and the buffer already holds the session text, so bind with the FILE text as `saved_text` and keep the buffer (dirty state then reads honestly).

- [ ] **Step 4: Build, run the tests**

Run: `cargo build -p dbc-ui && cargo test -p dbc-ui --bin dbc-ui`
Expected: green, including `keymap`'s existing bind-vs-SHORTCUTS audit.

- [ ] **Step 5: Live check**

Close, build, launch. Screenshot: the strip shows „Dotaz 1" (or the first line). Ask the user to: Ctrl+N twice, type in tab 3, Ctrl+Tab around, Ctrl+W on a tab, run a query in tab 2 and switch away while it runs. Report exactly what you could verify (rendering) and what they must (keyboard).

- [ ] **Step 6: Checkpoint**

Commit only if asked: `git commit -am "feat: editor tabs — Ctrl+N/W/Tab, per-tab context and results"`.

---

### Task 6: Database picker (Ctrl+D)

**Files:**
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteItem::Database`, `DatabaseSource`, `rank_databases`, `display_label`)
- Modify: `crates/dbc-ui/src/main.rs` (`PaletteState.mode`, `PickDatabase` action, `on_pick_database`, `build_palette_items` branch, `execute_palette_item` arm, `render_palette_overlay` placeholder, `actions!`, `bind_keys`, `on_action`)
- Modify: `crates/dbc-ui/src/keymap.rs` (`SHORTCUTS`)
- Test: `palette.rs` tests

**Interfaces:**
- Consumes: `switch_to_database(&str, Option<String>, Option<PendingTreeAction>, cx)`, `dbc_state::conn_cache::databases(&str) -> Option<Vec<String>>`, `self.config.connections` (`ConnectionConfig { id, name, folder: Vec<String>, database, .. }`), `fuzzy_score(query, target) -> Option<i64>`.
- Produces:
  ```rust
  pub enum PaletteMode { Commands, Databases }
  pub struct DatabaseSource { pub conn_id: String, pub conn_name: String, pub folder: Vec<String>, pub db: String, pub is_current: bool }
  pub fn database_sources(connections: &[ConnectionConfig], cached: impl Fn(&str) -> Option<Vec<String>>, current: Option<(&str, &str)>) -> Vec<DatabaseSource>;
  pub fn rank_databases(query: &str, sources: &[DatabaseSource], cap: usize) -> Vec<PaletteItem>;
  PaletteItem::Database { conn_id: String, db: String, label: String }
  action PickDatabase, chord "ctrl-d"
  ```

- [ ] **Step 1: Write the failing tests** (in `palette.rs`'s `mod tests`)

```rust
    fn conn(id: &str, name: &str, folder: &[&str], default_db: &str) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(), name: name.into(), folder: folder.iter().map(|s| s.to_string()).collect(),
            engine: Engine::Postgres, host: "h".into(), port: None, database: default_db.into(),
            user: "u".into(), read_only: false, timeout_secs: None, auto_limit: None, ssh: None,
            favourite: false, mssql: None,
        }
    }

    /// Spec §5: every cached database of every connection; a connection
    /// with nothing cached contributes its default database, so nothing
    /// saved is ever unreachable from the picker.
    #[test]
    fn database_sources_use_the_cache_and_fall_back_to_the_default() {
        let conns = vec![conn("c1", "prod", &["dw"], "sales"), conn("c2", "dev", &[], "app")];
        let cached = |id: &str| (id == "c1").then(|| vec!["sales".to_string(), "hr".to_string()]);
        let out = database_sources(&conns, cached, Some(("c1", "hr")));
        let names: Vec<(String, String)> = out.iter().map(|s| (s.conn_id.clone(), s.db.clone())).collect();
        assert_eq!(names, vec![("c1".into(), "sales".into()), ("c1".into(), "hr".into()), ("c2".into(), "app".into())]);
        assert!(out[1].is_current && !out[0].is_current && !out[2].is_current);
    }

    #[test]
    fn an_empty_query_lists_the_current_context_first() {
        let conns = vec![conn("c1", "prod", &[], "sales"), conn("c2", "dev", &[], "app")];
        let src = database_sources(&conns, |_| None, Some(("c2", "app")));
        let items = rank_databases("", &src, 50);
        assert!(matches!(&items[0], PaletteItem::Database { conn_id, db, .. } if conn_id == "c2" && db == "app"));
    }

    #[test]
    fn the_query_matches_connection_and_database_together() {
        let conns = vec![conn("c1", "prod", &["dw"], "sales"), conn("c2", "dev", &[], "sales")];
        let src = database_sources(&conns, |_| None, None);
        let items = rank_databases("dev sal", &src, 50);
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0], PaletteItem::Database { conn_id, .. } if conn_id == "c2"));
    }

    #[test]
    fn database_labels_read_connection_dot_database_with_the_folder_muted() {
        let conns = vec![conn("c1", "prod", &["dw", "eu"], "sales")];
        let src = database_sources(&conns, |_| None, Some(("c1", "sales")));
        let items = rank_databases("", &src, 50);
        assert_eq!(display_label(&items[0]), "● prod · sales  (dw/eu)");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p dbc-ui --bin dbc-ui -- palette::tests::database`
Expected: compile errors.

- [ ] **Step 3: Implement in `palette.rs`**

```rust
use dbc_state::ConnectionConfig;

// PaletteItem gains:
    /// → `switch_to_database(conn_id, Some(db))` for the ACTIVE editor
    /// (spec §5). `label` is prebuilt: the picker's rows carry the folder
    /// and the ● marker, which `display_label` has no source for.
    Database { conn_id: String, db: String, label: String },

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseSource {
    pub conn_id: String,
    pub conn_name: String,
    pub folder: Vec<String>,
    pub db: String,
    pub is_current: bool,
}

/// Spec §5. `cached` is `dbc_state::conn_cache::databases` in the app and
/// a closure in tests; the cache is on disk, so servers the user has
/// expanded before list fully even while collapsed. Order: config order,
/// databases in cache order.
pub fn database_sources(
    connections: &[ConnectionConfig],
    cached: impl Fn(&str) -> Option<Vec<String>>,
    current: Option<(&str, &str)>,
) -> Vec<DatabaseSource> {
    let mut out = Vec::new();
    for c in connections {
        let dbs = cached(&c.id).unwrap_or_else(|| vec![c.database.clone()]);
        for db in dbs {
            out.push(DatabaseSource {
                conn_id: c.id.clone(),
                conn_name: c.name.clone(),
                folder: c.folder.clone(),
                is_current: current == Some((c.id.as_str(), db.as_str())),
                db,
            });
        }
    }
    out
}

fn database_label(s: &DatabaseSource) -> String {
    let mark = if s.is_current { "● " } else { "" };
    if s.folder.is_empty() {
        format!("{mark}{} · {}", s.conn_name, s.db)
    } else {
        format!("{mark}{} · {}  ({})", s.conn_name, s.db, s.folder.join("/"))
    }
}

/// Empty query: everything, current context first. Otherwise fuzzy over
/// `"<connection> <database>"` so „dev sal" finds dev/sales.
pub fn rank_databases(query: &str, sources: &[DatabaseSource], cap: usize) -> Vec<PaletteItem> {
    let mut scored: Vec<(i64, usize, &DatabaseSource)> = sources
        .iter()
        .enumerate()
        .filter_map(|(ix, s)| {
            let score = if query.trim().is_empty() {
                0
            } else {
                fuzzy_score(query, &format!("{} {}", s.conn_name, s.db))?
            };
            Some((score + if s.is_current { FAVOURITE_BONUS } else { 0 }, ix, s))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(cap)
        .map(|(_, _, s)| PaletteItem::Database { conn_id: s.conn_id.clone(), db: s.db.clone(), label: database_label(s) })
        .collect()
}

// display_label gains the arm:
        PaletteItem::Database { label, .. } => label.clone(),
```

`fixed_actions` gets one more `PaletteAction::PickDatabase` with label „Vybrat databázi… (Ctrl+D)" so the Commands mode advertises it; the `PaletteAction` enum gains `PickDatabase`.

In `main.rs`:

```rust
// PaletteState
    mode: palette::PaletteMode,
// actions!  + bind_keys + on_action
        /// Ctrl+D — pick `connection / database` for the active tab (spec §5).
        PickDatabase,
            KeyBinding::new("ctrl-d", PickDatabase, None),
            .on_action(cx.listener(Self::on_pick_database))
// keymap SHORTCUTS (Global)
    s("ctrl-d", "Vybrat databázi", Scope::Global, true),

    fn on_pick_database(&mut self, _: &PickDatabase, window: &mut Window, cx: &mut Context<Self>) {
        // Toggle: Ctrl+D on an open picker closes it.
        if self.palette.as_ref().is_some_and(|p| p.mode == palette::PaletteMode::Databases) {
            self.palette = None;
            cx.notify();
            return;
        }
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        self.dropdown_open = false;
        let input = cx.new(|cx| connections_ui::TextField::new(cx, "Ctrl+D – připojení / databáze…", false));
        let focus = input.focus_handle(cx);
        self.palette = Some(PaletteState { input, items: Vec::new(), selected: 0, last_query: String::new(), mode: palette::PaletteMode::Databases });
        let items = self.build_palette_items("", cx);
        if let Some(p) = &mut self.palette { p.items = items; }
        window.focus(&focus, cx);
        cx.notify();
    }
```

`build_palette_items`: first line —

```rust
        if self.palette.as_ref().is_some_and(|p| p.mode == palette::PaletteMode::Databases) {
            let current = self.editor().connection.as_deref().map(|c| (c, self.effective_database()));
            let current = current.as_ref().and_then(|(c, d)| d.as_deref().map(|d| (*c, d)));
            let sources = palette::database_sources(&self.config.connections, dbc_state::conn_cache::databases, current);
            return palette::rank_databases(query, &sources, 50);
        }
```

`on_open_palette` sets `mode: palette::PaletteMode::Commands`. `execute_palette_item`:

```rust
            PaletteItem::Database { conn_id, db, .. } => {
                self.switch_to_database(&conn_id, Some(db), None, cx);
            }
            PaletteItem::Action { action: PaletteAction::PickDatabase, .. } => {
                self.on_pick_database(&PickDatabase, window, cx);
            }
```

(`PickDatabase` must be handled BEFORE the generic `Action` arm's match, or inside it — either way the `palette = None` at the top of `execute_palette_item` runs first, so the toggle in `on_pick_database` sees no open picker and opens it.)

- [ ] **Step 4: Run the tests and build**

Run: `cargo build -p dbc-ui && cargo test -p dbc-ui --bin dbc-ui`
Expected: green; the keymap audit sees `ctrl-d` in both registries.

- [ ] **Step 5: Live check**

Close, build, launch, screenshot. The user presses Ctrl+D: the list should show every connection's databases with the current one marked ●; Enter switches the active tab (the tree's ● moves, the dropdown caption changes).

- [ ] **Step 6: Checkpoint**

Commit only if asked: `git commit -am "feat: Ctrl+D picks a database across every connection"`.

---

### Task 7: Session round-trip, live verification, memory note

**Files:**
- Modify: `C:\Users\tomas\.claude\projects\D--workspace-home-db\memory\db-client-project.md` (one line: editor tabs shipped, per-tab context; Ctrl+N/W/Tab/D)

- [ ] **Step 1: Round-trip**

With two tabs open on different connections and one bound script: close the window (WM_CLOSE), reopen. Expected: both tabs return with their text, contexts (the active one connected, the other marked unverified until activated), the bound script's caption, and each tab's result tabs. Verify by screenshot + by reading the session TOML (`%APPDATA%`-free: `data/sessions/*.toml`) — it must contain `[[editors]]` twice and the legacy `editor = "..."` mirroring the active tab.

- [ ] **Step 2: Whole-workspace suite**

Run: `cargo test --workspace`
Expected: green.

- [ ] **Step 3: Update the project memory** — one line pointer in `db-client-project.md`.

- [ ] **Step 4: Checkpoint** — final `git diff --stat`; commit only if asked.
