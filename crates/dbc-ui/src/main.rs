mod autocomplete;
mod connect;
mod connections_ui;
mod export;
mod fk_join;
mod grid;
mod history_panel;
mod palette;
mod row_view;
mod runner;
mod sandbox;
mod schema_tree;
mod sql_input;
mod tabs;
mod text_model;
mod tunnel;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::{
    apply_auto_limit, is_read_statement, quote_qualified, CancelToken, FkRef, QueryError,
    SchemaSnapshot, TableInfo,
};
use dbc_state::{AppConfig, HistoryDb, HistoryEntry, TableViewPrefs, Vault, ViewPrefsStore};
use gpui::{
    actions, div, prelude::*, px, rgb, rgba, size, AnyElement, App, Bounds, ClipboardItem,
    Context, Entity, Focusable, KeyBinding, ScrollDelta, ScrollWheelEvent, Window, WindowBounds,
    WindowOptions,
};
use gpui_platform::application;
use grid::{GridEvent, ResultGrid};
use palette::{PaletteAction, PaletteItem};
use runner::{ConnectSpec, QueryEvent, QueryRunner};
use schema_tree::{SchemaTree, TreeEvent};
use sql_input::SqlInput;
use tabs::{collapse_title, ResultTab, TabContent, Tabs};

actions!(dbc, [RunQuery, RunQueryUnlimited, CancelQuery, ToggleTree, ToggleHistory, OpenPalette]);

/// G3 Task 5: Ctrl+K command palette state — created on `OpenPalette`,
/// dropped on close/execute. `items`/`selected` are recomputed from
/// `input`'s text (see `AppView::refresh_palette_items`), polled lazily at
/// render time the same way `history_search`/`last_history_query` are (see
/// history_panel.rs's module doc comment) rather than via an on-change hook
/// `connections_ui::TextField` doesn't have.
struct PaletteState {
    input: Entity<connections_ui::TextField>,
    items: Vec<PaletteItem>,
    selected: usize,
    /// The text `items` was last computed from — compared against `input`'s
    /// live text each render to detect an edit.
    last_query: String,
}

/// G2 Task 7: SQL builder for `TreeEvent::OpenPreview`. Pure — no GPUI, no
/// I/O — so quoting can be unit-tested directly. `quote_qualified` (shared
/// with `synthesize_create_table`'s DDL quoting) is what makes this safe
/// against a table literally named `we"ird`: the embedded quote is doubled,
/// not smuggled into the query as SQL syntax.
fn preview_sql(schema: Option<&str>, table: &str) -> String {
    format!("SELECT * FROM {} LIMIT 1000", quote_qualified(schema, table))
}

/// G4 Task 5: shared per-column FK lookup against an already-resolved
/// `TableInfo` (either the previewed table, or `fk_info_for_adhoc`'s
/// single-match heuristic result) — `result_cols` are the CURRENT result's
/// column names in order; for each, finds the same-named `ColumnInfo` in
/// `t` and reads its `fk`, plus (when present) the referenced table's own
/// column names from `snapshot` for the ☰ menu. A result column with no
/// same-named base column (e.g. an already-joined `"ref.col"` alias from a
/// previous preview re-run) gets `None` in both outputs — it's not treated
/// as an error, just "not an FK column".
fn fk_info_from_table(
    snapshot: &SchemaSnapshot,
    t: &TableInfo,
    result_cols: &[String],
) -> (Vec<Option<FkRef>>, Vec<Option<Vec<String>>>) {
    let mut fk_info = Vec::with_capacity(result_cols.len());
    let mut ref_cols = Vec::with_capacity(result_cols.len());
    for name in result_cols {
        let fk = t.columns.iter().find(|c| &c.name == name).and_then(|c| c.fk.clone());
        let refcols = fk.as_ref().and_then(|fk| {
            snapshot
                .tables
                .iter()
                .find(|rt| rt.schema.as_deref() == fk.schema.as_deref() && rt.name == fk.table)
                .map(|rt| rt.columns.iter().map(|c| c.name.clone()).collect())
        });
        fk_info.push(fk);
        ref_cols.push(refcols);
    }
    (fk_info, ref_cols)
}

/// G5 Task 3: the PK-mapping/read-only/engine decision behind a PREVIEW
/// tab's `sandbox::Editable` — pure (no GPUI/`Context`) so it's directly
/// testable, mirroring `fk_info_from_table`'s split (snapshot lookup stays
/// in `AppView::editable_for_preview`, the actual decision is a free
/// function here).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditableDecision {
    /// RESULT-column indices of the mapped PK columns (never empty).
    Editable(Vec<usize>),
    /// `table` was found but none of its PK columns map onto `headers` —
    /// drives the brief's "tabulka nemá primární klíč — jen pro čtení"
    /// status notice, independent of the read-only/engine/connection checks
    /// below (a PK-less table is worth flagging regardless of whether the
    /// connection could otherwise write to it).
    NoPrimaryKey,
    /// Not editable for any other reason (table not found in the snapshot —
    /// e.g. still loading — no connection-backed config at all, a read-only
    /// connection, or an MSSQL engine — G5 scope excludes MSSQL, see the
    /// project memory/brief).
    NotEditable,
}

/// G5 Task 3: the CLI-arg back-compat path (`AppView::conn_url`, no saved
/// `ConnectionConfig`) still gets `detect_editable_pk`'s `conn_meta` facts —
/// always writable (no read-only concept for a bare connection string) with
/// the engine inferred from the URL itself, via the SAME postgres-vs-sqlite
/// dispatch `connect::open` uses to pick a driver (`postgres[ql]://` ->
/// Postgres, anything else -> a SQLite file path — MSSQL has no CLI-arg URL
/// form at all in this app, so it never needs a branch here).
fn engine_from_url(url: &str) -> dbc_state::Engine {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        dbc_state::Engine::Postgres
    } else {
        dbc_state::Engine::Sqlite
    }
}

/// `conn_meta`: `Some((read_only, engine))` — for a saved `ConnectionConfig`
/// (`cfg.read_only`/`cfg.engine`) or the CLI-arg URL path (see
/// `engine_from_url`); `None` only when `run_query_with` couldn't build a
/// `ConnectSpec` at all (no active connection AND no CLI-arg URL — that path
/// returns before ever reaching `Started`, so `None` is effectively
/// unreachable today, but `detect_editable_pk` still treats it as
/// not-editable defensively rather than assuming a caller always has one).
/// `table`: the previewed table's `TableInfo` from the schema snapshot, or
/// `None` if it isn't in the snapshot (yet) — same "degrade gracefully"
/// precedent `fk_info_for_table` already sets. `headers`: the CURRENT
/// result's column names in order (same convention `fk_info_from_table`
/// uses for `result_cols`).
fn detect_editable_pk(
    conn_meta: Option<(bool, dbc_state::Engine)>,
    table: Option<&TableInfo>,
    headers: &[String],
) -> EditableDecision {
    let Some(t) = table else { return EditableDecision::NotEditable };
    // T3 review issue 3: only base tables are editable. This is the function
    // that gates whether the app may generate UPDATE/DELETE/INSERT, so it must
    // enforce table-vs-view itself rather than lean on the incidental fact that
    // neither driver currently reports `is_pk` for view columns — a future
    // driver, or a view with an INSTEAD OF trigger whose PK gets attributed,
    // would otherwise slip through and produce writes against a non-updatable
    // relation. Views/materialized views are never sandbox-editable.
    if t.kind != dbc_core::TableKind::Table {
        return EditableDecision::NotEditable;
    }
    let pk_cols: Vec<usize> = t
        .columns
        .iter()
        .filter(|c| c.is_pk)
        .filter_map(|c| headers.iter().position(|h| h == &c.name))
        .collect();
    if pk_cols.is_empty() {
        return EditableDecision::NoPrimaryKey;
    }
    let Some((read_only, engine)) = conn_meta else { return EditableDecision::NotEditable };
    if read_only || engine == dbc_state::Engine::Mssql {
        return EditableDecision::NotEditable;
    }
    EditableDecision::Editable(pk_cols)
}

/// G4 Task 6: maps SAVED preference names to the CURRENT result's source
/// column indices by exact name match — a name no longer present (a column
/// renamed or dropped since the prefs were saved) is silently skipped
/// (brief contract #4: "missing/renamed columns are ignored on apply"), not
/// an error. Order of `names` is preserved in the output (matches are
/// pushed in `names`' iteration order), duplicates/missing entries just
/// don't appear.
fn names_to_ixs(names: &[String], headers: &[String]) -> Vec<usize> {
    names.iter().filter_map(|n| headers.iter().position(|h| h == n)).collect()
}

/// G4 Task 6: the SAVE direction — builds a `TableViewPrefs` from a PREVIEW
/// grid's raw (ix-indexed) state, mapping every ix back to `headers`' name
/// at that position. `fk_joins` is passed straight through (already names,
/// from `ResultGrid::active_fk_join_names`). This is what makes "prefs
/// pruned on next save" (contract #4) automatic: a column that no longer
/// exists was already dropped by `view_prefs_to_grid_state`'s name→ix
/// mapping the moment it was applied, so it can never resurface here to be
/// written back out.
fn prefs_from_grid_state(
    headers: &[String],
    sort: Option<(usize, bool)>,
    hidden: &[bool],
    widths: &[f32],
    fk_joins: Vec<String>,
) -> TableViewPrefs {
    let hidden_columns: Vec<String> = hidden
        .iter()
        .enumerate()
        .filter(|(_, &h)| h)
        .filter_map(|(i, _)| headers.get(i).cloned())
        .collect();
    let col_widths: Vec<(String, f32)> =
        headers.iter().zip(widths.iter()).map(|(n, w)| (n.clone(), *w)).collect();
    let sort = sort.and_then(|(ix, asc)| headers.get(ix).cloned().map(|n| (n, asc)));
    TableViewPrefs { hidden_columns, col_widths, sort, fk_joins }
}

/// G4 Task 6: the APPLY direction — maps a saved `TableViewPrefs` (by name)
/// onto the CURRENT result's columns (by ix). A sort/hidden/width entry
/// whose column name isn't in `headers` any more is silently dropped
/// (contract #4) rather than erroring or leaving a dangling index. `hidden`
/// is always sized to `headers.len()` (matches
/// `ResultGrid::set_view_state`'s convention — every source column gets an
/// explicit true/false); `widths` is a sparse ix→px list, applied directly
/// onto `ResultGrid::col_widths` by the caller rather than routed through
/// `set_view_state`.
fn view_prefs_to_grid_state(
    prefs: &TableViewPrefs,
    headers: &[String],
) -> (Option<(usize, bool)>, Vec<bool>, Vec<(usize, f32)>) {
    let mut hidden = vec![false; headers.len()];
    for ix in names_to_ixs(&prefs.hidden_columns, headers) {
        hidden[ix] = true;
    }
    let sort = prefs
        .sort
        .as_ref()
        .and_then(|(name, asc)| headers.iter().position(|h| h == name).map(|ix| (ix, *asc)));
    let widths: Vec<(usize, f32)> = prefs
        .col_widths
        .iter()
        .filter_map(|(name, w)| headers.iter().position(|h| h == name).map(|ix| (ix, *w)))
        .collect();
    (sort, hidden, widths)
}

/// Review fix (Task 6 round 1, Issue 1): the outcome of
/// `apply_view_prefs_to_grid`'s join-state bookkeeping, given the three
/// facts it has available at a `Started` event — extracted as a pure
/// function so the ambiguity that caused the "uncheck-last-join lock-in" bug
/// can be tested directly (all 8 input combinations) rather than only
/// through the full grid/store integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinPrefAction {
    /// Persist THIS run's live join state (whatever it is, empty or not) as
    /// the new saved `fk_joins`.
    Save,
    /// This is a plain preview open with no joins of its own, and saved
    /// prefs have a non-empty `fk_joins` — rebuild `JoinSpec`s and re-run.
    Retrigger,
    /// Nothing to persist, nothing to retrigger.
    Nothing,
}

/// Pure decision function for `apply_view_prefs_to_grid`.
///
/// `from_join_change` is `true` only when this `Started` resulted from a
/// `GridEvent::RerunPreviewJoins` dispatch (an explicit user ☰ toggle) —
/// threaded through `PreviewTarget::from_join_change` rather than inferred
/// from `joins.is_empty()`, which is what let an explicit "uncheck the last
/// join" (empty joins, but very much user-driven) get misread as "plain
/// re-open, nothing changed" and silently reverted by stale saved prefs
/// (review Issue 1).
///
/// - `from_join_change == true`: the user just explicitly set the join
///   state (possibly to empty) — always `Save`, regardless of the other two
///   inputs. This is the fix: an explicit uncheck-to-zero now persists the
///   empty state instead of falling through to the retrigger branch.
/// - `from_join_change == false`, `joins_empty == false`: this `Started` is
///   a saved-fk-join retrigger's OWN result (queued by a prior call to this
///   same function) — `Save` (idempotent re-persist of what's already on
///   disk) and, critically, does NOT retrigger again — this is the original
///   loop guard, preserved.
/// - `from_join_change == false`, `joins_empty == true`: a genuine plain
///   preview (re-)open with no joins of its own. `Retrigger` if saved prefs
///   have a non-empty `fk_joins` to restore, else `Nothing`.
fn decide_join_pref_action(
    from_join_change: bool,
    joins_empty: bool,
    saved_fk_joins_nonempty: bool,
) -> JoinPrefAction {
    if from_join_change || !joins_empty {
        JoinPrefAction::Save
    } else if saved_fk_joins_nonempty {
        JoinPrefAction::Retrigger
    } else {
        JoinPrefAction::Nothing
    }
}

/// G5 Task 4 review fix (MAJOR 3): the outcome of the saved-fk-join
/// auto-retrigger's guard, given the two facts `QueryEvent::Finished`'s
/// handler has available for `pending_join_retrigger`'s tab. The retrigger
/// dispatches `run_query_with`, which — via `Started` — REPLACES the tab's
/// grid entity outright (`Tabs::close_by_preview_key` + a fresh
/// `ResultGrid`), silently dropping any `EditState` staged on the OLD one.
/// That's fine when there's nothing staged (the common case: the retrigger
/// exists purely to restore this table's saved FK joins after a plain
/// re-open), but if the user staged an edit WHILE the base query was still
/// streaming (between `Started` and this `Finished`), the retrigger must not
/// run at all — same "never silently drop staged edits" rule the T3-review
/// dirty guard already enforces at its three other sites
/// (`DiscardConfirmState`'s doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetriggerAction {
    /// Dispatch the retrigger — the tab is still open and not dirty.
    Run,
    /// The tab is still open but has staged edits — skip, leave the saved
    /// FK joins un-refreshed rather than risk dropping them.
    SkipDirty,
    /// The tab was closed mid-stream (pre-existing Task 6 round 1 Issue 2
    /// guard) — nothing to retrigger against.
    SkipClosed,
}

/// Pure decision function for the `QueryEvent::Finished` handler's
/// `pending_join_retrigger` dispatch. `dirty_change_count` is
/// `AppView::grid_dirty_change_count`'s result for the retrigger's tab —
/// `Some(_)` (never `Some(0)`, see that function's doc comment) means dirty.
fn decide_retrigger_action(tab_open: bool, dirty_change_count: Option<usize>) -> RetriggerAction {
    if !tab_open {
        RetriggerAction::SkipClosed
    } else if dirty_change_count.is_some() {
        RetriggerAction::SkipDirty
    } else {
        RetriggerAction::Run
    }
}

/// Set by `TreeEvent::OpenPreview` and threaded through `run_query_with` so
/// a preview runs through the exact same guarded pipeline as an
/// editor-typed query, without ever touching `self.sql`'s text: `title`
/// overrides the tab's title (`collapse_title(sql)` is used otherwise), and
/// `key` is the tab's `preview_key` — matched by `Tabs::close_by_preview_key`
/// so re-previewing the same (schema, table) replaces rather than stacks
/// (brief contract #1).
struct PreviewTarget {
    title: String,
    key: String,
    /// G4 Task 4: bare table name (no schema/quoting), threaded through to
    /// `ResultGrid::set_table_name` once the tab's grid exists (see
    /// `QueryEvent::Started` below) — used as the `INSERT INTO` target for
    /// this tab's exports. `key`/`title` both embed this too, but as
    /// free-form text meant for tab identity/display, not as a value a
    /// caller should parse back out.
    table: String,
    /// G4 Task 5: the previewed table's schema — `None` for a plain
    /// preview open (`preview_sql`'s own quoting already handles that), set
    /// alongside `table` so `fk_info_for_table`/a `GridEvent::
    /// RerunPreviewJoins` re-run can look the base `TableInfo` up in the
    /// snapshot again.
    schema: Option<String>,
    /// G4 Task 5: active FK joins for THIS run — empty for a plain preview
    /// open; populated when `on_grid_event`'s `RerunPreviewJoins` arm
    /// re-runs the preview with `fk_join::build_join_sql`'s rewritten SQL,
    /// so the brand-new grid entity `QueryEvent::Started` creates can
    /// restore checkbox/tint state via `ResultGrid::apply_active_joins`.
    joins: Vec<fk_join::JoinSpec>,
    /// Review fix (Task 6 round 1, Issue 1): `true` only when this run was
    /// dispatched from `on_grid_event`'s `GridEvent::RerunPreviewJoins` arm
    /// — i.e. this run's `joins` (empty or not) is the DIRECT result of an
    /// explicit user ☰ toggle, not a plain preview open or a saved-fk-join
    /// retrigger `apply_view_prefs_to_grid` queued itself. See
    /// `decide_join_pref_action` for why this needs to be an explicit
    /// marker rather than inferred from `joins.is_empty()`.
    from_join_change: bool,
}

/// Review fix (Task 5 round 1, clippy `too_many_arguments`): bundles
/// `GridEvent::RunLookup`'s payload for `AppView::start_lookup`, which
/// otherwise sits at 8 parameters once `generation` (Issue 1's fix) is
/// added — one struct beats a `#[allow]`.
struct LookupRequest {
    sql: String,
    ref_table: String,
    wanted_cols: Vec<String>,
    src_col: usize,
    generation: u64,
}

/// G5 Task 4: state for the Apply confirmation dialog — created by
/// `on_open_apply_dialog` (the apply bar's "Aplikovat"), from the ACTIVE
/// tab's `ResultGrid::editable`/`edit_state` at the moment it's clicked.
/// `statements`/`sql_text` are captured ONCE here rather than recomputed
/// live: the dialog must show (and, on confirm, execute) the EXACT SQL the
/// user reviewed, even if the underlying grid's staged edits somehow changed
/// in the gap before "Potvrdit a spustit" (not currently possible — the
/// dialog's own `.occlude()` blocks every click that could restage a cell —
/// but capturing once is also just the simpler, more obviously-correct
/// shape).
struct ApplyDialogState {
    /// Which tab's grid to clear/re-preview on success — looked up by id
    /// (not a held `Entity<ResultGrid>`) so a tab closed while the write is
    /// in flight (not reachable today, since the dialog's overlay occludes
    /// the tab strip, but checked defensively anyway) is simply not found
    /// rather than updating a dangling reference.
    tab_id: u64,
    /// `sandbox::generate_statements`' output verbatim — fed straight to
    /// `QueryRunner::run_write_transaction` on confirm.
    statements: Vec<(String, Option<u64>)>,
    /// `statements`' SQL text joined by newline (brief contract #3) — shown
    /// in the dialog AND recorded as the eventual history entry's `sql`.
    sql_text: String,
    /// `ResultGrid::preview_identity()`'s shape, captured at dialog-open
    /// time — lets a successful Apply re-run the SAME preview (brief:
    /// "re-run the preview, existing pipeline") without re-reading grid
    /// state after `clear_edits` has already run.
    preview_identity: (Option<String>, String),
    /// G5 Task 4 review fix (BLOCKER 1): the tab's `ResultTab::conn_identity`
    /// at dialog-open time — `on_confirm_apply` re-checks this against
    /// `AppView::current_conn_identity()` before dispatching (belt-and-
    /// braces alongside `on_open_apply_dialog`'s own check and the
    /// apply-bar's disabled button: the dialog's `.occlude()` should make a
    /// connection switch impossible while it's open, but this is the
    /// backstop if that assumption is ever wrong).
    conn_identity: String,
    /// True while `run_write_transaction` is in flight (brief contract #3:
    /// "aplikuji…", buttons disabled).
    running: bool,
    /// Set on failure — the dialog STAYS open showing this (brief contract
    /// #4: edits stay staged); cleared on a fresh "Potvrdit a spustit"
    /// retry.
    error: Option<String>,
    /// G5 Task 4 review fix (MINOR 4): captured once at open time so
    /// `on_open_apply_dialog` can `window.focus` it in the SAME update the
    /// overlay appears in (same convention `render_palette_overlay`'s/the
    /// connection dialogs' own `TextField` focus already follow) and
    /// `render_apply_dialog_overlay` can `.track_focus` the panel with the
    /// SAME handle — without this, focus stays on the SQL editor
    /// underneath, and a stray Enter/keystroke while the dialog is open
    /// would land there instead of being inert.
    focus_handle: gpui::FocusHandle,
}

/// G5 Task 4 (folded T3 review issue 2 — dirty guard): the action
/// `discard_confirm` performs on "Zahodit", or undoes (where applicable) on
/// "Zrušit" — see `DiscardConfirmState`'s doc comment for the three sites
/// that construct one instead of proceeding directly.
enum PendingDiscard {
    /// Close tab `id` outright (`Tabs::close`) — the tab strip's "✕".
    CloseTab { id: u64 },
    /// Run `sql` as `preview` through the normal `run_query_with` pipeline
    /// on "Zahodit" — used both for "re-open the same preview from the
    /// tree/palette" and for a ☰ join-toggle re-run. `revert` is `Some((grid,
    /// col, ref_col))` only for the join-toggle case: `ResultGrid::
    /// toggle_fk_column` already flipped `fk_checked` (and `cx.notify()`'d)
    /// before emitting the event that led here, so "Zrušit" must undo
    /// exactly that flip via `revert_fk_toggle` — the same one-off-flip
    /// reasoning `on_grid_event`'s pre-existing busy-guard revert already
    /// documents — or the checkbox is left showing a lie about what's
    /// actually joined.
    RunPreview {
        sql: String,
        preview: Box<PreviewTarget>,
        revert: Option<(Entity<ResultGrid>, usize, String)>,
    },
}

/// G5 Task 4 (folded T3 review issue 2): confirm prompt for an action that
/// would otherwise silently drop a dirty preview tab's staged edits — the
/// three sites: `on_grid_event`'s `GridEvent::RerunPreviewJoins` arm (a ☰
/// toggle re-runs the SAME tab's preview, replacing its grid entity and
/// hence its `EditState`), `TreeEvent::OpenPreview`/`PaletteItem::Table`
/// (re-opening the same (schema, table) closes the existing tab via
/// `Tabs::close_by_preview_key` before opening the fresh one), and the tab
/// strip's "✕" (`Tabs::close`). "Zrušit" aborts the action outright — no
/// query runs, no tab closes — via `on_discard_confirm_no`.
///
/// KNOWN GAP (T4 review round 1 NIT, not fixed here — pre-existing app-wide
/// behaviour): closing the whole app window/quitting while a preview tab is
/// dirty has NO equivalent guard — staged edits are simply lost, same as
/// they always were before this dirty-guard existed for in-app actions.
/// Only in-app actions that would silently replace/close a TAB are covered.
struct DiscardConfirmState {
    /// Row-granular staged-change count (brief: "Neuložené změny ({n})").
    change_count: usize,
    action: PendingDiscard,
}

struct AppView {
    tabs: Tabs,
    status: String,
    runner: QueryRunner,
    /// Back-compat CLI-arg connection string (phase 0-2 path). `None` when
    /// the app was started with no argument (Task 7's new startup path) or
    /// once a saved connection has been switched to.
    conn_url: Option<String>,
    sql: Entity<SqlInput>,
    cancel: Option<CancelToken>,
    started_at: Option<std::time::Instant>,
    /// Bumped at the top of every `run_query_with` dispatch; captured by
    /// that run's spawned event loop as `my_generation`. Final review
    /// fix #2: the loop's post-`while` tail clears `view.cancel` for its
    /// own run, but `rx.recv().await` can go `Pending` after the terminal
    /// event has already been processed (the runner thread hasn't yet
    /// dropped its sender) — a new run can legitimately start in that
    /// gap and set a fresh `cancel`. The tail (and any other end-of-run
    /// tail mutation) applies only when `run_generation` still matches,
    /// so a newer run's state is never clobbered by an older run's
    /// finally-arriving channel close. Supersedes the narrower
    /// `retriggered` flag, which only covered the Task 6 saved-fk-join
    /// retrigger — this covers every way a new run can start in that
    /// window (Ctrl+Enter, palette, preview, a ☰ toggle).
    run_generation: u64,
    // --- Task 7: connection manager state ---
    config: AppConfig,
    config_path: PathBuf,
    /// Set when `AppConfig::load` failed to parse an existing config.toml at
    /// startup (surfaced in the status bar; see `main`). Cleared by
    /// `finish_save` once the corrupt file has been safely moved aside to
    /// `config.toml.corrupt-bak` — never overwritten silently (final-review
    /// must-fix #2).
    config_load_error: Option<String>,
    vault_path: PathBuf,
    /// Unlocked vault, kept for the session once the user has entered the
    /// master password once (brief: prompt on first use, not at startup).
    vault: Option<Vault>,
    active_connection_id: Option<String>,
    /// Bumped on every dropdown connection switch; a switch result only
    /// applies if the generation still matches (last-dispatched wins, not
    /// last-resolved).
    switch_generation: u64,
    dropdown_open: bool,
    modal: Option<connections_ui::ModalState>,
    /// Cached folder/favourite grouping of `config.connections`, recomputed
    /// on dropdown-open and after config mutations (see
    /// `AppView::refresh_grouped_cache`) rather than on every render frame.
    grouped_cache: connections_ui::GroupedConnections,
    // --- G2 Task 6: schema tree panel ---
    /// Loading/error/snapshot state lives on the entity itself, driven by
    /// direct mutation from `trigger_schema_fetch` (see schema_tree.rs's
    /// header comment for why this isn't done via `TreeEvent` instead).
    tree: Entity<SchemaTree>,
    /// Ctrl+B (`ToggleTree`, app action, binding context `None`). `false`
    /// means the panel isn't rendered at all (0 px), not just visually
    /// hidden.
    tree_visible: bool,
    /// Bumped on every `trigger_schema_fetch` dispatch; a fetch result only
    /// applies if the generation still matches (last-dispatched wins — same
    /// pattern as `switch_generation`). Fixes review Issue 1: without this,
    /// a slow fetch for a connection the user has since switched away from
    /// can resolve after a faster fetch for the new connection and silently
    /// overwrite the tree with the wrong connection's schema.
    schema_fetch_generation: u64,
    /// Identity (see `conn_spec_key`) of the connection whose schema is
    /// currently being fetched/shown in `tree`, so `trigger_schema_fetch`
    /// can tell `SchemaTree::set_snapshot` whether an incoming snapshot is a
    /// same-connection refresh (preserve expand/filter/selection) or a
    /// switch to a different connection (reset them) — review Issue 3.
    schema_tree_connection_key: Option<String>,
    // --- G3 Task 3: history panel + query recording ---
    /// Opened from `default_history_path()` at startup; `None` when the open
    /// failed (surfaced once in the startup status — see `main`), in which
    /// case the app stays fully functional, just without recording/search
    /// (`record_history` and the panel's search both no-op gracefully).
    history: Option<HistoryDb>,
    /// Ctrl+H (`ToggleHistory`, app action, binding context `None`) — same
    /// "not rendered at all when hidden" convention as `tree_visible`.
    history_visible: bool,
    /// Search box for the history panel (unmasked `TextField`, reused from
    /// connections_ui.rs). Its text is polled (cheap string compare) at the
    /// start of every `render_history_panel` call against
    /// `last_history_query` to detect an edit — see history_panel.rs's
    /// module doc comment for the full caching strategy.
    history_search: Entity<connections_ui::TextField>,
    /// Cached result of the last `HistoryDb::search`, recomputed only by
    /// `AppView::refresh_history_cache` (startup, after a recorded run, star
    /// toggle, ToggleHistory-on, and search-text change detected in
    /// `render_history_panel`) rather than on every render frame — same
    /// precedent as `grouped_cache`. Post-review fix for Task 3 review
    /// Issue 1 (unindexed full-table sort on every window repaint).
    history_cache: Vec<HistoryEntry>,
    /// The search text `history_cache` was last computed from, compared
    /// against `history_search`'s live text each render to decide whether a
    /// refresh is needed (see `history_search`'s doc comment).
    last_history_query: String,
    // --- G3 Task 5: Ctrl+K command palette ---
    /// `None` when the palette isn't open — same "not rendered at all"
    /// convention as `modal`, and mutually exclusive with it (see
    /// `on_open_palette`/`render_palette_overlay`).
    palette: Option<PaletteState>,
    // --- G4 Task 6: per-table view memory ---
    /// Opened from `dbc_state::default_view_prefs_path()` at startup;
    /// `None` when the open failed (surfaced once in the startup status —
    /// see `main`), in which case the feature is simply off — no apply, no
    /// save — the rest of the app is fully functional either way, same
    /// "degrade gracefully" precedent as `history: Option<HistoryDb>`.
    view_prefs: Option<ViewPrefsStore>,
    // --- G5 Task 4: apply flow ---
    /// The Apply confirmation dialog (brief contract #1/#3/#4) — `None` when
    /// closed. Owned separately from `modal` (`connections_ui::ModalState`,
    /// a different concern/lifecycle) but mutually exclusive with it in
    /// practice; see `ApplyDialogState`'s doc comment.
    apply_dialog: Option<ApplyDialogState>,
    /// G5 Task 4 (folded T3 review issue 2 — dirty guard): a pending
    /// confirm-discard prompt, shown before any action that would silently
    /// drop a dirty preview tab's staged edits. `None` when no such prompt
    /// is pending; see `DiscardConfirmState`'s doc comment for the three
    /// trigger sites.
    discard_confirm: Option<DiscardConfirmState>,
}

/// Stable identity for a `ConnectSpec`, used only to decide whether two
/// `trigger_schema_fetch` dispatches target the "same connection" (see
/// `schema_tree_connection_key`) — not used for anything security-sensitive,
/// so the secret on `ConnectSpec::Config` is deliberately not part of it.
fn conn_spec_key(spec: &ConnectSpec) -> String {
    match spec {
        ConnectSpec::Config { cfg, .. } => format!("cfg:{}", cfg.id),
        ConnectSpec::Url(u) => format!("url:{u}"),
    }
}

/// G5 Task 4 review fix (BLOCKER 1): sentinel `ResultTab::conn_identity`/
/// `AppView::current_conn_identity` use for the CLI-arg back-compat path
/// (no saved `ConnectionConfig`, hence no stable id to use instead).
const CLI_CONN_IDENTITY: &str = "cli";

/// G5 Task 4 review fix (BLOCKER 1): pure decision behind the Apply flow's
/// connection-identity guard — `true` when it is safe to apply `tab`'s
/// staged edits against the connection identified by `current`. Trivial by
/// design (a plain equality check on two pre-resolved identity strings) —
/// pulled out as a named, independently testable function rather than an
/// inline `==` at each of the three call sites (`on_open_apply_dialog`,
/// `on_confirm_apply`, `render_apply_bar`) so a future change to the
/// comparison rule (e.g. treating a deleted-then-recreated connection with
/// the same id specially) has one place to land, and so the guard's
/// intent is documented once instead of three times.
fn conn_identity_matches(tab_identity: &str, current: &str) -> bool {
    tab_identity == current
}

impl AppView {
    fn on_run_query(&mut self, _: &RunQuery, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_query(false, cx);
    }

    /// `Ctrl+Shift+Enter`: bypasses ONLY the auto-limit guard. Read-only
    /// enforcement is not a "per-run convenience" the way auto-limit is —
    /// it stays enforced regardless of how the query was launched.
    fn on_run_query_unlimited(
        &mut self,
        _: &RunQueryUnlimited,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_query(true, cx);
    }

    /// Guard order (brief, Task 8): (1) read-only — rejected without ever
    /// connecting; (2) auto-limit — rewrites the SQL text, unless bypassed;
    /// (3) timeout — enforced inside `QueryRunner::connect_and_run`, since it
    /// must race the whole connect+query sequence, not just this call.
    ///
    /// Reads the SQL straight from the editor and delegates to
    /// `run_query_with` — the editor-typed-query path, as opposed to a
    /// preview's `run_query_with` call (see `on_tree_event`'s
    /// `TreeEvent::OpenPreview` arm), which supplies its own SQL/title and
    /// never touches `self.sql`.
    fn run_query(&mut self, bypass_auto_limit: bool, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).text();
        if sql.trim().is_empty() {
            return;
        }
        self.run_query_with(sql, None, bypass_auto_limit, cx);
    }

    /// The actual guarded run pipeline (guard order per the doc comment on
    /// `run_query`), shared by an editor-typed query (`run_query`, `preview
    /// == None`) and a schema-tree preview (`TreeEvent::OpenPreview`,
    /// `preview == Some(..)`). `sql` is whatever the caller wants executed —
    /// for a preview this is `preview_sql`'s output, never the editor's
    /// text. `bypass_auto_limit` is still the caller's choice (a preview
    /// always passes `true` since it already carries its own `LIMIT`).
    fn run_query_with(
        &mut self,
        sql: String,
        preview: Option<PreviewTarget>,
        bypass_auto_limit: bool,
        cx: &mut Context<Self>,
    ) {
        // G5 Task 4: also refuse under the Apply dialog / discard-confirm
        // prompt — both are `.occlude()`d overlays like `modal`, but a
        // GLOBAL keybinding (Ctrl+Enter) isn't blocked by occlusion the way
        // a click is, so this guard is the actual mechanism that stops a
        // stray Ctrl+Enter from starting a new run while either is up.
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return; // don't run queries under a modal/dialog/confirm prompt
        }
        if self.cancel.is_some() {
            return; // one query at a time in v1
        }
        if sql.trim().is_empty() {
            return;
        }

        let spec = if let Some(id) = self.active_connection_id.clone() {
            let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return;
            };
            let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
            // G5 Task 3: captured before `cfg` moves into `ConnectSpec::Config`
            // below — `Started`'s `Editable` detection needs both facts (see
            // `detect_editable_pk`), and `cfg` itself won't survive past this
            // `if` arm.
            let conn_meta = Some((cfg.read_only, cfg.engine));
            (cfg.read_only, cfg.auto_limit, cfg.timeout_secs, conn_meta, ConnectSpec::Config { cfg: Box::new(cfg), secret })
        } else if let Some(url) = self.conn_url.clone() {
            // CLI-arg back-compat path: no `ConnectionConfig` exists for
            // read-only/auto-limit/timeout, but a preview IS still
            // editable through it (G5 Task 3) — always writable (no
            // read-only concept for this path) with the engine inferred
            // from the URL itself via `engine_from_url`, the same
            // postgres-vs-sqlite dispatch `connect::open` already uses.
            let conn_meta = Some((false, engine_from_url(&url)));
            (false, None, None, conn_meta, ConnectSpec::Url(url))
        } else {
            self.status = "Bez připojení — vyberte připojení nahoře.".into();
            cx.notify();
            return;
        };
        let (read_only, auto_limit, timeout_secs, conn_meta, spec) = spec;

        // Guard 1: read-only — rejected client-side without connecting.
        // (Server-side enforcement lives in connect::open_config: Postgres
        // `default_transaction_read_only=on`, SQLite `SQLITE_OPEN_READ_ONLY`
        // — this check is the fast, no-connection-needed first line, not the
        // only line.)
        if read_only && !is_read_statement(&sql) {
            let err = QueryError::msg("connection is read-only");
            self.status = format!("error: {err}");
            cx.notify();
            return;
        }

        // Guard 2: auto-limit.
        let mut sql = sql;
        let mut limit_suffix = String::new();
        if !bypass_auto_limit {
            if let Some(n) = auto_limit {
                let (rewritten, changed) = apply_auto_limit(&sql, n);
                if changed {
                    sql = rewritten;
                    limit_suffix = format!(" · auto-LIMIT {n}");
                }
            }
        }

        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.started_at = Some(std::time::Instant::now());
        // Final review fix #2: this run's identity, checked by its own
        // spawned loop's tail before mutating `view.cancel` — see
        // `run_generation`'s doc comment.
        self.run_generation += 1;
        let my_generation = self.run_generation;
        self.status = format!("connecting…{limit_suffix}");
        cx.notify();

        // Captured for the new tab's title (single-line-collapsed SQL, see
        // `tabs::collapse_title`) — the actual SQL text being run, i.e.
        // post-auto-limit-rewrite. Unused when `preview` overrides the title
        // (still harmless to compute — the collapse is cheap).
        let sql_for_title = sql.clone();
        // G3 Task 3: captured at dispatch (not resolution) for
        // `record_history` — the unix time the run started, and the active
        // connection's name (or "cli" for the CLI-arg path), both fixed for
        // the lifetime of this run regardless of what the user does
        // meanwhile (e.g. switching connections while this query runs).
        let history_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let history_conn_name = self.active_connection_name_for_history();
        // G5 Task 4 review fix (BLOCKER 1): stamped onto the freshly-opened
        // tab (`Started`, below) so the Apply flow can later tell whether
        // the active connection has since changed out from under it — see
        // `current_conn_identity`'s doc comment.
        let conn_identity = self.current_conn_identity();
        let mut rx = self.runner.connect_and_run(spec, sql, cancel, timeout_secs);
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            // Set (to the buffer-push error text) once a buffer push fails;
            // suppresses further batch processing for this run while the
            // cancel we just fired propagates through the driver. The
            // captured text is what actually gets recorded to history when
            // the run's terminal event (`Finished` or `Failed` — the driver
            // sends exactly one) eventually arrives — review Issue 2: a
            // spill failure must still produce a history entry.
            let mut errored: Option<String> = None;
            // This run's own tab id, set once `Started` opens it. `Batch`
            // events target this tab specifically (by id, not "the active
            // tab") — if the tab was closed mid-stream, the run cancels
            // itself and stops consuming further events.
            let mut tab_id: Option<u64> = None;
            // G4 Task 6: set by `apply_view_prefs_to_grid` when a saved
            // fk-join needs re-triggering (this run's own preview target
            // didn't already carry joins, but the saved prefs do) — deferred
            // until this run's `Finished` clears `view.cancel` (see the
            // `QueryEvent::Finished` arm below), since `run_query_with`'s
            // one-query-at-a-time guard would otherwise silently drop it if
            // dispatched from inside `Started`.
            let mut pending_join_retrigger: Option<PreviewTarget> = None;
            // G4 Task 6: when `pending_join_retrigger` is actually
            // dispatched below, `view.run_query_with` synchronously bumps
            // `view.run_generation` and sets a FRESH `view.cancel` for
            // that new run before this closure returns — so the tail
            // cleanup after the loop, guarded on `run_generation` still
            // matching `my_generation` (final review fix #2), naturally
            // skips clobbering it, the same as it does for any other run
            // started while this one's channel-close is still pending.
            while let Some(ev) = rx.recv().await {
                let stop = this
                    .update(cx, |view, cx| {
                        let mut stop = false;
                        match ev {
                            QueryEvent::Started { columns } => {
                                let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
                                buffer = Some(buf.clone());
                                // G4 Task 5: FK metadata for the ☰ menu —
                                // computed BEFORE the grid entity exists
                                // (needs `view`/`cx` to read the schema-tree
                                // snapshot) so it can be handed to
                                // `set_fk_info` in the same `grid.update`
                                // below as `set_buffer`/`set_table_name`.
                                let result_cols: Vec<String> = buf
                                    .borrow()
                                    .schema()
                                    .fields()
                                    .iter()
                                    .map(|f| f.name().to_string())
                                    .collect();
                                let (fk_info, ref_cols) = if let Some(p) = &preview {
                                    view.fk_info_for_table(p.schema.as_deref(), &p.table, &result_cols, cx)
                                } else {
                                    view.fk_info_for_adhoc(&result_cols, cx)
                                };
                                // G5 Task 3: editability — PREVIEW tabs only
                                // (brief contract #1); `no_pk_notice` is
                                // surfaced as `view.status` further below,
                                // AFTER the "running…" status this arm
                                // already sets, so it isn't immediately
                                // clobbered.
                                let (editable, no_pk_notice) = if let Some(p) = &preview {
                                    let numeric_cols: Vec<bool> = buf
                                        .borrow()
                                        .schema()
                                        .fields()
                                        .iter()
                                        .map(|f| f.data_type().is_numeric())
                                        .collect();
                                    view.editable_for_preview(
                                        p.schema.as_deref(),
                                        &p.table,
                                        &result_cols,
                                        conn_meta,
                                        numeric_cols,
                                        cx,
                                    )
                                } else {
                                    (None, false)
                                };
                                let grid = cx.new(ResultGrid::new);
                                grid.update(cx, |g, cx| {
                                    g.set_buffer(buf.clone(), cx);
                                    // G4 Task 4: a preview tab knows its
                                    // source table (used as the `INSERT
                                    // INTO` target for exports) — an
                                    // ad-hoc SQL-editor run doesn't, and
                                    // keeps `set_buffer`'s "export"
                                    // placeholder.
                                    if let Some(p) = &preview {
                                        g.set_table_name(p.table.clone());
                                        g.set_preview_context(p.schema.clone(), p.key.clone(), p.title.clone());
                                    }
                                    g.set_fk_info(fk_info, ref_cols);
                                    // G5 Task 3: `None` on an ad-hoc tab
                                    // (never editable) or a preview that
                                    // failed one of `detect_editable_pk`'s
                                    // checks — `set_editable`'s default.
                                    g.set_editable(editable);
                                    // G4 Task 5: restores ☰-menu checkmarks
                                    // + join tinting on the FRESH grid
                                    // entity a preview re-run just created
                                    // (this grid replaces the one that
                                    // emitted `RerunPreviewJoins` — see
                                    // `GridEvent`'s doc comment). A no-op
                                    // (empty `joins`) for a plain preview
                                    // open or an ad-hoc tab.
                                    if let Some(p) = &preview {
                                        g.apply_active_joins(&p.joins);
                                    }
                                });
                                // G4 Task 6: per-table view memory — apply
                                // saved hidden/sort/widths to this fresh
                                // grid now that column names are known, and
                                // (loop-guarded) queue a saved fk-join
                                // re-run if this Started event isn't already
                                // ITS result. See `apply_view_prefs_to_grid`'s
                                // doc comment for the full design.
                                if let Some(p) = &preview {
                                    if let Some(retrigger) =
                                        view.apply_view_prefs_to_grid(&grid, p, &result_cols, cx)
                                    {
                                        pending_join_retrigger = Some(retrigger);
                                    }
                                }
                                // G4 Task 5: one subscription per grid
                                // entity — see `GridEvent`'s doc comment for
                                // why the event payload carries everything
                                // `on_grid_event` needs rather than this
                                // callback searching `self.tabs`.
                                cx.subscribe(&grid, AppView::on_grid_event).detach();
                                let title = preview
                                    .as_ref()
                                    .map(|p| p.title.clone())
                                    .unwrap_or_else(|| collapse_title(&sql_for_title));
                                // Brief contract #1: re-preview of the same
                                // (schema, table) replaces its existing
                                // preview tab rather than stacking a
                                // duplicate — must happen before `open` so
                                // the closed tab never overlaps the new one.
                                if let Some(p) = &preview {
                                    view.tabs.close_by_preview_key(&p.key);
                                }
                                let id = view.tabs.open(ResultTab {
                                    id: 0,
                                    title,
                                    pinned: false,
                                    preview_key: preview.as_ref().map(|p| p.key.clone()),
                                    // G5 Task 4 review fix (BLOCKER 1).
                                    conn_identity: conn_identity.clone(),
                                    content: TabContent::Grid { grid, buffer: buf },
                                });
                                tab_id = Some(id);
                                view.status = format!("running…{limit_suffix}");
                                // G5 Task 3, brief contract #5: a PK-less
                                // table is flagged regardless of read-only/
                                // engine — set AFTER the "running…" status
                                // above so it isn't immediately clobbered by
                                // it (streamed `Batch`/`Finished` status
                                // updates will still eventually overwrite
                                // this, same as every other transient
                                // status note in this file).
                                if no_pk_notice {
                                    view.status =
                                        "tabulka nemá primární klíč — jen pro čtení".to_string();
                                }
                            }
                            QueryEvent::Batch(b) => {
                                if errored.is_some() {
                                    // Already failed and cancelled this run —
                                    // drop any further in-flight batches.
                                } else if tab_id.is_some_and(|id| view.tabs.iter().all(|t| t.id != id)) {
                                    // This run's tab was closed mid-stream —
                                    // cancel and stop consuming; nothing left
                                    // to render the remaining batches into.
                                    stop = true;
                                    if let Some(token) = view.cancel.take() {
                                        token.cancel();
                                    }
                                    view.status = "zrušeno (tab zavřen)".into();
                                } else if let Some(Err(e)) =
                                    buffer.as_ref().map(|buf| buf.borrow_mut().push(b))
                                {
                                    let err_text = e.to_string();
                                    view.status = format!("error: {err_text}");
                                    errored = Some(err_text);
                                    if let Some(token) = view.cancel.take() {
                                        token.cancel();
                                    }
                                } else {
                                    let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                                    let secs =
                                        view.started_at.map_or(0.0, |t| t.elapsed().as_secs_f32());
                                    view.status = format!("{rows} rows… {secs:.1}s{limit_suffix}");
                                    // G4 Task 2: let this tab's grid know it
                                    // grew. When no sort/filter is active
                                    // this is a cheap identity-count refresh;
                                    // when one IS active it just marks dirty
                                    // rather than resorting on every batch
                                    // (see `ResultGrid::on_batch_grown`) —
                                    // the actual resort is deferred to
                                    // `Finished` below.
                                    if let Some(id) = tab_id {
                                        if let Some(TabContent::Grid { grid, .. }) =
                                            view.tabs.iter().find(|t| t.id == id).map(|t| &t.content)
                                        {
                                            grid.update(cx, |g, _| g.on_batch_grown());
                                        }
                                    }
                                }
                            }
                            // The driver sends exactly one terminal event
                            // per run (`Finished` xor `Failed` —
                            // `runner::stream_query`), so exactly one of
                            // these two arms fires, and each records
                            // history exactly once (review Issue 2): when a
                            // buffer-push spill error already latched
                            // (`errored`), record that as the failed entry
                            // (its text is the real root cause; a queued
                            // `Finished`'s fake success or `Failed`'s
                            // redundant "cancelled" text would be wrong)
                            // and leave the status bar alone — it already
                            // shows the spill error from the `Batch` arm
                            // above (bb2dd7c: never clobber it with a stale
                            // status). Otherwise record the terminal
                            // event's own outcome and update the status bar
                            // as before.
                            QueryEvent::Finished { elapsed } => {
                                // G4 Task 2: if a sort/filter was active on
                                // this tab's grid while batches streamed in,
                                // `on_batch_grown` deferred resorting rather
                                // than doing it per-batch — do the one
                                // deferred rebuild now. `None` when there was
                                // nothing deferred (identity view, already
                                // current).
                                let sort_note = tab_id.and_then(|id| {
                                    view.tabs.iter().find(|t| t.id == id).and_then(|t| {
                                        match &t.content {
                                            TabContent::Grid { grid, .. } => {
                                                grid.update(cx, |g, _| g.on_stream_finished())
                                            }
                                            TabContent::Text { .. } => None,
                                        }
                                    })
                                });
                                match &errored {
                                    None => {
                                        let rows =
                                            buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                                        view.status =
                                            format!("{rows} rows in {elapsed:.2?}{limit_suffix}");
                                        if let Some(note) = &sort_note {
                                            view.status.push_str(&format!(" · {note}"));
                                        }
                                        // G3 Task 3: record the run (previews
                                        // included — they run real SQL too).
                                        // Fire-and-forget; a write failure never
                                        // surfaces here.
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            Some(elapsed.as_millis() as i64),
                                            Some(rows as i64),
                                            None,
                                            cx,
                                        );
                                    }
                                    Some(err_text) => {
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            None,
                                            None,
                                            Some(err_text),
                                            cx,
                                        );
                                    }
                                }
                                view.cancel = None;
                                // G4 Task 6: fire the deferred saved-fk-join
                                // re-run now that `view.cancel` is clear —
                                // only on a genuine success (an errored run
                                // has no valid base result to re-join), see
                                // `pending_join_retrigger`'s doc comment.
                                //
                                // Review fix (Task 6 round 1, Issue 2): also
                                // require this run's tab to still be open —
                                // same "tab closed mid-stream" guard the
                                // `Batch` arm above already applies. Without
                                // it, a tab closed in the gap between the
                                // last `Batch` (or `Started`, if there were
                                // none) and `Finished` never gets noticed,
                                // and the retrigger silently re-opens a
                                // preview tab the user just explicitly
                                // closed.
                                if errored.is_none() {
                                    if let Some(pt) = pending_join_retrigger.take() {
                                        let tab = tab_id
                                            .and_then(|id| view.tabs.iter().find(|t| t.id == id));
                                        let tab_still_open = tab.is_some();
                                        // G5 Task 4 review fix (MAJOR 3): a
                                        // dirty tab must not have its grid
                                        // entity (and staged EditState)
                                        // silently replaced by this
                                        // retrigger — see
                                        // `decide_retrigger_action`'s doc
                                        // comment.
                                        let dirty = tab
                                            .and_then(|t| AppView::grid_dirty_change_count(t, cx));
                                        match decide_retrigger_action(tab_still_open, dirty) {
                                            RetriggerAction::Run => {
                                                let sql = fk_join::build_join_sql(
                                                    pt.schema.as_deref(),
                                                    &pt.table,
                                                    &pt.joins,
                                                );
                                                view.run_query_with(sql, Some(pt), true, cx);
                                            }
                                            RetriggerAction::SkipDirty => {
                                                view.status =
                                                    "FK joins neaktualizovány — máš rozpracované změny"
                                                        .to_string();
                                            }
                                            RetriggerAction::SkipClosed => {}
                                        }
                                    }
                                }
                            }
                            QueryEvent::Failed(e) => {
                                match &errored {
                                    None => {
                                        view.status = format!("error: {e}");
                                        let err_text = e.to_string();
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            None,
                                            None,
                                            Some(&err_text),
                                            cx,
                                        );
                                    }
                                    Some(err_text) => {
                                        view.record_history(
                                            &sql_for_title,
                                            &history_conn_name,
                                            history_started_at,
                                            None,
                                            None,
                                            Some(err_text),
                                            cx,
                                        );
                                    }
                                }
                                view.cancel = None;
                            }
                        }
                        cx.notify();
                        stop
                    })
                    .unwrap_or(false);
                if stop {
                    break;
                }
            }
            let _ = this.update(cx, |view, cx| {
                // Final review fix #2: only clear `view.cancel` if no
                // newer run has started since — `rx.recv()` above can go
                // `Pending` after the terminal event was already
                // processed (the runner thread hasn't dropped its sender
                // yet), and a new run legitimately started in that gap
                // (Ctrl+Enter, palette, preview, a ☰ toggle, or this run's
                // own saved-fk-join retrigger) already bumped
                // `run_generation` and set its own `cancel` — an
                // unconditional clear here would wipe that out from under
                // it. See `run_generation`'s doc comment.
                if view.run_generation == my_generation {
                    view.cancel = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_cancel_query(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut Context<Self>) {
        // M6: Escape closes the dropdown / a modal first, rather than
        // falling through to query-cancel underneath it. A modal holding
        // unsaved password state (a master-password prompt/creation modal,
        // or the connection dialog with a non-empty password field) is
        // deliberately NOT closed by Escape — same "no accidental dismissal
        // while a password is typed" reasoning as the overlay `.occlude()`
        // fix.
        // G3 Task 5: this check is THE mechanism that makes Esc close the
        // palette — do not remove it as "redundant". The palette's scoped
        // "escape" binding (context "Palette", palette.rs `bind_keys`) does
        // NOT win GPUI's keymap resolution: focus sits on the palette's
        // nested TextField, so "Palette" is an ancestor context, and per the
        // pinned gpui's `keymap.rs::bindings_for_input` an unscoped binding
        // (this `escape → CancelQuery`) outranks ancestor-scoped ones.
        // Verified against the vendored source in the Task 5 review.
        if self.palette.is_some() {
            self.palette = None;
            cx.notify();
            return;
        }
        if self.dropdown_open {
            self.dropdown_open = false;
            cx.notify();
            return;
        }
        if let Some(modal) = self.modal.clone() {
            let closable = match &modal {
                connections_ui::ModalState::ConnectionDialog(ui) => ui.password.read(cx).text().is_empty(),
                _ => false,
            };
            if closable {
                self.close_modal(cx);
            }
            return;
        }
        // G5 Task 4: Esc on the discard-confirm prompt is exactly "Zrušit"
        // — abort the pending action (reverting a speculative ☰-toggle flip
        // when there is one), never "Zahodit" (Esc must never be the thing
        // that destroys staged edits).
        if self.discard_confirm.is_some() {
            self.on_discard_confirm_no(cx);
            return;
        }
        // G5 Task 4: Esc on the Apply dialog closes it (edits stay staged,
        // same as its own "Zrušit" button) — but ONLY while not `running`:
        // a write already in flight has no cancellation support in v1
        // (`Connection::execute`'s "no mid-statement interrupt" design
        // note), so Esc here would just detach the UI from a result it
        // still needs to react to deterministically, not actually stop
        // anything server-side.
        if let Some(ad) = &self.apply_dialog {
            if !ad.running {
                self.apply_dialog = None;
                cx.notify();
            }
            return;
        }
        // G4 Task 3: Esc closes an open cell-detail popup / find bar on the
        // active tab's grid before falling through to query-cancel — same
        // "no scoped-binding shortcut, the check here IS the mechanism"
        // reasoning as the palette/modal cases above (a `"ResultGrid"`-
        // scoped `escape` binding would lose to this unscoped one).
        if let Some(active) = self.tabs.active() {
            if let TabContent::Grid { grid, .. } = &active.content {
                let closed = grid.update(cx, |g, _| g.close_overlay_if_open());
                if closed {
                    cx.notify();
                    return;
                }
            }
        }
        if let Some(c) = self.cancel.take() {
            c.cancel();
            self.status = "cancelling…".into();
            cx.notify();
        }
    }

    fn on_toggle_tree(&mut self, _: &ToggleTree, _window: &mut Window, cx: &mut Context<Self>) {
        self.tree_visible = !self.tree_visible;
        cx.notify();
    }

    fn on_toggle_history(&mut self, _: &ToggleHistory, _window: &mut Window, cx: &mut Context<Self>) {
        self.history_visible = !self.history_visible;
        if self.history_visible {
            // The cache may be stale relative to runs recorded while the
            // panel was hidden (record_history's own refresh still ran, but
            // this is cheap insurance and matches the review's explicit
            // "ToggleHistory-on" trigger list).
            self.refresh_history_cache(cx);
        }
        cx.notify();
    }

    /// Ctrl+K (brief contract #1). Guarded against another modal being up
    /// (contract #5) — the reverse (a modal opening while the palette is up)
    /// is prevented for free by the palette overlay's `.occlude()` blocking
    /// the clicks that would open one (top-bar/dropdown), same as the
    /// existing modal overlay does for the dropdown. Also closes the
    /// connection dropdown if it happened to be open, so the two overlays
    /// never stack. Sources are assembled fresh on every open (contract #2).
    fn on_open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some()
            || self.palette.is_some()
            || self.apply_dialog.is_some()
            || self.discard_confirm.is_some()
        {
            return;
        }
        self.dropdown_open = false;
        let input = cx.new(|cx| connections_ui::TextField::new(cx, "Ctrl+K – tabulky, historie, spojení, akce…", false));
        let focus = input.focus_handle(cx);
        let items = self.build_palette_items("", cx);
        self.palette = Some(PaletteState { input, items, selected: 0, last_query: String::new() });
        // G1 lesson (binding per the brief): focus must move to the
        // palette's own input in the SAME update the overlay appears in, or
        // a stray keystroke lands on whatever had focus before Ctrl+K.
        window.focus(&focus, cx);
        cx.notify();
    }

    fn on_palette_up(&mut self, _: &palette::PaletteUp, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.palette {
            p.selected = p.selected.saturating_sub(1);
        }
        cx.notify();
    }

    fn on_palette_down(&mut self, _: &palette::PaletteDown, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.palette {
            if p.selected + 1 < p.items.len() {
                p.selected += 1;
            }
        }
        cx.notify();
    }

    fn on_palette_confirm(&mut self, _: &palette::PaletteConfirm, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.palette.as_ref().and_then(|p| p.items.get(p.selected).cloned()) else { return };
        self.execute_palette_item(item, window, cx);
    }

    fn on_palette_close(&mut self, _: &palette::PaletteClose, _window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        cx.notify();
    }

    /// Recomputes `palette.items` from `palette.input`'s current text — same
    /// lazy "compare against last-computed text at render time" trigger as
    /// `history_panel`'s `refresh_history_cache` (see `render_palette_overlay`).
    /// Resets `selected` to 0 since a re-ranked list makes the previous
    /// index meaningless.
    fn refresh_palette_items(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.palette.as_ref().map(|p| p.input.read(cx).text()) else { return };
        let items = self.build_palette_items(&query, cx);
        if let Some(p) = &mut self.palette {
            p.items = items;
            p.selected = 0;
            p.last_query = query;
        }
    }

    /// Assembles + ranks every palette source (brief contract #2): tables/
    /// views from the tree's current snapshot (with the favourite bonus —
    /// matched against `config.favourite_objects` filtered to the active
    /// connection, kind "table"|"view"), history top-20 for `query` (via
    /// `HistoryDb::search`, same call `history_panel` makes), every saved
    /// connection, and the 5 fixed actions — delegated to `palette::rank_items`,
    /// the pure scoring/assembly function.
    fn build_palette_items(&self, query: &str, cx: &Context<Self>) -> Vec<PaletteItem> {
        let is_favourite_table = |schema: &Option<String>, name: &str| {
            self.active_connection_id.as_deref().is_some_and(|conn_id| {
                self.config.favourite_objects.iter().any(|f| {
                    f.connection_id == conn_id
                        && &f.schema == schema
                        && f.name == name
                        && (f.kind == "table" || f.kind == "view")
                })
            })
        };
        let tables: Vec<palette::TableSource> = self
            .tree
            .read(cx)
            .snapshot()
            .map(|s| {
                s.tables
                    .iter()
                    .map(|t| palette::TableSource {
                        schema: t.schema.clone(),
                        name: t.name.clone(),
                        favourite: is_favourite_table(&t.schema, &t.name),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let history: Vec<palette::HistorySource> = self
            .history
            .as_ref()
            .and_then(|h| h.search(query, 20).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| palette::HistorySource { id: e.id, sql: e.sql })
            .collect();

        let connections: Vec<palette::ConnectionSource> = self
            .config
            .connections
            .iter()
            .map(|c| palette::ConnectionSource { id: c.id.clone(), name: c.name.clone(), favourite: c.favourite })
            .collect();

        palette::rank_items(query, &tables, &history, &connections, 30)
    }

    /// Brief contract #4: execution routes through EXISTING paths only —
    /// no new execution logic here, just dispatch to the same
    /// methods/pipeline the tree/history-panel/dropdown/actions already use.
    fn execute_palette_item(&mut self, item: PaletteItem, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        match item {
            PaletteItem::Table { schema, name } => {
                // Exactly `on_tree_event`'s `TreeEvent::OpenPreview` arm.
                self.open_table_preview(schema, name, cx);
            }
            PaletteItem::HistoryEntry { sql, .. } => {
                // Exactly the history panel's row click: load into the
                // editor and focus it, never run it.
                self.sql.update(cx, |s, cx| s.set_text(&sql, cx));
                let editor_focus = self.sql.focus_handle(cx);
                window.focus(&editor_focus, cx);
            }
            PaletteItem::Connection { id, .. } => {
                // G3 final-review fix (F3): route through the SAME
                // vault-prompt path the dropdown uses, not straight to
                // `switch_to_connection` — otherwise a locked vault (the
                // normal state at every app start) makes the palette's
                // connection switch dispatch without the secret and die
                // with a connect error instead of prompting for the master
                // password. The palette is already closed (line above)
                // before this call, so `on_dropdown_item_click` opening the
                // `MasterPasswordPrompt` modal cannot conflict with it.
                self.on_dropdown_item_click(id, window, cx);
            }
            PaletteItem::Action { action, .. } => match action {
                PaletteAction::RunQuery => self.run_query(false, cx),
                PaletteAction::ToggleTree => {
                    self.tree_visible = !self.tree_visible;
                }
                PaletteAction::ToggleHistory => {
                    self.history_visible = !self.history_visible;
                    if self.history_visible {
                        self.refresh_history_cache(cx);
                    }
                }
                PaletteAction::NewConnection => {
                    // Exactly the dropdown's "Nové spojení…" click — sets
                    // its own focus, which must win over anything below.
                    self.open_connection_dialog(None, window, cx);
                }
                PaletteAction::RefreshSchema => {
                    // Exactly `on_tree_event`'s `TreeEvent::RefreshRequested` arm.
                    if let Some(spec) = self.active_conn_spec() {
                        self.trigger_schema_fetch(spec, cx);
                    } else {
                        self.schema_tree_connection_key = None;
                        self.tree.update(cx, |t, cx| t.clear(cx));
                    }
                }
            },
        }
        cx.notify();
    }

    /// Centered overlay (brief contract #1), same full-screen-backdrop +
    /// `.occlude()` shape as `connections_ui::render_modal_overlay` — key
    /// context "Palette" on the panel wraps the input so Up/Down/Enter/Esc
    /// (`palette::bind_keys`) resolve even though focus sits on the input's
    /// own nested "TextField" context. `None` (renders nothing) both when
    /// the palette is closed and — belt and suspenders alongside the guard
    /// in `on_open_palette` — while a modal is up.
    fn render_palette_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.palette.is_none() || self.modal.is_some() {
            return None;
        }
        let current_query = self.palette.as_ref().unwrap().input.read(cx).text();
        if current_query != self.palette.as_ref().unwrap().last_query {
            self.refresh_palette_items(cx);
        }
        let p = self.palette.as_ref()?;
        let items = p.items.clone();
        let selected = p.selected;
        let input = p.input.clone();

        let mut list = div().id("palette-list").flex().flex_col().flex_1().overflow_hidden();
        for (ix, item) in items.into_iter().enumerate() {
            let label = palette::display_label(&item);
            let is_selected = ix == selected;
            let bg = if is_selected { rgb(0x45475a) } else { rgb(0x1e1e2e) };
            list = list.child(
                div()
                    .id(("palette-item", ix))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .bg(bg)
                    .text_color(rgb(0xcdd6f4))
                    .hover(|s| s.bg(rgb(0x313244)))
                    .child(label)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.execute_palette_item(item.clone(), window, cx);
                    })),
            );
        }

        let panel = div()
            .id("palette-panel")
            .key_context("Palette")
            .w(px(560.))
            .max_h(px(420.))
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded_md()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_palette_up))
            .on_action(cx.listener(Self::on_palette_down))
            .on_action(cx.listener(Self::on_palette_confirm))
            .on_action(cx.listener(Self::on_palette_close))
            .child(div().px_2().py_2().border_b_1().border_color(rgb(0x45475a)).child(input))
            .child(list);

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000099))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    /// Builds the `ConnectSpec` for the *currently active* connection (saved
    /// config or CLI-arg URL) — used by the schema tree's initial fetch and
    /// its `RefreshRequested` handler. Unlike `run_query`'s spec, callers
    /// here don't need `read_only`/`auto_limit`/`timeout_secs`, so this just
    /// returns the spec. `None` means there's nothing to fetch a schema for
    /// (tree shows "Bez připojení").
    fn active_conn_spec(&self) -> Option<ConnectSpec> {
        if let Some(id) = self.active_connection_id.clone() {
            let cfg = self.config.connections.iter().find(|c| c.id == id)?.clone();
            let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
            Some(ConnectSpec::Config { cfg: Box::new(cfg), secret })
        } else {
            self.conn_url.clone().map(ConnectSpec::Url)
        }
    }

    /// G4 Task 5, PREVIEW tabs: looks the previewed `(schema, table)` up in
    /// the CURRENT schema-tree snapshot and delegates to `fk_info_from_table`.
    /// Empty (`None` for every column) when there's no snapshot yet, or the
    /// table isn't in it (schema fetch still in flight, or a connection that
    /// can't see its own catalog) — same "degrade gracefully" precedent
    /// `TreeEvent::OpenPreview` already sets for a missing snapshot.
    fn fk_info_for_table(
        &self,
        schema: Option<&str>,
        table: &str,
        result_cols: &[String],
        cx: &Context<Self>,
    ) -> (Vec<Option<FkRef>>, Vec<Option<Vec<String>>>) {
        let empty = (vec![None; result_cols.len()], vec![None; result_cols.len()]);
        let Some(snapshot) = self.tree.read(cx).snapshot() else { return empty };
        let Some(t) =
            snapshot.tables.iter().find(|t| t.schema.as_deref() == schema && t.name == table)
        else {
            return empty;
        };
        fk_info_from_table(snapshot, t, result_cols)
    }

    /// G5 Task 3, PREVIEW tabs only: looks the previewed `(schema, table)`
    /// up in the CURRENT schema-tree snapshot (same lookup
    /// `fk_info_for_table` does) and delegates the PK-mapping/read-only/
    /// engine decision to the pure `detect_editable_pk`. Returns
    /// `(editable, no_pk_notice)` — `no_pk_notice` is the brief's "table
    /// found but no PK mapped" case, which `QueryEvent::Started`'s handler
    /// surfaces as a status notice regardless of `editable` (always `None`
    /// in that case).
    fn editable_for_preview(
        &self,
        schema: Option<&str>,
        table: &str,
        result_cols: &[String],
        conn_meta: Option<(bool, dbc_state::Engine)>,
        numeric_cols: Vec<bool>,
        cx: &Context<Self>,
    ) -> (Option<sandbox::Editable>, bool) {
        let Some(snapshot) = self.tree.read(cx).snapshot() else { return (None, false) };
        let t = snapshot.tables.iter().find(|t| t.schema.as_deref() == schema && t.name == table);
        match detect_editable_pk(conn_meta, t, result_cols) {
            EditableDecision::Editable(pk_cols) => {
                (Some(sandbox::Editable { pk_cols, numeric_cols }), false)
            }
            EditableDecision::NoPrimaryKey => (None, true),
            EditableDecision::NotEditable => (None, false),
        }
    }

    /// G4 Task 5, AD-HOC tabs (brief contract #2's documented heuristic):
    /// matches EVERY result column name against each snapshot table's
    /// columns — if exactly ONE table contains all of them, its FK data is
    /// used the same way `fk_info_for_table` does; otherwise (no snapshot,
    /// no match, or more than one plausible source table) returns
    /// all-`None` — no ☰ menu rather than guessing which table an
    /// ambiguous/multi-table-join result actually came from.
    fn fk_info_for_adhoc(
        &self,
        result_cols: &[String],
        cx: &Context<Self>,
    ) -> (Vec<Option<FkRef>>, Vec<Option<Vec<String>>>) {
        let empty = (vec![None; result_cols.len()], vec![None; result_cols.len()]);
        // Review fix (Task 5 round 1, Issue 4): with an empty `result_cols`,
        // `.all(...)` below is vacuously true for EVERY table in the
        // snapshot, so the "exactly one table matches" ambiguity check would
        // incorrectly treat a single-table snapshot as an unambiguous match
        // for a result that has no columns to match against at all. Guard
        // explicitly rather than relying on snapshot cardinality to save us.
        if result_cols.is_empty() {
            return empty;
        }
        let Some(snapshot) = self.tree.read(cx).snapshot() else { return empty };
        let mut matches = snapshot
            .tables
            .iter()
            .filter(|t| result_cols.iter().all(|rc| t.columns.iter().any(|c| &c.name == rc)));
        let Some(t) = matches.next() else { return empty };
        if matches.next().is_some() {
            return empty; // ambiguous — more than one table has every column
        }
        fk_info_from_table(snapshot, t, result_cols)
    }

    /// G4 Task 6: applies saved per-table view prefs (Task 1's
    /// `ViewPrefsStore`) to a PREVIEW tab's just-`Started` grid — hidden
    /// columns/sort/widths always; a saved fk-join is handled specially and
    /// returned to the caller rather than dispatched here directly (see
    /// below). A no-op (returns `None`, touches nothing) when `view_prefs`
    /// failed to load, there's no active connection, or no prefs are saved
    /// for `(connection, p.schema, p.table)` yet.
    ///
    /// **Join re-trigger loop guard**: delegates the save/retrigger/nothing
    /// decision to the pure `decide_join_pref_action` (review fix, Task 6
    /// round 1, Issue 1) rather than inferring it from `p.joins.is_empty()`
    /// alone — the old inference conflated "plain preview open" with "user
    /// explicitly unchecked the last join", so an uncheck-to-zero was read
    /// as a no-op re-open, the empty state was never saved, and the stale
    /// on-disk `fk_joins` kept re-triggering forever. `p.from_join_change`
    /// (`true` only for a `GridEvent::RerunPreviewJoins`-dispatched run)
    /// breaks that ambiguity: an explicit toggle (empty joins or not)
    /// always saves; only a marker-less empty-joins `Started` (a genuine
    /// plain open) can still trigger a saved-fk-join retrigger, and that
    /// retrigger's own `Started` (non-empty joins, no marker) saves and
    /// does not recurse — same loop guard as before, just unambiguous now.
    fn apply_view_prefs_to_grid(
        &mut self,
        grid: &Entity<ResultGrid>,
        p: &PreviewTarget,
        headers: &[String],
        cx: &mut Context<Self>,
    ) -> Option<PreviewTarget> {
        let store = self.view_prefs.as_ref()?;
        let conn_id = self.active_connection_id.clone()?;
        // No saved entry is NOT an early return: a `from_join_change` run on
        // a table with no prior prefs must still reach the Save branch below
        // (re-review issue 3 — otherwise the very first join on a virgin
        // table never persists). Default prefs = nothing to apply, no saved
        // joins.
        let prefs = store
            .get(&conn_id, p.schema.as_deref(), &p.table)
            .cloned()
            .unwrap_or_default();
        let (sort, hidden, widths) = view_prefs_to_grid_state(&prefs, headers);
        grid.update(cx, |g, _| {
            g.set_view_state(sort, hidden);
            for (ix, w) in &widths {
                if let Some(cw) = g.col_widths.get_mut(*ix) {
                    *cw = *w;
                }
            }
        });
        match decide_join_pref_action(p.from_join_change, p.joins.is_empty(), !prefs.fk_joins.is_empty()) {
            JoinPrefAction::Save => {
                self.save_view_prefs_for_grid(grid, cx);
                None
            }
            JoinPrefAction::Nothing => None,
            JoinPrefAction::Retrigger => {
                let join_specs = self.build_join_specs_from_names(
                    &prefs.fk_joins,
                    p.schema.as_deref(),
                    &p.table,
                    headers,
                    cx,
                );
                if join_specs.is_empty() {
                    return None;
                }
                Some(PreviewTarget {
                    title: p.title.clone(),
                    key: p.key.clone(),
                    table: p.table.clone(),
                    schema: p.schema.clone(),
                    joins: join_specs,
                    from_join_change: false,
                })
            }
        }
    }

    /// G4 Task 6: rebuilds `fk_join::JoinSpec`s from saved fk-join COLUMN
    /// NAMES (`TableViewPrefs::fk_joins` — just names, no per-ref-column
    /// selection is persisted) against `headers` (the un-joined result's
    /// current columns) via `fk_info_for_table`. A name with no matching
    /// current column, no FK metadata, or whose referenced table has no
    /// columns at all is silently skipped (same "missing → ignored" contract
    /// as `names_to_ixs`). Since which specific ref-table columns were
    /// checked isn't persisted, every column of the referenced table is
    /// selected — the closest available reconstruction of "this fk was
    /// joined" from the saved shape.
    fn build_join_specs_from_names(
        &self,
        fk_join_names: &[String],
        schema: Option<&str>,
        table: &str,
        headers: &[String],
        cx: &Context<Self>,
    ) -> Vec<fk_join::JoinSpec> {
        let (fk_info, ref_cols) = self.fk_info_for_table(schema, table, headers, cx);
        let mut specs = Vec::new();
        for name in fk_join_names {
            let Some(ix) = headers.iter().position(|h| h == name) else { continue };
            let Some(Some(fk)) = fk_info.get(ix).cloned() else { continue };
            let Some(Some(cols)) = ref_cols.get(ix).cloned() else { continue };
            if cols.is_empty() {
                continue;
            }
            specs.push(fk_join::JoinSpec {
                fk_col: name.clone(),
                ref_schema: fk.schema.clone(),
                ref_table: fk.table.clone(),
                ref_key: fk.column.clone(),
                cols,
            });
        }
        specs
    }

    /// G4 Task 6: persists a PREVIEW tab's CURRENT view state (sort, hidden
    /// columns, widths, active fk-joins) to `view_prefs` — the single save
    /// path used both by `GridEvent::ViewChanged` (sort/visibility/
    /// width-drag-end, emitted directly by `grid.rs`) and by
    /// `apply_view_prefs_to_grid` right after a join re-run's `Started`
    /// event lands (see that method's loop-guard doc comment). A no-op for
    /// an ad-hoc tab (`grid` reports no preview identity), no active
    /// connection, or a disabled `view_prefs` store (load failed at
    /// startup). A save failure (e.g. an unwritable config dir) is
    /// surfaced in the status bar but never blocks the UI action that
    /// triggered it — same "best-effort persistence" precedent
    /// `record_history` already follows.
    fn save_view_prefs_for_grid(&mut self, grid: &Entity<ResultGrid>, cx: &mut Context<Self>) {
        let Some(conn_id) = self.active_connection_id.clone() else { return };
        let (schema, table, headers, sort, hidden, widths, fk_joins) = {
            let g = grid.read(cx);
            let Some((schema, table)) = g.preview_identity() else { return };
            let headers = g.column_names();
            let (sort, hidden) = g.view_state();
            let widths = g.col_widths.clone();
            let fk_joins = g.active_fk_join_names();
            (schema, table, headers, sort, hidden, widths, fk_joins)
        };
        let Some(store) = self.view_prefs.as_mut() else { return };
        let prefs = prefs_from_grid_state(&headers, sort, &hidden, &widths, fk_joins);
        if let Err(e) = store.set(&conn_id, schema.as_deref(), &table, prefs) {
            self.status = format!("error ukládání view prefs: {}", e.message);
        }
    }

    /// G4 Task 5, AD-HOC tabs: `GridEvent::RunLookup`'s handler — resolves
    /// the active connection (brief: "no connection → status error", same
    /// guard `run_query_with` applies), dispatches `runner.fetch_lookup`,
    /// and on success builds one `VirtualCol` per `wanted_cols` entry from
    /// the materialized `(cols, rows)` (`rows[i][0]` is always the key
    /// column — `fk_join::build_lookup_sql` puts it first — `rows[i][1 +
    /// j]` is `wanted_cols[j]`), replacing `grid`'s virtual columns for
    /// `src_col` via `set_virtual_cols_for_src`. `grid` is the SAME entity
    /// that emitted the event (ad-hoc tabs are never replaced, unlike a
    /// preview re-run) — captured from `on_grid_event`'s `emitter` rather
    /// than looked up from `self.tabs`.
    ///
    /// Review fix (Task 5 round 1, Issue 5, informational): this dispatches
    /// `runner.fetch_lookup` regardless of `self.cancel` — no
    /// one-query-at-a-time check here, unlike `run_query_with`. Deliberate,
    /// and safe: `fetch_lookup` opens its own independent connection (same
    /// `open_spec` dispatch every one-shot in `runner.rs` uses —
    /// `connect_and_run`, `fetch_schema`, `test_connect`), so there is no
    /// shared/pooled connection object it could contend with an already-
    /// running query over. Confirmed by reading `open_spec`/every one-shot's
    /// connect path, not assumed.
    ///
    /// Review fix (Task 5 round 1, Issue 1): `generation` is
    /// `GridEvent::RunLookup`'s captured `ResultGrid::lookup_generation`
    /// value at dispatch time. On success, before applying anything, the
    /// completion re-checks two things against the grid's CURRENT state:
    /// - the originating tab is still open with THIS exact grid entity
    ///   (`Entity::update` on a strong handle never fails even if every tab
    ///   referencing it was closed — `grid` here is a strong handle kept
    ///   alive by this very closure — so a missing-entity `Result` can't be
    ///   relied on to detect "tab closed"; `self.tabs` has to be searched
    ///   for a matching `entity_id()` instead), and
    /// - `grid.accept_lookup_result(..)` — generation still current AND
    ///   every `wanted_cols` entry still checked (see that method's doc
    ///   comment for the two races this catches).
    ///
    /// A response that fails either check is dropped silently (no status
    /// override): the newer request that superseded it (if any) will set its
    /// own status when IT resolves, and clobbering that with a stale
    /// "join přidán"/error here would just reintroduce the same class of bug.
    fn start_lookup(&mut self, grid: Entity<ResultGrid>, req: LookupRequest, cx: &mut Context<Self>) {
        let LookupRequest { sql, ref_table, wanted_cols, src_col, generation } = req;
        let Some(spec) = self.active_conn_spec() else {
            self.status = "Bez připojení — vyberte připojení nahoře.".into();
            cx.notify();
            return;
        };
        self.status = "hledám hodnoty pro join…".into();
        cx.notify();
        let rx = self.runner.fetch_lookup(spec, sql);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                let tab_alive = view.tabs.iter().any(|t| {
                    matches!(&t.content, TabContent::Grid { grid: g, .. } if g.entity_id() == grid.entity_id())
                });
                if !tab_alive {
                    // Originating ad-hoc tab was closed mid-flight — nothing
                    // left to apply this to.
                    return;
                }
                match result {
                    Ok(Ok((_cols, rows))) => {
                        if !grid.read(cx).accept_lookup_result(src_col, generation, &wanted_cols) {
                            // Stale: a newer request for this column has
                            // since been dispatched (or the user unchecked a
                            // wanted column) — drop rather than resurrect an
                            // outdated selection (last-dispatched wins).
                            return;
                        }
                        let mut maps: Vec<std::collections::HashMap<String, Option<String>>> =
                            vec![std::collections::HashMap::new(); wanted_cols.len()];
                        for row in rows {
                            let Some(Some(key_val)) = row.first().cloned() else { continue };
                            for (i, m) in maps.iter_mut().enumerate() {
                                m.insert(key_val.clone(), row.get(i + 1).cloned().flatten());
                            }
                        }
                        let virtual_cols: Vec<fk_join::VirtualCol> = wanted_cols
                            .into_iter()
                            .zip(maps)
                            .map(|(c, map)| fk_join::VirtualCol {
                                name: format!("{ref_table}.{c}"),
                                map,
                                src_col,
                            })
                            .collect();
                        grid.update(cx, |g, cx| {
                            g.set_virtual_cols_for_src(src_col, virtual_cols, cx);
                        });
                        view.status = "join přidán".into();
                    }
                    Ok(Err(e)) => {
                        view.status = format!("error: {e}");
                    }
                    Err(_) => {
                        view.status = "lookup zrušen".into();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// G4 Task 5: `ResultGrid`'s `GridEvent` subscription (wired per grid
    /// entity in `QueryEvent::Started`, see `run_query_with`) — see
    /// `GridEvent`'s doc comment for why the preview path needs the whole
    /// identity payload while the lookup path just needs `emitter`.
    fn on_grid_event(&mut self, emitter: Entity<ResultGrid>, event: &GridEvent, cx: &mut Context<Self>) {
        match event {
            GridEvent::RerunPreviewJoins { schema, table, key, title, joins, col, ref_col } => {
                // Review fix (Task 5 round 1, Issue 2): `run_query_with`
                // would silently no-op here under the same guards (a modal
                // is open, or another query is already running) — but
                // `toggle_fk_column` already flipped `fk_checked` and
                // `cx.notify()`'d before emitting this event, so the
                // checkbox is already showing the NEW state. Left alone,
                // that's a silent lie: the tab's actual data never changes
                // because the re-run never starts. Check the same guards
                // here FIRST, and if either is set, revert exactly the
                // toggle that caused this event and surface why, instead of
                // routing through `run_query_with` and letting it drop the
                // request with no trace.
                if self.modal.is_some() || self.cancel.is_some() {
                    emitter.update(cx, |g, cx| g.revert_fk_toggle(*col, ref_col, cx));
                    self.status = "počkejte — běží dotaz".into();
                    cx.notify();
                    return;
                }
                let sql = fk_join::build_join_sql(schema.as_deref(), table, joins);
                let preview = PreviewTarget {
                    title: title.clone(),
                    key: key.clone(),
                    table: table.clone(),
                    schema: schema.clone(),
                    joins: joins.clone(),
                    // Review fix (Task 6 round 1, Issue 1): this run's
                    // `joins` (even if the user just unchecked the last one,
                    // making it empty) is a direct, explicit user action —
                    // `apply_view_prefs_to_grid` must save it unconditionally
                    // rather than reading `joins.is_empty()` as "plain open".
                    from_join_change: true,
                };
                // G5 Task 4 (folded T3 review issue 2 — dirty guard): this
                // re-run REPLACES `emitter`'s grid entity (see `GridEvent`'s
                // doc comment) — its `EditState` would be silently dropped.
                // `toggle_fk_column` already flipped `fk_checked` before
                // emitting, so a "Zrušit" here must undo exactly that flip,
                // same as the busy-guard revert two lines above.
                let dirty_n = emitter.read(cx).edit_state.change_count();
                if dirty_n > 0 {
                    self.discard_confirm = Some(DiscardConfirmState {
                        change_count: dirty_n,
                        action: PendingDiscard::RunPreview {
                            sql,
                            preview: Box::new(preview),
                            revert: Some((emitter.clone(), *col, ref_col.clone())),
                        },
                    });
                    cx.notify();
                    return;
                }
                self.run_query_with(sql, Some(preview), true, cx);
            }
            GridEvent::RunLookup { sql, ref_table, wanted_cols, src_col, generation } => {
                let req = LookupRequest {
                    sql: sql.clone(),
                    ref_table: ref_table.clone(),
                    wanted_cols: wanted_cols.clone(),
                    src_col: *src_col,
                    generation: *generation,
                };
                self.start_lookup(emitter, req, cx);
            }
            // G4 Task 6: sort/visibility/width-drag-end on a PREVIEW tab —
            // see `GridEvent::ViewChanged`'s doc comment and
            // `save_view_prefs_for_grid`.
            GridEvent::ViewChanged => {
                self.save_view_prefs_for_grid(&emitter, cx);
            }
        }
    }

    // -----------------------------------------------------------------
    // G5 Task 4: dirty-edit discard guard (folded T3 review issue 2).
    // -----------------------------------------------------------------

    /// Row-granular staged-change count for `tab`'s grid, if it has any —
    /// `None` for a `Text` tab or a clean/non-editable `Grid` tab (both are
    /// safe to proceed past without a confirm prompt). Shared by the two
    /// lookup helpers below.
    fn grid_dirty_change_count(tab: &ResultTab, cx: &Context<Self>) -> Option<usize> {
        let TabContent::Grid { grid, .. } = &tab.content else { return None };
        let n = grid.read(cx).edit_state.change_count();
        (n > 0).then_some(n)
    }

    /// Tab-strip "✕" guard: `Some(n)` when closing tab `id` would drop `n`
    /// staged changes.
    fn dirty_change_count_for_tab_id(&self, id: u64, cx: &Context<Self>) -> Option<usize> {
        self.tabs.iter().find(|t| t.id == id).and_then(|t| Self::grid_dirty_change_count(t, cx))
    }

    /// Re-open-same-preview guard (`TreeEvent::OpenPreview`/
    /// `PaletteItem::Table`): `Some(n)` when a tab with this `preview_key` is
    /// ALREADY open and dirty — `run_query_with` would otherwise close it
    /// via `Tabs::close_by_preview_key` right before opening the fresh one.
    fn dirty_change_count_for_preview_key(&self, key: &str, cx: &Context<Self>) -> Option<usize> {
        self.tabs
            .iter()
            .find(|t| t.preview_key.as_deref() == Some(key))
            .and_then(|t| Self::grid_dirty_change_count(t, cx))
    }

    /// Shared by `TreeEvent::OpenPreview` and `PaletteItem::Table` (both
    /// open exactly the same kind of preview tab) — dirty-guards (folded T3
    /// review issue 2) before dispatching: `run_query_with` would otherwise
    /// silently close an EXISTING dirty tab for the same (schema, table) via
    /// `Tabs::close_by_preview_key` right before opening the fresh one.
    fn open_table_preview(&mut self, schema: Option<String>, table: String, cx: &mut Context<Self>) {
        let sql = preview_sql(schema.as_deref(), &table);
        let key = format!("{}.{table}", schema.clone().unwrap_or_default());
        let preview = PreviewTarget {
            title: format!("Náhled: {table}"),
            key: key.clone(),
            table,
            schema,
            joins: Vec::new(),
            from_join_change: false,
        };
        if let Some(n) = self.dirty_change_count_for_preview_key(&key, cx) {
            self.discard_confirm = Some(DiscardConfirmState {
                change_count: n,
                action: PendingDiscard::RunPreview { sql, preview: Box::new(preview), revert: None },
            });
            cx.notify();
            return;
        }
        self.run_query_with(sql, Some(preview), true, cx);
    }

    /// "Zahodit" on the discard-confirm prompt — performs the action that
    /// was withheld pending confirmation. The dropped tab's/grid's
    /// `EditState` is not explicitly cleared here: `CloseTab` removes the
    /// whole tab (and its grid entity) outright, and `RunPreview` replaces
    /// the grid entity via the normal `Started` pipeline (`set_buffer`
    /// resets `edit_state` on the FRESH entity) — there is nothing left to
    /// clear on the old one either way.
    fn on_discard_confirm_yes(&mut self, cx: &mut Context<Self>) {
        let Some(dc) = self.discard_confirm.take() else { return };
        match dc.action {
            PendingDiscard::CloseTab { id } => {
                self.tabs.close(id);
            }
            PendingDiscard::RunPreview { sql, preview, .. } => {
                self.run_query_with(sql, Some(*preview), true, cx);
            }
        }
        cx.notify();
    }

    /// "Zrušit" on the discard-confirm prompt (also Esc, see
    /// `on_cancel_query`) — aborts the pending action outright: no tab
    /// closes, no query runs. For a `RunPreview` originating from a ☰
    /// toggle, also undoes the checkbox flip `toggle_fk_column` already
    /// applied before emitting `RerunPreviewJoins` (see `PendingDiscard::
    /// RunPreview`'s doc comment) so the ☰ menu doesn't keep showing a join
    /// that was never actually re-run.
    fn on_discard_confirm_no(&mut self, cx: &mut Context<Self>) {
        let Some(dc) = self.discard_confirm.take() else { return };
        if let PendingDiscard::RunPreview { revert: Some((grid, col, ref_col)), .. } = dc.action {
            grid.update(cx, |g, cx| g.revert_fk_toggle(col, &ref_col, cx));
        }
        cx.notify();
    }

    // -----------------------------------------------------------------
    // G5 Task 4: Apply flow (sandbox edits -> generated SQL -> one tx).
    // -----------------------------------------------------------------

    /// Builds the `ConnectSpec` + `timeout_secs` for `run_write_transaction`
    /// — the SAME lookup `run_query_with` performs for its own spec (active
    /// saved connection with its vault secret, or the CLI-arg URL, else
    /// `None`) — brief contract #5's "secret handling identical to
    /// `run_query_with`". Doesn't re-check `read_only` itself: a read-only
    /// connection never produces an `Editable` grid in the first place
    /// (`detect_editable_pk`), so the apply bar/dialog can't even be reached
    /// through one — `run_write_transaction` hard-refuses again regardless
    /// (belt-and-braces, brief contract #5).
    /// G5 Task 4 review fix (BLOCKER 1): the identity stamped onto every
    /// freshly-opened `ResultTab::conn_identity` — `active_connection_id`
    /// when a saved connection is active, else the CLI-arg sentinel. Unlike
    /// `active_connection_name_for_history` (which resolves to a NAME, and
    /// collapses "connection since deleted" into the same `"cli"` bucket as
    /// the real CLI path), this is the raw, stable id — a connection can be
    /// renamed without invalidating a tab's stamped identity, which a
    /// name-based comparison would get wrong.
    fn current_conn_identity(&self) -> String {
        self.active_connection_id.clone().unwrap_or_else(|| CLI_CONN_IDENTITY.to_string())
    }

    /// Human-readable name for a `ResultTab::conn_identity` value — used
    /// only in the Apply flow's mismatch error text ("changes came from
    /// connection X"). Falls back to the raw identity string itself if the
    /// connection has since been deleted (rare, but must never panic or
    /// silently say "cli" for a real connection that's simply gone).
    fn conn_name_for_identity(&self, identity: &str) -> String {
        if identity == CLI_CONN_IDENTITY {
            return "cli".to_string();
        }
        self.config
            .connections
            .iter()
            .find(|c| c.id == identity)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| identity.to_string())
    }

    fn apply_conn_spec(&self) -> Option<(ConnectSpec, Option<u64>)> {
        if let Some(id) = self.active_connection_id.clone() {
            let cfg = self.config.connections.iter().find(|c| c.id == id)?.clone();
            let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
            let timeout_secs = cfg.timeout_secs;
            Some((ConnectSpec::Config { cfg: Box::new(cfg), secret }, timeout_secs))
        } else {
            self.conn_url.clone().map(|url| (ConnectSpec::Url(url), None))
        }
    }

    /// Apply bar's "Aplikovat" (brief contract #1) — builds the exact
    /// `sandbox::generate_statements` output for the ACTIVE tab's staged
    /// edits and opens the confirmation dialog. A no-op when there's no
    /// active tab, it isn't a `Grid` tab, it isn't `editable`, or (shouldn't
    /// happen — the apply bar only renders when dirty, but checked
    /// defensively) it generates zero statements.
    ///
    /// G5 Task 4 review fix (BLOCKER 1): also refuses — with a clear Czech
    /// status message, never silently — when the tab's stamped
    /// `conn_identity` no longer matches the CURRENTLY active connection
    /// (belt-and-braces: the apply bar's own "Aplikovat" is already
    /// disabled in that state, see `render_apply_bar`, but this is reachable
    /// defensively too).
    ///
    /// G5 Task 4 review fix (MINOR 4): closes any open cell editor on this
    /// grid first (stale residue otherwise sits underneath the dialog and
    /// reappears when it's cancelled) and moves keyboard focus onto the
    /// dialog panel itself in this SAME update, via `window`.
    fn on_open_apply_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.discard_confirm.is_some() || self.apply_dialog.is_some() {
            return;
        }
        let Some(active) = self.tabs.active() else { return };
        let (tab_id, tab_conn_identity, grid) = match &active.content {
            TabContent::Grid { grid, .. } => (active.id, active.conn_identity.clone(), grid.clone()),
            TabContent::Text { .. } => return,
        };
        let current_identity = self.current_conn_identity();
        if !conn_identity_matches(&tab_conn_identity, &current_identity) {
            let from = self.conn_name_for_identity(&tab_conn_identity);
            self.status = format!("změny pocházejí z jiného připojení ({from}) — přepni se zpět");
            cx.notify();
            return;
        }
        grid.update(cx, |g, _| {
            g.close_overlay_if_open();
        });
        let (statements, preview_identity) = {
            let g = grid.read(cx);
            let Some(editable) = g.editable.clone() else { return };
            let Some(buf_rc) = g.buffer.clone() else { return };
            let headers = g.column_names();
            let table = g.table_name.clone();
            let preview_identity =
                g.preview_identity().unwrap_or_else(|| (None, table.clone()));
            let meta = sandbox::TableMeta {
                schema: preview_identity.0.as_deref(),
                table: &table,
                headers: &headers,
                pk_cols: &editable.pk_cols,
                numeric_cols: &editable.numeric_cols,
            };
            let mut original = |row: usize, col: usize| -> Option<String> {
                let mut b = buf_rc.borrow_mut();
                if b.cell_is_null(row, col) { None } else { Some(b.cell_text(row, col)) }
            };
            let statements = sandbox::generate_statements(&meta, &g.edit_state, &mut original);
            (statements, preview_identity)
        };
        if statements.is_empty() {
            return;
        }
        let sql_text = statements.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join("\n");
        let focus_handle = cx.focus_handle();
        self.apply_dialog = Some(ApplyDialogState {
            tab_id,
            statements,
            sql_text,
            preview_identity,
            conn_identity: tab_conn_identity,
            running: false,
            error: None,
            focus_handle: focus_handle.clone(),
        });
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// Apply dialog's "Potvrdit a spustit" (brief contract #2/#3/#4) —
    /// dispatches `runner.run_write_transaction` over the dialog's captured
    /// `statements` and reacts to the outcome. Re-clickable after a failure
    /// (brief contract #4: the dialog stays open showing the error) — a
    /// retry just re-dispatches the same statements.
    ///
    /// G5 Task 4 review fix (BLOCKER 1): re-checks `ad.conn_identity` against
    /// the CURRENTLY active connection before dispatching — belt-and-braces
    /// alongside `on_open_apply_dialog`'s own check (the dialog's
    /// `.occlude()` should make switching connections while it's open
    /// impossible, but this is the backstop if that's ever wrong). A
    /// mismatch surfaces as `ad.error` (dialog stays open, same shape as any
    /// other Apply failure) rather than silently running against the wrong
    /// connection.
    fn on_confirm_apply(&mut self, cx: &mut Context<Self>) {
        let Some(ad) = &self.apply_dialog else { return };
        if ad.running {
            return;
        }
        let statements = ad.statements.clone();
        let sql_text = ad.sql_text.clone();
        let tab_id = ad.tab_id;
        let preview_identity = ad.preview_identity.clone();
        let dialog_conn_identity = ad.conn_identity.clone();
        let n_statements = statements.len();

        let current_identity = self.current_conn_identity();
        if !conn_identity_matches(&dialog_conn_identity, &current_identity) {
            let from = self.conn_name_for_identity(&dialog_conn_identity);
            if let Some(ad) = &mut self.apply_dialog {
                ad.error =
                    Some(format!("změny pocházejí z jiného připojení ({from}) — přepni se zpět"));
            }
            cx.notify();
            return;
        }

        let Some((spec, timeout_secs)) = self.apply_conn_spec() else {
            if let Some(ad) = &mut self.apply_dialog {
                ad.error = Some("Bez připojení — vyberte připojení nahoře.".to_string());
            }
            cx.notify();
            return;
        };

        if let Some(ad) = &mut self.apply_dialog {
            ad.running = true;
            ad.error = None;
        }
        cx.notify();

        let history_conn_name = self.active_connection_name_for_history();
        let history_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let started = std::time::Instant::now();
        let rx = self.runner.run_write_transaction(spec, statements, timeout_secs);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(Ok(total)) => {
                        // Brief contract #3, in order: close modal, clear
                        // edit_state, status, re-run the preview, record ONE
                        // history entry.
                        view.apply_dialog = None;
                        if let Some(tab) = view.tabs.iter().find(|t| t.id == tab_id) {
                            if let TabContent::Grid { grid, .. } = &tab.content {
                                grid.clone().update(cx, |g, cx| g.clear_edits(cx));
                            }
                        }
                        view.status = format!("aplikováno ({n_statements} příkazů)");
                        // Re-run the preview via the EXISTING pipeline
                        // (brief: "preserves joins via from_join_change=false
                        // machinery" — `apply_view_prefs_to_grid`'s saved-
                        // fk-join retrigger picks the active joins back up
                        // from this table's persisted view prefs once this
                        // run's own `Started` lands, exactly like a plain
                        // preview re-open does). This immediately overwrites
                        // `view.status` above with its own "connecting…" /
                        // progress text — expected: the "aplikováno (…)"
                        // status is a transient confirmation, the refreshed
                        // preview's own status (ending in "N rows in …")
                        // takes over next, same as every other status
                        // transition in this file.
                        let (schema, table) = preview_identity;
                        let sql = preview_sql(schema.as_deref(), &table);
                        let key = format!("{}.{table}", schema.clone().unwrap_or_default());
                        let title = format!("Náhled: {table}");
                        let preview = PreviewTarget {
                            title,
                            key,
                            table,
                            schema,
                            joins: Vec::new(),
                            from_join_change: false,
                        };
                        view.run_query_with(sql, Some(preview), true, cx);
                        // Record ONE history entry for the write itself
                        // (brief contract #3's final step) — the re-run's
                        // own SELECT gets its OWN separate history entry
                        // once ITS `Finished`/`Failed` lands, same as any
                        // other preview.
                        view.record_history(
                            &sql_text,
                            &history_conn_name,
                            history_started_at,
                            Some(started.elapsed().as_millis() as i64),
                            Some(total as i64),
                            None,
                            cx,
                        );
                    }
                    Ok(Err(e)) => {
                        if let Some(ad) = &mut view.apply_dialog {
                            ad.running = false;
                            ad.error = Some(e.to_string());
                        }
                    }
                    Err(_) => {
                        if let Some(ad) = &mut view.apply_dialog {
                            ad.running = false;
                            ad.error = Some("apply zrušeno".to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Dispatches `runner.fetch_schema(spec)` off the UI thread and updates
    /// `self.tree`'s loading/snapshot/error state as it resolves — same
    /// "UI thread only ever awaits a channel via `cx.spawn`" shape as
    /// `run_query`/`switch_to_connection`. Called from the
    /// `switch_to_connection` success arm, `TreeEvent::RefreshRequested`,
    /// and once at CLI-arg startup (see `main`).
    ///
    /// Guarded by `schema_fetch_generation` (review Issue 1, mirroring
    /// `switch_generation`): every dispatch bumps the counter and captures
    /// it, and the `cx.spawn` completion drops its result if the generation
    /// has since moved on — so a slow fetch for a connection the user has
    /// already switched away from can never overwrite a newer one
    /// (last-dispatched wins, not last-resolved).
    fn trigger_schema_fetch(&mut self, spec: ConnectSpec, cx: &mut Context<Self>) {
        self.tree.update(cx, |t, cx| t.set_loading(cx));
        let key = conn_spec_key(&spec);
        self.schema_fetch_generation += 1;
        let my_generation = self.schema_fetch_generation;
        let rx = self.runner.fetch_schema(spec);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                // A newer fetch was dispatched meanwhile — this result is
                // stale, drop it (last-dispatched wins).
                if view.schema_fetch_generation != my_generation {
                    return;
                }
                // `same_connection` is decided at APPLY time against the key
                // of the snapshot actually shown in the tree — deciding it at
                // dispatch time let a superseded switch-fetch leave the key
                // pointing at the new target before any reset ever applied,
                // so a same-target refresh would "preserve" the previous
                // connection's expand/filter state (re-review residual race).
                match result {
                    Ok(Ok(snapshot)) => {
                        let same_connection =
                            view.schema_tree_connection_key.as_deref() == Some(key.as_str());
                        view.schema_tree_connection_key = Some(key.clone());
                        // G3 Task 4: (re-)apply the favourite set alongside
                        // every snapshot — a fresh connection switch needs it
                        // for its "Oblíbené" section to show anything at all,
                        // and a same-connection refresh needs it re-applied
                        // too since `set_snapshot` doesn't touch it.
                        let favourites = view.config.favourite_objects.clone();
                        let active_id = view.active_connection_id.clone();
                        view.tree.update(cx, |t, cx| {
                            t.set_snapshot(snapshot, same_connection, cx);
                            t.set_favourites(favourites, active_id, cx);
                        });
                    }
                    Ok(Err(e)) => {
                        view.tree.update(cx, |t, cx| t.set_error(e.to_string(), cx));
                    }
                    Err(_) => {
                        view.tree
                            .update(cx, |t, cx| t.set_error("fetch zrušen".to_string(), cx));
                    }
                }
            });
        })
        .detach();
    }

    /// `SchemaTree`'s `TreeEvent` subscription (wired in `main`). G2 Task 7:
    /// `OpenPreview` builds the SQL via `preview_sql` and runs it through the
    /// normal guarded pipeline (`run_query_with`, `bypass_auto_limit = true`
    /// — the SQL already carries its own `LIMIT 1000`) without touching the
    /// editor's text; `OpenDdl` (double-click on a routine/trigger, or the
    /// tree header's "DDL" button via `SchemaTree::handle_generate_ddl`)
    /// just opens a read-only `Text` tab — no DB round-trip either way.
    fn on_tree_event(&mut self, _emitter: Entity<SchemaTree>, event: &TreeEvent, cx: &mut Context<Self>) {
        match event {
            TreeEvent::OpenPreview { schema, table } => {
                self.open_table_preview(schema.clone(), table.clone(), cx);
            }
            TreeEvent::OpenDdl { title, ddl } => {
                self.tabs.open(ResultTab {
                    id: 0,
                    title: format!("DDL: {title}"),
                    pinned: false,
                    preview_key: None,
                    // Never editable/Grid — the identity is inert here, but
                    // every `ResultTab` needs a value (see its doc comment).
                    conn_identity: self.current_conn_identity(),
                    content: TabContent::Text { text: ddl.clone(), scroll_lines: 0 },
                });
                self.status = format!("DDL otevřeno: {title}");
                cx.notify();
            }
            TreeEvent::RefreshRequested => {
                if let Some(spec) = self.active_conn_spec() {
                    self.trigger_schema_fetch(spec, cx);
                } else {
                    self.schema_tree_connection_key = None;
                    self.tree.update(cx, |t, cx| t.clear(cx));
                }
            }
            // G3 Task 4: a row's ★/☆ toggle (a table/view/routine/trigger/
            // sequence in the schema tree proper, or an item already listed
            // under the "Oblíbené" section) — mirrors
            // `connections_ui::AppView::toggle_connection_favourite`'s
            // guarded-save shape for the dropdown's connection stars.
            TreeEvent::ToggleFavourite(fav) => {
                if !self.guard_corrupt_config(cx) {
                    return;
                }
                self.config.toggle_favourite(fav.clone());
                self.status = match self.config.save(&self.config_path) {
                    Ok(()) => "Uloženo".to_string(),
                    Err(e) => format!("error saving config: {}", e.message),
                };
                let favourites = self.config.favourite_objects.clone();
                let active_id = self.active_connection_id.clone();
                self.tree.update(cx, |t, cx| t.set_favourites(favourites, active_id, cx));
                cx.notify();
            }
        }
    }

    /// Tab strip between the SQL editor and result content: title +
    /// row-count badge (`Grid` tabs read `buffer.row_count()` fresh at
    /// render time rather than caching it on the tab) + pin toggle + close.
    /// Click activates. Active tab bg 0x313244, inactive 0x181825. Only
    /// called when there's at least one open tab (see `Render::render`).
    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_id = self.tabs.active().map(|t| t.id);
        let rows: Vec<(u64, String, bool, usize)> = self
            .tabs
            .iter()
            .map(|t| {
                let (row_count, dirty) = match &t.content {
                    TabContent::Grid { buffer, grid } => {
                        (buffer.borrow().row_count(), grid.read(cx).edit_state.is_dirty())
                    }
                    TabContent::Text { .. } => (0, false),
                };
                // G5 Task 3, brief contract #7: dirty (unapplied staged
                // edits) tabs get a " •" title suffix — the apply bar
                // itself is a later task, but the indicator is wired now.
                let title = if dirty { format!("{} •", t.title) } else { t.title.clone() };
                (t.id, title, t.pinned, row_count)
            })
            .collect();

        let mut strip = div().id("tab-strip").flex().flex_row().h(px(28.)).bg(rgb(0x181825));
        for (id, title, pinned, row_count) in rows {
            let is_active = Some(id) == active_id;
            let bg = if is_active { rgb(0x313244) } else { rgb(0x181825) };
            let pin_color = if pinned { rgb(0xf9e2af) } else { rgb(0x6c7086) };
            strip = strip.child(
                div()
                    .id(("tab", id as usize))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .h_full()
                    .bg(bg)
                    .text_color(rgb(0xcdd6f4))
                    .cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.tabs.activate(id);
                        cx.notify();
                    }))
                    .child(format!("{title} ({row_count})"))
                    .child(
                        div()
                            .id(("tab-pin", id as usize))
                            .px_1()
                            .cursor_pointer()
                            .text_color(pin_color)
                            .child("📌")
                            .on_click(cx.listener(move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.tabs.toggle_pin(id);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(("tab-close", id as usize))
                            .px_1()
                            .cursor_pointer()
                            .child("✕")
                            .on_click(cx.listener(move |view, _, _, cx| {
                                cx.stop_propagation();
                                // G5 Task 4 (folded T3 review issue 2 —
                                // dirty guard): closing a dirty tab would
                                // silently drop its staged edits.
                                if let Some(n) = view.dirty_change_count_for_tab_id(id, cx) {
                                    view.discard_confirm = Some(DiscardConfirmState {
                                        change_count: n,
                                        action: PendingDiscard::CloseTab { id },
                                    });
                                    cx.notify();
                                    return;
                                }
                                view.tabs.close(id);
                                cx.notify();
                            })),
                    ),
            );
        }
        strip
    }

    /// Only the active tab's content renders. `Grid` tabs render their own
    /// `Entity<ResultGrid>`; `Text` tabs render read-only monospace lines
    /// (scrolled via `scroll_lines`, mutated by mouse wheel) plus a
    /// "Kopírovat" button that copies the whole text to the clipboard. With
    /// no tabs open at all, renders a neutral placeholder.
    fn render_tab_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(active) = self.tabs.active() else {
            return div().flex_1().bg(rgb(0x1e1e2e)).into_any_element();
        };

        match &active.content {
            TabContent::Grid { grid, .. } => {
                // G4 Task 2: `ResultGrid` doesn't own a status bar itself —
                // `status_note` (currently just the large-sort "řadím…"
                // marker set by `rebuild_view`) is the minimal seam for
                // surfacing grid-originated notices in `AppView::status`.
                // `take()`'d so it's shown exactly once, not stuck forever.
                if let Some(note) = grid.update(cx, |g, _| g.status_note.take()) {
                    self.status = note;
                }
                grid.clone().into_any_element()
            }
            TabContent::Text { text, scroll_lines } => {
                let lines: Vec<&str> = text.lines().collect();
                let scroll = (*scroll_lines).min(lines.len());
                let text_for_copy = text.clone();

                let mut body = div()
                    .id("tab-text-body")
                    .font_family("Consolas")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .p_2()
                    .text_color(rgb(0xcdd6f4))
                    .on_scroll_wheel(cx.listener(|view, e: &ScrollWheelEvent, _, cx| {
                        let delta_lines = match e.delta {
                            ScrollDelta::Lines(p) => p.y,
                            ScrollDelta::Pixels(p) => p.y.as_f32() / 20.0,
                        };
                        if let Some(TabContent::Text { text, scroll_lines }) =
                            view.tabs.active_mut().map(|t| &mut t.content)
                        {
                            let max_scroll = text.lines().count().saturating_sub(1);
                            let current = *scroll_lines as f32;
                            let new_scroll = (current - delta_lines).round();
                            *scroll_lines = new_scroll.max(0.0).min(max_scroll as f32) as usize;
                        }
                        cx.notify();
                    }));
                for line in &lines[scroll..] {
                    body = body.child(div().child(line.to_string()));
                }

                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .bg(rgb(0x1e1e2e))
                    .child(
                        div().flex().flex_row().justify_end().p_1().child(
                            div()
                                .id("tab-copy")
                                .cursor_pointer()
                                .bg(rgb(0x313244))
                                .text_color(rgb(0xcdd6f4))
                                .px_2()
                                .rounded_md()
                                .child("Kopírovat")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text_for_copy.clone()));
                                })),
                        ),
                    )
                    .child(body)
                    .into_any_element()
            }
        }
    }

    /// G5 Task 4, brief contract #1: apply bar above the status bar
    /// ("{n} změn · Aplikovat · Zahodit") — `None` (renders nothing) unless
    /// the ACTIVE tab is a `Grid` tab with staged edits.
    ///
    /// G5 Task 4 review fix (BLOCKER 1): when the tab's stamped
    /// `conn_identity` no longer matches the currently active connection
    /// (staged the edits on connection A, switched to B), "Aplikovat" is
    /// rendered WITHOUT `cursor_pointer`/`on_click` (visually disabled, dim
    /// text) plus an inline hint — the bar itself stays visible/dirty-count
    /// accurate (brief: "bar can stay visible with the hint") since
    /// "Zahodit" is still a legitimate, safe action in this state.
    fn render_apply_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let active = self.tabs.active()?;
        let TabContent::Grid { grid, .. } = &active.content else { return None };
        let n = grid.read(cx).edit_state.change_count();
        if n == 0 {
            return None;
        }
        let identity_ok =
            conn_identity_matches(&active.conn_identity, &self.current_conn_identity());
        let grid_for_discard = grid.clone();
        Some(
            div()
                .id("apply-bar")
                .h(px(28.))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .bg(rgb(0x3a3a1e))
                .text_color(rgb(0xf9e2af))
                .child(format!("{n} změn"))
                .child(
                    div()
                        .id("apply-bar-apply")
                        .when(identity_ok, |d| {
                            d.cursor_pointer()
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.on_open_apply_dialog(window, cx)
                                }))
                        })
                        .px_2()
                        .rounded_md()
                        .bg(rgb(0x45475a))
                        .text_color(if identity_ok { rgb(0xa6e3a1) } else { rgb(0x6c7086) })
                        .child("Aplikovat"),
                )
                .when(!identity_ok, |d| {
                    d.child(
                        div()
                            .text_color(rgb(0x6c7086))
                            .child("(jiné připojení — přepni se zpět)"),
                    )
                })
                .child(
                    div()
                        .id("apply-bar-discard")
                        .cursor_pointer()
                        .px_2()
                        .rounded_md()
                        .bg(rgb(0x45475a))
                        .text_color(rgb(0xf38ba8))
                        .child("Zahodit")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            grid_for_discard.update(cx, |g, cx| g.clear_edits(cx));
                        })),
                )
                .into_any_element(),
        )
    }

    /// G5 Task 4 (folded T3 review issue 2): the "Neuložené změny ({n}) —
    /// zahodit?" confirm prompt — same centered-overlay `.occlude()`
    /// convention as every other modal in this file (`render_modal_overlay`,
    /// `render_palette_overlay`). No text input, so no `window.focus` call
    /// is needed here (unlike those two) — Esc/click are the only ways to
    /// answer it, and Esc is wired in `on_cancel_query`.
    fn render_discard_confirm_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dc = self.discard_confirm.as_ref()?;
        let n = dc.change_count;

        let panel = div()
            .id("discard-confirm-panel")
            .w(px(420.))
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded_md()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .text_color(rgb(0xcdd6f4))
            .child(format!("Neuložené změny ({n}) — zahodit?"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .id("discard-confirm-yes")
                            .cursor_pointer()
                            .bg(rgb(0x313244))
                            .text_color(rgb(0xf38ba8))
                            .px_2()
                            .rounded_md()
                            .child("Zahodit")
                            .on_click(cx.listener(|view, _, _, cx| view.on_discard_confirm_yes(cx))),
                    )
                    .child(
                        div()
                            .id("discard-confirm-no")
                            .cursor_pointer()
                            .bg(rgb(0x313244))
                            .text_color(rgb(0xcdd6f4))
                            .px_2()
                            .rounded_md()
                            .child("Zrušit")
                            .on_click(cx.listener(|view, _, _, cx| view.on_discard_confirm_no(cx))),
                    ),
            );

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000099))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    /// G5 Task 4, brief contract #1/#3/#4: the Apply confirmation dialog —
    /// same centered-overlay `.occlude()` shape as every other modal, body
    /// reuses the `TabContent::Text`/cell-detail "plain wrapped monospace
    /// lines" pattern (brief: "monospace, scrollable — reuse Text-tab body
    /// pattern"; a `max_h` + `overflow_hidden` block stands in for real
    /// scroll handling, same simplification `render_cell_detail_overlay`
    /// already made for v1). While `running`, "aplikuji…" shows and both
    /// buttons are visually disabled (no `cursor_pointer`/`on_click`) via
    /// `.when(!running, ..)`; a set `error` stays visible alongside
    /// re-enabled buttons so the user can retry or back out (brief contract
    /// #4: edits stay staged either way).
    fn render_apply_dialog_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ad = self.apply_dialog.as_ref()?;
        let running = ad.running;
        let error = ad.error.clone();
        let focus_handle = ad.focus_handle.clone();
        let lines: Vec<String> = ad.statements.iter().map(|(s, _)| s.clone()).collect();

        let mut body = div()
            .id("apply-dialog-body")
            .font_family("Consolas")
            .flex()
            .flex_col()
            .max_h(px(280.))
            .overflow_hidden()
            .p_2()
            .bg(rgb(0x181825))
            .rounded_md()
            .text_color(rgb(0xcdd6f4));
        for line in &lines {
            body = body.child(div().whitespace_normal().child(line.clone()));
        }

        let mut panel = div()
            .id("apply-dialog-panel")
            // G5 Task 4 review fix (MINOR 4): same handle
            // `on_open_apply_dialog` moved `window` focus onto — keeps
            // keyboard focus off the SQL editor underneath while the dialog
            // is open.
            .track_focus(&focus_handle)
            .w(px(640.))
            .max_h(px(480.))
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded_md()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .text_color(rgb(0xcdd6f4))
            .child(format!("Aplikovat {} příkazů", lines.len()))
            .child(body);

        if running {
            panel = panel.child(div().text_color(rgb(0xf9e2af)).child("aplikuji…"));
        }
        if let Some(err) = &error {
            panel = panel.child(div().text_color(rgb(0xf38ba8)).child(format!("error: {err}")));
        }

        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .gap_2()
                .child(
                    div()
                        .id("apply-dialog-confirm")
                        .when(!running, |d| {
                            d.cursor_pointer()
                                .on_click(cx.listener(|view, _, _, cx| view.on_confirm_apply(cx)))
                        })
                        .bg(rgb(0x313244))
                        .text_color(if running { rgb(0x6c7086) } else { rgb(0xa6e3a1) })
                        .px_2()
                        .rounded_md()
                        .child("Potvrdit a spustit"),
                )
                .child(
                    div()
                        .id("apply-dialog-cancel")
                        .when(!running, |d| {
                            d.cursor_pointer().on_click(cx.listener(|view, _, _, cx| {
                                view.apply_dialog = None;
                                cx.notify();
                            }))
                        })
                        .bg(rgb(0x313244))
                        .text_color(if running { rgb(0x6c7086) } else { rgb(0xcdd6f4) })
                        .px_2()
                        .rounded_md()
                        .child("Zrušit"),
                ),
        );

        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000099))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The SQL editor + tab strip + tab content column, unchanged from
        // pre-Task-6 except that it's now one column in a horizontal row
        // rather than filling the whole window body.
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(
                // Fixed height of 8 lines (SqlInput's own line_height is
                // px(20.), see sql_input.rs render()); the input scrolls
                // internally once the buffer grows past that.
                div()
                    .h(px(20. * 8. + 4. * 2.))
                    .px_2()
                    .bg(rgb(0x181825))
                    .child(self.sql.clone()),
            );

        // Tab strip only renders when there's at least one open tab (brief
        // contract #2); with none, `render_tab_content` fills the area with
        // a neutral placeholder instead.
        if self.tabs.iter().next().is_some() {
            column = column.child(self.render_tab_strip(cx));
        }
        column = column.child(self.render_tab_content(cx));

        // G2 Task 6: the schema tree panel sits LEFT of `column`, fixed
        // 260 px, collapsible via Ctrl+B (`ToggleTree`) — collapsed means
        // not rendered at all (width 0), not just visually hidden.
        let mut body = div().flex().flex_row().flex_1().min_h_0();
        if self.tree_visible {
            body = body.child(
                div()
                    .w(px(260.))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(rgb(0x45475a))
                    .child(self.tree.clone()),
            );
        }
        body = body.child(column);

        // G3 Task 3: the history panel sits RIGHT of `column`, fixed 280 px,
        // collapsible via Ctrl+H (`ToggleHistory`) — same collapse-to-0px
        // convention as the schema tree panel above.
        if self.history_visible {
            body = body.child(self.render_history_panel(cx));
        }

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .on_action(cx.listener(Self::on_run_query))
            .on_action(cx.listener(Self::on_run_query_unlimited))
            .on_action(cx.listener(Self::on_cancel_query))
            .on_action(cx.listener(Self::on_toggle_tree))
            .on_action(cx.listener(Self::on_toggle_history))
            .on_action(cx.listener(Self::on_open_palette))
            .child(self.render_top_bar(cx))
            .child(body);

        // G5 Task 4, brief contract #1: apply bar sits directly above the
        // status bar (spec mockup: "apply bar (when dirty) / status bar"),
        // rendered only when the ACTIVE tab's grid is dirty.
        if let Some(bar) = self.render_apply_bar(cx) {
            root = root.child(bar);
        }
        root = root.child(
            div()
                .h(px(28.))
                .px_2()
                .bg(rgb(0x313244))
                .text_color(rgb(0xa6adc8))
                .child(self.status.clone()),
        );

        if self.dropdown_open && self.modal.is_none() {
            root = root.child(self.render_dropdown_overlay(cx));
        }
        if let Some(overlay) = self.render_modal_overlay(cx) {
            root = root.child(overlay);
        }
        if let Some(overlay) = self.render_palette_overlay(cx) {
            root = root.child(overlay);
        }
        if let Some(overlay) = self.render_discard_confirm_overlay(cx) {
            root = root.child(overlay);
        }
        if let Some(overlay) = self.render_apply_dialog_overlay(cx) {
            root = root.child(overlay);
        }
        root
    }
}

fn main() {
    // CLI arg is now optional: back-compat direct-connect path (phase 0-2)
    // when present, otherwise the app starts with no active connection and
    // the user picks one from the top-bar switcher (Task 7).
    let conn_url = std::env::args().nth(1);
    let config_path = dbc_state::default_config_path();
    let vault_path = dbc_state::default_vault_path();
    // A parse error (as opposed to a missing file, which `AppConfig::load`
    // treats as an empty default) means an existing config.toml is
    // corrupt — surfaced in the status bar below rather than silently
    // discarded (final-review must-fix #2). `finish_save` refuses to
    // overwrite the file until it's been moved aside.
    let (config, config_load_error) = match AppConfig::load(&config_path) {
        Ok(cfg) => (cfg, None),
        Err(e) => (AppConfig::default(), Some(e.to_string())),
    };
    // G3 Task 3: opened once at startup; a failure (e.g. an unwritable
    // config dir) is surfaced in the status bar below but never blocks the
    // rest of the app — `record_history`/the panel's search both treat
    // `history: None` as "no history available" rather than panicking.
    let (history, history_open_error) = match HistoryDb::open(&dbc_state::default_history_path()) {
        Ok(h) => (Some(h), None),
        Err(e) => (None, Some(e.to_string())),
    };
    // G4 Task 6: opened once at startup; a failure (e.g. a corrupt
    // views.toml) is surfaced in the status bar below but never blocks the
    // rest of the app — the feature is just off (`view_prefs: None`), same
    // "degrade gracefully" precedent `history_open_error` already follows.
    let (view_prefs, view_prefs_open_error) =
        match ViewPrefsStore::load(&dbc_state::default_view_prefs_path()) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e.to_string())),
        };

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("ctrl-shift-enter", RunQueryUnlimited, None),
            KeyBinding::new("escape", CancelQuery, None),
            KeyBinding::new("ctrl-b", ToggleTree, None),
            KeyBinding::new("ctrl-h", ToggleHistory, None),
            KeyBinding::new("ctrl-k", OpenPalette, None),
        ]);
        sql_input::bind_keys(cx);
        grid::bind_keys(cx);
        connections_ui::bind_keys(cx);
        schema_tree::bind_keys(cx);
        palette::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let window_handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(format!("dbc v{}", env!("CARGO_PKG_VERSION")).into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let sql = cx.new(|cx| SqlInput::new(cx, "Type SQL, then Ctrl+Enter…"));
                        window.focus(&sql.focus_handle(cx), cx);
                        let grouped_cache = connections_ui::group_connections(&config.connections);
                        // config.toml corruption takes priority (it blocks
                        // saving/editing connections outright); a history
                        // open failure is a lesser, non-blocking notice; a
                        // view-prefs open failure (G4 Task 6) is the least
                        // severe of the three (only per-table grid memory is
                        // affected).
                        let status = if let Some(detail) = &config_load_error {
                            format!("error: config.toml je poškozený – oprav nebo smaž soubor ({detail})")
                        } else if let Some(detail) = &history_open_error {
                            format!("error: historie nedostupná ({detail})")
                        } else if let Some(detail) = &view_prefs_open_error {
                            format!("error: view prefs nedostupné ({detail})")
                        } else {
                            "ready".into()
                        };
                        let editor_focus = sql.focus_handle(cx);
                        let tree = cx.new(|cx| SchemaTree::new(cx, editor_focus));
                        cx.subscribe(&tree, AppView::on_tree_event).detach();
                        let history_search = cx.new(|cx| connections_ui::TextField::new(cx, "Hledat…", false));
                        AppView {
                            tabs: Tabs::new(),
                            status,
                            runner: QueryRunner::new(),
                            conn_url,
                            sql,
                            cancel: None,
                            started_at: None,
                            run_generation: 0,
                            config,
                            config_path,
                            config_load_error,
                            vault_path,
                            vault: None,
                            active_connection_id: None,
                            switch_generation: 0,
                            dropdown_open: false,
                            modal: None,
                            grouped_cache,
                            tree,
                            tree_visible: true,
                            schema_fetch_generation: 0,
                            schema_tree_connection_key: None,
                            history,
                            history_visible: true,
                            history_search,
                            history_cache: Vec::new(),
                            last_history_query: String::new(),
                            palette: None,
                            view_prefs,
                            apply_dialog: None,
                            discard_confirm: None,
                        }
                    })
                },
            )
            .unwrap();
        cx.activate(true);

        // CLI-arg back-compat startup path (brief contract #6): also fires
        // the initial schema fetch, exactly like a dropdown connection
        // switch does — `active_conn_spec` reads `conn_url` when no saved
        // connection is active yet, which is always true this early.
        let _ = window_handle.update(cx, |view, _window, cx| {
            if let Some(spec) = view.active_conn_spec() {
                view.trigger_schema_fetch(spec, cx);
            }
            // G3 Task 3 review fix: populate `history_cache` once at
            // startup (history panel defaults to visible) instead of
            // leaving it empty until the first recorded run/search edit.
            view.refresh_history_cache(cx);
        });
    });
}

#[cfg(test)]
mod preview_sql_tests {
    use super::*;

    #[test]
    fn quotes_schema_and_table_with_limit_1000() {
        assert_eq!(preview_sql(Some("public"), "orders"), "SELECT * FROM \"public\".\"orders\" LIMIT 1000");
    }

    #[test]
    fn omits_schema_qualifier_when_none() {
        assert_eq!(preview_sql(None, "orders"), "SELECT * FROM \"orders\" LIMIT 1000");
    }

    /// Brief contract #4: a table literally named `we"ird` must not break
    /// out of the query or inject anything — `quote_qualified` doubles the
    /// embedded quote.
    #[test]
    fn survives_a_table_name_with_an_embedded_quote() {
        assert_eq!(preview_sql(None, "we\"ird"), "SELECT * FROM \"we\"\"ird\" LIMIT 1000");
        assert_eq!(
            preview_sql(Some("we\"ird"), "t"),
            "SELECT * FROM \"we\"\"ird\".\"t\" LIMIT 1000"
        );
    }
}

/// G5 Task 3: `detect_editable_pk` — the PK-mapping/read-only/engine
/// decision behind a PREVIEW tab's `sandbox::Editable`. Pure/GPUI-free (the
/// snapshot lookup itself lives in `AppView::editable_for_preview`, which
/// needs a `Context` and so isn't unit-tested directly — same split
/// `fk_info_for_table`/`fk_info_from_table` already use).
#[cfg(test)]
mod editable_detection_tests {
    use super::*;
    use dbc_core::ColumnInfo;

    fn col(name: &str, is_pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.to_string(), is_pk, ..Default::default() }
    }

    fn table(columns: Vec<ColumnInfo>) -> TableInfo {
        TableInfo { name: "t".to_string(), columns, ..Default::default() }
    }

    fn headers(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn rw_engine(engine: dbc_state::Engine) -> Option<(bool, dbc_state::Engine)> {
        Some((false, engine))
    }

    #[test]
    fn table_not_in_snapshot_is_not_editable() {
        let h = headers(&["id", "name"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), None, &h),
            EditableDecision::NotEditable
        );
    }

    #[test]
    fn table_found_no_pk_column_at_all_is_no_primary_key() {
        let t = table(vec![col("id", false), col("name", false)]);
        let h = headers(&["id", "name"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), Some(&t), &h),
            EditableDecision::NoPrimaryKey
        );
    }

    #[test]
    fn table_found_pk_column_not_in_result_headers_is_no_primary_key() {
        // The table HAS a PK, but it isn't among this result's columns (e.g.
        // a hand-written SELECT that omitted it) — still "no PK mapped".
        let t = table(vec![col("id", true), col("name", false)]);
        let h = headers(&["name"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), Some(&t), &h),
            EditableDecision::NoPrimaryKey
        );
    }

    #[test]
    fn table_found_pk_mapped_writable_connection_is_editable() {
        let t = table(vec![col("id", true), col("name", false)]);
        let h = headers(&["id", "name"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), Some(&t), &h),
            EditableDecision::Editable(vec![0])
        );
    }

    #[test]
    fn multi_column_pk_maps_every_pk_column_in_table_order() {
        let t = table(vec![col("a", true), col("v", false), col("b", true)]);
        let h = headers(&["a", "v", "b"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), Some(&t), &h),
            EditableDecision::Editable(vec![0, 2])
        );
    }

    #[test]
    fn view_with_a_mapped_pk_is_not_editable() {
        // T3 review issue 3: even if a view somehow reports a PK column that
        // maps onto the headers, a view is never sandbox-editable — the
        // table-kind guard must reject it rather than relying on drivers never
        // marking view columns as PK.
        use dbc_core::TableKind;
        let t = TableInfo {
            name: "v".to_string(),
            kind: TableKind::View,
            columns: vec![col("id", true), col("name", false)],
            ..Default::default()
        };
        let h = headers(&["id", "name"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), Some(&t), &h),
            EditableDecision::NotEditable
        );
        // A materialized view is likewise not editable.
        let mv = TableInfo { kind: TableKind::MaterializedView, ..t };
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Postgres), Some(&mv), &h),
            EditableDecision::NotEditable
        );
    }

    #[test]
    fn read_only_connection_is_not_editable_even_with_a_mapped_pk() {
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert_eq!(
            detect_editable_pk(Some((true, dbc_state::Engine::Postgres)), Some(&t), &h),
            EditableDecision::NotEditable
        );
    }

    #[test]
    fn mssql_engine_is_not_editable_even_with_a_mapped_pk() {
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Mssql), Some(&t), &h),
            EditableDecision::NotEditable
        );
    }

    #[test]
    fn sqlite_engine_with_mapped_pk_is_editable() {
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Sqlite), Some(&t), &h),
            EditableDecision::Editable(vec![0])
        );
    }

    #[test]
    fn no_conn_meta_at_all_is_not_editable_even_with_a_mapped_pk() {
        // Defensive-only today (`run_query_with` always builds `Some(..)`
        // for both the saved-connection and CLI-arg paths — see
        // `conn_meta`'s doc comment) — `detect_editable_pk` must still fail
        // closed rather than assume editable when it has no read-only/
        // engine facts at all.
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert_eq!(detect_editable_pk(None, Some(&t), &h), EditableDecision::NotEditable);
    }
}

/// G5 Task 3: `engine_from_url` — the CLI-arg path's postgres-vs-sqlite
/// dispatch, mirroring `connect::open`'s own (untested-in-isolation, since
/// it also performs real I/O) prefix check.
#[cfg(test)]
mod engine_from_url_tests {
    use super::*;

    #[test]
    fn postgres_scheme_prefixes_map_to_postgres() {
        assert_eq!(engine_from_url("postgres://localhost/db"), dbc_state::Engine::Postgres);
        assert_eq!(engine_from_url("postgresql://localhost/db"), dbc_state::Engine::Postgres);
    }

    #[test]
    fn anything_else_is_treated_as_a_sqlite_file_path() {
        assert_eq!(engine_from_url("C:/data/app.db"), dbc_state::Engine::Sqlite);
        assert_eq!(engine_from_url("./relative.sqlite"), dbc_state::Engine::Sqlite);
        assert_eq!(engine_from_url(":memory:"), dbc_state::Engine::Sqlite);
    }
}

/// G4 Task 6: pure name↔ix mapping helpers behind view-prefs apply/save —
/// no GPUI, no I/O, so tested directly against fixture headers rather than
/// through a full `ResultGrid`/`ViewPrefsStore` round trip (that's covered
/// end-to-end by Task 1's persistence tests + manual review per the brief).
#[cfg(test)]
mod view_prefs_mapping_tests {
    use super::*;

    fn headers() -> Vec<String> {
        vec!["id".to_string(), "name".to_string(), "email".to_string()]
    }

    #[test]
    fn names_to_ixs_skips_missing_names_but_keeps_found_ones() {
        let h = headers();
        let names = vec!["name".to_string(), "ghost".to_string(), "id".to_string()];
        assert_eq!(names_to_ixs(&names, &h), vec![1, 0]);
    }

    #[test]
    fn names_to_ixs_empty_when_nothing_matches() {
        let h = headers();
        let names = vec!["ghost1".to_string(), "ghost2".to_string()];
        assert!(names_to_ixs(&names, &h).is_empty());
    }

    #[test]
    fn prefs_from_grid_state_maps_every_ix_to_its_current_name() {
        let h = headers();
        let hidden = vec![false, true, false];
        let widths = vec![100.0, 150.0, 200.0];
        let prefs =
            prefs_from_grid_state(&h, Some((2, false)), &hidden, &widths, vec!["id".to_string()]);
        assert_eq!(prefs.hidden_columns, vec!["name".to_string()]);
        assert_eq!(
            prefs.col_widths,
            vec![
                ("id".to_string(), 100.0),
                ("name".to_string(), 150.0),
                ("email".to_string(), 200.0),
            ]
        );
        assert_eq!(prefs.sort, Some(("email".to_string(), false)));
        assert_eq!(prefs.fk_joins, vec!["id".to_string()]);
    }

    #[test]
    fn prefs_from_grid_state_no_sort_stays_none() {
        let h = headers();
        let hidden = vec![false, false, false];
        let widths = vec![160.0, 160.0, 160.0];
        let prefs = prefs_from_grid_state(&h, None, &hidden, &widths, Vec::new());
        assert_eq!(prefs.sort, None);
        assert!(prefs.hidden_columns.is_empty());
        assert!(prefs.fk_joins.is_empty());
    }

    #[test]
    fn view_prefs_to_grid_state_roundtrips_through_names() {
        let h = headers();
        let prefs = TableViewPrefs {
            hidden_columns: vec!["email".to_string()],
            col_widths: vec![("name".to_string(), 222.0)],
            sort: Some(("name".to_string(), true)),
            fk_joins: vec!["id".to_string()],
        };
        let (sort, hidden, widths) = view_prefs_to_grid_state(&prefs, &h);
        assert_eq!(sort, Some((1, true)));
        assert_eq!(hidden, vec![false, false, true]);
        assert_eq!(widths, vec![(1, 222.0)]);
    }

    /// A saved sort column that no longer exists (renamed/dropped) is
    /// dropped entirely rather than falling back to some other column or
    /// panicking on an out-of-range index — brief contract #4.
    #[test]
    fn view_prefs_to_grid_state_ignores_a_missing_sort_column() {
        let h = headers();
        let prefs = TableViewPrefs {
            hidden_columns: Vec::new(),
            col_widths: Vec::new(),
            sort: Some(("deleted_col".to_string(), true)),
            fk_joins: Vec::new(),
        };
        let (sort, hidden, widths) = view_prefs_to_grid_state(&prefs, &h);
        assert_eq!(sort, None);
        assert_eq!(hidden, vec![false, false, false]);
        assert!(widths.is_empty());
    }

    #[test]
    fn view_prefs_to_grid_state_ignores_missing_hidden_and_width_names() {
        let h = headers();
        let prefs = TableViewPrefs {
            hidden_columns: vec!["ghost".to_string(), "id".to_string()],
            col_widths: vec![("ghost".to_string(), 50.0), ("email".to_string(), 300.0)],
            sort: None,
            fk_joins: Vec::new(),
        };
        let (sort, hidden, widths) = view_prefs_to_grid_state(&prefs, &h);
        assert_eq!(sort, None);
        assert_eq!(hidden, vec![true, false, false]);
        assert_eq!(widths, vec![(2, 300.0)]);
    }
}

/// Review fix (Task 6 round 1, Issue 1): `decide_join_pref_action` is the
/// pure save/retrigger/nothing decision that used to be an inline
/// `if !p.joins.is_empty()` check conflating "plain open" with "user
/// explicitly emptied the joins" — all 8 combinations of its 3 boolean
/// inputs, exhaustively.
#[cfg(test)]
mod decide_join_pref_action_tests {
    use super::*;

    #[test]
    fn from_join_change_always_saves_regardless_of_the_other_two_inputs() {
        // This is the Issue 1 fix itself: an explicit uncheck-to-zero
        // (from_join_change = true, joins_empty = true) must save, not
        // fall through to a retrigger that silently reverts it.
        assert_eq!(
            decide_join_pref_action(true, true, true),
            JoinPrefAction::Save
        );
        assert_eq!(
            decide_join_pref_action(true, true, false),
            JoinPrefAction::Save
        );
        assert_eq!(
            decide_join_pref_action(true, false, true),
            JoinPrefAction::Save
        );
        assert_eq!(
            decide_join_pref_action(true, false, false),
            JoinPrefAction::Save
        );
    }

    #[test]
    fn no_marker_non_empty_joins_saves_without_retriggering() {
        // A saved-fk-join retrigger's own `Started`: no marker, but its
        // joins are already the correct, non-empty state — save (idempotent)
        // and do not recurse into another retrigger (the original loop
        // guard).
        assert_eq!(
            decide_join_pref_action(false, false, true),
            JoinPrefAction::Save
        );
        assert_eq!(
            decide_join_pref_action(false, false, false),
            JoinPrefAction::Save
        );
    }

    #[test]
    fn no_marker_empty_joins_retriggers_only_when_saved_prefs_have_joins() {
        // A genuine plain preview open: retrigger iff saved prefs have a
        // non-empty fk_joins to restore, otherwise nothing to do.
        assert_eq!(
            decide_join_pref_action(false, true, true),
            JoinPrefAction::Retrigger
        );
        assert_eq!(
            decide_join_pref_action(false, true, false),
            JoinPrefAction::Nothing
        );
    }
}

// G5 Task 4 review fix (BLOCKER 1): `conn_identity_matches` — the pure
// decision behind the Apply flow's connection-identity guard.
#[cfg(test)]
mod conn_identity_matches_tests {
    use super::*;

    #[test]
    fn matches_when_identities_are_equal() {
        assert!(conn_identity_matches("conn-a", "conn-a"));
        assert!(conn_identity_matches(CLI_CONN_IDENTITY, CLI_CONN_IDENTITY));
    }

    #[test]
    fn does_not_match_when_identities_differ() {
        assert!(!conn_identity_matches("conn-a", "conn-b"));
        // Switched from a saved connection to the CLI-arg path (or vice
        // versa) is also a mismatch — never conflate the two.
        assert!(!conn_identity_matches("conn-a", CLI_CONN_IDENTITY));
        assert!(!conn_identity_matches(CLI_CONN_IDENTITY, "conn-a"));
    }
}

// G5 Task 4 review fix (MAJOR 3): `decide_retrigger_action` — the pure
// decision behind the saved-fk-join auto-retrigger's dirty-skip guard.
#[cfg(test)]
mod decide_retrigger_action_tests {
    use super::*;

    #[test]
    fn runs_when_tab_open_and_clean() {
        assert_eq!(decide_retrigger_action(true, None), RetriggerAction::Run);
    }

    #[test]
    fn skips_dirty_when_tab_open_but_has_staged_changes() {
        assert_eq!(decide_retrigger_action(true, Some(1)), RetriggerAction::SkipDirty);
        assert_eq!(decide_retrigger_action(true, Some(3)), RetriggerAction::SkipDirty);
    }

    #[test]
    fn skips_closed_when_tab_no_longer_open_regardless_of_dirty() {
        // Closed-tab takes priority: there is genuinely nothing to
        // retrigger against either way, and a closed tab can't report a
        // dirty count in practice, but the guard shouldn't depend on that.
        assert_eq!(decide_retrigger_action(false, None), RetriggerAction::SkipClosed);
        assert_eq!(decide_retrigger_action(false, Some(1)), RetriggerAction::SkipClosed);
    }
}
