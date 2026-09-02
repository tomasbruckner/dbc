# Editor tabs, per-tab context, database picker — design

Date: 2026-09-02. Status: approved in chat (approach A, per-tab context,
per-editor results); the remaining sections were approved as a block
(„tak to všechno udělej").

## 0. Why

The app has ONE global SQL editor; the tab strip under it holds *results*
(grid, plan, DDL, monitor…). Two user requests from 2026-09-02 do not fit
that shape:

- „nějaké zkratky na otevření nového tabu pro query" — there is nothing a
  new-tab shortcut could open.
- „nějaké zkratky pro vybrání databáze (asi napříč všemi connection)" — the
  palette lists connections, never databases, and with one global context a
  switch for one query switches everything.

The user chose real editor tabs (option A) over a single-buffer stopgap,
with **each tab owning its connection/database** (DataGrip model) and
**each tab owning its results**.

## 1. Data model

```rust
/// One editor tab. Everything that today assumes „the one editor" moves
/// in here; `AppView` keeps only what is per window.
pub(crate) struct EditorTab {
    id: u64,                         // stable; never recycled
    sql: Entity<SqlInput>,
    connection: Option<String>,      // was AppView::active_connection_id
    database: Option<String>,        // was AppView::active_database
    results: Tabs,                   // was AppView::tabs — the whole strip
    script: Option<ScriptBinding>,   // + script_dirty_flag, script_binding_generation,
                                     //   editor_discard_grant, script_save_in_flight
    autocomplete: Option<AutocompleteState>, last_ac_text, last_ac_cursor,
    cancel: Option<…>, run_generation, started_at,   // THIS tab's running query
    status: String,                  // the status line is per tab
}
```

`AppView` gains `editors: Vec<EditorTab>`, `active_editor: usize`,
`next_editor_id: u64`, and two accessors, `editor()` / `editor_mut()`.
The ~260 call sites in `main.rs` (`self.sql` 36×, `self.tabs` 42×,
`active_connection_id` 36×, `current_conn_identity()` 37×,
`script_binding` 39×, autocomplete ~30×) are rewritten mechanically to go
through the accessors. `resolve_active()`, `current_conn_identity()`,
`effective_database()` keep their signatures and read the active tab, so
`schema_tree`, `history_panel`, `backup`, `connections_ui` do not change.

**Stays on `AppView` (per window):** the tree and its caches, vault,
config, history, prefetch, `switch_generation`, `sidebar_fetch_generation`,
modal / palette / dropdown / discard-confirm, workspace, `attempted_restore`.

There is always at least one editor tab. Closing the last one replaces it
with a fresh empty tab rather than leaving none.

## 2. Async rail — results land by tab id

Every `cx.spawn` that today writes into editor state captures the
**editor id** at dispatch and, on completion, resolves
`view.editor_by_id_mut(id)`. `None` (the tab was closed meanwhile) drops
the result, exactly as the existing generation guards drop stale ones.
This covers:

- the query pipeline (`QueryEvent::Started/Batch/Finished/WriteFinished`):
  result tabs, `status`, `cancel`, history recording;
- script save (`script_save_in_flight`, saved_text);
- `switch_to_database`: success writes `connection`/`database` into the
  tab that **requested** the switch. It re-pushes tree scope
  (`refresh_tree_context`) only if that tab is still the active one.

The rejected alternative (keep fields on `AppView`, `mem::swap` on tab
switch) would make every one of those closures write into whichever tab
happens to be active at completion — the class of bug the generation
guards exist to prevent.

**Concurrency:** each tab may have its own in-flight query (`cancel` moves
into the tab). Escape cancels the active tab's. Closing a tab with a
running query cancels first, then closes. The runner is per operation
(design fact 0.1), so nothing else changes.

**Identity guards** (admin tab, sandbox apply bar, `ResultTab::conn_identity`)
keep comparing against `current_conn_identity()` — the semantics „this
result belongs to this context" is unchanged; the context now comes from
the active tab.

## 3. Session

`SessionState` gains:

```rust
pub struct SessionEditor {
    pub title: Option<String>,        // user-renamed later; None = derived
    pub sql: String,
    pub cursor: usize,
    pub connection: Option<String>,
    pub database: Option<String>,
    pub script_path: Option<PathBuf>, // bound .sql, absolute
    pub tabs: Vec<SessionTab>,        // this editor's result tabs
}
pub editors: Vec<SessionEditor>,      // serde(default)
pub active_editor: usize,             // serde(default)
```

The old top-level `connection/database/editor/cursor/tabs` stay
**readable** (serde default) and are still **written** for the active tab,
so a downgrade opens with the active editor intact. On load: if `editors`
is empty, the legacy fields become editor 0 — one migration path, pinned
by a test. `clamped()` caps `editors` at 32 and each editor's `tabs` as
today.

Restore: only the **active** editor reconnects at startup
(`switch_to_database(conn, db, Some(LoadDatabases))` — one vault prompt,
one round trip). Other editors keep their `connection`/`database` as
*intent* (`verified: false`); the first time such a tab becomes active,
`activate_editor` runs the switch for it, without the `LoadDatabases`
follow-up (the sidebar repair from `load_missing_db_lists` covers that).
`attempted_restore` becomes per editor for the same reason `capture_session`
uses it today (a tab that never reconnected must not lose its context on
the next save).

## 4. UI and shortcuts

**Layout.** An editor tab strip sits above the editor (same visual language
as `render_tab_strip`: 22 px rows, `bg_selected` for the active tab, ✕ on
hover, a `●` dirty dot when the bound script is dirty). The results strip
below the editor renders the **active editor's** `results` — the existing
`render_tab_strip`/`render_tab_content` just read through `editor()`.
The script caption row (bound file + „Zavřít") stays under the editor tab
strip and shows the active tab's binding.

**Title.** Bound script → its file name; else the first non-empty line
trimmed to 32 chars; else „Dotaz N" where N is the tab's creation ordinal.
Derived every render, never stored, unless `title: Some` (reserved for a
future rename).

**Shortcuts** (added to `keymap::SHORTCUTS`, Scope::Global, so F1 shows
them; all four chords verified unbound today):

| chord | action | behaviour |
|---|---|---|
| `ctrl-n` | `NewEditorTab` | new tab, inherits the current tab's `connection`/`database`, focuses the editor |
| `ctrl-w` | `CloseEditorTab` | dirty bound script → existing discard confirm; running query → cancel first; last tab → replaced by an empty one |
| `ctrl-tab` / `ctrl-shift-tab` | `NextEditorTab` / `PrevEditorTab` | cycle; wraps |
| `ctrl-d` | `PickDatabase` | §5 |

Clicking a tab activates it; middle-click closes (same as result tabs).
Activating a tab: swaps the editor entity in the render, re-pushes tree
scope, re-runs `refresh_tree_context`, restores that tab's `status`.

**Dropdown / tree / ● / history filter** all follow the active editor via
the unchanged `resolve_active()`.

## 5. Database picker (Ctrl+D)

A second **mode** of the existing palette, not a new dialog: `PaletteState`
gains `mode: PaletteMode::{Commands, Databases}`; `open_database_picker`
opens it in `Databases` mode with placeholder „Ctrl+D – připojení / databáze…".

Items: for every saved connection, one row per database from
`dbc_state::conn_cache::databases(conn_id)` (on-disk cache — servers you
have expanded before list fully even when collapsed), plus the
connection's default database when the cache has nothing. Label
`connection · database` with the folder path muted; the current tab's
context is marked `●` and ranked first on an empty query; `fuzzy_score`
runs over `connection database` joined. New `PaletteItem::Database
{ conn_id, db }` → `switch_to_database(conn_id, Some(db), None)` **for the
active editor**.

Escape closes; `ctrl-d` while open closes it (toggle). The Commands mode
also gets a fixed action „Vybrat databázi… (Ctrl+D)" so it is discoverable.

## 6. Testing

- `EditorTab` bookkeeping is pure and tested like `Tabs`: open/close/
  activate/cycle, last-tab replacement, id stability, `editor_by_id`.
- Session: round-trip of `editors`, legacy → editor 0 migration,
  clamping.
- Title derivation: bound name / first line / „Dotaz N".
- Picker: item assembly from cache + defaults, ranking with `●` first,
  `PaletteItem::Database` dispatch.
- Async rail: the pure decision functions already under test
  (`should_retrigger…`, `Finished` handler helpers) gain the id
  parameter; a closed-tab completion is pinned to drop.
- Live: launch, `Ctrl+N` ×2, run a query in tab 2, switch to tab 1 while
  it runs, confirm the result lands in tab 2; restart, both tabs and their
  contexts return. Keyboard injection does not reach GPUI here, so this
  part is the user's manual check; screenshots verify the strip renders.

## 7. Out of scope

Undo/redo, run-selection, find in editor, window bounds, scrollbars
(separate bounded task, design approved in chat), editor tab rename,
drag-reordering tabs, per-tab transactions.
