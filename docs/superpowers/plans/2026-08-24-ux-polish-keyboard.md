# UX Polish — Keyboard & Interaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enter confirms modal dialogs (where the binding policy table allows it), Tab/Shift+Tab cycles a dialog's inputs via GPUI's native `TabStopMap`, Delete stages row deletion in an editable results grid, „+ řádek" scrolls to the new row and opens the cell editor on its first visible column — plus the two sweep fixes (shared `modal_focus_handle` so no-input modals actually hold keyboard focus, and Esc closing `AdminModal`).

**Architecture:** Zero new dependencies and zero new components. Enter/Tab ride the pinned GPUI rev's existing machinery: a `"ModalForm"` key context on the two modal backdrop wrappers (app-level `render_modal_overlay` in connections_ui.rs, admin-level in admin_panel.rs) with scoped `ModalConfirm`/`ModalFocusNext`/`ModalFocusPrev` actions that resolve by the same binding-fallthrough mechanism the find bar already documents (grid.rs:139-147); tab stops come from a new `TextField::form_field` constructor that flags the handle `tab_stop(true)`, so ordering/wrap-around are `TabStopMap`'s for free. Delete is a `"ResultGrid"`-scoped action behind a three-step guard chain that reuses the existing gutter staging fns. All decision logic is pulled into pure, unit-tested fns (`modal_confirm_kind`, `delete_targets`, `first_visible_col`, `admin_esc_closable`); traversal/focus/scroll behavior goes to the manual visual checklist (§ at the end of this plan) because the codebase has no GPUI test harness.

**Tech Stack:** Rust, GPUI (git-pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — `FocusHandle::tab_stop` verified pub at window.rs:583, `Window::focus_next/focus_prev` at window.rs:2093/2104, `TabStopMap` wrap-around per its own `test_tab_handles`), existing `dbc-ui` widgets only.

**Spec:** `docs/superpowers/specs/drafts/ux-polish-keyboard-design.md` — curated, its decisions are **binding** (the §1.2 Enter policy table including every explicit Ignore arm, native `TabStopMap` over an explicit `Vec<FocusHandle>`, exact-gutter-equivalence Delete semantics, `scroll_to_item(.., Center)` + auto-open cell editor, the §5 17-item sweep decision list).

## Global Constraints

- cargo is ALWAYS invoked as `%USERPROFILE%\.cargo\bin\cargo.exe` with `-p <crate>`; zero warnings allowed in both `build -p` and `test -p` (test build included).
- GPUI is git-pinned rev 907ed09 — no dependency changes of any kind this phase.
- **§3-novela (Enter edition): Enter must NEVER be wired to any last-gate-before-write dialog.** Per the design's §1.2 policy table the Enter-ignored list is, verbatim and exhaustively: **KillConfirm, AnalyzeWriteConfirm, BackupRestore (all states), ScriptRun, CsvImport, apply_dialog, discard_confirm, AdminModal::DropSchema.** The first five get explicit handled no-op `Ignore` arms; apply_dialog and discard_confirm get NO `"ModalForm"` context at all (Enter structurally inert, §1.3); DropSchema gets an explicit no-op arm in the admin match. Any task that drifts from this list is wrong by definition.
- The read-only/identity/cancel guards live INSIDE the existing confirm fns (`confirm_query_params`' `build_param_sql` rescan, `confirm_compare_dialog`'s both-picked guard, `confirm_chart_picker`'s `y_selected` check, `on_save_clicked`'s vault flow, admin confirms' staging-then-apply-dialog gate) — `on_modal_confirm` dispatches the IDENTICAL fns the buttons call and must not bypass, duplicate, or "helpfully" pre-check any of them.
- Single-modal invariant: every `ModalState` opener already guards `self.modal.is_some()` (plus `apply_dialog`/`discard_confirm` where applicable) — nothing in this phase may open a second overlay or weaken an existing guard.
- Deep-recursion hazard class: iterative build/drop/flatten for tree-like structures; no derived Clone/Debug/PartialEq on deep node types; never assert_eq! full deep trees. (Nothing in this phase builds a tree — stated so reviewers can reject any drift.)
- `runner.rs` is untouched this phase — no task may modify it.
- `main.rs` and `connections_ui.rs` are serialized single-writer files: Tasks 3 → 4 → 5 → 6 form a strict chain (each touches one or both); only Tasks 1–2 (grid.rs) run parallel to that chain, and Tasks 1→2 are themselves serialized against each other (same file).
- UI strings are Czech (labels, statuses, tooltips); English only in code/comments/tests.
- Line references are against this branch at commit `f87b2be` (v0.15.0 + the committed design). **Re-locate by symbol name, not line number**, if anything merges underneath.

## The Enter policy table (binding — copied from design §1.2, restated so no task re-litigates)

| Dialog | Enter → |
|---|---|
| ConnectionDialog | `on_save_clicked(window, cx)` |
| MasterPasswordPrompt | `on_master_password_submit(window, cx)` |
| CreateMasterPassword | `on_create_master_password_submit(window, cx)` |
| QueryParams | `confirm_query_params(cx)` (the headline case) |
| KillConfirm | **ignored** (explicit no-op arm) |
| AnalyzeWriteConfirm | **ignored** |
| CompareDialog | `confirm_compare_dialog(cx)` (self-guarding until both sides picked) |
| BackupRestore (all states) | **ignored** |
| ScriptRun | **ignored** |
| CsvImport | **ignored** |
| Settings | `close_modal(cx)` (Enter=Esc, „Zavřít" is the only action) |
| ChartPicker | `confirm_chart_picker(cx)` (self-guards `y_selected`) |
| apply_dialog | **ignored** — no `"ModalForm"` context added, structurally inert |
| discard_confirm | **ignored** — same, Enter must never destroy staged edits |
| AdminModal::NewRole | `confirm_new_role(cx)` (stages only; apply dialog still gates execution) |
| AdminModal::ChangePassword | `confirm_change_password(cx)` (same) |
| AdminModal::NewSchema | `confirm_new_schema(cx)` (emits `RequestApply` → apply dialog, a second gate) |
| AdminModal::DropSchema | **ignored** (the pause is the point — CASCADE warning) |

Ignore arms are **handled no-ops**: the listener runs, GPUI stops propagation (on_action consumes unless `cx.propagate()` is called), so the keystroke cannot fall through to any other binding.

## Grounding corrections (design vs. the actual code on this branch — all verified at `f87b2be`)

1. **Line drift after the tech-debt merge.** Current anchors: `render_modal_overlay` connections_ui.rs:1267 (backdrop wrapper 1329-1343), `ModalState` enum connections_ui.rs:941, `close_modal` 1483, `on_save_clicked` 1753, `on_master_password_submit` 2015, `on_create_master_password_submit` 2041, `confirm_kill_confirm` 2109, `confirm_compare_dialog` connections_ui.rs:1572 (design said main.rs — it lives in connections_ui.rs's `impl AppView`), `kill_confirm_tests` 3666; main.rs: `AppView` struct 972, ctor field block 7855-7890, `on_cancel_query` 3467, `Render for AppView` 7354 (overlay appends 7539-7561), `open_query_params_dialog` 1235, `run_explain` 3170, ScriptRun opener 2378, CsvImport opener 2859, `open_chart_picker` 4660 (+re-pick 4823), KillConfirm opener 5332, `start_backup_session` 6483, `begin_restore_confirm` 6962, discard-confirm construction sites 4606 & 4892, `render_discard_confirm_overlay` 6245; grid.rs: `actions!` 50, `bind_keys` 148, root 2550-2560, `toggle_row_delete` 1974, `remove_insert_row` 1984, `add_insert_row` 1993 (click site 1649), `open_cell_editor` 2025, `close_overlay_if_open` 1172, `selection` 238, `hidden_cols` 248, `editable` 389, uniform_list count 2581-2588; admin_panel.rs: `AdminModal` 754, field ctors 1159/1160/1179/1264, confirm fns 1209/1236/1295/1316, modal wrapper 2173-2188, discard overlay 2190, `Render` 2267.
2. **Every no-input modal opener lacks `&mut Window`.** The design offered "openers that have `&mut Window` call `window.focus` directly"; in current code that set is EMPTY — `run_explain`, `open_settings`, `open_compare_dialog`, `open_chart_picker`, the ScriptRun/CsvImport post-pick continuations, the KillConfirm subscribe arm, `start_backup_session`, and both discard-confirm sites are all `cx`-only. So the `modal_needs_focus` flag path (design §1.4's fallback) is used **uniformly** for all of them, consumed in `AppView::render` (which has `window`).
3. **Actions live in connections_ui.rs, not `main.rs`'s `actions!(dbc, ..)`.** The design's §1.1 snippet is a sketch; its own stated precedent ("following the palette/find-bar precedent exactly") is a module-owned `actions!` + `bind_keys` (palette.rs:345-352, grid.rs:148-155). `ModalConfirm`/`ModalFocusNext`/`ModalFocusPrev` therefore go in connections_ui.rs (which owns `ModalState`, the wrapper, and already has an `actions!`/`bind_keys` pair) — and `actions!` makes them `pub`, so admin_panel.rs can bind handlers on the same action types. Cross-module dispatch works because connections_ui.rs is a child module of the crate root (main.rs): its `impl AppView` code can call main.rs's private `confirm_query_params`/`confirm_chart_picker`/`close_modal` (root-private items are visible to descendant modules — the file already does exactly this).
4. **`modal_confirm_kind` cannot be unit-tested for all 13 variants.** `ConnectionDialog`, `MasterPasswordPrompt`, and `CreateMasterPassword` embed `Entity<TextField>` handles that cannot be constructed in a plain `#[test]` (no GPUI context; the repo has no `TestAppContext` harness — design §7 records standing one up as out of proportion). The table test covers the 10 constructible variants (`QueryParams` with empty `inputs`, `BackupRestore` with `confirm_input: None`, `ChartPicker` over an empty `ResultBuffer`, plus the 7 plain-data ones); the 3 Entity-bearing arms are pinned by the match being total (compile error on a new variant) and by the manual checklist's Enter spot-checks.
5. **`AdminModal::DropSchema`'s opener focuses nothing** (admin_panel.rs:1274-1281 — the design's §0.3 table marked all AdminModal rows "yes"). Folded into Task 6 via a new `AdminPanel::modal_focus_handle` (the same §1.4 mechanism, panel-local): `open_drop_schema_modal` gains `window` (its only call site is a click listener that has it) and focuses the handle, closing the same stray-keystroke hole item 8 fixes app-side. The admin discard-confirm overlay gets the same `track_focus` so Esc-consumption and inertness hold there too.
6. **`add_insert_row` signature change is safe:** the only caller is the „+ řádek" click listener (grid.rs:1649), which already receives `window` (currently discarded as `_`).
7. **`Window::focus_next(cx)`/`focus_prev(cx)` take `&mut App`** — passing `&mut Context<Self>` works via deref coercion (same as every existing `window.focus(&handle, cx)` call site).

## Task dependency graph

| Task | Deliverable | Files | Depends on | Ordering class |
|---|---|---|---|---|
| 1 | Delete = staged row delete (§3) | `crates/dbc-ui/src/grid.rs` | — | **parallel** with 3–6 (grid.rs only) |
| 2 | „+ řádek" scroll + first-cell editor (§4) | `crates/dbc-ui/src/grid.rs` | 1 (same file) | parallel with 3–6, serialized after 1 |
| 3 | Focus foundation: `form_field` ctor, `modal_focus_handle`/`modal_needs_focus`, no-input opener fix (sweep #8) | `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/main.rs` | — | **SERIALIZED trunk, first** |
| 4 | Enter routing: actions, `"ModalForm"` wrapper, `modal_confirm_kind` + tests, `on_modal_confirm` | `crates/dbc-ui/src/connections_ui.rs` | 3 | **SERIALIZED** |
| 5 | Tab stops: tab/shift-tab bindings + handlers, `form_field` adoption at every modal field ctor | `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/main.rs` | 3, 4 | **SERIALIZED** |
| 6 | Admin adoption: Enter/Tab on `AdminModal`, DropSchema focus, Esc Admin arm (sweep #9) | `crates/dbc-ui/src/admin_panel.rs`, `crates/dbc-ui/src/main.rs` | 4, 5 | **SERIALIZED** |
| 7 | Final audit: grep gates, full test pass, manual-checklist hand-off, version bump | root `Cargo.toml` | all | **SERIALIZED, last** |

**Execution order:** dispatch Task 1→2 as one worker's sequence (grid.rs) in parallel with the trunk chain 3→4→5→6; Task 7 last, alone. If running strictly inline, the order 3,4,5,1,2,6,7 (or 1,2,3,4,5,6,7) is equally valid — the only hard edges are 1→2, 3→4→5→6→7, and 2→7.

---

### Task 1: Delete stages row deletion in an editable grid (design §3)

**Files:**
- Modify: `crates/dbc-ui/src/grid.rs` (actions! at :50, `bind_keys` at :148, pure fn + tests near `lookup_generation_tests` at :2984, handler in `impl ResultGrid`, root `.on_action` list at :2557-2560)

**Interfaces:**
- Consumes: existing `EditState::toggle_delete(source_row)` / `EditState::remove_insert_row(ins_ix)` (sandbox.rs), `self.view.source_row(display_ix)`, `self.selection: Option<((usize, usize), (usize, usize))>` (display coords, anchor/focus), `self.editable: Option<Editable>`, `self.cell_editor`/`self.cell_detail`.
- Produces: `grid::DeleteRow` action bound to `delete` in `"ResultGrid"`; pure fn `delete_targets(sel: ((usize, usize), (usize, usize)), view_len: usize, inserted_len: usize) -> (Vec<usize>, Vec<usize>)` (real display rows ascending, inserted indices **descending**).

- [ ] **Step 1: Write the failing tests** — new module at the end of grid.rs, next to `lookup_generation_tests`:

```rust
#[cfg(test)]
mod delete_targets_tests {
    use super::delete_targets;

    #[test]
    fn single_real_row() {
        assert_eq!(delete_targets(((2, 0), (2, 3)), 10, 0), (vec![2], vec![]));
    }

    #[test]
    fn reversed_anchor_focus_span_normalizes() {
        assert_eq!(delete_targets(((5, 1), (3, 0)), 10, 0), (vec![3, 4, 5], vec![]));
    }

    #[test]
    fn span_straddling_view_len_splits_real_and_inserted() {
        // view_len 4, 3 inserted rows: display 2..=5 -> real [2,3], ins [0,1]
        let (real, ins) = delete_targets(((2, 0), (5, 0)), 4, 3);
        assert_eq!(real, vec![2, 3]);
        assert_eq!(ins, vec![1, 0]); // descending — Vec::remove-safe order
    }

    #[test]
    fn all_inserted_span_is_descending() {
        assert_eq!(delete_targets(((4, 0), (6, 0)), 4, 3), (vec![], vec![2, 1, 0]));
    }

    #[test]
    fn rows_past_the_insert_range_are_ignored() {
        // display rows 3..=9 but only view_len 2 + 1 insert exist
        assert_eq!(delete_targets(((0, 0), (9, 0)), 2, 1), (vec![0, 1], vec![0]));
    }

    #[test]
    fn insert_display_index_is_view_len_plus_ins_ix() {
        // §4's display-index arithmetic pinned here (design §7: folded in):
        // fresh add on view_len 4 -> display 4; second add -> display 5.
        assert_eq!(4 + 0, 4usize);
        assert_eq!(4 + 1, 5usize);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui delete_targets`
Expected: FAIL — `delete_targets` not found.

- [ ] **Step 3: Implement the pure fn** — place it as a free fn near the top-of-file helpers (after the `GridEvent` enum is fine):

```rust
/// UX-polish §3: display-row span of `selection` split into real-row
/// delete-toggles and inserted-row removals. Real rows come back ascending
/// (they're flag toggles — order irrelevant, ascending is just stable);
/// inserted indices come back DESCENDING so the caller's sequential
/// `EditState::remove_insert_row` calls (a `Vec::remove` each, which
/// shifts later indices — sandbox.rs) never invalidate the next index.
/// Display rows past `view_len + inserted_len` are ignored (a stale
/// selection can outlive a shrinking filter).
fn delete_targets(
    sel: ((usize, usize), (usize, usize)),
    view_len: usize,
    inserted_len: usize,
) -> (Vec<usize>, Vec<usize>) {
    let ((r1, _), (r2, _)) = sel;
    let (lo, hi) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
    let mut real = Vec::new();
    let mut ins = Vec::new();
    for r in lo..=hi {
        if r < view_len {
            real.push(r);
        } else if r < view_len + inserted_len {
            ins.push(r - view_len);
        }
    }
    ins.reverse();
    (real, ins)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui delete_targets`
Expected: PASS (6 tests).

- [ ] **Step 5: Action + binding.** Extend grid.rs:50 to

```rust
actions!(grid, [CopySelection, FindInResult, FindNext, FindPrev, DeleteRow]);
```

and add to `bind_keys` (grid.rs:149-154), with the contention note:

```rust
        // UX-polish §3: Delete stages row deletion (gutter equivalence).
        // Contention: while any TextField INSIDE the grid (filter row, find
        // bar, cell editor) is focused, the deeper "TextField"-scoped
        // `delete` binding (connections_ui.rs bind_keys) wins keymap
        // resolution and its handler consumes the key — forward-delete in
        // text inputs is untouched. This only fires when the grid body
        // itself holds focus (cell click focuses `self.focus_handle`).
        KeyBinding::new("delete", DeleteRow, Some("ResultGrid")),
```

- [ ] **Step 6: Handler + wiring.** In `impl ResultGrid`, next to `toggle_row_delete` (grid.rs:1974):

```rust
    /// UX-polish §3: Delete key on a selected row span — stages EXACTLY
    /// what the mouse gutter stages, per display row: real rows get the
    /// reversible `toggle_delete` flag (pressing Delete again un-stages,
    /// keyed by SOURCE row so sort/filter can't misdirect it — brief
    /// contract #6), inserted rows get `remove_insert_row` (permanent, the
    /// only way to un-stage an insert). Zero new SQL surface — nothing
    /// reaches the database before the Apply dialog, same as the gutter.
    fn on_delete_row(&mut self, _: &DeleteRow, _window: &mut Window, cx: &mut Context<Self>) {
        // Guard chain (design §3.2) — each a silent no-op:
        // 1. never act "through" an open editor/detail overlay (belt — those
        //    hold focus in their own TextField/popup anyway);
        if self.cell_editor.is_some() || self.cell_detail.is_some() {
            return;
        }
        // 2. `editable` covers ad-hoc tabs, read-only connections, MSSQL,
        //    and PK-less tables in one check (sandbox.rs `Editable` docs);
        if self.editable.is_none() {
            return;
        }
        // 3. nothing selected, nothing to delete.
        let Some(sel) = self.selection else { return };

        let (real_rows, ins_desc) =
            delete_targets(sel, self.view.len(), self.edit_state.inserted_rows.len());
        if real_rows.is_empty() && ins_desc.is_empty() {
            return;
        }
        for &r in &real_rows {
            let source_row = self.view.source_row(r);
            self.edit_state.toggle_delete(source_row);
        }
        for &ins_ix in &ins_desc {
            self.edit_state.remove_insert_row(ins_ix);
        }
        // Removing an insert shifts/kills display indices past `view_len`
        // — drop the selection rather than let it dangle. Pure toggles
        // keep it (deletion is a flag; rows don't move).
        if !ins_desc.is_empty() {
            self.selection = None;
        }
        cx.notify();
    }
```

Wire it on the grid root (grid.rs:2557-2560), after the existing four:

```rust
            .on_action(cx.listener(Self::on_delete_row));
```

- [ ] **Step 7: Build + full test, zero warnings**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` then `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: both clean, zero warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/dbc-ui/src/grid.rs
git commit -m "feat: Delete key stages row deletion in editable grids"
```

---

### Task 2: „+ řádek" scrolls to the new row and opens the cell editor (design §4)

**Files:**
- Modify: `crates/dbc-ui/src/grid.rs` (`add_insert_row` at :1993, its click site at :1649, pure fn + tests)

**Interfaces:**
- Consumes: `EditState::add_insert_row(cols) -> usize` (returns new `ins_ix`, sandbox.rs:89), `self.scroll_handle: UniformListScrollHandle`, `ScrollStrategy::Center` (already imported — used at grid.rs:1105/1153), `self.open_cell_editor(EditTarget::Insert { ins_ix, col }, column_name, original_text, window, cx)` (grid.rs:2025), `self.hidden_cols: Vec<bool>`.
- Produces: `add_insert_row(&mut self, window: &mut Window, cx: &mut Context<Self>)` (NEW signature — Task 7 audits no other caller appeared); pure fn `first_visible_col(ncols: usize, hidden_cols: &[bool]) -> Option<usize>`.

- [ ] **Step 1: Write the failing tests** — same test-module region as Task 1:

```rust
#[cfg(test)]
mod first_visible_col_tests {
    use super::first_visible_col;

    #[test]
    fn none_hidden_returns_col_zero() {
        assert_eq!(first_visible_col(3, &[false, false, false]), Some(0));
    }

    #[test]
    fn leading_hidden_cols_are_skipped() {
        assert_eq!(first_visible_col(3, &[true, true, false]), Some(2));
    }

    #[test]
    fn all_hidden_returns_none() {
        assert_eq!(first_visible_col(2, &[true, true]), None);
    }

    #[test]
    fn zero_cols_returns_none() {
        assert_eq!(first_visible_col(0, &[]), None);
    }
}
```

- [ ] **Step 2: Run to verify FAIL** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui first_visible_col` → `first_visible_col` not found.

- [ ] **Step 3: Implement the pure fn** (free fn next to `delete_targets`):

```rust
/// UX-polish §4: first SOURCE column not hidden via „Sloupce ▾" — the cell
/// the editor auto-opens on after „+ řádek". Virtual FK columns sit past
/// `ncols` and are never editable, so they're structurally out of range
/// here. `None` when every source column is hidden (possible via the
/// columns menu) — caller then scrolls only. Total over a short
/// `hidden_cols` (same defensive `.get().unwrap_or(false)` idiom the rest
/// of grid.rs uses).
fn first_visible_col(ncols: usize, hidden_cols: &[bool]) -> Option<usize> {
    (0..ncols).find(|&c| !hidden_cols.get(c).copied().unwrap_or(false))
}
```

- [ ] **Step 4: Run to verify PASS** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui first_visible_col` → 4 pass.

- [ ] **Step 5: Rewrite `add_insert_row` (grid.rs:1993-1997) and its call site.** New body:

```rust
    /// "+ řádek" toolbar click (editable tabs only, brief contract #4) —
    /// appends one blank insert row sized to the CURRENT result's column
    /// count, scrolls it into view, and opens the cell editor on its first
    /// visible column (UX-polish §4). Inserted rows always render appended
    /// AFTER the filtered `view` (see the uniform_list count in `render`),
    /// so `view.len() + ins_ix` is always a valid display index — the
    /// visually-last row regardless of active filters/sort/cap.
    /// ScrollStrategy::Center clamps "at the closest possible position"
    /// for a last item (uniform_list.rs) — if the visual pass shows an odd
    /// resting position, switch to `Bottom` (one enum change, design §8).
    fn add_insert_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ncols = self.buffer.as_ref().map_or(0, |b| b.borrow().column_count());
        let ins_ix = self.edit_state.add_insert_row(ncols);
        let display_ix = self.view.len() + ins_ix;
        self.scroll_handle.scroll_to_item(display_ix, ScrollStrategy::Center);
        // §4: auto-open the editor on the first visible column — skipped
        // (scroll only) when „Sloupce ▾" hid every source column. Risk that
        // auto-opening annoys rapid multi-row adding is a §8 flag: the
        // fallback is deleting exactly this if-block, scroll stays.
        if let Some(col) = first_visible_col(ncols, &self.hidden_cols) {
            let column_name = self
                .buffer
                .as_ref()
                .and_then(|b| b.borrow().schema().fields().get(col).map(|f| f.name().clone()))
                .unwrap_or_default();
            self.open_cell_editor(
                EditTarget::Insert { ins_ix, col },
                column_name,
                String::new(),
                window,
                cx,
            );
        }
        cx.notify();
    }
```

Call site (grid.rs:1649-1651) changes from `|this, _, _, cx| { this.add_insert_row(cx); }` to:

```rust
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_insert_row(window, cx);
                    })),
```

- [ ] **Step 6: Build + full test, zero warnings** — `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` and `test -p dbc-ui`.

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/src/grid.rs
git commit -m "feat: + radek scrolls to the new row and opens the cell editor"
```

---

### Task 3: Focus foundation — `form_field`, `modal_focus_handle`, no-input-modal focus fix (design §1.4, sweep #8)

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`TextField` impl at :265, `render_modal_overlay` backdrop at :1329-1343, `open_settings` :1496, `open_compare_dialog` :1509)
- Modify: `crates/dbc-ui/src/main.rs` (`AppView` struct :972-1115, ctor :7855-7890, `Render::render` top :7355, `run_explain` :3201-3208, ScriptRun continuation :2378, CsvImport continuation :2859, `open_chart_picker` :4710 and re-pick :4823, KillRequested :5332, `start_backup_session` :6483-6514, discard sites :4606 and :4892, `render_discard_confirm_overlay` :6245)

**Interfaces:**
- Consumes: `gpui::FocusHandle::tab_stop(bool) -> Self` (pinned rev, window.rs:583), the `ApplyDialogState.focus_handle` precedent (main.rs:898-909).
- Produces (later tasks rely on these exact names):
  - `TextField::form_field(cx: &mut Context<Self>, placeholder: impl Into<SharedString>, masked: bool) -> Self` (pub, connections_ui.rs) — Task 5/6 switch call sites to it.
  - `AppView.modal_focus_handle: gpui::FocusHandle` and `AppView.modal_needs_focus: bool` — Task 4 `.track_focus`es the handle's wrapper with `"ModalForm"`.

- [ ] **Step 1: Add `TextField::form_field`** right after `TextField::new` (connections_ui.rs:277):

```rust
    /// UX-polish §2: constructor for MODAL FORM fields ONLY — identical to
    /// `new` except the focus handle is flagged `tab_stop(true)`, entering
    /// the window-global `TabStopMap` so `Window::focus_next/focus_prev`
    /// (the "ModalForm" Tab/Shift+Tab bindings) traverse it in paint order
    /// with wrap-around. GREP INVARIANT (merge gate, design §8): every
    /// `form_field` call site must be a modal dialog opener. The app
    /// underneath a modal keeps painting, so a `form_field` on a non-modal
    /// input (grid filter row, find bar, history search, palette, cell
    /// editor — all deliberately `new`) would leak that background field
    /// into an open dialog's Tab cycle.
    pub fn form_field(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        masked: bool,
    ) -> Self {
        let mut field = Self::new(cx, placeholder, masked);
        field.focus_handle = field.focus_handle.clone().tab_stop(true);
        field
    }
```

- [ ] **Step 2: `AppView` fields.** In the struct (after `discard_confirm` at main.rs:1105 is a natural spot):

```rust
    // --- UX-polish §1.4: modal keyboard-focus plumbing ---
    /// Shared focus target for every overlay that owns no TextField of its
    /// own (KillConfirm, AnalyzeWriteConfirm, CompareDialog, ScriptRun,
    /// CsvImport, Settings, ChartPicker, Backup-kind/Running BackupRestore,
    /// and the discard-confirm prompt) — the `ApplyDialogState.focus_handle`
    /// G5 precedent generalized. `.occlude()` blocks clicks, not keys:
    /// without this, keyboard focus stays on the SQL editor underneath an
    /// open modal and stray typing mutates it invisibly (sweep item 8).
    /// Always `.track_focus`ed by `render_modal_overlay`'s backdrop wrapper
    /// and by `render_discard_confirm_overlay`.
    modal_focus_handle: gpui::FocusHandle,
    /// Set by modal/discard openers (all of which lack `&mut Window` —
    /// subscribe callbacks and cx-only helpers); consumed at the top of
    /// `AppView::render`, the first post-open point with Window access,
    /// which focuses `modal_focus_handle`. Input-owning modals never set
    /// it — they keep focusing their own first field at open time.
    modal_needs_focus: bool,
```

Ctor (main.rs:7885-7887, next to `apply_dialog: None,`):

```rust
                            modal_focus_handle: cx.focus_handle(),
                            modal_needs_focus: false,
```

- [ ] **Step 3: Consume the flag in `render`.** First lines of `AppView::render`'s body (main.rs:7355, before `refresh_autocomplete`):

```rust
        // UX-polish §1.4: deferred focus for overlay openers without a
        // `&mut Window` (see `modal_needs_focus`). Guarded: if the overlay
        // already closed again before this frame, just clear the flag.
        if self.modal_needs_focus {
            self.modal_needs_focus = false;
            if self.modal.is_some() || self.discard_confirm.is_some() {
                window.focus(&self.modal_focus_handle, cx);
            }
        }
```

- [ ] **Step 4: Track the handle on both app overlays.**
  - `render_modal_overlay` backdrop (connections_ui.rs:1330-1342): add `.track_focus(&self.modal_focus_handle)` to the wrapper div (before `.occlude()`; note the fn only takes `cx` — reading the handle off `self` is fine, no Window needed to track).
  - `render_discard_confirm_overlay` (main.rs:6245): add `.track_focus(&self.modal_focus_handle)` to ITS backdrop div. NO key context here or ever (Global Constraints: Enter stays structurally inert on discard-confirm). Update the stale comment at the discard-confirm open site that says "no window.focus call is needed" to point at this mechanism.

- [ ] **Step 5: Set the flag at every no-input opener** — one line, `self.modal_needs_focus = true;` (or `view.modal_needs_focus = true;` inside `this.update` closures), immediately after the `self.modal = Some(..)` / `self.discard_confirm = Some(..)` assignment:
  1. main.rs `run_explain`, `AnalyzeGate::NeedsConfirm` arm (:3202-3208).
  2. main.rs `start_script_pick` continuation (`view.modal = Some(ModalState::ScriptRun {..})`, :2378).
  3. main.rs `start_csv_import` continuation (:2859).
  4. main.rs `open_chart_picker` (:4710) AND the `on_chart_view_event` re-pick (:4823).
  5. main.rs `on_monitor_view_event` KillRequested arm (:5332-5340).
  6. main.rs `start_backup_session` (:6512): compute `let needs_focus = confirm_input.is_none();` BEFORE the session struct consumes `confirm_input`, then after `self.modal = Some(..)`: `if needs_focus { self.modal_needs_focus = true; }`. This covers Backup-kind (opens straight to Running, no input) and the Restore Confirming→Running re-session from `confirm_restore`; `begin_restore_confirm` passes `Some(input)` and keeps its existing direct `window.focus(&input_focus, cx)` (:7045) — unchanged.
  7. main.rs both discard-confirm construction sites (:4606, :4892).
  8. connections_ui.rs `open_settings` (:1500) and `open_compare_dialog` (:1517).

  Do NOT touch the input-owning openers (`open_connection_dialog`, `on_test_clicked`/`on_save_clicked`/dropdown master-password prompts, `open_query_params_dialog`, `begin_restore_confirm`) — they already focus their first field with a real `window` in the same update.

- [ ] **Step 6: Build + full test, zero warnings** — `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` and `test -p dbc-ui`. (`form_field` is not yet called anywhere — that's Task 5; a `pub fn` on a pub type is not dead code, so no warning.)

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/main.rs
git commit -m "feat: modal focus foundation - form_field ctor and no-input-modal focus fix"
```

---

### Task 4: Enter confirms modals — `"ModalForm"`, `modal_confirm_kind`, `on_modal_confirm` (design §1)

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`actions!` :225, `bind_keys` :233, `render_modal_overlay` backdrop, new fns + test module next to `kill_confirm_tests` :3666)

**Interfaces:**
- Consumes: `AppView.modal_focus_handle` (Task 3), the existing confirm fns exactly as named in the policy table, `ModalState` (:941).
- Produces: pub actions `ModalConfirm`, `ModalFocusNext`, `ModalFocusPrev` (namespace `modal_form`) — Task 5 binds tab/shift-tab to the latter two, Task 6 reuses all three in admin_panel.rs; `pub(crate) enum ModalConfirmKind`, `pub(crate) fn modal_confirm_kind(&ModalState) -> ModalConfirmKind`, `AppView::on_modal_confirm`.

- [ ] **Step 1: Actions + Enter binding.** After the existing `actions!(text_field, ..)` block (connections_ui.rs:228):

```rust
// UX-polish §1/§2: dialog-scoped keyboard actions, module-owned like
// palette.rs's "Palette" set. Bound under key context "ModalForm", carried
// by the two modal backdrop wrappers (this file's `render_modal_overlay`
// and admin_panel.rs's) — nowhere else, and DELIBERATELY not by the apply
// dialog or discard-confirm overlays (§1.3: with no "ModalForm" on their
// focus path, `enter` resolves to `Newline` only, which has no handler
// there, so Enter is dead by construction — the §3-novela list).
actions!(modal_form, [ModalConfirm, ModalFocusNext, ModalFocusPrev]);
```

And inside `bind_keys` (connections_ui.rs:234-251), append:

```rust
        // UX-polish §1: Enter-confirm. FALL-THROUGH DEPENDENCY (design §8,
        // same discipline as grid.rs's FindNext note): with focus inside a
        // modal TextField, `bindings_for_input` returns [Newline (unscoped),
        // ModalConfirm ("ModalForm" ancestor)]; Newline dispatches first,
        // finds no handler on the modal focus path (SqlInput is a sibling
        // subtree), and dispatch falls through to ModalConfirm. If a future
        // modal embeds a multiline SqlInput, its Newline handler IS on the
        // path and consumes Enter — the multiline exemption is structural,
        // not special-cased. Do not "fix" that.
        KeyBinding::new("enter", ModalConfirm, Some("ModalForm")),
```

- [ ] **Step 2: Write the failing decision-table tests** — new module next to `kill_confirm_tests`:

```rust
#[cfg(test)]
mod modal_confirm_kind_tests {
    use super::*;
    use crate::backup;
    use std::cell::RefCell;
    use std::rc::Rc;

    // ConnectionDialog / MasterPasswordPrompt / CreateMasterPassword embed
    // Entity<TextField> handles and cannot be constructed in a plain
    // #[test] (no GPUI context; grounding correction 4). Their arms are
    // pinned by `modal_confirm_kind`'s match being total (a new/changed
    // variant is a compile error) and Enter is spot-checked per family in
    // the manual visual checklist. Every other variant — including EVERY
    // §3-novela Ignore arm — is asserted here.

    fn query_params() -> ModalState {
        ModalState::QueryParams {
            names: Vec::new(),
            inputs: Vec::new(),
            null_flags: Vec::new(),
            sql_template: String::new(),
            bypass_auto_limit: false,
            error: None,
        }
    }

    fn kill_confirm() -> ModalState {
        ModalState::KillConfirm {
            pid: 1,
            label: "u · app · běží 1s".into(),
            sql: "SELECT pg_terminate_backend(1)".into(),
            tab_id: 1,
            error: None,
            dispatched: false,
        }
    }

    fn analyze_write() -> ModalState {
        ModalState::AnalyzeWriteConfirm {
            sql: "UPDATE t SET a = 1".into(),
            engine: Engine::Postgres,
            running: false,
            error: None,
        }
    }

    fn backup_restore(status: backup::BackupStatus) -> ModalState {
        ModalState::BackupRestore(backup::BackupSession {
            kind: backup::BackupKind::Backup,
            engine: Engine::Postgres,
            connection_id: "c".into(),
            connection_name: "c".into(),
            database: "db".into(),
            log: Rc::new(RefCell::new(backup::BackupLogState::default())),
            status: Rc::new(RefCell::new(status)),
            started_at: std::time::Instant::now(),
            cancel: Rc::new(RefCell::new(None)),
            confirm_input: None,
            expected_name: "db".into(),
            command_line: String::new(),
            target_path: String::new(),
        })
    }

    fn script_run() -> ModalState {
        ModalState::ScriptRun {
            files: Vec::new(),
            file_counts: Vec::new(),
            tx_scope: crate::runner::TxScope::PerFile,
            error_policy: crate::runner::ErrorPolicy::Stop,
            source_label: String::new(),
            conn_label: String::new(),
            read_only: false,
            timeout_secs: None,
            conn_identity: "cfg:x".into(),
        }
    }

    fn csv_import() -> ModalState {
        ModalState::CsvImport {
            path: std::path::PathBuf::new(),
            schema: None,
            table: "t".into(),
            headers: Vec::new(),
            columns: Vec::new(),
            targets: Vec::new(),
            row_count: 0,
            first_rows: Vec::new(),
            sample_sql: None,
            error: None,
            conn_identity: "cfg:x".into(),
            conn_label: String::new(),
        }
    }

    fn chart_picker() -> ModalState {
        let schema = std::sync::Arc::new(dbc_core::arrow::datatypes::Schema::empty());
        ModalState::ChartPicker {
            source_title: String::new(),
            buffer: Rc::new(RefCell::new(ResultBuffer::new(schema))),
            columns: Vec::new(),
            kind: ChartKind::Bar,
            x_col: 0,
            y_selected: Vec::new(),
            edit_tab: None,
        }
    }

    #[test]
    fn query_params_runs_params() {
        assert!(matches!(modal_confirm_kind(&query_params()), ModalConfirmKind::RunParams));
    }

    #[test]
    fn compare_dialog_confirms_compare() {
        let m = ModalState::CompareDialog { conn_a: None, conn_b: None, error: None };
        assert!(matches!(modal_confirm_kind(&m), ModalConfirmKind::Compare));
    }

    #[test]
    fn settings_closes() {
        assert!(matches!(modal_confirm_kind(&ModalState::Settings), ModalConfirmKind::CloseSettings));
    }

    #[test]
    fn chart_picker_confirms_chart() {
        assert!(matches!(modal_confirm_kind(&chart_picker()), ModalConfirmKind::ChartConfirm));
    }

    // --- §3-novela: the last-gate-before-write dialogs are ALL Ignore ---

    #[test]
    fn kill_confirm_is_ignored() {
        assert!(matches!(modal_confirm_kind(&kill_confirm()), ModalConfirmKind::Ignore));
    }

    #[test]
    fn analyze_write_confirm_is_ignored() {
        assert!(matches!(modal_confirm_kind(&analyze_write()), ModalConfirmKind::Ignore));
    }

    #[test]
    fn backup_restore_is_ignored_in_every_state() {
        for status in [
            backup::BackupStatus::Confirming,
            backup::BackupStatus::Running,
            backup::BackupStatus::Succeeded,
        ] {
            assert!(matches!(modal_confirm_kind(&backup_restore(status)), ModalConfirmKind::Ignore));
        }
    }

    #[test]
    fn script_run_is_ignored() {
        assert!(matches!(modal_confirm_kind(&script_run()), ModalConfirmKind::Ignore));
    }

    #[test]
    fn csv_import_is_ignored() {
        assert!(matches!(modal_confirm_kind(&csv_import()), ModalConfirmKind::Ignore));
    }
}
```

- [ ] **Step 3: Run to verify FAIL** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui modal_confirm_kind` → `ModalConfirmKind` not found.

- [ ] **Step 4: Implement the decision fn** (free items near `ModalState`'s definition):

```rust
/// UX-polish §1.2: what Enter does per open modal — THE policy table as
/// code, unit-tested table-style like `kill_confirm_tests`. The rule, so
/// future dialogs don't re-litigate: Enter is allowed where confirm (a)
/// runs nothing against the database, or (b) resumes an already-expressed
/// run intent (QueryParams interrupted a Ctrl+Enter), or (c) leads only
/// into a further explicit confirmation gate. Enter is `Ignore` where the
/// button is the LAST gate before an immediate write/kill/restore/batch
/// dispatch (§3-novela): KillConfirm, AnalyzeWriteConfirm, BackupRestore
/// (all states), ScriptRun, CsvImport. `Ignore` is a HANDLED no-op — the
/// listener consumes the keystroke so it cannot fall through elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalConfirmKind {
    SaveConnection,
    UnlockVault,
    CreateVault,
    RunParams,
    Compare,
    CloseSettings,
    ChartConfirm,
    Ignore,
}

pub(crate) fn modal_confirm_kind(modal: &ModalState) -> ModalConfirmKind {
    match modal {
        ModalState::ConnectionDialog(_) => ModalConfirmKind::SaveConnection,
        ModalState::MasterPasswordPrompt { .. } => ModalConfirmKind::UnlockVault,
        ModalState::CreateMasterPassword { .. } => ModalConfirmKind::CreateVault,
        ModalState::QueryParams { .. } => ModalConfirmKind::RunParams,
        ModalState::CompareDialog { .. } => ModalConfirmKind::Compare,
        ModalState::Settings => ModalConfirmKind::CloseSettings,
        ModalState::ChartPicker { .. } => ModalConfirmKind::ChartConfirm,
        // §3-novela Ignore arms — kept as explicit variants (not a `_`
        // catch-all) so a NEW ModalState variant is a compile error here
        // and must consciously pick a side of the policy table.
        ModalState::KillConfirm { .. }
        | ModalState::AnalyzeWriteConfirm { .. }
        | ModalState::BackupRestore(_)
        | ModalState::ScriptRun { .. }
        | ModalState::CsvImport { .. } => ModalConfirmKind::Ignore,
    }
}
```

- [ ] **Step 5: Run to verify PASS** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui modal_confirm_kind` → 9 pass.

- [ ] **Step 6: The dispatcher.** In connections_ui.rs's `impl AppView` (next to `close_modal` is a good spot):

```rust
    /// UX-polish §1: Enter on an open modal — dispatches the IDENTICAL fn
    /// the dialog's primary button calls. Every target is self-guarding at
    /// the top of its own body (design §6: `confirm_query_params` re-runs
    /// `build_param_sql`'s rescan and stays open on error;
    /// `confirm_compare_dialog` no-ops until both sides are picked;
    /// `confirm_chart_picker` validates `y_selected`; `on_save_clicked`
    /// routes through the vault flow + corrupt-config guard) — this fn
    /// adds ZERO new authority and must never pre-check or duplicate
    /// those guards.
    fn on_modal_confirm(
        &mut self,
        _: &ModalConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(modal) = &self.modal else { return };
        match modal_confirm_kind(modal) {
            ModalConfirmKind::SaveConnection => self.on_save_clicked(window, cx),
            ModalConfirmKind::UnlockVault => self.on_master_password_submit(window, cx),
            ModalConfirmKind::CreateVault => self.on_create_master_password_submit(window, cx),
            ModalConfirmKind::RunParams => self.confirm_query_params(cx),
            ModalConfirmKind::Compare => self.confirm_compare_dialog(cx),
            ModalConfirmKind::CloseSettings => self.close_modal(cx),
            ModalConfirmKind::ChartConfirm => self.confirm_chart_picker(cx),
            // Handled no-op: propagation already stopped, Enter dies here.
            ModalConfirmKind::Ignore => {}
        }
    }
```

- [ ] **Step 7: Wrapper adoption.** `render_modal_overlay`'s backdrop div (connections_ui.rs:1330-1342, now carrying Task 3's `track_focus`) additionally gets, before `.occlude()`:

```rust
                .key_context("ModalForm")
                .on_action(cx.listener(AppView::on_modal_confirm))
```

- [ ] **Step 8: Build + full test, zero warnings** — `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` and `test -p dbc-ui`. (`ModalFocusNext`/`ModalFocusPrev` are declared but unbound until Task 5 — `actions!` generates pub types, no dead-code warning.)

- [ ] **Step 9: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs
git commit -m "feat: Enter confirms modals per the ModalForm policy table"
```

---

### Task 5: Tab/Shift+Tab cycles modal inputs (design §2)

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`bind_keys`, wrapper, `impl AppView`, field ctors :1419-1431, :1692, :1761, :1770-1771, :1881, :2059)
- Modify: `crates/dbc-ui/src/main.rs` (QueryParams inputs :1253-1257, restore `confirm_input` :7029)

**Interfaces:**
- Consumes: `TextField::form_field` (Task 3), `ModalFocusNext`/`ModalFocusPrev` (Task 4), `Window::focus_next(cx)`/`focus_prev(cx)` (pinned rev; wrap-around and skip-non-stops are `TabStopMap`'s documented behavior).
- Produces: `AppView::on_modal_focus_next` / `on_modal_focus_prev` (referenced by name in the wrapper only).

- [ ] **Step 1: Bindings.** Append to connections_ui.rs `bind_keys`, under the Task 4 enter line:

```rust
        // UX-polish §2: Tab order IS paint order (all tab_index 0 →
        // TabStopMap falls back to insertion order): ConnectionDialog runs
        // Název → Host → Port → Databáze → Uživatel → Heslo → Složka →
        // Timeout → Auto-limit → (SSH host → port → uživatel → klíč, only
        // while ssh_enabled — unpainted fields drop out of the cycle
        // automatically). No unscoped `tab` exists and "SqlInput"'s scoped
        // one is never on a modal's focus path, so these match directly —
        // no fall-through needed. Dialogs with no stops: focus_next
        // returns None → safe no-op.
        KeyBinding::new("tab", ModalFocusNext, Some("ModalForm")),
        KeyBinding::new("shift-tab", ModalFocusPrev, Some("ModalForm")),
```

- [ ] **Step 2: Handlers.** In connections_ui.rs's `impl AppView`, next to `on_modal_confirm`:

```rust
    /// UX-polish §2: Tab inside a modal — `TabStopMap` supplies ordering,
    /// wrap-around, and skips non-stops; only `form_field` handles are
    /// stops, and the single-modal invariant guarantees at most one
    /// dialog's fields are painted, so the map contains exactly the open
    /// dialog's inputs. Buttons/checkboxes are plain divs and stay out of
    /// the cycle in v1 (design §2.1).
    fn on_modal_focus_next(
        &mut self,
        _: &ModalFocusNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
    }

    fn on_modal_focus_prev(
        &mut self,
        _: &ModalFocusPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }
```

Wire both on the `render_modal_overlay` backdrop, after Task 4's `on_action`:

```rust
                .on_action(cx.listener(AppView::on_modal_focus_next))
                .on_action(cx.listener(AppView::on_modal_focus_prev))
```

- [ ] **Step 3: `form_field` adoption — the complete call-site list** (mechanical `TextField::new(` → `TextField::form_field(` at exactly these sites, args unchanged; verify each is a modal opener while editing):
  - connections_ui.rs `open_connection_dialog` — all 13 fields (:1419-1431: name, host, port, database, user, password, folder, timeout_secs, auto_limit, ssh_host, ssh_port, ssh_user, ssh_key_path).
  - connections_ui.rs master-password prompt inputs: `on_test_clicked` (:1692), `on_save_clicked` (:1761), and the two other `MasterPasswordPrompt` construction sites (:1881, :2059 — the dropdown-connect prompt and the retry/reopen path; confirm each constructs a `ModalState::MasterPasswordPrompt` before converting).
  - connections_ui.rs `on_save_clicked` CreateMasterPassword `input1`/`input2` (:1770-1771).
  - main.rs `open_query_params_dialog` param inputs (:1253-1257 — the `cx.new(|cx| { let mut f = connections_ui::TextField::new(cx, "", false); ... })` closure becomes `form_field`).
  - main.rs `begin_restore_confirm` typed-name `confirm_input` (:7029).
  - Leave `TextField::new` at: grid filter row (grid.rs:461, :831), find bar (:1073), cell editor (:2061), palette input (main.rs:3603), history search (main.rs:7840) — **the leak guard**; single-field dialogs are converted anyway (a one-stop cycle wraps to itself, harmless, and keeps the invariant grep-simple: modal ⇒ form_field).
  - (admin_panel.rs's four fields are Task 6's.)

- [ ] **Step 4: Build + full test, zero warnings** — `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` and `test -p dbc-ui`.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/main.rs
git commit -m "feat: Tab/Shift+Tab cycles modal form fields via native tab stops"
```

---

### Task 6: Admin modal adoption + Esc closes AdminModal (design §1.1/§1.2 admin rows, sweep #9)

**Files:**
- Modify: `crates/dbc-ui/src/admin_panel.rs` (struct :856-863 + ctor :891, field ctors :1159/:1160/:1179/:1264, `open_drop_schema_modal` :1274 + call site :1838, modal wrapper :2173-2188, discard overlay :2190-2205, new fns + tests)
- Modify: `crates/dbc-ui/src/main.rs` (`on_cancel_query`'s active-tab block :3554-3562)

**Interfaces:**
- Consumes: `connections_ui::{ModalConfirm, ModalFocusNext, ModalFocusPrev}` (Task 4), `connections_ui::TextField::form_field` (Task 3), admin confirm fns `confirm_new_role`/`confirm_change_password`/`confirm_new_schema` (existing), `discard_confirm_no` (existing „Zpět" semantics).
- Produces: `AdminPanel::close_overlay_if_open(&mut self, cx: &mut Context<Self>) -> bool` (pub — main.rs's Esc arm calls it), pure `admin_esc_closable(has_password_field: bool, password_empty: bool) -> bool`, `AdminPanel.modal_focus_handle`.

- [ ] **Step 1: Write the failing tests** — in admin_panel.rs's existing `#[cfg(test)] mod tests` (:2308):

```rust
    // UX-polish sweep #9: the M6 password rule mirrored onto admin modals —
    // a modal holding a typed (non-empty) password is NOT closable by Esc,
    // same reasoning as ConnectionDialog in `AppView::on_cancel_query`.
    #[test]
    fn esc_closable_without_password_field() {
        assert!(super::admin_esc_closable(false, true));
    }

    #[test]
    fn esc_closable_with_empty_password() {
        assert!(super::admin_esc_closable(true, true));
    }

    #[test]
    fn esc_not_closable_with_typed_password() {
        assert!(!super::admin_esc_closable(true, false));
    }
```

- [ ] **Step 2: Run to verify FAIL** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui admin_esc` → not found.

- [ ] **Step 3: Pure fn + close_overlay_if_open.** In admin_panel.rs (free fn near `AdminModal`):

```rust
/// UX-polish sweep #9: whether Esc may close an admin modal — the M6
/// password rule (never dismiss a modal holding a typed password) as a
/// pure decision, same tier as connections_ui's `blocks_clipboard_write`.
fn admin_esc_closable(has_password_field: bool, password_empty: bool) -> bool {
    !has_password_field || password_empty
}
```

In `impl AdminPanel`:

```rust
    /// UX-polish sweep #9: called from `AppView::on_cancel_query`'s new
    /// `TabContent::Admin` arm — the SAME "the unscoped root Esc binding
    /// is the mechanism" shape as `ResultGrid::close_overlay_if_open`.
    /// Discard-confirm first (Esc = „Zpět", never „Zahodit" — the G5
    /// rule), then the modal. Returns true when Esc was CONSUMED — which
    /// includes the refused password case, so Esc against a typed
    /// password does nothing at all rather than cancelling a query
    /// underneath (mirrors the app-modal `closable` match's `return`).
    pub fn close_overlay_if_open(&mut self, cx: &mut Context<Self>) -> bool {
        if self.discard_confirm.is_some() {
            self.discard_confirm_no(cx);
            return true;
        }
        if let Some(modal) = &self.modal {
            let (has_password, password_empty) = match modal {
                AdminModal::NewRole { password, .. }
                | AdminModal::ChangePassword { password, .. } => {
                    (true, password.read(cx).text().is_empty())
                }
                AdminModal::NewSchema { .. } | AdminModal::DropSchema { .. } => (true, true),
            };
            if admin_esc_closable(has_password, password_empty) {
                self.close_modal(cx);
            }
            return true;
        }
        false
    }
```

(Note the `(true, true)` trick keeps the tuple simple: "no password field" is encoded as an always-empty one; the pure fn's `has_password_field=false` case is equivalent and covered by its own test.)

- [ ] **Step 4: Run to verify PASS** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui admin_esc` → 3 pass.

- [ ] **Step 5: Enter/Tab handlers + wrapper + focus.**
  - Add field to `AdminPanel` (after `focus_handle` :863): `/// UX-polish: focus target for the no-input DropSchema modal — panel-local twin of AppView's modal_focus_handle (§1.4).` `modal_focus_handle: FocusHandle,` — init in the ctor (:891 vicinity): `modal_focus_handle: cx.focus_handle(),`.
  - `open_drop_schema_modal` (:1274) gains `window: &mut Window` (before `cx`), and after `self.modal = Some(AdminModal::DropSchema { .. });` add `window.focus(&self.modal_focus_handle, cx);`. Update its stale "No `TextField` here, so no focus hand-off" doc comment. Call site :1838: change the click listener's `_window` to `window` and pass it.
  - Handlers in `impl AdminPanel`:

```rust
    /// UX-polish §1.2 (admin rows): Enter = the confirm button. NewRole/
    /// ChangePassword stage a WriteStatement (execution still goes through
    /// the apply bar → apply dialog gate); NewSchema emits RequestApply →
    /// opens the apply dialog (a second explicit gate). DropSchema is a
    /// DELIBERATE handled no-op (§3-novela: destructive intent — the pause
    /// is the point, CASCADE warning) — do not wire it.
    fn on_modal_confirm(
        &mut self,
        _: &connections_ui::ModalConfirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.modal {
            Some(AdminModal::NewRole { .. }) => self.confirm_new_role(cx),
            Some(AdminModal::ChangePassword { .. }) => self.confirm_change_password(cx),
            Some(AdminModal::NewSchema { .. }) => self.confirm_new_schema(cx),
            Some(AdminModal::DropSchema { .. }) | None => {}
        }
    }

    fn on_modal_focus_next(
        &mut self,
        _: &connections_ui::ModalFocusNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
    }

    fn on_modal_focus_prev(
        &mut self,
        _: &connections_ui::ModalFocusPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
    }
```

  - `render_modal_overlay`'s backdrop (:2174-2186) gains, before `.occlude()`:

```rust
                .key_context("ModalForm")
                .track_focus(&self.modal_focus_handle)
                .on_action(cx.listener(Self::on_modal_confirm))
                .on_action(cx.listener(Self::on_modal_focus_next))
                .on_action(cx.listener(Self::on_modal_focus_prev))
```

  - `render_discard_confirm_overlay`'s backdrop (:2195-2205) gains `.track_focus(&self.modal_focus_handle)` only (no key context — Enter stays inert on a discard prompt, admin or app, per Global Constraints). Where admin discard-confirms open without focus moving (`request_sub_view`/`request_select_schema`/`request_select_grantee`, all cx-only): accepted gap for v1 — Esc works regardless (root binding) and the prompt has no input; noted in the manual checklist.
  - Known edge (design §2.2, accepted): admin modal is per-tab state, so a stacked ConnectionDialog + AdminModal would union their tab stops — cosmetic, not scoped-around in v1.

- [ ] **Step 6: Admin field ctors → `form_field`** — admin_panel.rs :1159, :1160 (NewRole name+password), :1179 (ChangePassword), :1264 (NewSchema): `connections_ui::TextField::new(` → `connections_ui::TextField::form_field(`.

- [ ] **Step 7: The Esc Admin arm.** In main.rs `on_cancel_query`, replace the active-tab block (:3554-3562) with a match adding the Admin arm (mirroring the grid arm's shape, per sweep #9):

```rust
        if let Some(active) = self.tabs.active() {
            match &active.content {
                TabContent::Grid { grid, .. } => {
                    let closed = grid.update(cx, |g, _| g.close_overlay_if_open());
                    if closed {
                        cx.notify();
                        return;
                    }
                }
                // UX-polish sweep #9: Esc closes an open AdminModal / the
                // admin discard-confirm — with the M6 password rule (a
                // typed password refuses Esc but still consumes it); see
                // `AdminPanel::close_overlay_if_open`.
                TabContent::Admin { view } => {
                    let closed = view.update(cx, |p, cx| p.close_overlay_if_open(cx));
                    if closed {
                        cx.notify();
                        return;
                    }
                }
                _ => {}
            }
        }
```

- [ ] **Step 8: Build + full test, zero warnings** — `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` and `test -p dbc-ui`.

- [ ] **Step 9: Commit**

```bash
git add crates/dbc-ui/src/admin_panel.rs crates/dbc-ui/src/main.rs
git commit -m "feat: admin modals adopt ModalForm keys and Esc closes them"
```

---

### Task 7: Final audit — grep gates, full pass, manual checklist hand-off, version bump (SERIALIZED, last)

**Files:**
- Modify: `Cargo.toml` (workspace root — `[workspace.package] version`)

**Interfaces:** consumes everything; produces the release-ready branch state.

- [ ] **Step 1: Tab-stop invariant grep (the design §8 merge gate — hard gate, not suggestion).**

Run (Git Bash): `grep -n "form_field(\|tab_stop(true)" crates/dbc-ui/src/*.rs`
Expected: hits ONLY at (a) the `TextField::form_field` definition itself (its one `tab_stop(true)` call + doc comment), and (b) modal dialog opener call sites in connections_ui.rs (13 + 4 master-password + 2 create), main.rs (QueryParams closure, restore confirm_input), admin_panel.rs (4). ANY other hit (grid filter/find/cell editor, palette, history search, or a new stray) is a Tab-cycle leak — fix before proceeding.

- [ ] **Step 2: `"ModalForm"` scope grep.**

Run: `grep -n '"ModalForm"' crates/dbc-ui/src/*.rs`
Expected: exactly the two backdrop wrappers (connections_ui.rs `render_modal_overlay`, admin_panel.rs `render_modal_overlay`) plus the three `KeyBinding::new` lines in connections_ui.rs `bind_keys`. The apply dialog and discard-confirm overlays (main.rs `render_apply_dialog_overlay` / `render_discard_confirm_overlay`, admin_panel.rs `render_discard_confirm_overlay`) must NOT appear — that absence IS the §3-novela structural-inertness mechanism.

- [ ] **Step 3: §3-novela spot-check.** Confirm `modal_confirm_kind`'s `Ignore` arm still lists exactly `KillConfirm | AnalyzeWriteConfirm | BackupRestore | ScriptRun | CsvImport`, and admin's `on_modal_confirm` no-ops `DropSchema`. Run the pinning tests: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui modal_confirm_kind` → all pass.

- [ ] **Step 4: Full build + full test, zero warnings, all crates that changed:**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` then `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: clean, zero warnings in both (test build included).

- [ ] **Step 5: Emit the manual visual checklist.** Surface the "Manual visual checklist" section below VERBATIM in the task's final report so the orchestrator can hand it to the user — traversal/focus/scroll cannot be asserted headlessly in this codebase (design §7's honest split; a `TestAppContext` harness is recorded as a future option, not this phase's task).

- [ ] **Step 6: Bump the workspace version to the next minor per merge order.** Root `Cargo.toml`, `[workspace.package]` `version = "0.15.0"` at plan-writing time — the actual number is assigned at merge time by the orchestrator based on what has merged before this phase (`0.16.0` if nothing did); write the next unclaimed minor.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml
git commit -m "chore: bump version for UX polish keyboard phase"
```

---

## Manual visual checklist (design §7 — hand to the user at Task 7 Step 5; every line is a PASS/FAIL check)

**Tab traversal (§2):**
1. ConnectionDialog, SSH unchecked: Tab cycles Název → Host → Port → Databáze → Uživatel → Heslo → Složka → Timeout → Auto-limit → wraps to Název; Shift+Tab reverses (and wraps first → last).
2. ConnectionDialog, SSH checked: the four SSH fields join the cycle after Auto-limit; unchecking drops them out immediately.
3. CreateMasterPassword: input1 → input2 → wrap. QueryParams with 3 `:name` params: cycles in `names` order. Restore confirm: single field, Tab stays put (self-wrap). Admin NewRole: name → heslo → wrap.
4. Tab inside the SQL editor with the autocomplete popup CLOSED still does nothing (no indent — sweep #16 out of scope, unchanged); grid filter row / find bar / palette / history search are NOT reachable by Tab from inside any dialog.

**Enter (§1 — one Yes and one Ignore per family):**
5. QueryParams: type values, Enter → runs the query (the headline case). Bad param → error shows, dialog stays.
6. MasterPasswordPrompt: wrong heslo + Enter → error in-modal; right heslo + Enter → unlocks. ConnectionDialog: Enter in „Heslo" saves.
7. KillConfirm: Enter does NOTHING (no kill, no close, no beep-through to the editor). Same for ScriptRun, CsvImport, BackupRestore (Confirming with the correct typed name — Enter still must not restore), AnalyzeWriteConfirm.
8. Apply dialog and discard-confirm: Enter does nothing. Settings: Enter closes. CompareDialog: Enter no-ops until both sides picked, then dispatches. ChartPicker: Enter no-ops without a Y column. Admin NewRole/NewSchema: Enter stages/opens apply dialog; DropSchema: Enter does nothing.

**Focus inertness (§1.4, sweep #8):**
9. Open KillConfirm (monitor tab → kill), type letters — the SQL editor text must NOT change. Repeat for Settings, ChartPicker, CompareDialog, ScriptRun, CsvImport, Backup-kind BackupRestore, the app discard-confirm, and admin DropSchema.
10. KillConfirm flicker check (design §8 risk): open it and type IMMEDIATELY — no keystroke may land in the editor between the subscribe-path open and the next frame.

**Delete (§3):**
11. Editable preview: select a row, Delete → row stages deleted (strikethrough, apply bar counts it); Delete again → un-stages. Multi-row span (drag) → all toggle. Span over real+inserted rows → real toggle, inserted vanish, selection clears.
12. Read-only/ad-hoc/MSSQL/PK-less tab: Delete does nothing. With the cell editor open: Delete edits text only (forward-delete), never stages a row delete. Filter/find inputs: same.

**„+ řádek" (§4):**
13. On a filtered + sorted preview scrolled to the top: „+ řádek" → view scrolls to the new last row, cell editor opens on the first VISIBLE column with empty text; Esc closes the editor without staging. Second click repeats on the next row.
14. With all columns hidden via „Sloupce ▾": „+ řádek" scrolls only, no editor. (Judgement call while here: does auto-open annoy rapid multi-row adding? If yes → report; fallback is removing the one `open_cell_editor` call, scroll stays.)

**Esc (sweep #9):**
15. Admin NewSchema open → Esc closes it. NewRole with typed heslo → Esc does nothing (and does not cancel a query behind); clear the field → Esc closes. Admin discard-confirm (switch sub-view with staged changes) → Esc = „Zpět". DropSchema → Esc closes.
16. Regression: Esc still closes palette / dropdown / closable app modals / app discard-confirm („Zrušit") / apply dialog (not while running) / grid overlays, in that priority order, and still cancels a running query last.

## Spec coverage (self-review, run at write time)

- §1.1 mechanism → Tasks 3, 4 (grounding correction 3 relocates the `actions!` — behavior identical); resolution walk-through preserved as the binding's comment. §1.2 table → the restated policy table + `modal_confirm_kind` + Task 6's admin match; every Ignore arm asserted or explicitly no-op'd. §1.3 → Task 7 Step 2's negative grep. §1.4 → Task 3 (uniform flag path per grounding correction 2; discard-confirm folded in; BackupRestore Backup-kind covered via `confirm_input.is_none()`).
- §2.1 → Tasks 3 (ctor), 5 (bindings/handlers/adoption); leak guard = Task 5 Step 3's leave-list + Task 7 Step 1 grep; skip-disabled vacuous (noted in ctor comment via §2.1's "modal openers only" rule); no-stops no-op noted in binding comment. §2.2 edge → recorded in Task 6 Step 5.
- §3.1 binding+contention → Task 1 Step 5 comment; §3.2 guard chain → Step 6 (order preserved, each silent); §3.3 semantics → `delete_targets` + handler (toggle for real rows, descending removes for inserts, selection cleared only when inserts removed).
- §4 → Task 2 (scroll Center + clamp note, first visible col, skip-when-all-hidden, editor auto-open + §8 fallback note).
- §5 sweep: 1/2/3/10 no-change (verified still true at f87b2be); 4→T4, 5→T5, 6→T1, 7→T2, 8→T3, 9→T6; 11/12/13/16 out of scope (recorded here so they aren't "discovered" again); 14/15 rejected-by-design → no context added (Task 7 Step 2 enforces); 17 → masked fields are Enter-confirm rows (T4) and `form_field` sites (T5), masking untouched.
- §6 security → Global Constraints (guards stay inside confirm fns) + `on_modal_confirm`'s doc comment; Ignore = handled no-op; §1.4 strictly guard-tightening; masked-field behavior untouched (no new reads/logs; clipboard guard untouched).
- §7 tests → `modal_confirm_kind` (grounding correction 4 for the 3 unconstructible variants), `delete_targets`, display-index, `first_visible_col`, `admin_esc_closable`; everything else → the manual checklist section.
- §8 → task table mirrors T1-T4 (split finer per one-mechanism-per-task); all five risks carried: fall-through comment (T4 S1), grep invariant (T7 S1), stacked-modal edge (T6 S5), auto-open fallback (T2 S5 comment + checklist 14), deferred-focus flicker (checklist 10), Center-clamp fallback (T2 S5 comment).
