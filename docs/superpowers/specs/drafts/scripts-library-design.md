# Scripts Library (pure Bruno model) — Design Pass

Date: 2026-08-25
Status: designed per the resolved user decision (binding, do not expand):
**pure Bruno model** — saved queries/scripts are plain `.sql` files in ONE
user-chosen folder; the app shows a scripts tree over that folder (open
into editor, save, rename, new folder, delete); **git stays EXTERNAL** —
no git engine, no git credentials, no commit/push/status/diff UI in the
app, ever (the +git-status and +commit/diff variants were explicitly
rejected). Security alignment: no new secrets in the app; script files
are user content — **never auto-executed**.

Read before implementing: `crates/dbc-ui/src/schema_tree.rs`
(`SidebarRow`, `OuterId` + its polarity note, `flatten_sidebar`,
`emit_schema_slot`, `toggle_outer`, `handle_chevron`/`handle_single_click`
/`handle_double_click`, the inline icon rows ★/⊞/⇪, the Notice color
dispatch on the `"error:"` prefix); `crates/dbc-ui/src/main.rs`
(`start_script_pick` 2600–2750 — the G12 confirm flow this design REUSES,
`list_sql_files` 399, `count_statements_in_file` 374, the editor column
8728–8760, `actions!` 67, `bind_keys` 9157, `DiscardConfirmState`/
`PendingDiscard` 988–1046); `crates/dbc-ui/src/sql_input.rs`
(`SqlInput::text`/`set_text`); `crates/dbc-state/src/config.rs`
(`AppConfig`, the `ToolPaths` additive-field precedent + paired
back-compat tests, `AppConfig::save`'s tmp+rename atomic write);
`crates/dbc-ui/src/connections_ui.rs` (`ModalState`,
`modal_confirm_kind`'s exhaustive policy table, `render_settings_panel`
1626, `TextField`).

## 0. Grounding facts the design leans on

1. **Tabs are RESULT tabs only.** `TabContent` has nine variants — Grid/
   Text/Monitor/Plan/Diagram/Compare/Chart/ScriptRun/Admin — and none is
   an editor. The SQL editor is ONE global `Entity<SqlInput>`
   (`AppView.sql`, main.rs:1071) rendered as a fixed 8-line pane ABOVE
   the tab strip. There is no per-tab editor state, no editor dirty
   tracking, no file binding, and no Ctrl+S/Ctrl+O binding anywhere
   (verified: repo-wide grep). This forces §3's central adaptation.
2. **The G12 script runner already runs `.sql` files from disk** with a
   mandatory confirm modal (statement-count pre-scan, tx-scope/error-
   policy radios, Enter deliberately inert) and a per-statement
   read-only gate in the runner. It has NO notion of a scripts folder
   and NO recent list — files come from an ad-hoc `prompt_for_paths`
   pick each time. This design gives those files a home; the run path
   itself is reused verbatim (§6).
3. **The sidebar is a multi-root `uniform_list`** built by the pure
   `flatten_sidebar`; pinned root sections („Správa serveru",
   „Oblíbené (n)") already coexist with connection roots, so a new
   pinned root section is the established shape — not a new panel.
   There are NO context menus and NO tooltips at the pinned GPUI rev;
   row actions are always-rendered inline icon divs (★/⊞/⇪ precedent),
   and there is no inline-rename anywhere (modals are the precedent).
4. **Config lives in `%APPDATA%\dbc\config.toml`** (`AppConfig`), new
   fields are additive `#[serde(default)]` with paired back-compat
   tests; `ToolPaths` is the existing PATH-type-setting precedent
   (paths in config.toml are fine — they are not secrets). A settings
   modal exists (`ModalState::Settings`, „Nastavení", theme-only today)
   and is the natural home for the folder setting.
5. **No `notify`, no `walkdir` in the dependency tree** (walkdir only
   transitively via GPUI). Directory walking is hand-rolled
   `std::fs::read_dir` (`list_sql_files` precedent), always dispatched
   off the UI thread via `cx.background_spawn`.
6. **GPUI file dialogs have no extension filter** at the pinned rev
   (resolved G12 spike, main.rs:2626) — `.sql` enforcement is always
   client-side.

## 1. Decisions the phase brief left open — resolved

### 1.1 One global folder (chosen) vs per-workspace/per-connection

**ONE global folder**, stored as `AppConfig.scripts_dir: Option<String>`
(§2). Rationale: (a) Bruno's own model is one collection folder opened
by path; (b) the app has no workspace concept, and per-connection
folders would multiply empty trees and settings UI for a v1 nobody
asked for; (c) the user who wants per-server organisation just makes
subfolders — the tree renders them natively. **Documented convention,
not enforced:** a subfolder per connection name (e.g.
`prod-pg/reporting.sql`) — the app never creates, names, or interprets
such folders.

### 1.2 File-watching vs manual refresh

**No `notify` watcher this phase.** The refresh story is:
- automatic rescan after EVERY in-app mutation (create/rename/delete/
  save-as — §5), so the app's own actions are never stale;
- a `⟳` icon on the „Skripty" root row (manual, e.g. after a
  `git pull`);
- automatic scan on startup (when configured), on folder (re)selection,
  and on expanding the root while `NotLoaded`/`Error`.

Rationale: `notify` is a new dependency with per-platform watcher
threads, debounce tuning, and event-storm handling — real complexity
purchased to save one click after an external edit. The scan itself is
one bounded background `read_dir` walk (≤ 2000 entries, §7), i.e.
milliseconds; a stale tree is self-healing and harmless because every
file operation re-validates against the real filesystem at dispatch
time (a vanished file yields a Czech error, never corruption).
"Refresh-on-window-focus" was considered and rejected too: the pinned
GPUI rev's activation-observer surface is unproven in this codebase and
the ⟳ affordance covers the git-pull story honestly. Watcher support is
recorded as a possible follow-up, not a debt.

### 1.3 Editor relation — bind the GLOBAL editor, not per-script tabs

The brief sketched "opening a script = editor tab bound to the file",
but grounding fact 0.1 says the app has no editor tabs at all — tabs
are results, the editor is one global pane. Building a per-tab editor
model would be an editor-architecture rework (that is the g6-editor-pro
draft's territory), not a scripts-library feature. **Resolved: opening
a script binds the single global editor to the file:**

- `AppView.script_binding: Option<ScriptBinding { path: PathBuf,
  saved_text: String }>` — `path` is ABSOLUTE (survives a scripts-dir
  change; display re-relativizes against the current root and falls
  back to the file name).
- A thin caption strip renders above the editor ONLY when bound:
  „Skript: {rel}" plus the „ •" dirty suffix (the exact tab-title
  convention), an „Uložit" button (dim when clean) and „Zavřít"
  (unbind). Dirty = `sql.text() != saved_text` (exact compare, bounded
  by the 1 MiB open cap §7; length short-circuits first).
- **Ctrl+S** (new `SaveScript` action — the chord is free, verified):
  bound → atomic save (§5); unbound → save-as into the library (§5.4).
- Ctrl+Enter semantics are UNCHANGED: it runs the editor TEXT through
  the normal query path (auto-limit, params, multi-statement unlock) —
  binding never changes what runs. Running the FILE goes through the
  tree's ▶ and the G12 confirm modal (§6), and always runs the DISK
  content — a dirty binding means editor and disk differ, which the •
  makes visible; the confirm modal's from-disk statement count is the
  honest number. (Documented, not "fixed": auto-saving before run
  would be a silent write.)
- Dirty-discard guards (§5.5) protect the binding against silent text
  replacement: opening another script, closing the binding, changing
  the scripts folder, and the two existing history/palette
  "load SQL into editor" sites all route through one guarded helper.
  Note the baseline: today the editor text is clobbered with NO guard
  anywhere; this phase strictly improves that for bound scripts and
  leaves unbound ad-hoc text exactly as guarded as before (not at
  all).
- App exit with a dirty binding is NOT guarded this phase (the app has
  no exit interception anywhere; adding one is out of scope). Same
  posture as today's editor text, disclosed in release notes.

### 1.4 Where the tree lives

**A third pinned root section „Skripty" in the existing sidebar**,
emitted after „Oblíbené" and before the CLI/connection roots, collapsed
by default. Rationale: fact 0.3 — pinned sections are the established
multi-root shape; a separate panel would cost fixed-width real estate
(the 260 px sidebar is not resizable), a new toggle, and a second
uniform_list for zero interaction gain. The section is GLOBAL: unlike
★/⊞/⇪ it does not depend on the active scope and renders its icons
unconditionally (scripts are files, not database objects; running one
is where connection context enters, via the existing G12 gates).
The section renders even when `scripts_dir` is unset — expanding it
shows one clickable notice row „složka skriptů není nastavena —
klikněte pro Nastavení" that opens the settings modal (discoverability
without a wizard).

### 1.5 `.sql` filter — show only `.sql` (chosen)

The tree shows folders and `*.sql` files (case-insensitive), nothing
else — matching `list_sql_files` and Bruno's only-`.bru` posture. The
library is a query library, not a file manager; a `README.md` or
`.git/` in the folder is invisible and untouched. (`.git` specifically:
it is just another non-matching directory — the scan descends into it
never, see §7's dot-dir rule.) Disclosed in the design, not in-UI.

## 2. Config + settings UI

```rust
// config.rs — AppConfig gains (ToolPaths precedent):
/// Scripts library (Bruno model): absolute path of the user-chosen
/// folder with plain `.sql` files. `None` = feature dormant (the
/// sidebar section shows a pointer to Settings). A path, not a secret —
/// config.toml is the right home. Git integration is deliberately
/// EXTERNAL (user decision 2026-08-25): the app never reads or writes
/// anything git-related about this folder.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub scripts_dir: Option<String>,
```

Paired back-compat tests per house convention (old file loads with
`None` + roundtrip stays byte-identical until set).

Settings modal gains a „Složka skriptů" block under „Motiv": the
current path (or „nenastavena") in muted text, buttons „Vybrat složku…"
(`prompt_for_paths { directories: true }`; on pick: store the absolute
path, save config, dispatch a scan) and „Odebrat" (set `None`, save,
clear the tree state; a dirty binding routes through the §5.5 guard
first — the binding itself survives, since it holds an absolute path,
but the guard fires when the SECTION removal would strand a dirty
buffer with no tree affordance — resolved: „Odebrat" only clears tree
state and never touches the binding; no guard needed, the caption
strip's „Uložit" still works). „Zavřít"/Esc semantics unchanged.

## 3. Tree model

### 3.1 New module `crates/dbc-ui/src/scripts.rs` (pure + std-fs only)

```rust
/// One entry of the scanned library, in DISPLAY order (depth-first;
/// within a directory: folders first, then files, each name-ordered).
/// `rel` uses '/' separators on all platforms (stable expand keys and
/// event payloads; resolved back to components by `resolve_rel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptEntry {
    pub rel: String,
    pub is_dir: bool,
    pub depth: usize, // 0 = direct child of the root
}

pub struct ScriptScan {
    pub entries: Vec<ScriptEntry>,
    pub truncated: bool,     // entry cap hit (SCRIPTS_ENTRY_CAP)
    pub depth_clipped: bool,  // depth cap hit (SCRIPTS_DEPTH_CAP)
}

pub const SCRIPTS_ENTRY_CAP: usize = 2000;  // 2000-db cap precedent
pub const SCRIPTS_DEPTH_CAP: usize = 12;
pub const SCRIPT_OPEN_CAP: u64 = 1_048_576; // 1 MiB editor-open cap
pub const SCRIPT_NAME_CAP: usize = 80;

pub fn scan_scripts(root: &Path) -> Result<ScriptScan, String>;
pub fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String>;
pub fn validate_script_name(name: &str, is_file: bool) -> Result<String, String>;
pub fn create_script(root: &Path, parent_rel: &str, name: &str) -> Result<String, String>;
pub fn create_folder(root: &Path, parent_rel: &str, name: &str) -> Result<String, String>;
pub fn rename_entry(root: &Path, rel: &str, new_name: &str, is_dir: bool) -> Result<String, String>;
pub fn delete_entry(root: &Path, rel: &str, is_dir: bool) -> Result<(), String>;
pub fn write_script(path: &Path, text: &str) -> Result<(), String>;
pub fn read_script(path: &Path) -> Result<String, String>;
```

All errors are Czech display strings (runner precedent). `write_script`
is atomic (`.tmp` + `sync_all` + `rename` — the `AppConfig::save`
shape). `create_*`/`rename_entry` return the NEW rel path.

### 3.2 Scan rules (§7 carries the safety rationale)

- Iterative walk (explicit stack of `(PathBuf, rel_prefix, depth)`), no
  recursion — house rule.
- Entries: directories (except names starting with `.` — this is what
  keeps `.git/` etc. invisible AND undescended) and files with a
  case-insensitive `.sql` extension. Everything else skipped silently.
- **Symlinks are skipped entirely** (checked via
  `fs::symlink_metadata().file_type().is_symlink()` before descending
  or listing): a symlinked directory could walk outside the chosen
  root or cycle; a symlinked file's target is equally out-of-root.
  One rule, zero traversal exposure.
- Per-directory ordering: folders first, then files, each by
  case-insensitive name; depth-first splice so the output is already
  display order.
- Caps: stop emitting past `SCRIPTS_ENTRY_CAP` total entries
  (`truncated`); do not descend past `SCRIPTS_DEPTH_CAP` (`depth_clipped`);
  each gets its own Notice row (§3.4).

### 3.3 Sidebar state + rows

`SchemaTree` gains one slot (same state-machine family as `DbListState`):

```rust
pub enum ScriptsListState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { entries: Vec<ScriptEntry>, truncated: bool, depth_clipped: bool },
}
```

`SidebarRow` gains variants (all matches are exhaustive — compiler-
guided sweep):

```rust
ScriptsRoot,                       // pinned section header „Skripty"
ScriptFolder { rel: String },
ScriptFile { rel: String },
ScriptNotice { text: String, open_settings: bool }, // notices; click opens Nastavení when flagged
```

`OuterId` gains `Scripts` and `ScriptFolder(String)` (both default
COLLAPSED — presence in the set = expanded, the lazy polarity;
folders-default-open applies to CONNECTION grouping folders only, the
scripts tree is lazy like connections).

`flatten_sidebar` takes one new parameter
`scripts: Option<(&ScriptsListState, bool)>` (`None` until the flip
task passes state; the bool = „scripts_dir configured"). Emission: the
„Skripty" root row after the Oblíbené block; when expanded —
unconfigured ⇒ the settings-pointer notice; `Loading` ⇒ „Načítám
skripty…"; `Error(e)` ⇒ `error: {e}` retry row; `Loaded` ⇒ entries
whose ancestor folders are all expanded, at `1 + entry.depth`, plus cap
notices. Speed search: same contract as everything else — filters
LOADED rows only, never fetches; folders auto-expand under an active
filter and childless non-matching rows truncate away (existing
pattern).

### 3.4 Czech strings (binding)

- Section: „Skripty"; empty loaded root: „žádné skripty (*.sql)".
- Unconfigured notice: „složka skriptů není nastavena — klikněte pro
  Nastavení".
- Loading: „Načítám skripty…"; errors: `error: {msg}` (color sentinel).
- Cap notices: „… zobrazeno prvních 2000 položek — zmenšete knihovnu
  skriptů" and „… některé podsložky jsou příliš hluboko (limit 12
  úrovní)".
- Settings: „Složka skriptů", „nenastavena", „Vybrat složku…",
  „Odebrat".
- Caption strip: „Skript: {rel}" (+„ •"), „Uložit", „Zavřít".
- Statuses: „skript uložen: {name}", „skript vytvořen: {name}",
  „přejmenováno: {name}", „smazáno: {name}", „error: nastavte složku
  skriptů v Nastavení".

## 4. Interactions (rows, icons, events)

Inline icon divs (★/⊞/⇪ precedent: always rendered, `stop_propagation`,
then emit):

| Row | Icons | Click | Double-click |
|---|---|---|---|
| ScriptsRoot | `⟳` refresh, `+` new item at root | select | toggle expand |
| ScriptFolder | `+` new item here, `✎` rename, `✕` delete (empty only) | select | toggle expand |
| ScriptFile | `▶` run (G12 flow), `✎` rename, `✕` delete | select | open into editor (§5.1) |
| ScriptNotice | — | retry scan / open Nastavení | — |

New `TreeEvent` variants (handlers land with the flip — the exhaustive
match forces same-task): `ScriptsRefresh`, `OpenScriptsSettings`,
`ScriptOpen { rel }`, `ScriptRunFile { rel }`,
`ScriptCreate { parent_rel }`, `ScriptRename { rel, is_dir }`,
`ScriptDelete { rel, is_dir }`.

Modals (both new `ModalState` variants; `modal_confirm_kind` is
exhaustive and each must pick a policy side):

- `ScriptName { mode, parent_rel, target_rel, is_dir, field:
  Entity<TextField>, error: Option<String> }` — one dialog for new
  script / new folder / rename („Nový skript" with a Skript/Složka
  radio; „Přejmenovat"). Enter = confirm (creates/renames a FILE, runs
  nothing against the database — policy table clause (a)). Esc closes.
- `ScriptDeleteConfirm { rel, is_dir }` — „Smazat {skript|složku}
  {name}? Akce je nevratná (maže se z disku, ne do koše)." Buttons
  „Smazat"/„Zrušit". Enter = **Ignore** (§3-novela spirit: the button
  is the last gate before an irreversible action — the action targets
  the filesystem, not the database, but the rule's substance is
  irreversibility, not SQL). Esc closes.

All fs mutations dispatch through `scripts.rs` ops in
`cx.background_spawn`, then rescan on success; errors land in the
modal (`error` field) or status line. Rename/delete of the currently
BOUND file fix the binding up: rename updates `binding.path`; delete
clears the binding (its dirty guard runs FIRST — deleting a dirty-bound
file prompts the §5.5 discard confirm before the delete confirm even
opens — resolved simpler: the delete confirm text gains a second line
„Skript má neuložené změny v editoru." when it targets the dirty-bound
file; one modal, both facts).

## 5. Editor binding mechanics

### 5.1 Open (`TreeEvent::ScriptOpen`)

Guarded by §5.5. Resolve `resolve_rel(root, rel)`; background: stat
size (> `SCRIPT_OPEN_CAP` ⇒ „error: soubor je příliš velký pro editor
(limit 1 MiB) — spusťte jej jako skript"), read to `String` (lossy
UTF-8 conversion is an error, not a mangle: non-UTF-8 ⇒ „error: soubor
není platné UTF-8"); then on the UI thread: `sql.set_text`, set
`script_binding = Some(ScriptBinding { path, saved_text })`, status
cleared. Opening never runs anything (brief: script files are user
content — never auto-execute).

### 5.2 Save (Ctrl+S / „Uložit", bound)

Capture `(path, text)`; background `write_script` (atomic tmp+rename);
success ⇒ `saved_text = text`, status „skript uložen: {name}"; failure
⇒ `error: {e}`. Last-writer-wins on external edits — by the user's own
model, git is the history/merge layer; the app does not diff or
version (that is exactly the rejected variant).

### 5.3 Close binding („Zavřít")

Guarded by §5.5; sets `script_binding = None`, editor text stays (it
is just no longer bound).

### 5.4 Save-as (Ctrl+S unbound)

Empty editor ⇒ no-op status „editor je prázdný". No `scripts_dir` ⇒
„error: nastavte složku skriptů v Nastavení". Else
`prompt_for_new_path(&root, Some("dotaz.sql"))`; append `.sql` when
missing (client-side, fact 0.6); write, bind, and rescan when the
saved path is inside the root (outside is allowed — it is the user's
disk — but the tree honestly won't show it).

### 5.5 One dirty guard

`fn editor_load_guarded(&mut self, action: PendingScriptAction, cx)` —
when `script_binding` is dirty, park the action in the existing
`DiscardConfirmState` machinery via a new
`PendingDiscard::Script(PendingScriptAction)` arm (message branches:
„Neuložené změny skriptu {name} budou zahozeny."); else perform
immediately. Actions: `Open { rel }`, `Unbind`, `LoadText { sql }`
(the history-panel and palette history-click sites route here) —
deleting a dirty-bound file needs NO action here, §4 resolved it to a
second line inside the delete confirm. The guard NEVER protects unbound
ad-hoc text —
identical exposure to today, zero behavioral regression surface.

## 6. Running a saved script — G12 reuse, verbatim gates

`TreeEvent::ScriptRunFile { rel }` runs the EXISTING confirm flow with
the picker stage replaced by the known path:

1. Same entry gates as `start_script_pick` (no modal/apply/discard
   open, no run in flight, spec resolves, dialect exists), same
   `conn_identity` capture BEFORE the pre-scan.
2. Background: `resolve_rel`, extension re-check, existence check
   (stale tree ⇒ Czech error + rescan), `count_statements_in_file`.
3. The post-pre-scan continuation of `start_script_pick` (modal-races
   + identity re-check + `ModalState::ScriptRun` construction) is
   FACTORED into one helper both paths call — the scripts library must
   not fork the confirm policy. Everything downstream (`confirm_script_run`'s
   re-checks, `script_run_dispatch_allowed`, tx/error radios, the
   runner's per-statement read-only gate, progress tab, history's
   `[skript]` synthetic entry) is untouched by construction.

The ad-hoc „SQL soubor…"/„SQL složku…" buttons and palette actions stay
— they serve out-of-library files; no affordance is removed.

## 7. Safety rails (the audit)

1. **Root escape:** every fs op goes through `resolve_rel`, which
   splits on `/`, rejects empty/`.`/`..` components and any component
   containing `\`, `:`, control characters, or a Windows drive/UNC
   shape, then joins onto the root. Rels only ever ORIGINATE from the
   scan (which builds them from single `file_name()` components) or
   from `validate_script_name` output — the check is defense in depth,
   pinned by tests (`..`, `a/../b`, `C:\x`, `\\srv\share`, `x\0y`).
2. **Symlinks:** skipped at scan (never descended, never listed —
   §3.2), therefore never openable/renamable/deletable via the tree.
   No canonicalize-and-compare dance needed: nothing out-of-root ever
   gets a rel.
3. **Hostile filenames (created in-app):** `validate_script_name` —
   trim; non-empty; ≤ `SCRIPT_NAME_CAP`; no `/ \ : * ? " < > |`, no
   control chars; no leading/trailing dot or space; case-insensitive
   reserved-device check (CON, PRN, AUX, NUL, COM1–9, LPT1–9, also
   with any extension); files get `.sql` appended when missing (then
   re-validated). Collision ⇒ „název už existuje" (pre-checked
   case-insensitively via the scan snapshot + `try_exists`).
4. **Hostile filenames (from disk):** rendered as text in GPUI rows
   (no interpolation into SQL/paths beyond the join in 1); lossy
   display is acceptable, operations use the real `OsString` path
   carried by `resolve_rel`'s join.
5. **Large folders:** `SCRIPTS_ENTRY_CAP = 2000` + disclosure Notice
   (the 2000-db precedent verbatim); `SCRIPTS_DEPTH_CAP = 12` +
   disclosure; the walk is iterative, so depth is a cap policy, not a
   stack-safety need.
6. **Large files:** `SCRIPT_OPEN_CAP = 1 MiB` for EDITOR opens only —
   running via ▶ streams through the G12 splitter in 64 KiB chunks and
   has no such cap (unchanged).
7. **Never auto-execute:** open = read text; save/rename/delete never
   touch a database connection; run = explicit ▶ + the unchanged G12
   confirm modal + runtime read-only gates. No path from scan to
   execution exists.
8. **No new secrets:** the only new persisted datum is a folder path in
   config.toml (ToolPaths posture). No git credentials, no git
   subprocess, no network. dbc-mcp is untouched (its config read gains
   an inert optional field; the merge gate proves NONE diff).
9. **Deletes are explicit and bounded:** files only after the confirm
   modal; folders only when EMPTY („složka není prázdná — smažte
   nejdřív její obsah") — no recursive delete in v1 (git can restore a
   file; an un-tracked recursive delete cannot be undone by us).

## 8. What this phase deliberately does NOT do (recorded)

- No git engine/status/commit/diff UI — permanently out (user
  decision, restated so no future phase "helpfully" adds badges).
- No `notify` watcher (§1.2 — possible follow-up, not debt).
- No per-tab editors / multi-open scripts (g6-editor-pro territory).
- No recursive folder delete; no drag-drop move; no duplicate/copy.
- No palette items per script (would require the palette to hold the
  scan; follow-up candidate) — the palette gains only „Uložit skript".
- No app-exit dirty guard (no exit interception exists app-wide).
- No MCP exposure of the library.
- No `.sql` templates/snippets, no non-`.sql` files in the tree.

## 9. Task decomposition (serialization explicit)

| T | Content | Files (owner) | Depends on |
|---|---|---|---|
| T1 | dbc-state: `scripts_dir` + back-compat tests | dbc-state/config.rs | — |
| T2 | `scripts.rs`: scan/validate/resolve/fs ops + full tempfile suite | dbc-ui/scripts.rs (+1 `mod` line in main.rs) | — |
| T3 | schema_tree additive: rows, `OuterId`, `ScriptsListState`, flatten emission (dark: `scripts: None`), expand/click plumbing, pure tests | schema_tree.rs (+its flatten call sites) | T2 |
| T4 | FLIP 1 — settings row, scan dispatch wiring, section live, ⟳/notice events | connections_ui.rs, main.rs | T1, T3 |
| T5 | Editor binding: open/save/close/save-as, Ctrl+S, caption strip, §5.5 guard, palette entry | main.rs, sql_input read-only usage | T4 |
| T6 | Mutations + run: name/delete modals, fs dispatch, binding fixups, ▶ → factored G12 confirm | main.rs, connections_ui.rs | T5 |
| T7 | Sweep: docs as-built, memory, version 0.22.0, full gates + smoke | Cargo.toml, docs | T6 |

`main.rs` and `connections_ui.rs` serialize T4 → T5 → T6; T1 ∥ T2 up
front. Versioning: one minor bump — **0.22.0** (verify free on main at
merge time, house convention).

## 10. Self-review notes

- Checked against fact 0.1: no decision assumes per-tab editors; the
  binding is the minimal honest adaptation and the brief's "editor tab"
  intent (dirty tracking, Ctrl+S to file, discard guards) is fully
  preserved on the global editor.
- Checked the G12 seam: reuse is by FACTORING the existing
  continuation, not by a parallel modal — one confirm policy site.
- Checked exhaustiveness blast radius: `SidebarRow`, `OuterId`,
  `TreeEvent`, `ModalState`, `modal_confirm_kind`, the Esc-closable
  match, and the tab-strip content match (unchanged — no new
  TabContent) — each lands with its arms in the same task.
- Rejected alternatives, for the record: separate scripts panel
  (§1.4); `notify` watcher (§1.2); per-connection folders (§1.1);
  recursive delete (§7.9); auto-save-before-run (§1.3); rfd dependency
  for filtered dialogs (client-side `.sql` checks are the established
  workaround); storing rel paths in the binding (breaks on root
  change).
