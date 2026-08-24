# UX Polish — Keyboard & Interaction (design)

Date: 2026-08-24
Status: curated design from explicit user requirements (verbatim intent
recorded below); decisions recorded here for review before execution.
Executes AFTER the tech-debt sweep merges (see §8).
Scope — the four reported gaps plus a keyboard-consistency sweep:

1. „Enter nefunguje pro potvrzení ve formulářích" — Enter triggers the
   primary/confirm action in every modal dialog (where allowed, §1).
2. „Tab nepřepíná mezi inputy" — Tab/Shift+Tab cycles focus through a
   dialog's inputs in form order (§2).
3. „Delete klávesa nemaže řádek ve výsledcích" — with a selected row in an
   editable results grid, Delete stages row deletion (the „✕/␡" gutter
   equivalent, sandbox staging only) (§3).
4. Po „+ řádek" má být uživatel nascrollovaný na nový řádek, ideálně
   s fokusem v první editovatelné buňce (§4).
5. User said „například" — §5 is the sweep: every similar gap found, each
   with an explicit fix-now / out-of-scope decision.

## 0. Survey (basis for every decision below)

### 0.1 GPUI focus + tab-stop support at the pinned rev (907ed09) — verified in source

- **Native tab-stop machinery EXISTS at this rev.**
  `crates/gpui/src/tab_stop.rs` implements `TabStopMap` (SumTree-ordered by
  `(tab_index path, insertion order)`, with `next()`/`prev()` that **wrap
  around** — see its own `test_tab_handles`: `next` from the last stop
  returns the first, `prev` from the first returns the last).
- `FocusHandle::tab_index(isize)` / `FocusHandle::tab_stop(bool)` are pub
  builder methods (`window.rs:572`, `window.rs:583`) that also update the
  shared `FocusMap` entry. Defaults: `tab_index = 0`, `tab_stop = false`.
- `Window::focus_next(cx)` / `Window::focus_prev(cx)` (`window.rs:2093`,
  `window.rs:2104`) read `rendered_frame.tab_stops` and move focus.
- Registration is automatic: `Div` paint inserts its
  `tracked_focus_handle` into `window.next_frame.tab_stops`
  (`elements/div.rs:2437-2439`) — i.e. **every `.track_focus(&handle)`
  element is already in the map**; whether it participates in traversal is
  decided solely by the handle's `tab_stop` flag (`TabStopMap::insert`
  stores `tab_stop: focus_handle.tab_stop`, non-stops are skipped by
  `next_inner`/`prev_inner`). `InteractiveElement::tab_index/tab_stop/
  tab_group` also exist on divs (`div.rs:764-790`) but are not needed —
  our focusables are `TextField`s that own their handles.
- **Keymap precedence + binding fall-through (load-bearing for §1):**
  `keymap.rs::bindings_for_input` (lines 165-190) sorts matched bindings
  by context-match depth (deeper wins), then bind recency. Dispatch
  (`window.rs:5390-5399`) tries them in order and **falls through to the
  next binding when the dispatched action finds no handler on the focus
  path** (`if !cx.propagate_event { … return }` inside the loop). This is
  already documented and relied on in-house: `grid.rs:139-147` („It works
  by binding-fallthrough, NOT by ancestor-scope-wins") for the find bar's
  `enter → FindNext` coexisting with SqlInput's unscoped `enter → Newline`,
  and `main.rs::on_cancel_query`'s doc comment for Escape.
- `UniformListScrollHandle::scroll_to_item(ix, ScrollStrategy)`
  (`elements/uniform_list.rs:150`; strategies Top/Center/Bottom, Center
  clamps „at the closest possible position" when the item is at the list
  end) — already used twice by `grid.rs` (`find_step` grid.rs:1105,
  `poll_find` grid.rs:1153).

### 0.2 Keybinding inventory (complete, from `cx.bind_keys` call sites)

| Key | Action | Context | Site |
|---|---|---|---|
| ctrl-enter / ctrl-shift-enter | RunQuery / RunQueryUnlimited | None | main.rs:7658-7659 |
| escape | CancelQuery | None | main.rs:7660 |
| ctrl-b / ctrl-h / ctrl-k / ctrl-space | panels/palette/autocomplete | None | main.rs:7661-7666 |
| enter | Newline | **None (unscoped!)** | sql_input.rs:121 |
| escape, tab | Escape, Tab | "SqlInput" | sql_input.rs:130-131 |
| editing keys (backspace, delete, arrows, clipboard, home/end) | — | "TextField" | connections_ui.rs:208-223 |
| editing keys + up/down | — | None | sql_input.rs:101-120 |
| up/down/enter/escape | Palette\* | "Palette" | palette.rs:349-352 |
| escape | TreeEscape | "SchemaTree" | schema_tree.rs:597 |
| ctrl-c, ctrl-f, enter, shift-enter | CopySelection, FindInResult, FindNext, FindPrev | "ResultGrid" | grid.rs:150-153 |

Notable: **no `tab`/`shift-tab` binding exists outside "SqlInput"** (and
SqlInput's Tab handler always propagates unless the autocomplete popup is
open — sql_input.rs:85-93). **No `delete` binding exists outside text
editing.** `enter` is claimed unscoped by `Newline`, but per §0.1 that only
matters where a `Newline` handler is on the focus path (the SQL editor) —
everywhere else it falls through, which is exactly what the palette
(`"Palette"`-scoped `enter → PaletteConfirm`, handled at
main.rs:3808/3526) and the find bar already exploit.

### 0.3 Modal/overlay inventory + focus-on-open behavior

Three overlay families on `AppView` (mutually exclusive by the
single-modal invariant — every opener checks
`self.modal.is_some() || self.apply_dialog.is_some() ||
self.discard_confirm.is_some()`, main.rs:1368, 2160, 2620, 3079, 5310)
plus per-widget overlays:

| Overlay | Inputs | Focus moved on open? | Primary action (Czech label → fn) |
|---|---|---|---|
| `ModalState::ConnectionDialog` | 13 `TextField`s (`ConnectionDialogUi`, connections_ui.rs:759-784) | yes → `name` (connections_ui.rs:1434) | „Uložit" → `on_save_clicked(window, cx)` (connections_ui.rs:2320/1683) |
| `ModalState::MasterPasswordPrompt` | 1 masked | yes (connections_ui.rs:1698/1819) | „Odemknout" → `on_master_password_submit` (2352/1911) |
| `ModalState::CreateMasterPassword` | 2 masked | yes → input1 (1709/1958) | „Vytvořit" → `on_create_master_password_submit(window, cx)` (2388/1937) |
| `ModalState::QueryParams` | N `TextField`s | yes → first (main.rs:1259-1270) | „Spustit" → `confirm_query_params` (connections_ui.rs:2490, main.rs:1290) |
| `ModalState::KillConfirm` | none | **no** | „Ukončit proces" → `confirm_kill_confirm` (2634/main.rs) |
| `ModalState::AnalyzeWriteConfirm` | none | **no** | confirm → `on_confirm_analyze_write` (2741, main.rs:3273) |
| `ModalState::CompareDialog` | none (two pickers) | **no** | „Spustit porovnání" → `confirm_compare_dialog` (2819, main.rs:1527) |
| `ModalState::BackupRestore` | 1 (`confirm_input`, Restore-Confirming only) | yes for Restore (main.rs:6907/6918); **no** for Backup | „Obnovit" → `confirm_restore` (2980, main.rs:6930) |
| `ModalState::ScriptRun` | none | **no** | „Spustit" → `confirm_script_run` (3257, main.rs:2351) |
| `ModalState::CsvImport` | none | **no** | „Spustit import" → `confirm_csv_import` (3366, main.rs:2863) |
| `ModalState::Settings` | none | **no** | „Zavřít" → `close_modal` (1353) |
| `ModalState::ChartPicker` | none | **no** | „Vytvořit graf/Použít" → `confirm_chart_picker` (3504, main.rs:4622) |
| `apply_dialog` (`ApplyDialogState`) | none | yes → its own `focus_handle` (main.rs:909, 6228) — **the precedent** | „Potvrdit a spustit" → `on_confirm_apply` (6266, main.rs:5398) |
| `discard_confirm` | none | no (deliberate — main.rs:6114-6117) | „Zahodit" → `on_discard_confirm_yes` / „Zrušit" → `_no` |
| `AdminModal::{NewRole, ChangePassword, NewSchema, DropSchema}` (admin_panel.rs:754-780, per-panel state, NOT `ModalState`) | NewRole 2, ChangePassword 1, NewSchema 1 | yes (admin_panel.rs:1161/1180) | „Vytvořit"/confirm fns (admin_panel.rs:1995/2027/2090) |
| palette | 1 | yes (main.rs:3506) | Enter already works (`PaletteConfirm`) |
| grid cell editor / find bar / filter row | 1 each | editor: yes (grid.rs:2066) | editor „Uložit"; find Enter works |

The **„no" rows are a real bug** the sweep surfaces (§5 item 8): `.occlude()`
blocks clicks, not keys, so while e.g. `KillConfirm` is open, keyboard focus
still sits on the SQL editor underneath and stray typing mutates it
invisibly. `ApplyDialogState.focus_handle` (main.rs:898-909) was added by a
G5 review fix for exactly this; the other seven no-input panels never got
the same treatment. Fixing this is also the *prerequisite* for Enter
routing (the dispatch path must include the modal panel).

### 0.4 Esc today (`AppView::on_cancel_query`, main.rs:3363-3464)

Priority chain: palette → dropdown → `modal` (closable match:
ConnectionDialog only with empty password; QueryParams / ScriptRun /
CsvImport / Settings / ChartPicker always; BackupRestore only when
`!is_running()`; `_ => false` for KillConfirm / AnalyzeWriteConfirm /
CompareDialog / MasterPasswordPrompt / CreateMasterPassword) →
discard_confirm („Zrušit" semantics) → apply_dialog (only `!running`) →
active grid `close_overlay_if_open()` (cell editor → detail → find,
grid.rs:1172-1189) → cancel running query. **Gap: no `TabContent::Admin`
arm** — Esc does nothing against an open `AdminModal` or the admin panel's
own discard-confirm (admin_panel.rs:2190). Sweep item 9.

### 0.5 Grid + sandbox facts (for §3/§4)

- Selection: `selection: Option<((row, col), (row, col))>` in DISPLAY
  coordinates (grid.rs:238), set by mouse only; mapped through
  `view.source_row` at use time.
- Grid root: `.key_context("ResultGrid").track_focus(&self.focus_handle)`
  + `on_action` list (grid.rs:2550-2560).
- Editable gate: `editable: Option<Editable>` (grid.rs:389) — `None` for
  ad-hoc tabs, read-only connections, MSSQL, PK-less tables (sandbox.rs:26-32),
  and it gates the „✕" gutter, „+ řádek", the cell editor, and the apply bar.
- Row space: `uniform_list` count = `view.len() + inserted_rows.len()`;
  display rows `>= view.len()` render from `edit_state.inserted_rows`
  (grid.rs:2585-2613) — **inserted rows always render at the end,
  unaffected by sort/filter/find** (they live outside `RowView`).
- Staging calls: real row „✕" → `toggle_row_delete(source_row)` →
  `EditState::toggle_delete` (reversible flag, sandbox.rs:81-85); inserted
  row „␡" → `remove_insert_row(ins_ix)` → `EditState::remove_insert_row`
  (permanent, `Vec::remove` shifts later indices — bounds-checked no-op,
  sandbox.rs:120-124); „+ řádek" → `add_insert_row` (grid.rs:1993-1997,
  click listener HAS the `window` param available, grid.rs:1649).
- Nothing touches the database until the Apply dialog's
  „Potvrdit a spustit"; „Zahodit" (`clear_edits`) drops all staged ops.

## 1. Enter potvrzuje modaly

### 1.1 Mechanism (grounded in §0.1)

New action + binding, following the palette/find-bar precedent exactly:

```rust
// main.rs
actions!(dbc, [ModalConfirm, ModalFocusNext, ModalFocusPrev]);
cx.bind_keys([
    KeyBinding::new("enter",     ModalConfirm,   Some("ModalForm")),
    KeyBinding::new("tab",       ModalFocusNext, Some("ModalForm")),
    KeyBinding::new("shift-tab", ModalFocusPrev, Some("ModalForm")),
]);
```

`connections_ui::render_modal_overlay`'s backdrop wrapper
(connections_ui.rs:1284-1298) gains
`.key_context("ModalForm").track_focus(&app.modal_focus_handle)` (new
field, §1.4) and `.on_action(cx.listener(AppView::on_modal_confirm))` +
the two focus actions. The admin panel's own modal overlay wrapper
(admin_panel.rs:2173-2188) gets the same `key_context` with
`AdminPanel`-local `on_action` handlers.

Resolution walk-through for a keypress of `enter` with focus inside a
modal `TextField`: `bindings_for_input` returns
`[Newline (unscoped, deepest), ModalConfirm ("ModalForm" ancestor)]`;
`Newline` dispatches first and finds **no handler** on the modal focus
path (the only `Newline` handler lives on `SqlInput`'s render node, a
sibling subtree), so dispatch falls through and `ModalConfirm` is handled
by the wrapper. Same mechanism `grid.rs:139-147` already documents for
`FindNext`. **Multiline exemption is therefore structural, not
special-cased:** if a dialog ever embeds a multiline `SqlInput`, its
`Newline` handler IS on the path, consumes Enter, and confirm never fires
— which is the wanted behavior.

Interaction with `RunQuery`: `ctrl-enter` is a different keystroke — no
contention. Additionally `run_query`/`run_query_with` already refuse while
`self.modal.is_some()` (main.rs:3263-3266), so even the global binding
can't act behind a modal. No change needed.

### 1.2 Per-dialog decision table (complete; no TBDs)

Policy (the rule, so future dialogs don't re-litigate): **Enter is allowed
where confirm (a) runs nothing against the database, or (b) resumes an
already-expressed run intent (QueryParams interrupted a Ctrl+Enter), or
(c) leads only into a further explicit confirmation gate. Enter is
ignored where the button is the LAST gate before an immediate
write/kill/restore/batch dispatch.**

| Dialog | Enter → | Rationale |
|---|---|---|
| ConnectionDialog | `on_save_clicked(window, cx)` | Saves config only (may chain into the master-password modal — the handler has `window`, required by that path, connections_ui.rs:1698). Masked password field included — Enter in „Heslo" saves, per the requirement. |
| MasterPasswordPrompt | `on_master_password_submit` | The canonical password+Enter case. Wrong password already stays in-modal with error (1920-1926). |
| CreateMasterPassword | `on_create_master_password_submit(window, cx)` | Mismatch/empty already handled in-modal. |
| QueryParams | `confirm_query_params` | Resumes the interrupted Ctrl+Enter; `build_param_sql` failure stays in-modal (main.rs:1290+). **This is requirement 1's headline case.** |
| KillConfirm | **ignored** (explicit no-op arm) | The modal IS the kill's confirmation gate; `dispatched` double-click guard stays click-only. |
| AnalyzeWriteConfirm | **ignored** | Executes the write (rolled back, but still executed). |
| CompareDialog | `confirm_compare_dialog` | Read-only compare; fn is a structural no-op until both sides are picked (unit test connections_ui.rs:3100 proves the guard). |
| BackupRestore (all states) | **ignored** | Restore is the most destructive action in the app; the typed-name gate must end in a deliberate click. Backup-kind and Running/terminal states have no primary to trigger anyway. |
| ScriptRun | **ignored** | Dispatches an arbitrary-SQL batch immediately. |
| CsvImport | **ignored** | Dispatches a bulk write immediately. |
| Settings | `close_modal` | „Zavřít" is the only action; Enter=Esc here. |
| ChartPicker | `confirm_chart_picker` | Read-only; fn self-guards (`y_selected` check, main.rs:4623-4632). |
| apply_dialog | **ignored** (no `"ModalForm"` context added — Enter is structurally inert, see §1.3) | Last gate before the actual write transaction. |
| discard_confirm | **ignored** (same, no context added) | Enter must never be the thing that destroys staged edits — mirror of the G5 Esc rule (main.rs:3423-3426). |
| AdminModal::NewRole | admin `confirm_new_role` | Stages a `WriteStatement`; execution still goes through the apply bar → apply dialog gate. |
| AdminModal::ChangePassword | admin confirm fn | Same — staged, gated. |
| AdminModal::NewSchema | admin confirm fn | Emits `RequestApply` → opens the apply dialog (a second explicit gate, admin_panel.rs:767-771). |
| AdminModal::DropSchema | **ignored** | Destructive intent; even though it also routes via the apply dialog, the pause is the point (CASCADE warning dialog). |

Implementation shape: a pure decision fn
`modal_confirm_kind(&ModalState) -> ModalConfirmKind` (enum:
`SaveConnection | UnlockVault | CreateVault | RunParams | Compare |
CloseSettings | ChartConfirm | Ignore`) unit-tested table-style like
`kill_confirm_tests` (connections_ui.rs:3512), with
`AppView::on_modal_confirm` matching on it and dispatching to the
existing fns. Admin gets its own small match in `AdminPanel`.

The **Ignore arms are explicit handled no-ops** (the listener runs and
propagation stops) — deliberate, so Enter against e.g. KillConfirm does
nothing at all rather than falling through to some other binding.

### 1.3 Where `"ModalForm"` is and is NOT added

Added: `render_modal_overlay` wrapper (covers all 13 `ModalState` panels)
and the admin modal wrapper. NOT added: apply dialog and discard-confirm
overlays — with no `"ModalForm"` context in their focus path, `enter`
resolves to `Newline` only, which has no handler there (apply dialog focus
sits on `ad.focus_handle`, main.rs:6228; discard-confirm — see §1.4),
so Enter is dead by construction, no code needed. Tab likewise no-ops.

### 1.4 Focus prerequisite — fixing the no-input panels (sweep item 8)

New `AppView` fields: `modal_focus_handle: FocusHandle` (created once in
`AppView` construction) + `modal_needs_focus: bool`.

- The `render_modal_overlay` wrapper always `.track_focus(&modal_focus_handle)`.
- Every **no-input** `ModalState` opener that has `&mut Window` calls
  `window.focus(&self.modal_focus_handle, cx)` in the same update
  (the exact `ApplyDialogState` precedent, main.rs:898-909). Openers
  without `Window` (the `KillConfirm` path via `on_monitor_view_event`'s
  subscribe callback — same limitation `open_admin_apply_dialog`
  documents) set `modal_needs_focus = true` instead; `AppView::render`
  (which does have `&mut Window`) consumes the flag and focuses.
- Input-owning modals keep focusing their first field (unchanged);
  BackupRestore's Backup-kind (no input) joins the no-input treatment.
- discard_confirm gets the same treatment via `modal_focus_handle` — it
  currently leaves focus on the editor too (main.rs:6114-6117 documents
  „no window.focus call is needed" — that was true for Esc-only handling,
  but stray printable keys still reach the editor; fold the fix in).

Result: while ANY modal/overlay is up, keyboard focus is provably inside
it — stray keystrokes become inert, and Enter/Tab resolve against the
right context. Esc is unaffected (unscoped `CancelQuery` handled on the
AppView root, an ancestor of every wrapper — current behavior, verified
main.rs:3371-3378).

## 2. Tab / Shift+Tab mezi inputy

### 2.1 Mechanism — decision: native `TabStopMap`, NOT an explicit `Vec<FocusHandle>`

The pinned rev has first-class support (§0.1): flag the handle, get
ordering + wrap-around + skip-non-stops from `TabStopMap` for free, and
`window.focus_next/focus_prev` are one-liners. An explicit per-dialog
`Vec<FocusHandle>` would duplicate ordering state that must be kept in
sync with conditional rendering (the SSH block!) — rejected.

- `TextField` gains a second constructor
  `TextField::form_field(cx, placeholder, masked)` = `new()` +
  `focus_handle = focus_handle.tab_stop(true)` (tab_index stays 0).
  Only **modal form fields** use it: the 13 `ConnectionDialogUi` fields
  (connections_ui.rs:1374-1386), master-password inputs (1691, 1700-1701,
  1811, 1955), QueryParams inputs (main.rs:1253-1257), the restore
  `confirm_input` (main.rs:6902), and the admin modal fields
  (admin_panel.rs:1159-1160, 1179, 1264).
- Everything else stays `new()` (non-stop): grid filter row, find bar,
  history search, palette input, cell editor. **This is the leak guard:**
  `TabStopMap` is window-global and the app underneath a modal keeps
  painting, so if those were stops, Tab inside a dialog would escape into
  the grid's filter inputs. With only modal fields flagged, and the
  single-modal invariant (§0.3) guaranteeing at most one `ModalState`
  overlay paints at a time, the map contains exactly the open dialog's
  fields.
- **Order lives in paint order** (all `tab_index` 0 → `TabStopMap` falls
  back to insertion order, which is paint order): for ConnectionDialog
  that is Název → Host → Port → Databáze → Uživatel → Heslo → Složka →
  Timeout → Auto-limit → (SSH host → SSH port → SSH uživatel → SSH klíč,
  only while `ssh_enabled` — conditional fields drop out of the cycle
  automatically because they simply aren't painted). CreateMasterPassword:
  input1 → input2. QueryParams: `names` order. No separate order table to
  maintain — the visual form order IS the tab order by construction.
- Handlers: `on_modal_focus_next/prev` on the same `"ModalForm"` wrappers
  call `window.focus_next(cx)` / `window.focus_prev(cx)`. Wrap-around is
  `TabStopMap`'s documented behavior (§0.1). Shift+Tab = `focus_prev`.
- Skip-disabled: vacuous today — no dialog disables a `TextField` (only
  buttons get disabled states); noted so a future disabled-input feature
  knows to clear `tab_stop`.
- Buttons/checkboxes/engine-cycle are plain divs (not focusable) and stay
  out of the cycle in v1 — Tab cycles text inputs; buttons remain
  mouse/Enter targets. Extending stops to buttons is a possible v2, not
  now (would need a focusable-button component that doesn't exist).
- Dialogs with no inputs: map has no stops → `focus_next` is a no-op
  (`TabStopMap::next` returns `None`) — safe by construction.
- Tab resolution note: `"TextField"` binds no `tab`, `"SqlInput"`'s
  scoped `tab` is not on a modal's focus path, and no unscoped `tab`
  exists (§0.2) — so the `"ModalForm"`-scoped binding matches directly,
  no fall-through needed.

### 2.2 Known edge (accepted, documented)

The admin panel's modal is per-tab state, not `ModalState`, so the
single-modal invariant does not forbid e.g. opening the ConnectionDialog
(top bar is NOT occluded by the admin modal's tab-area overlay) while an
`AdminModal` is open. Both dialogs' fields would then be stops and Tab
would traverse the union. Severity: cosmetic, needs two stacked modal
families; not worth scoping machinery in v1 — recorded in §8 risks.

## 3. Delete = staged row delete v editovatelném gridu

### 3.1 Binding + contention

`actions!(grid, [.., DeleteRow])`;
`KeyBinding::new("delete", DeleteRow, Some("ResultGrid"))` in
`grid::bind_keys`, handler `.on_action(cx.listener(Self::on_delete_row))`
on the grid root (grid.rs:2550-2560). Contention analysis: when any
`TextField` inside the grid (filter row, find bar, cell editor) is
focused, the `"TextField"`-scoped `delete` (connections_ui.rs:209)
matches DEEPER and its handler consumes the key — forward-delete in text
inputs is untouched. When the grid body itself is focused (click on a
cell focuses the grid root via `track_focus`), `DeleteRow` fires.

### 3.2 Guard chain (in order, each a silent no-op)

1. `self.cell_editor.is_none() && self.cell_detail.is_none()` — belt only
   (those overlays hold focus in their own TextField/popup anyway), but
   structural: Delete must never act „through" an open editor.
2. `self.editable.is_some()` — this single check covers ad-hoc tabs,
   read-only connections, MSSQL, and PK-less tables (§0.5, sandbox.rs:26-32).
3. `self.selection.is_some()` — otherwise nothing to delete.

### 3.3 Semantics — decision: exact gutter equivalence, per selected row

Pure fn (unit-tested, no GPUI):

```rust
/// display-row span of `selection` split into real-row toggles and
/// inserted-row removals (descending, so Vec::remove indices stay valid).
fn delete_targets(
    sel: ((usize, usize), (usize, usize)),
    view_len: usize,
    inserted_len: usize,
) -> (Vec<usize> /* display rows < view_len */, Vec<usize> /* ins_ix desc */)
```

`on_delete_row` normalizes the anchor/focus row span, then for each real
display row calls `toggle_row_delete(view.source_row(r))` (grid.rs:1974)
— i.e. **toggle semantics, identical to clicking each row's „✕" once**:
pressing Delete again un-stages, matching the reversible-flag model. For
inserted display rows it calls `remove_insert_row(ins_ix)` in descending
order (identical to „␡"; permanent removal is the only way to un-stage an
insert, sandbox.rs:112-119). If any insert was removed, `selection` is
cleared (display indices past `view_len` may now dangle); for pure
toggles it is kept (indices remain valid — deletion is a flag, rows don't
move, brief-contract #6 keying by SOURCE row).

Undo story: unchanged from G5 — re-press Delete / click „✕" to un-toggle
a real row; „Zahodit" on the apply bar (or a failed identity check)
discards everything; nothing reaches the database before the Apply
dialog. Delete stages exactly what the mouse affordance stages — zero new
SQL surface.

## 4. „+ řádek" → scroll na nový řádek + fokus do první buňky

`add_insert_row` (grid.rs:1993) gains `window: &mut Window` (the „+ řádek"
click listener already receives it, grid.rs:1649) and after
`edit_state.add_insert_row(ncols)` (which returns the new `ins_ix`,
sandbox.rs:89-92):

1. **Scroll:** `let display_ix = self.view.len() + ins_ix;`
   `self.scroll_handle.scroll_to_item(display_ix, ScrollStrategy::Center);`
   — same API + strategy as the two existing find-scroll call sites
   (grid.rs:1105/1153; Center clamps to bottom for a last item, §0.1).
   Filters/sort/cap interplay is a non-issue by construction: inserted
   rows render appended AFTER the filtered `view` (§0.5), so the target
   index is always valid and always the visually-last row, regardless of
   active filters or a capped/spilled buffer (`view.len()` is whatever
   the current view holds; the sum indexes the uniform_list correctly,
   grid.rs:2585-2613).
2. **Focus:** open the cell editor on the new row's first VISIBLE column:
   `first_col = (0..ncols).find(|&c| !self.hidden_cols[c])` (virtual FK
   columns sit past `ncols` and are never editable; if ALL source columns
   are hidden — possible via „Sloupce ▾" — skip this step, scroll only).
   Then `self.open_cell_editor(EditTarget::Insert { ins_ix, col: first_col },
   column_name, String::new(), window, cx)` — `open_cell_editor` already
   focuses its `TextField` in the same update (grid.rs:2061-2067), so
   „+ řádek" becomes: click → new row visible → caret ready → type →
   Enter-adjacent flow („Uložit" click or Esc; the editor's own buttons
   are unchanged). This satisfies the „ideálně first editable cell
   focused" wish with an existing, staging-safe component instead of
   inventing inline cell focus. Risk that auto-opening annoys rapid
   multi-row adding is flagged in §8 — the fallback is deleting this one
   call, scroll stays.

## 5. Sweep — nalezené mezery a rozhodnutí

| # | Gap | Decision |
|---|---|---|
| 1 | Enter in palette | Already works (`"Palette"` enter → `PaletteConfirm`, palette.rs:351 / main.rs:3526). No change. |
| 2 | Enter/arrows in autocomplete popup | Already works — SqlInput propagates Up/Down/Newline while `autocomplete_active` so AppView's handlers accept/navigate (sql_input.rs:331-334, 553-567, 615-619). No change. |
| 3 | Enter / Shift+Enter in find bar | Already works via binding fall-through (grid.rs:139-153). No change. |
| 4 | Enter confirm in modals | **Fix now** — §1 (headline requirement). |
| 5 | Tab/Shift+Tab in modals | **Fix now** — §2 (headline requirement). |
| 6 | Delete stages row delete | **Fix now** — §3 (headline requirement). |
| 7 | Scroll+focus after „+ řádek" | **Fix now** — §4 (headline requirement). |
| 8 | Stray keystrokes land in the SQL editor under every no-input modal (KillConfirm, AnalyzeWriteConfirm, CompareDialog, ScriptRun, CsvImport, Settings, ChartPicker, Backup-kind BackupRestore, discard_confirm) — `.occlude()` blocks clicks only; the apply dialog fixed this for itself in G5 (main.rs:898-909), nothing else did | **Fix now** — §1.4 (also the Enter prerequisite). |
| 9 | Esc does not close `AdminModal` / the admin discard-confirm (`on_cancel_query` has no `TabContent::Admin` arm, §0.4) | **Fix now** — add an Admin arm before the grid arm: `view.update(cx, |p, cx| p.close_overlay_if_open(cx))` mirroring the grid's shape (admin discard-confirm first as „Zrušit", then modal), with the M6 password rule mirrored: `NewRole`/`ChangePassword` with a non-empty password field are NOT closable by Esc (same reasoning as ConnectionDialog, main.rs:3364-3370). |
| 10 | Esc coverage for the 13 `ModalState` variants | Already deliberate and complete (closable match, §0.4) — no change; §1's table deliberately mirrors its shape. |
| 11 | Arrow keys don't move the grid selection | **Out of scope** — keyboard cell-navigation (selection move + scroll-follow + Shift-extend) is its own feature with real interaction-design surface; Delete (§3) works off the existing mouse selection. Candidate for a future grid-navigation phase. |
| 12 | No keyboard navigation in the schema tree (only `TreeEscape`) | **Out of scope** — same reasoning as 11. |
| 13 | Connection dropdown is mouse-only (no arrows/Enter) | **Out of scope** — it's a popover menu, not a modal; the palette (Ctrl+K → connections with full keyboard flow) is the designated keyboard path to switching connections. |
| 14 | Enter on discard-confirm could mean „Zahodit" | **Rejected deliberately** — Enter must never destroy staged edits (mirror of the G5 Esc rule); Enter stays inert there (§1.2/§1.3). |
| 15 | Enter on the apply dialog | **Rejected deliberately** — last gate before a real write; structurally inert (§1.3). |
| 16 | Tab in the SQL editor does nothing when the autocomplete popup is closed (no indent) | **Out of scope** — editor-behavior territory (G6 line), unrelated to dialog consistency; noted so it isn't „discovered" again. |
| 17 | Masked fields + Enter | Included by design — MasterPasswordPrompt/CreateMasterPassword are Enter-confirm rows in §1.2; masking rendering untouched. |

## 6. Security notes

- **Enter adds zero new authority.** `on_modal_confirm` dispatches the
  IDENTICAL fns the buttons call, and every one is self-guarding at the
  top of its body — verified per fn: `confirm_query_params` re-runs
  `build_param_sql`'s post-substitution rescan (main.rs:1290+);
  `confirm_compare_dialog` no-ops until both picked (test
  connections_ui.rs:3100); `confirm_chart_picker` validates `y_selected`
  (main.rs:4623); `on_save_clicked` routes through the vault flow
  (connections_ui.rs:1683-1711) and `finish_save`'s corrupt-config guard
  (1728-1743); admin confirms stage `WriteStatement`s that still pass the
  apply dialog. The dialogs whose confirm fns dispatch writes/kills
  (`confirm_kill_confirm`, `on_confirm_analyze_write`,
  `confirm_script_run` + its `conn_identity`/`cancel` guards
  (main.rs:2351-2373), `confirm_csv_import` + identity re-check
  (2863-2885), `confirm_restore` + typed-name/identity guards
  (6930-6948), `on_confirm_apply` + identity re-check (5398-5418)) are
  all **Enter-ignored** anyway — belt on top of their own belts.
- Enter-ignored arms are handled no-ops: propagation stops, so the
  keystroke cannot fall through to any other binding.
- Focus fix (§1.4) is strictly guard-tightening: it removes the existing
  „type into the editor behind a modal" hole; it cannot loosen anything
  (Esc still resolves via the unscoped root binding).
- Masked `TextField` behavior is untouched — no new code reads or logs
  field text; `ConnectionFormData`'s redacted `Debug`
  (connections_ui.rs:847-865) is unaffected; the Esc
  password-nonempty-not-closable rule is EXTENDED to admin modals (§5#9),
  never weakened.
- Delete (§3) stages sandbox ops only — the same `EditState` mutations the
  mouse gutter performs; SQL generation/execution still happens solely
  behind the Apply dialog's existing identity/read-only gates. Read-only
  connections are structurally excluded (`editable == None`).

## 7. Tests (honest split)

Unit-testable (pure, no GPUI — same tier as `kill_confirm_tests` /
`sandbox` tests):

- `modal_confirm_kind` decision table: one assertion per `ModalState`
  variant (13), including every `Ignore` arm — this pins §1.2 as code.
- `delete_targets`: empty/None selection → empty; single row; reversed
  anchor/focus span; span straddling `view_len` (real + inserted split);
  inserted indices returned descending; all-inserted span.
- „+ řádek" display index: `view_len + ins_ix` for fresh and repeated
  adds (trivial, folded into the delete_targets module).
- `first_visible_col(hidden_cols)`: none hidden, some hidden, all hidden
  → `None` (drives §4's skip).

NOT unit-tested (needs the visual pass — the codebase has no GPUI test
harness today; the pinned rev does ship `TestAppContext` with
`focus_next` coverage (div.rs:5122-5157), so a headless focus-traversal
test is *possible* later, but standing up the first harness is out of
proportion for this phase — recorded as an option, not a task):

- actual Tab/Shift+Tab traversal + wrap-around per dialog (checklist:
  ConnectionDialog with and without SSH enabled, CreateMasterPassword,
  QueryParams with 3 params, restore confirm, admin NewRole);
- Enter per §1.2 table (spot-check one Yes and one Ignore per family);
- focus-inertness: open KillConfirm, type — editor must not change;
- Delete key: toggle on/off, multi-row span, inserted-row removal,
  read-only tab no-op, no-op while cell editor open;
- „+ řádek" on a filtered + sorted preview scrolls to the new row and
  opens the editor on the first visible column;
- Esc closes AdminModal, refuses while its password field is non-empty.

## 8. Task decomposition + risks

Executes AFTER the tech-debt sweep merges (rebase point). Single UI
worktree; files are mostly serialized through `main.rs`/`connections_ui.rs`:

- **T1 — grid.rs only (parallel-safe with T2):** §3 Delete
  (`DeleteRow` action/binding/handler + `delete_targets` + tests) and §4
  („+ řádek" scroll + first-cell editor + `first_visible_col` + tests).
- **T2 — connections_ui.rs + main.rs (the serialized trunk):**
  `TextField::form_field`, `"ModalForm"` wrapper (context + track_focus +
  on_actions), `ModalConfirm`/`ModalFocusNext`/`ModalFocusPrev` actions +
  bindings, `modal_confirm_kind` + `on_modal_confirm`,
  `modal_focus_handle`/`modal_needs_focus` no-input-panel focus fix,
  opener updates to `form_field`, decision-table tests.
- **T3 — admin_panel.rs + main.rs (after T2, small):** admin modal
  `"ModalForm"` adoption (Enter per §1.2, Tab via `form_field` fields),
  `TabContent::Admin` Esc arm with the password rule (§5#9).
- **T4 — closing serial step:** the §7 visual checklist pass, version
  bump per the merge checklist.

Risks / needs-verification:

- **Fall-through dependency:** §1's Enter relies on `Newline` staying
  unhandled on modal paths. A future modal that embeds `SqlInput` loses
  Enter-confirm silently — by design (multiline exemption), but T2 must
  leave a comment on the binding pointing here, same discipline as
  grid.rs:139-147.
- **Tab-stop convention is a grep-able invariant:** stops enter the map
  ONLY via `TextField::form_field`. A future `.tab_index(..)`/`form_field`
  on a non-modal field would leak background fields into a dialog's Tab
  cycle (§2.1). Merge gate: grep audit for `tab_stop(true)|form_field(`
  call sites, all must be modal openers.
- **Stacked admin+app modal Tab union (§2.2)** — accepted cosmetic edge;
  revisit only if it's actually hit.
- **Auto-opening the cell editor after „+ řádek" (§4)** may prove
  annoying when adding many blank rows in a row — verify in the T4 visual
  pass; fallback is removing one call (scroll stays).
- **`modal_needs_focus` deferred focus (§1.4)** — verify in T4 there's no
  one-frame flicker where a keystroke can still reach the editor between
  the subscribe-path open and the next render (expected fine: keystrokes
  can't arrive between the two phases of one update cycle, but confirm
  against the real KillConfirm flow).
- **`ScrollStrategy::Center` on the last item** relies on the documented
  clamp behavior (uniform_list.rs:87-90) — if the visual pass shows the
  new row hugging an odd position, switch to `Bottom` (one enum change).
