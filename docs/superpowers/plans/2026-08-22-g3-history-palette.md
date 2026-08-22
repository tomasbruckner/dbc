# G3 History & Palette Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persistent query history (SQLite, right panel, fulltext, ★ pins, click-to-load), Ctrl+K command palette (tables / history / connections / app actions), and generalized favourites (★ on tree objects, Favourites tree section, connection stars).

**Architecture:** dbc-state gains `history.rs` (rusqlite + FTS5, file in the user profile) and a `FavouriteObject` list persisted in config.toml. dbc-ui gains `history_panel.rs` (right panel, Ctrl+H) and `palette.rs` (overlay + pure fuzzy matcher). Query recording hooks into the existing run pipeline's Finished/Failed arms. Tree gets a ★ affordance + Oblíbené section fed from config.

**Tech Stack:** Rust, GPUI (pinned 907ed09), rusqlite (bundled, FTS5), toml.

**Spec:** docs/superpowers/specs/2026-08-22-gui-target-design.md §1 (History lines ~64-68, Palette ~69-70, Favourites ~80-85), §2 row G3.

## Global Constraints

- dbc-core never sees GPUI; dbc-ui never imports driver crates outside connect.rs; persistence lives in dbc-state.
- Errors are values — history DB failures must never crash or block queries (recording is fire-and-forget; a broken history file degrades to "history unavailable" status, queries still run).
- History stores SQL text, timestamp, connection name, row count, duration — NEVER result data, NEVER passwords/URLs with credentials (connection NAME only).
- Build/test only with explicit `-p`; cargo at `%USERPROFILE%\.cargo\bin\cargo.exe`.
- Czech UI labels (Historie, Hledat…, Oblíbené, Paleta příkazů); existing status-string conventions unchanged.
- Existing test suites stay green: dbc-ui 52, dbc-state 9, dbc-core 13, dbc-buffer 5, dbc-driver-sqlite 8.
- Version bump to 0.3.0 happens at merge time (spec §Versioning), not in a task.

---

### Task 1: History store (dbc-state)

**Files:**
- Create: `crates/dbc-state/src/history.rs`
- Modify: `crates/dbc-state/Cargo.toml` (add `rusqlite = { workspace = true }` — already a workspace dep via the sqlite driver; verify features include `bundled`), `crates/dbc-state/src/lib.rs` (exports)

**Interfaces:**
- Produces:

```rust
pub struct HistoryDb { /* conn: rusqlite::Connection */ }

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub sql: String,
    pub connection: String,      // connection NAME, never a URL/credentials
    pub started_at: i64,         // unix seconds
    pub duration_ms: Option<i64>,
    pub row_count: Option<i64>,
    pub error: Option<String>,   // failed runs recorded with the error text
    pub starred: bool,
}

impl HistoryDb {
    /// Opens/creates the DB and migrates the schema. Never panics.
    pub fn open(path: &Path) -> Result<HistoryDb, StateError>;
    /// Returns the new entry id.
    pub fn add(&mut self, sql: &str, connection: &str, started_at: i64,
               duration_ms: Option<i64>, row_count: Option<i64>,
               error: Option<&str>) -> Result<i64, StateError>;
    /// query empty → recent entries; otherwise FTS/LIKE fulltext over sql.
    /// Starred entries first (both modes), then newest first. Max `limit`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryEntry>, StateError>;
    pub fn set_starred(&mut self, id: i64, starred: bool) -> Result<(), StateError>;
    /// Dedup rule: consecutive identical (sql, connection) within 5 s
    /// collapse into the newest entry (update timestamp/stats) — protects
    /// against re-run spam. Implemented inside add().
}

pub fn default_history_path() -> PathBuf;   // dirs::config_dir()/dbc/history.sqlite
```

- Schema: `entries(id INTEGER PRIMARY KEY, sql TEXT NOT NULL, connection TEXT NOT NULL, started_at INTEGER NOT NULL, duration_ms INTEGER, row_count INTEGER, error TEXT, starred INTEGER NOT NULL DEFAULT 0)`. Fulltext: FTS5 external-content table `entries_fts(sql)` synced by triggers IF the bundled SQLite has FTS5; else compile-time fallback to `LIKE '%q%'` (escape `%_`). Detect at open() by trying to create the FTS table; store the mode; search() branches.

- [ ] **Step 1: Write failing tests** (bottom of history.rs):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> (tempfile::TempDir, HistoryDb) {
        let d = tempfile::tempdir().unwrap();
        let h = HistoryDb::open(&d.path().join("h.sqlite")).unwrap();
        (d, h)
    }

    #[test]
    fn add_and_recent() {
        let (_d, mut h) = db();
        h.add("select 1", "demo", 1000, Some(5), Some(1), None).unwrap();
        h.add("select 2", "demo", 2000, Some(6), Some(1), None).unwrap();
        let r = h.search("", 10).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].sql, "select 2"); // newest first
    }

    #[test]
    fn fulltext_finds_and_misses() {
        let (_d, mut h) = db();
        h.add("select * from orders where id = 1", "demo", 1000, None, None, None).unwrap();
        h.add("update inventory set qty = 0", "demo", 2000, None, None, None).unwrap();
        let r = h.search("orders", 10).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].sql.contains("orders"));
        assert!(h.search("nonexistent_zzz", 10).unwrap().is_empty());
    }

    #[test]
    fn starred_first_and_persists() {
        let (d, mut h) = db();
        let a = h.add("aaa", "demo", 1000, None, None, None).unwrap();
        h.add("bbb", "demo", 2000, None, None, None).unwrap();
        h.set_starred(a, true).unwrap();
        let r = h.search("", 10).unwrap();
        assert!(r[0].starred && r[0].sql == "aaa");
        drop(h);
        let h2 = HistoryDb::open(&d.path().join("h.sqlite")).unwrap();
        assert!(h2.search("", 10).unwrap()[0].starred); // survives reopen
    }

    #[test]
    fn consecutive_dedup_within_window() {
        let (_d, mut h) = db();
        h.add("select 1", "demo", 1000, Some(5), Some(1), None).unwrap();
        h.add("select 1", "demo", 1003, Some(4), Some(1), None).unwrap(); // within 5 s
        h.add("select 1", "demo", 2000, Some(4), Some(1), None).unwrap(); // outside
        assert_eq!(h.search("", 10).unwrap().len(), 2);
    }

    #[test]
    fn failed_run_recorded_with_error() {
        let (_d, mut h) = db();
        h.add("select bad", "demo", 1000, None, None, Some("syntax error")).unwrap();
        let r = h.search("", 10).unwrap();
        assert_eq!(r[0].error.as_deref(), Some("syntax error"));
    }
}
```

- [ ] **Step 2: Run** `cargo test -p dbc-state history` → compile error. **Step 3: Implement.** **Step 4:** all dbc-state tests green (9 existing + 5 new).
- [ ] **Step 5: Commit** — `git commit -m "feat: persistent query history store (sqlite + fts)"`

---

### Task 2: Favourite objects (dbc-state)

**Files:**
- Modify: `crates/dbc-state/src/config.rs`, `crates/dbc-state/src/lib.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavouriteObject {
    pub connection_id: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: String,   // "table" | "view" | "routine" | "trigger" | "sequence"
}

// AppConfig gains:
//   #[serde(default)] pub favourite_objects: Vec<FavouriteObject>,
// plus helpers:
impl AppConfig {
    pub fn is_favourite(&self, f: &FavouriteObject) -> bool;
    /// Adds if absent, removes if present; returns the new state.
    pub fn toggle_favourite(&mut self, f: FavouriteObject) -> bool;
}
```

- Back-compat: existing config.toml files without the field must load (serde default — covered by a test).

- [ ] **Step 1: Tests** — `favourite_objects_roundtrip` (toggle on → save → load → is_favourite true; toggle off → false), `old_config_without_favourites_loads` (TOML string without the key parses with empty vec). **Step 2: Run → fail → implement → green** (`cargo test -p dbc-state`). **Step 3: Commit** — `git commit -m "feat: favourite objects in app config"`

---

### Task 3: History panel + query recording (dbc-ui)

**Files:**
- Create: `crates/dbc-ui/src/history_panel.rs`
- Modify: `crates/dbc-ui/src/main.rs`

**Interfaces:**
- Consumes: `HistoryDb` (Task 1), `SqlInput::set_text` (exists, currently `#[allow(dead_code)]` — remove the allow), tabs/run pipeline.
- Behaviour contract:
  1. AppView owns `history: Option<HistoryDb>` (open at startup from `default_history_path()`; on open error → status "error: historie nedostupná ({e})", app fully functional without it) and `history_visible: bool` (default true), toggled by **Ctrl+H** (new app action `ToggleHistory`, context None).
  2. Recording: in the run pipeline, capture `started_at` (unix) at dispatch; on `Finished` → `history.add(sql, connection_name, started_at, Some(elapsed_ms), Some(rows), None)`; on `Failed` → `add(..., Some(&err.to_string()))`. Connection name = active connection's `name` (CLI-arg path: "cli"). Recording errors are IGNORED (fire-and-forget, at most a one-time status note). Previews are recorded too (they run real SQL).
  3. Panel: right side 280 px. Header "Historie" + search input (reuse the `TextField` from connections_ui — check its constructor; unmasked). Below: list of entries (newest first, starred first): first line = SQL collapsed to one line (≤48 chars + …), second line small: `{connection} · {rows} řádků · {duration} ms` or `error` in red; ★ button toggles star (persists immediately). Search box edits re-query `history.search(q, 100)` on every change (fast — local sqlite).
  4. Click on an entry → `SqlInput::set_text(sql)` + focus editor. NEVER auto-runs.
  5. List refresh: after every recorded run (successful add → re-query with current filter), and on panel open.
  6. Layout: root row becomes [tree | middle column | history panel] — mirror the tree-panel pattern including collapse-to-0px.
- Pure-logic tests: entry-line formatting function (collapse, metadata line, error variant) extracted GPUI-free and unit-tested; recording call-site correctness is review-verified (GPUI paths untestable headless).

- [ ] **Step 1: formatting tests → implement.** **Step 2: panel + recording wiring; build zero new warnings; `cargo test -p dbc-ui` green.** **Step 3: Sanity launches (sqlite-arg + no-arg, 15 s, no panic); run one query against a sqlite fixture via... (headless limit — record that interactive verification of panel remains for the human checklist).** **Step 4: Commit** — `git commit -m "feat: history panel with fulltext search and pins"`

---

### Task 4: Generalized favourites in tree + connection stars (dbc-ui)

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs`, `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/connections_ui.rs`

**Interfaces:**
- Consumes: `FavouriteObject`/`AppConfig::toggle_favourite` (Task 2), tree NodeId/flatten.
- Behaviour contract:
  1. Tree rows for tables/views/routines/triggers/sequences get a ★ toggle (hollow ☆ / filled ★, right-aligned, visible on hover or always — simpler is fine). Clicking ★ emits a new `TreeEvent::ToggleFavourite(FavouriteObject)`; main.rs applies `config.toggle_favourite` + `AppConfig::save` (existing corrupt-config guard path reused) and pushes the updated favourite set back into the tree.
  2. Tree gains a top "Oblíbené" section (before schemas) listing favourited objects of the ACTIVE connection across schemas (label `{schema}.{name}` or `{name}`), with the same double-click semantics (preview/DDL). Empty → section hidden. The tree needs the favourite set: `SchemaTree::set_favourites(Vec<FavouriteObject>, cx)` called on snapshot apply and after toggles; flatten() consumes it (extend the pure tests: favourites section first, only matching connection, hidden when empty).
  3. Connections: the dropdown already sorts favourites first (G1). Add the ★ toggle next to each dropdown row (click = toggle `favourite` on ConnectionConfig + save + refresh grouped cache; stop_propagation so it doesn't connect).
  4. Persistence: favourites survive restart (config.toml) — covered by Task 2 tests; UI wiring review-verified.
- Tests: extended flatten() pure tests (3+ new cases).

- [ ] **Step 1: flatten tests → implement tree changes.** **Step 2: wiring + dropdown ★; build clean; tests green.** **Step 3: sanity launches; human checklist note.** **Step 4: Commit** — `git commit -m "feat: generalized favourites (tree section, object and connection stars)"`

---

### Task 5: Ctrl+K command palette (dbc-ui)

**Files:**
- Create: `crates/dbc-ui/src/palette.rs`
- Modify: `crates/dbc-ui/src/main.rs`

**Interfaces:**
- Consumes: schema snapshot (via the tree entity's snapshot or a shared handle — read access only), `HistoryDb::search`, config connections, existing app actions.
- Produces `palette.rs`:

```rust
/// Pure fuzzy scorer: case-insensitive subsequence match; returns None if
/// query chars don't all appear in order; higher = better (consecutive runs
/// and word-boundary hits score higher; shorter targets win ties).
pub fn fuzzy_score(query: &str, target: &str) -> Option<i64>;

pub enum PaletteItem {
    Table { schema: Option<String>, name: String },          // → open preview
    HistoryEntry { id: i64, sql: String },                   // → load into editor
    Connection { id: String, name: String },                 // → switch
    Action { label: String, action: PaletteAction },         // → dispatch
}
pub enum PaletteAction { RunQuery, ToggleTree, ToggleHistory, NewConnection, RefreshSchema }
```

- Behaviour contract:
  1. **Ctrl+K** (app action `OpenPalette`, context None) opens a centered overlay (same modal pattern as connections_ui incl. `.occlude()` + focus capture — G1 lessons are binding: focus moves to the palette input in the same update). Esc closes. Enter executes the selected item. Up/Down move selection. Typing filters live.
  2. Sources, assembled on open + re-scored per keystroke: tables/views from the current snapshot (prefix `T`), history top 20 for the query (prefix `H`, via history.search), connections (prefix `C`), fixed actions (prefix `A`, Czech labels: "Spustit dotaz", "Přepnout strom", "Přepnout historii", "Nové spojení…", "Obnovit schéma"). Favourite objects and favourite connections get a score bonus (+1000) so they rank first among equals (spec: palette ranking).
  3. Empty query → favourites first, then recent history, then connections, then actions (capped ~30 rows).
  4. Execution routes through EXISTING paths: preview → the same TreeEvent::OpenPreview handler logic; history → set_text + focus editor; connection → switch_to_connection; actions → call the respective methods. No new execution logic.
  5. Palette must not open while another modal is up (and vice versa Esc precedence follows existing modal-first conventions).
- Tests (pure): fuzzy_score ordering cases (subsequence miss → None; consecutive-run beats scattered; word-boundary bonus; favourite bonus applied by the assembly fn — extract `rank_items(query, sources) -> Vec<PaletteItem>` pure and test top-N ordering with a favourite present).

- [ ] **Step 1: fuzzy/rank tests → implement pure part.** **Step 2: overlay + wiring; build clean; all dbc-ui tests green.** **Step 3: sanity launches; human checklist.** **Step 4: Commit** — `git commit -m "feat: ctrl+k command palette"`

---

## Self-Review Notes

- Spec coverage (G3 row): persistent history SQLite → T1; right panel/fulltext/pins/click-to-load → T3; Ctrl+K palette → T5; generalized favourites (tree ★, Favourites section, connection pin, palette ranking) → T2+T4+T5. Per-table view memory is G4 (not here) per spec.
- Type consistency: HistoryEntry/HistoryDb signatures used by T3/T5 match T1; FavouriteObject in T4/T5 matches T2; TreeEvent::ToggleFavourite added in T4 and consumed in main.rs; set_text exists (G1 T4, allow removed in T3).
- Order: T1 ∥ T2 → T3 → T4 → T5 (T3/T4/T5 all touch main.rs → sequential).
- Known risks: layout re-flow to three columns (T3) touches the render root; palette focus/modal interplay (T5) repeats G1's focus-capture lessons — reviewers must re-probe both.
