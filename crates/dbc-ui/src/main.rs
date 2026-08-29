// RE-VERIFY MINOR-C. Every witness in this crate — `save_guard::SaveAllowed`
// and anything that follows it — rests on a private constructor, and
// `unsafe { std::mem::zeroed() }` forges any of them in one line, from
// anywhere, with no warning. A private field is only a rail while the
// crate cannot spell `unsafe`.
//
// `forbid` rather than `deny`: `deny` can be turned off again by an
// `#[allow(unsafe_code)]` on the offending item, which is one line and no
// warning — exactly the escape this is meant to close. `forbid` cannot be
// overridden anywhere below it. dbc-ui contains no `unsafe` today (the
// only occurrences are the word in comments and in the audit's own
// `fn`-spelling probes), so this costs nothing now and makes adding any
// later a deliberate, visible act.
#![forbid(unsafe_code)]

mod admin_panel;
mod admin_sql;
mod autocomplete;
mod backup;
mod chart_data;
mod chart_view;
mod compare;
mod connect;
mod connections_ui;
mod csv_import;
mod er_diagram_view;
mod export;
mod fk_join;
mod grid;
mod history_panel;
mod monitor;
mod monitor_sql;
mod monitor_view;
mod palette;
mod plan;
mod pwchange;
mod row_view;
mod runner;
mod sandbox;
mod schema_tree;
mod scripts;
mod sql_highlight;
mod tree_menu;
mod sql_input;
mod tabs;
mod text_model;
mod theme;
mod tunnel;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{
    apply_auto_limit_d, find_params, is_read_statement_d, quote_qualified_d, substitute_params,
    CancelToken, FkRef, QueryError, SchemaSnapshot, TableInfo,
};
use dbc_state::{
    AppConfig, ConnectionConfig, HistoryDb, HistoryEntry, ParamValue, ParamValuesStore,
    TableViewPrefs, Vault, ViewPrefsStore,
};
use gpui::{
    actions, div, prelude::*, px, size, uniform_list, AnyElement, App, Bounds, ClipboardItem,
    Context, Entity, FocusHandle, Focusable, KeyBinding, PathPromptOptions, ScrollDelta,
    ScrollWheelEvent,
    Window, WindowBounds, WindowOptions,
};
use gpui_platform::application;
use grid::{GridEvent, ResultGrid};
use palette::{PaletteAction, PaletteItem};
use runner::{
    ConnectSpec, CsvImportEvent, CsvImportJob, MultiQueryEvent, QueryEvent, QueryRunner,
    ScriptEvent, ScriptRunOptions,
};
use schema_tree::{SchemaTree, TreeEvent};
use sql_input::SqlInput;
use tabs::{
    collapse_title, ResultTab, ScriptFileRow, ScriptFileStatus, ScriptRunOutcome, ScriptRunState,
    TabContent, Tabs,
};
use theme::ActiveTheme;

actions!(
    dbc,
    [
        RunQuery,
        RunQueryUnlimited,
        CancelQuery,
        ToggleTree,
        ToggleHistory,
        OpenPalette,
        OpenAutocomplete,
        // Workspace T8 (Part S §5.2/§5.4): bound => save, unbound =>
        // save-as. Global, context `None`, same posture as `RunQuery`.
        SaveScript,
        /// Pretty-print the editor buffer (user request 2026-08-28).
        FormatSql
    ]
);

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

/// G6 T7: autocomplete popup state — `None` when closed. `candidates` is
/// recomputed lazily (`AppView::refresh_autocomplete`, driven by
/// `last_ac_text`/`last_ac_cursor`), exactly the `history_search`/
/// `last_history_query` lazy-diff idiom (history_panel.rs's module doc
/// comment) design §2 calls out by name.
struct AutocompleteState {
    candidates: Vec<autocomplete::Candidate>,
    selected: usize,
}

/// Pure: clamps `selected + delta` into `[0, len.saturating_sub(1)]` — no
/// wraparound (design §2: "Up/Down navigate ... (clamped)"). `len == 0`
/// always yields `0` (the popup is never rendered with zero candidates —
/// `refresh_autocomplete`/`on_open_autocomplete` both close it instead — but
/// this stays total rather than panicking on a hypothetical empty list).
fn move_selection(selected: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let max = (len - 1) as i32;
    (selected as i32 + delta).clamp(0, max) as usize
}

/// Pure decision table for the AppView wrapper-div popup-action handlers
/// (`on_ac_up`/`on_ac_down`/`on_ac_confirm`/`on_ac_confirm_tab`/
/// `on_ac_escape`): `true` means the handler should treat the action as its
/// own (consume it — no `cx.propagate()`); `false` means it must
/// propagate. Review round 3, BLOCKER: `on_ac_escape` previously returned
/// early WITHOUT propagating whenever `popup_open` was false, which
/// silently ate every `Escape` keystroke and made the global
/// `"escape" -> CancelQuery` binding (one level up the SAME bubble path)
/// unreachable while the SQL editor had focus — see each handler's own doc
/// comment for the full grounding. Trivial by construction (`popup_open`
/// IS the answer), but named and centralized so the contract is explicit
/// and every handler stays consistent rather than re-deriving it inline.
fn autocomplete_handles_action(popup_open: bool) -> bool {
    popup_open
}

/// Pure: given `text`, `cursor`, and the candidate's `text` to insert,
/// returns the byte range to replace (the identifier prefix ending at
/// `cursor`, or an empty range at `cursor` if there is none — e.g. a
/// force-triggered accept with no partial prefix typed) and the final
/// string. Extracted so T7's `accept_completion` wiring has a pure,
/// directly-testable core instead of only being exercisable through a live
/// `SqlInput` (plan T7 step 1).
///
/// The replaced range is "the identifier prefix ending EXACTLY at `cursor`"
/// (`autocomplete::cursor_context`'s own contract) — if the cursor sits in
/// the MIDDLE of a longer identifier (e.g. `usXer` with the cursor after
/// `us`), only the part BEFORE the cursor is replaced; whatever comes after
/// the cursor is left untouched, not merged/deduped against the inserted
/// text. This is intentional (review round 3 NIT): matching most editors'
/// "complete up to the cursor" model rather than attempting to also
/// understand/replace a suffix the user hasn't necessarily finished typing.
///
/// RE-VERIFY: the RANGE half is now the shared rail, because
/// `SqlInput::accept_completion` uses it too. That function used to be
/// handed a `prefix_len` by its caller, which is what let a caller pass
/// `text().len()` and wipe the whole unsaved buffer with no permit and no
/// audited identifier. There is one definition of „the identifier prefix
/// ending at the cursor" and both the pure test and the live buffer
/// surgery read it.
pub(crate) fn completion_range(text: &str, cursor: usize) -> std::ops::Range<usize> {
    let ctx = autocomplete::cursor_context(text, cursor);
    let start = cursor - ctx.prefix.len();
    start..cursor
}

/// The pure whole-string form, kept for the unit tests that pin the
/// mid-word behaviour described above. Production goes through
/// [`completion_range`] and splices in the buffer.
#[cfg(test)]
fn completion_edit(text: &str, cursor: usize, insert: &str) -> (std::ops::Range<usize>, String) {
    let range = completion_range(text, cursor);
    let mut new_text = text.to_string();
    new_text.replace_range(range.clone(), insert);
    (range, new_text)
}

/// G2 Task 7 (G15 §2d: dialect-aware): SQL builder for `TreeEvent::
/// OpenPreview`. Pure — no GPUI, no I/O — so quoting can be unit-tested
/// directly. `quote_qualified_d` (shared with `synthesize_create_table_d`'s
/// DDL quoting) is what makes this safe against a table literally named
/// `we"ird`: the embedded quote is doubled, not smuggled into the query as
/// SQL syntax. `LIMIT 1000` is invalid T-SQL — `TOP 1000` is the
/// grammar-correct cap for MSSQL.
fn preview_sql(dialect: dbc_core::Dialect, schema: Option<&str>, table: &str) -> String {
    let target = quote_qualified_d(dialect, schema, table);
    match dialect {
        dbc_core::Dialect::Mssql => format!("SELECT TOP 1000 * FROM {target}"),
        _ => format!("SELECT * FROM {target} LIMIT 1000"),
    }
}

/// G6 Task 3: substitutes `sql_template`'s `:name` params (via
/// `sandbox::sql_value`, `numeric = true` for opportunistic unquoting per
/// the design doc's §3) using `values` (same order as `names`; `(text,
/// is_null)` per entry), then re-scans the result and refuses if any bare
/// `:name` survives — the CURATION-mandated defense (design §5) against a
/// substituted-but-still-parametrized query silently reaching the driver
/// (e.g. SQLite's own native `:name`/`@name`/`$name` bind-parameter syntax
/// binding NULL to whatever wasn't actually replaced), for every engine.
/// Pure — no GPUI — so it's directly unit-testable, and it's the single
/// source of truth `render_query_params_panel`'s live preview and
/// `confirm_query_params`'s actual dispatch both go through, so the
/// preview can never diverge from what running would do.
fn build_param_sql(
    sql_template: &str,
    names: &[String],
    values: &[(String, bool)],
) -> Result<String, String> {
    let lookup: std::collections::HashMap<&str, &(String, bool)> =
        names.iter().map(String::as_str).zip(values.iter()).collect();
    let substituted = substitute_params(sql_template, &mut |name| match lookup.get(name) {
        Some((_, true)) => sandbox::sql_value(None, true),
        Some((text, false)) => sandbox::sql_value(Some(text.as_str()), true),
        None => sandbox::sql_value(None, true), // unreachable: every :name in sql_template is in `names`
    })
    .ok_or_else(|| "nepodařilo se sestavit SQL".to_string())?;

    match find_params(&substituted) {
        Some(remaining) if !remaining.is_empty() => {
            Err("po dosazení hodnot zůstal v SQL neplatný parametr — spuštění zrušeno".to_string())
        }
        Some(_) => Ok(substituted),
        // Fail closed: an unscannable substitution result (e.g. a value that
        // re-opened a dollar-quote, `$tag:x$` + `1` → `$tag1$…`) means the
        // rescan proved nothing — refuse rather than hand the driver SQL the
        // guard chain could not inspect.
        None => Err("po dosazení hodnot nelze SQL znovu ověřit — spuštění zrušeno".to_string()),
    }
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
/// form at all in this app, so it never needs a branch here). G16: a
/// `.duckdb` CLI arg is an explicit non-goal (design §3) — saved
/// connections are the only DuckDB entry point, so no branch here either.
fn engine_from_url(url: &str) -> dbc_state::Engine {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        dbc_state::Engine::Postgres
    } else {
        dbc_state::Engine::Sqlite
    }
}

// ---------------------------------------------------------------------
// Workspace T4 — startup context resolution (design §W4).
// ---------------------------------------------------------------------

/// Folder name used for the BLOCKED start's paths when the pointer names
/// nothing we can trust, so there is no target folder to name.
///
/// Deliberately not a real directory: nothing in this app ever creates it
/// deliberately, so every store OPEN against it fails and finds nothing —
/// which is the half that carries design §W4's never-a-silent-fallback
/// rule.
///
/// FINAL-REVIEW MINOR-3: an earlier version of this comment also claimed
/// „every SAVE against it fails loudly", and that was FALSE. All four
/// store savers `create_dir_all` their own parent first
/// (`config.rs:154`, `vault.rs:216`, `view_prefs.rs:65`, `params.rs:53` —
/// `fsutil::write_atomic` correctly does not), so a stray save would
/// SUCCEED and leave `%APPDATA%\dbc\__pracovni-prostor-nenalezen__\config.toml`
/// on disk. Debris in a folder nobody reads, not a data loss — the
/// invariant that actually matters, „never the profile's real files", is
/// structural (see [`blocked_base`]) and pinned — but the stated property
/// was not the real one, so it is corrected rather than quietly relied on.
///
/// The comment was corrected rather than the behaviour made to match it.
/// Making the sentinel genuinely unwritable means either a
/// platform-specific unusable name (`NUL` fails `create_dir_all` on
/// Windows and succeeds on Linux — a rail that silently stops working on
/// one target is worse than an honest comment) or removing
/// `create_dir_all` from four PRE-EXISTING profile-store savers, which is
/// what makes a first run work at all and is a `dbc-state` refactor this
/// phase has already declined once (as-built §C). Neither buys anything:
/// nothing can reach a save on a blocked start
/// (`StartupContext::loads` opens no store, and the modal cannot be
/// dismissed), and if something ever could, the file it would create is
/// not one the app will read back.
const BLOCKED_WORKSPACE_SENTINEL: &str = "__pracovni-prostor-nenalezen__";

/// Paths for a BLOCKED start (design §W4). They point INTO the unusable
/// workspace (or, when the pointer is unreadable, into a sentinel folder
/// that does not exist) — NEVER at the profile's real files. Two reasons,
/// both binding: (a) never a silent fallback — a bug that dismissed the
/// blocking modal must find nothing to connect to; (b) never destructive —
/// an empty default config saved over `%APPDATA%\dbc\config.toml` would
/// erase connections the user never agreed to lose.
pub(crate) fn blocked_paths(root: Option<&Path>) -> dbc_state::workspace::Paths {
    let profile = dbc_state::workspace::profile_dir();
    // T4 review MAJOR-2: the doc comment above promises "NEVER at the
    // profile's real files", but `workspace_paths(root)` was handed
    // whatever the POINTER said — and a `workspace.toml` containing
    // `path = "…\AppData\Roaming\dbc"` resolves to exactly the profile's
    // real `config.toml`/`vault.bin`/`views.toml`/`params.toml`. The app
    // cannot write such a pointer itself (init demands `Empty`, adopt
    // demands a marker) and no writer is reachable while the blocking
    // modal is up — but "not reachable today" is not an invariant, and the
    // failure mode is an empty default config written over the user's real
    // connections. So the promise is now STRUCTURAL: a root that resolves
    // to the profile dir falls through to the sentinel.
    //
    // FINAL-REVIEW MINOR-2, the residual the structural promise still had.
    // The pointer's `path` is arbitrary TOML text that nothing validates
    // on the way IN (`write_pointer` demands an absolute path, but a
    // hand-edited `workspace.toml` never goes through it and
    // `read_pointer` hands back whatever string it finds). A `path = ""`
    // therefore reached here as `Some("")`, and every guard above missed
    // it: `"" != profile`, and `Path::new("").canonicalize()` ERRORS on
    // Windows, so `is_same_dir` answered `false` and `base` became `""`.
    // `workspace_paths("")` then yields the bare RELATIVE names
    // `config.toml`, `vault.bin`, … which the OS resolves against the
    // process CWD — and if the app was launched from `%APPDATA%\dbc`,
    // those ARE the profile's real files, i.e. exactly what the doc
    // comment above promises can never happen.
    //
    // Not reachable today (a blocked start disables every store, see
    // `StartupContext::loads`), and that is deliberately not the standard
    // this function is held to — the T4 review MAJOR-2 note two paragraphs
    // up says so in as many words. So: a root we cannot name ABSOLUTELY is
    // not a root at all, and falls through to the sentinel with everything
    // else we cannot trust.
    let base = match blocked_base(root, &profile) {
        Some(r) => r,
        None => profile.join(BLOCKED_WORKSPACE_SENTINEL),
    };
    dbc_state::workspace::workspace_paths(&base)
}

/// The folder a BLOCKED start's paths may point into, or `None` for „use
/// the sentinel". Split out so [`blocked_paths`]'s rule is testable
/// against a supplied profile dir rather than the real one.
///
/// Three ways to fail, all meaning the same thing — we do not know where
/// this pointer meant, so we must name somewhere that does not exist:
/// the pointer named nothing (`""`); it named something RELATIVE; or it
/// named the profile directory itself.
///
/// The relative arm is deliberately absolute-or-nothing rather than the
/// review's narrower „relative and non-canonicalizable". Canonicalizing
/// `..` succeeds — and resolves against the process CWD, which this app
/// does not set and an installer shortcut may well point at the profile
/// dir. That is the same CWD dependence the empty path had, just harder
/// to see. `write_pointer` already refuses to WRITE a relative root, for
/// this exact reason spelled out in its own doc („a relative path would
/// round-trip into a DIFFERENT folder"); this is the matching refusal on
/// the way back IN, where a hand-edited pointer arrives without ever
/// having passed that rail.
fn blocked_base(root: Option<&Path>, profile: &Path) -> Option<PathBuf> {
    let r = root?;
    if !r.is_absolute() {
        return None;
    }
    (!is_same_dir(r, profile)).then(|| r.to_path_buf())
}

/// "Do these two paths name the same directory?" — exact match first (works
/// for paths that do not exist), then a canonicalised compare, which on
/// Windows also normalises casing, `.`/`..` segments and short names.
/// `canonicalize` FAILS on a non-existent path, so a failure means the two
/// cannot both be the (existing) profile dir and `false` is the right
/// answer — the exact-match arm above has already covered the identical
/// spelling case.
fn is_same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// THE scripts-root seam (design §W8), as a free fn so both arms are
/// testable without an `AppView`. Workspace mode always wins and always
/// resolves to `<workspace>/scripts`: a per-workspace override would
/// reintroduce absolute paths into a folder whose whole point is
/// portability, so `AppConfig.scripts_dir` is INERT there — deliberately
/// not "merged", not "preferred if set". Profile mode is Part S §2's
/// behavior, unchanged.
pub(crate) fn scripts_root_for(
    workspace_root: Option<&Path>,
    scripts_dir: Option<&str>,
) -> Option<PathBuf> {
    match workspace_root {
        Some(root) => Some(root.join(dbc_state::workspace::SCRIPTS_SUBDIR)),
        None => scripts_dir.map(PathBuf::from),
    }
}

/// Which persistent stores `main()` is ALLOWED to open. Exists so design
/// §W4's actual enforcement — "a broken pointer loads NOTHING" — is a
/// pinned unit test instead of three `blocked.is_some()` conditions buried
/// in `fn main()`, which no test can reach (T4 review MINOR-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupLoads {
    /// `AppConfig::load` — the connection list. NEVER on a blocked start.
    pub config: bool,
    /// `ViewPrefsStore::load`.
    pub view_prefs: bool,
    /// `ParamValuesStore::load`.
    pub param_values: bool,
    /// `HistoryDb::open` — machine-local in BOTH modes (§W5), so it opens
    /// even on a blocked start: it carries no context-specific connection
    /// state and losing it would degrade an unrelated feature.
    pub history: bool,
}

/// What `main()` needs to know before it opens a single store. Pure over
/// `Resolution` (workspace T2), so the whole never-silent-fallback rule is
/// testable without a filesystem.
pub(crate) struct StartupContext {
    /// Where every store opens from.
    pub paths: dbc_state::workspace::Paths,
    /// `Some(root)` = workspace mode. Drives the Settings block (T5) and
    /// `effective_scripts_root` (T7). A BROKEN workspace is NOT an active
    /// workspace, so this stays `None` while `blocked` is `Some`.
    pub workspace_root: Option<PathBuf>,
    /// `Some((root, reason))` ⇒ open `ModalState::WorkspaceMissing` and load
    /// NOTHING (design §W4).
    pub blocked: Option<(Option<PathBuf>, String)>,
}

/// Turns a [`dbc_state::workspace::Resolution`] into the three things
/// `main()` acts on. Pure — no I/O, no GPUI.
impl StartupContext {
    /// Design §W4, as a value rather than as control flow. A blocked start
    /// opens NOTHING that carries context: not the workspace's files (they
    /// are unusable — that is what "broken" means) and above all not the
    /// profile's (that would be the silent fallback this design bans).
    pub(crate) fn loads(&self) -> StartupLoads {
        let blocked = self.blocked.is_some();
        StartupLoads {
            config: !blocked,
            view_prefs: !blocked,
            param_values: !blocked,
            history: true,
        }
    }
}

pub(crate) fn startup_context(res: dbc_state::workspace::Resolution) -> StartupContext {
    match res {
        dbc_state::workspace::Resolution::Profile(paths) => {
            StartupContext { paths, workspace_root: None, blocked: None }
        }
        dbc_state::workspace::Resolution::Workspace { root, paths } => {
            StartupContext { paths, workspace_root: Some(root), blocked: None }
        }
        dbc_state::workspace::Resolution::Broken { root, reason } => StartupContext {
            paths: blocked_paths(root.as_deref()),
            workspace_root: None,
            blocked: Some((root, reason)),
        },
    }
}

/// May a „Najít složku…" continuation still commit its pick? (T4 review
/// MAJOR-1.) Extracted as a pure predicate — the `pwchange::esc_closable`
/// precedent — because the racing code path lives inside a `cx.spawn`
/// continuation that no unit test can drive.
///
/// TWO independent conditions, and both are load-bearing:
///
/// * the `WorkspaceMissing` modal must still be OPEN — if the user has
///   meanwhile clicked „Použít lokální profil", the app is in profile mode
///   by their EXPLICIT choice, and a folder pick landing afterwards would
///   override it silently, which is the whole class of bug §W4 exists to
///   prevent;
/// * the pick's generation must still be current — every context swap
///   (`apply_context`) bumps it, so a superseded pick is inert even in the
///   window where a new modal has been opened by something else.
pub(crate) fn recovery_pick_may_commit(
    workspace_missing_modal_open: bool,
    dispatched_generation: u64,
    current_generation: u64,
) -> bool {
    workspace_missing_modal_open && dispatched_generation == current_generation
}

/// What `start_workspace_pick`'s continuation is allowed to do when its
/// folder classification finally lands (T5 review MAJOR-1).
///
/// The race, and why `recovery_pick_may_commit`'s shape is not enough
/// here: the platform folder picker IS modal to the app, but the
/// `classify()` that follows it is a `cx.background_spawn` — it yields the
/// UI thread, and `ModalState::Settings` is Esc-closable. So between the
/// pick and its result the user can close Settings and reach ANY other
/// state: a second workspace pick that is already initializing, a running
/// `BackupRestore`, a half-typed `ConnectionDialog`. `open_workspace_confirm`
/// used to raw-assign `self.modal` regardless, which meant a stale
/// continuation could (a) reset a `running: true` confirm's latch and let
/// the context swap after the user dismissed it, (b) overwrite a running
/// backup session WITHOUT going through `close_modal` (the one teardown
/// funnel that cancels the child process), or (c) simply wipe a typed
/// password.
///
/// Two refusals, deliberately distinct, because they deserve different
/// treatment:
/// * `Superseded` — a context swap has happened under the task
///   (`apply_context` bumps the generation). Say NOTHING and change
///   nothing: the user has already reached a newer, explicit decision, and
///   even a status line would be a stale write over it. This is
///   `recovery_pick_may_commit`'s posture, for the same reason.
/// * `OtherDialog` — the generation is current, but the app is no longer
///   sitting on the Settings modal this pick started from. A status line
///   here is safe and informative (`WORKSPACE_PICK_DISCARDED`), matching
///   `start_script_pick`'s and `start_csv_import`'s „… zahozen — je
///   otevřený jiný dialog" wording.
///
/// The generation is checked FIRST so a superseded pick stays silent even
/// when it is also in the wrong modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspacePickVerdict {
    /// Commit: open the confirm modal over the Settings modal.
    Open,
    /// A context swap happened under this pick — inert AND silent.
    Superseded,
    /// Still current, but some other dialog owns the screen — refuse with
    /// a status line.
    OtherDialog,
}

/// The guard behind [`WorkspacePickVerdict`]. Pure, because the
/// continuation it protects is a `cx.spawn` no unit test can drive — the
/// `recovery_pick_may_commit` precedent.
pub(crate) fn workspace_pick_verdict(
    settings_modal_open: bool,
    dispatched_generation: u64,
    current_generation: u64,
) -> WorkspacePickVerdict {
    if dispatched_generation != current_generation {
        return WorkspacePickVerdict::Superseded;
    }
    if !settings_modal_open {
        return WorkspacePickVerdict::OtherDialog;
    }
    WorkspacePickVerdict::Open
}

/// G12 T5: engine -> splitter dialect for the editor's multi-statement
/// unlock / script runner. `Mssql -> Some(Dialect::Mssql)` since G15 T8's
/// ON-flip (live-verified: `mssql_go_script_with_procedure_and_top_auto_limit_live`
/// runs a real GO-batched script — incl. a `CREATE PROCEDURE` body with
/// interior semicolons — against a live server through this exact
/// dispatch). A future engine without a dialect at all would still map to
/// `None` here (today's single-statement fallback); no such engine exists
/// on this branch's `dbc_state::Engine`.
fn dialect_for_engine(engine: dbc_state::Engine) -> Option<dbc_core::Dialect> {
    match engine {
        dbc_state::Engine::Postgres => Some(dbc_core::Dialect::Postgres),
        dbc_state::Engine::Sqlite => Some(dbc_core::Dialect::Sqlite),
        dbc_state::Engine::Mssql => Some(dbc_core::Dialect::Mssql),
        // G16: DuckDB maps to the pg dialect — `"…"`-doubling ident
        // quoting, trailing `LIMIT n`, `$tag$` dollar-quote splitting are
        // all exactly DuckDB's rules (G12 curation item 2 delivered).
        dbc_state::Engine::Duckdb => Some(dbc_core::Dialect::Postgres),
    }
}

/// G15 §2d: total Engine -> Dialect mapping for SQL COMPOSITION (sandbox
/// Apply, CSV import, preview/fk-join SELECTs, admin_sql delegation).
/// Distinct from `dialect_for_engine` (the SPLITTER gate, above) — both
/// return `Some(Dialect::Mssql)`/`Dialect::Mssql` for MSSQL since G15 T8's
/// ON-flip, but this one existed independently before that flip too:
/// composers needed the dialect even while the multi-statement path was
/// still gated — an MSSQL connection's Apply dialog had to show/execute
/// bracket-quoted, `N''`-literal SQL before `run_query_with`'s
/// multi-statement unlock went live for it.
fn sql_dialect(engine: dbc_state::Engine) -> dbc_core::Dialect {
    match engine {
        dbc_state::Engine::Postgres => dbc_core::Dialect::Postgres,
        dbc_state::Engine::Sqlite => dbc_core::Dialect::Sqlite,
        dbc_state::Engine::Mssql => dbc_core::Dialect::Mssql,
        // G16: pg-dialect composition is exactly DuckDB's SQL (G12
        // curation item 2) — see `dialect_for_engine`'s arm.
        dbc_state::Engine::Duckdb => dbc_core::Dialect::Postgres,
    }
}

/// `run_query_with`'s Guard 1 pure decision (batch C review BLOCKER 2):
/// `true` means refuse. Dialect-aware via `is_read_statement_d` — for MSSQL
/// this client-side check is the ONLY read-only enforcement (no server-side
/// backstop, driver integration note 5), so it must not false-reject a
/// bracket-quoted reserved word like `SELECT [Delete] FROM AuditLog`.
/// Extracted so this is directly unit-testable without a GPUI `Context`.
fn read_only_guard_rejects(sql: &str, read_only: bool, dialect: dbc_core::Dialect) -> bool {
    read_only && !is_read_statement_d(sql, dialect)
}

/// G15 §2c: `SplitError` -> user-facing Czech text. Used by
/// `count_statements_in_file` and `run_query_with`'s `split_sql` `Err(e)`
/// arm; `runner.rs`'s script path duplicates the one Czech literal
/// deliberately (T4/T5 stay parallel-safe, disjoint files).
pub(crate) fn split_error_message(e: dbc_core::SplitError) -> String {
    match e {
        dbc_core::SplitError::UnsupportedGoCount => {
            "GO s počtem opakování není podporováno".to_string()
        }
        other => format!("{other:?}"),
    }
}

/// G12 T5: per-statement auto-limit (design §4) — only bare `SELECT`s in
/// the already-split statement list get a `LIMIT` appended (before the
/// split, a multi-statement blob never got limited at all: `apply_auto_limit`
/// only fires when the WHOLE string starts with `SELECT`, guards.rs). Returns
/// the rewritten list plus whether ANY statement changed (drives the caller's
/// " · auto-LIMIT {n}" status suffix, same convention as the single-statement
/// path).
fn auto_limit_each(
    statements: Vec<String>,
    limit: Option<u64>,
    bypass: bool,
    dialect: dbc_core::Dialect,
) -> (Vec<String>, bool) {
    let Some(n) = limit.filter(|_| !bypass) else { return (statements, false) };
    let mut changed_any = false;
    let out = statements
        .into_iter()
        .map(|s| {
            let (rewritten, changed) = apply_auto_limit_d(&s, n, dialect);
            changed_any |= changed;
            rewritten
        })
        .collect();
    (out, changed_any)
}

/// G12 T3: read-chunk size for streaming a `.sql` file through its own
/// `StatementSplitter` — mirrors `runner::SCRIPT_READ_CHUNK`'s size (kept as
/// an independent constant since `main.rs` doesn't depend on `runner`'s
/// private items; the two are intentionally the same value).
const SCRIPT_COUNT_CHUNK: usize = 64 * 1024;

/// G12 T3: streams `path` through a fresh `StatementSplitter` (never the UI
/// thread — always called inside `cx.background_spawn`) solely to COUNT
/// statements for the pre-scan modal. An IO error or a split failure
/// (including an unterminated construct at EOF) yields `Err(text)` — shown
/// in the status line, the run is not offered for that file.
fn count_statements_in_file(path: &std::path::Path, dialect: dbc_core::Dialect) -> Result<usize, String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut splitter = dbc_core::StatementSplitter::new(dialect);
    let mut buf = vec![0u8; SCRIPT_COUNT_CHUNK];
    let mut count = 0usize;
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        let stmts = splitter
            .push(&buf[..n])
            .map_err(|e| format!("{}: {}", path.display(), split_error_message(e)))?;
        count += stmts.len();
    }
    match splitter.finish() {
        Ok(Some(_)) => count += 1,
        Ok(None) => {}
        Err(e) => return Err(format!("{}: {}", path.display(), split_error_message(e))),
    }
    Ok(count)
}

/// G12 T3: non-recursive `*.sql` listing (case-insensitive extension),
/// ordered by `file_name()` string comparison — NOT full path (design §3).
fn list_sql_files(dir: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // T9 review NIT-3: THE extension rail (design §1.5), shared with
        // the scan, the editor's save-as and the library's run.
        if crate::scripts::is_sql_path(&path) {
            files.push(path);
        }
    }
    files.sort_by(|a, b| {
        a.file_name().unwrap_or_default().to_string_lossy().cmp(&b.file_name().unwrap_or_default().to_string_lossy())
    });
    Ok(files)
}

/// G12 T3: the design §2 matrix's UI rule — whole-run transaction scope is
/// only selectable under a Stop error policy (never `Continue` inside one
/// open transaction, see `runner::failure_action`'s defensive fallback for
/// the runner's own belt-and-braces enforcement of this same rule).
fn script_options_valid(scope: runner::TxScope, policy: runner::ErrorPolicy) -> bool {
    !(scope == runner::TxScope::WholeRun && policy == runner::ErrorPolicy::Continue)
}

/// G12 T3: history `sql` synthesis (design §3) — a synthetic description,
/// NEVER file contents (§3-novela: no credential/result data leaks into
/// history beyond what the app's existing convention already logs — see
/// `AppView::confirm_script_run`'s doc comment for the full rationale).
fn script_history_sql(files: &[(PathBuf, usize)], statements_run: usize, statements_failed: usize) -> String {
    let total: usize = files.iter().map(|(_, n)| n).sum();
    if files.len() == 1 {
        format!(
            "[skript] {} — {total} příkazů, {statements_run} OK, {statements_failed} chyb",
            files[0].0.display()
        )
    } else {
        format!(
            "[skript] {} souborů, {total} příkazů, {statements_run} OK, {statements_failed} chyb",
            files.len()
        )
    }
}

/// G12 T3/T4: `TabContent::ScriptRun`'s render — a free function (not an
/// `AppView` method) precisely so it can be called from inside
/// `AppView::render_tab_content`'s `match &active.content` without
/// conflicting with `active`'s still-live borrow of `self.tabs` (see the
/// call site's comment). Renders the summary bar (files/statements/rows
/// progress, elapsed, outcome, "Zrušit" while running), the per-file status
/// list, and the log tail — reusing `TabContent::Text`'s wrapped-monospace
/// idiom for the log rather than a scrollbar (same "render everything,
/// newest lines visible" posture as `push_log`'s cap already assumes).
fn render_script_run_tab(state: Rc<RefCell<ScriptRunState>>, cx: &mut Context<AppView>) -> AnyElement {
    let s = state.borrow();
    let files_done = s
        .files
        .iter()
        .filter(|f| !matches!(f.status, ScriptFileStatus::Pending | ScriptFileStatus::Running))
        .count();
    let files_total = s.files.len();
    let elapsed = s.elapsed.unwrap_or_else(|| s.started_at.elapsed());
    let running = matches!(s.outcome, ScriptRunOutcome::Running);
    let theme = *cx.theme();
    let (outcome_label, outcome_color) = match s.outcome {
        ScriptRunOutcome::Running => ("běží…", theme.warn),
        ScriptRunOutcome::Done => ("Hotovo", theme.success),
        ScriptRunOutcome::Failed => ("Selhalo", theme.danger),
        // G14 T9: no Rulebook-listed field for this exact hex (0x9399b2,
        // Catppuccin overlay2) — nearest role match is the general
        // secondary/de-emphasized text field, kept distinct from
        // warn/success/danger used by the other three arms above.
        ScriptRunOutcome::Cancelled => ("Zrušeno", theme.text_muted),
    };

    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .p_2()
        .bg(theme.bg_app)
        .text_color(theme.text_primary)
        .child(format!("{files_done}/{files_total} souborů"))
        .child(format!("{}/{} příkazů", s.statements_run, s.total_statements));
    if let Some((done, total)) = s.progress_rows {
        header = header.child(format!("{done}/{total} řádků"));
    }
    header = header
        .child(format!("{:.1}s", elapsed.as_secs_f32()))
        .child(div().text_color(outcome_color).child(outcome_label));
    if running {
        header = header.child(
            div()
                .id("script-run-cancel")
                .cursor_pointer()
                .px_2()
                .py_1()
                .bg(theme.diff_deleted_bg)
                .rounded_md()
                .child("Zrušit")
                .on_click(cx.listener(|view, _, _, cx| {
                    if let Some(c) = view.cancel.take() {
                        c.cancel();
                        view.status = "cancelling…".to_string();
                    }
                    cx.notify();
                })),
        );
    }

    let mut file_list = div().flex().flex_col().gap_1().p_2().text_color(theme.text_muted);
    for f in &s.files {
        let glyph = match f.status {
            ScriptFileStatus::Pending => "·",
            ScriptFileStatus::Running => "▶",
            ScriptFileStatus::Done => "✓",
            ScriptFileStatus::Failed => "✗",
            ScriptFileStatus::Skipped => "⊘",
        };
        file_list = file_list.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(format!(
                    "{glyph} {} ({} OK, {} chyb)",
                    f.name, f.statements_run, f.statements_failed
                )),
        );
    }

    let mut log_body = div()
        .id("script-run-log")
        .font_family("Consolas")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_hidden()
        .p_2()
        .text_color(theme.text_muted);
    for line in s.log.iter() {
        log_body = log_body.child(div().child(line.clone()));
    }

    div()
        .flex()
        .flex_col()
        .flex_1()
        .bg(theme.bg_panel)
        .child(header)
        .child(file_list)
        .child(log_body)
        .into_any_element()
}

/// G12 T4: any empty CSV field -> SQL NULL, any non-empty field -> a value
/// (the `csv` crate 1.4.0's `StringRecord` unescapes fields and retains no
/// "was this quoted" metadata, verified against the resolved crate's source
/// per Task 4 Step 1 — `a,,c` and `a,"",c` are indistinguishable post-parse,
/// so the design's quoted-empty-vs-unquoted-empty distinction is
/// unimplementable without hand-writing an RFC-4180 scanner, which §5
/// explicitly decided against). Also used by `runner::run_csv_import_inner`
/// (called there as `crate::csv_field_to_value`) for the actual import, not
/// just this preview/mapping path — one rule, one place.
pub(crate) fn csv_field_to_value(field: &str) -> Option<String> {
    if field.is_empty() { None } else { Some(field.to_string()) }
}

/// G12 T4: auto-maps CSV headers onto target columns by case-insensitive
/// name equality; any header with no matching column starts as skipped
/// (`None`) — the user can still map it manually via the mapping modal's
/// cycle-button.
fn default_csv_mapping(
    headers: &[String],
    columns: &[csv_import::TargetColumn],
) -> csv_import::ColumnMapping {
    let targets = headers
        .iter()
        .map(|h| columns.iter().position(|c| c.name.eq_ignore_ascii_case(h)))
        .collect();
    csv_import::ColumnMapping { targets }
}

/// G12 T4 review fix (BLOCKER): pure decision behind `confirm_csv_import`'s
/// connection-identity guard — the file picker + background pre-count pass
/// in `start_csv_import` don't block the UI, so the connection dropdown
/// stays clickable while `ModalState::CsvImport` is being built and while
/// it's open. `captured_identity` is the identity `start_csv_import`
/// snapshotted at dispatch time (before the picker ever opened);
/// `current_identity` is `self.current_conn_identity()` evaluated fresh at
/// confirm time. `false` means "the active connection changed under this
/// import" — `confirm_csv_import` refuses (closes the modal, sets a status
/// message) BEFORE resolving a spec or building a `CsvImportJob`, so a
/// stale `(schema, table, columns)` snapshot can never be dispatched
/// against a different, currently-active (writable) database. Just
/// `conn_identity_matches` under a task-specific name — pulled out as its
/// own named predicate (not an inline call) so this guard has a direct
/// unit test without needing a full GPUI window (`confirm_csv_import`
/// itself can't be driven headlessly).
fn csv_import_dispatch_allowed(captured_identity: &str, current_identity: &str) -> bool {
    conn_identity_matches(captured_identity, current_identity)
}

/// G12 T3 review fix (MAJOR 1): pure decision behind `confirm_script_run`'s
/// connection-identity guard — same shape/rationale as
/// `csv_import_dispatch_allowed` above (the script picker + background
/// pre-scan pass don't block the UI either, so the connection dropdown
/// stays clickable while `ModalState::ScriptRun` is being built and while
/// it's open). `captured_identity` is what `start_script_pick` snapshotted
/// before the picker ever opened; `current_identity` is
/// `self.current_conn_identity()` evaluated fresh at confirm time.
fn script_run_dispatch_allowed(captured_identity: &str, current_identity: &str) -> bool {
    conn_identity_matches(captured_identity, current_identity)
}

/// G12 T4 review fix (MINOR 5): char budget for `ModalState::CsvImport`'s
/// DISPLAYED `sample_sql` — a real first-batch `INSERT` can run to
/// several hundred rows' worth of text. `self.modal` is `.clone()`d every
/// render frame (an app-wide convention this fix deliberately does NOT
/// restructure — see the review), so the cap is applied ONCE, wherever
/// `sample_sql` is computed (`start_csv_import`'s completion closure,
/// `recompute_csv_sample`), never inside a render function — the STORED
/// string is already display-ready, not re-truncated per frame.
const CSV_SAMPLE_SQL_DISPLAY_CAP: usize = 2000;

fn cap_sql_sample(sql: String) -> String {
    if sql.chars().count() <= CSV_SAMPLE_SQL_DISPLAY_CAP {
        sql
    } else {
        let truncated: String = sql.chars().take(CSV_SAMPLE_SQL_DISPLAY_CAP).collect();
        format!("{truncated}…")
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
    // G15 T8 ON-flip: MSSQL sandbox editing — live-verified
    // (mssql_sandbox_apply_bracket_quoted_weird_column_and_czech_diacritics_live
    // in runner.rs's mssql_docker_tests: bracket-quoted UPDATE/INSERT with a
    // `we]ird` column name and a Czech-diacritics N'' value staged, applied,
    // and re-read correctly against a live server). `read_only` still
    // excludes any engine, MSSQL included — client-side is the ONLY
    // read-only enforcement for MSSQL (no server-side mode exists), same
    // posture `is_read_statement`/the runner choke point already document.
    let Some((read_only, _engine)) = conn_meta else { return EditableDecision::NotEditable };
    if read_only {
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

/// G10 T4: which staged-edit owner this dialog applies for — drives the
/// success-arm cleanup only; the confirm/dispatch/error mechanics below are
/// IDENTICAL for both (the whole point of the shared write path — §3-novela:
/// one confirm modal, one `run_write_transaction`, one shared read-only
/// guard, regardless of which caller staged the statements).
#[derive(Clone)]
enum ApplyTarget {
    SandboxTab {
        /// Which tab's grid to clear/re-preview on success — looked up by id
        /// (not a held `Entity<ResultGrid>`) so a tab closed while the write
        /// is in flight (not reachable today, since the dialog's overlay
        /// occludes the tab strip, but checked defensively anyway) is simply
        /// not found rather than updating a dangling reference.
        tab_id: u64,
        /// `ResultGrid::preview_identity()`'s shape, captured at dialog-open
        /// time — lets a successful Apply re-run the SAME preview (brief:
        /// "re-run the preview, existing pipeline") without re-reading grid
        /// state after `clear_edits` has already run.
        preview_identity: (Option<String>, String),
    },
    Admin {
        panel: Entity<admin_panel::AdminPanel>,
    },
    /// A `DROP`/`TRUNCATE` staged from the sidebar's context menu. Nothing
    /// to re-run afterwards — the object is gone, so success just refreshes
    /// the schema so the tree stops showing it.
    Tree,
}

/// G5 Task 4 (G10 T4: generalized for the admin Apply flow too): state for
/// the Apply confirmation dialog — created by `on_open_apply_dialog` (the
/// apply bar's "Aplikovat") or `open_admin_apply_dialog` (the admin panel's
/// own "Aplikovat"). `statements`/`sql_text` are captured ONCE here rather
/// than recomputed live: the dialog must show (and, on confirm, execute) the
/// EXACT SQL the user reviewed, even if the underlying staged edits somehow
/// changed in the gap before "Potvrdit a spustit" (not currently possible —
/// the dialog's own `.occlude()` blocks every click that could restage
/// anything — but capturing once is also just the simpler, more
/// obviously-correct shape).
struct ApplyDialogState {
    target: ApplyTarget,
    /// G10 T3/T4: `admin_sql::WriteStatement` — `display_sql` is the ONLY
    /// string this struct's `sql_text` (and hence the dialog/history) is
    /// ever built from; `exec_sql` is used exactly once, by
    /// `run_write_transaction` on confirm, and is never read anywhere in
    /// this file (CURATION item 3's redaction discipline, structural here).
    statements: Vec<admin_sql::WriteStatement>,
    /// `statements`' `display_sql`s joined by newline (brief contract #3) —
    /// shown in the dialog AND recorded as the eventual history entry's
    /// `sql`. '***'-redacted wherever a statement carries a password.
    sql_text: String,
    /// G10 T6 (databases sub-view): a red warning line above the buttons —
    /// the CASCADE-drop confirmation. `None` everywhere else.
    warning: Option<String>,
    /// G5 Task 4 review fix (BLOCKER 1): the identity (`ResultTab::
    /// conn_identity` for a sandbox tab, `AdminPanel::conn_identity()` for
    /// admin) at dialog-open time — `on_confirm_apply` re-checks this
    /// against `AppView::current_conn_identity()` before dispatching
    /// (belt-and-braces alongside the opener's own check and the apply
    /// bar's disabled button: the dialog's `.occlude()` should make a
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
    /// would land there instead of being inert. `open_admin_apply_dialog`
    /// (a subscribe callback with no `Window` access — see its own doc
    /// comment) still constructs one but doesn't call `window.focus` itself,
    /// same posture `on_monitor_view_event`'s `KillRequested` already sets.
    focus_handle: gpui::FocusHandle,
}

// -----------------------------------------------------------------
// Workspace T8 — the script editor binding's pure decisions (Part S §5).
// Free functions so the dirty rule, the caption and the `.sql` rule are
// unit-pinned without a GPUI window, the `decide_retrigger_action` /
// `context_switch_refusal` precedent.
// -----------------------------------------------------------------

/// Part S §5: dirty = the editor text differs from what was last read from
/// (or written to) disk. Exact compare, bounded by the 1 MiB open cap;
/// `str`'s `!=` already short-circuits on length. Whitespace and line
/// endings COUNT — the file is the truth and „ •" must not lie about it.
fn script_text_is_dirty(editor: &str, saved: &str) -> bool {
    editor != saved
}

/// The binding's display path: relative to the CURRENT scripts root when
/// it lives under it, otherwise the bare file name. The binding itself
/// holds an ABSOLUTE path (resolved rejected alternative: storing a rel
/// breaks the moment the root changes) — this is only the label.
fn script_caption_rel(path: &Path, root: Option<&Path>) -> String {
    // T9 review MINOR-1: the SAME fold as every other binding comparison.
    // A byte-exact `strip_prefix` here silently degraded to the bare file
    // name whenever the configured root's casing differed from disk — the
    // reason that whole bug class had no visible tell.
    if let Some(root) = root {
        if path_starts_with_ci(path, root) {
            return path
                .components()
                .skip(root.components().count())
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/");
        }
    }
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// The caption strip's label — the EXACT tab-title dirty convention (`" •"`,
/// see `tabs::collapse_title`'s callers).
fn script_caption(rel: &str, dirty: bool) -> String {
    if dirty {
        format!("Skript: {rel} •")
    } else {
        format!("Skript: {rel}")
    }
}

/// Part S §5.4 / fact 0.6: `.sql` is enforced client-side because the
/// pinned GPUI rev's `prompt_for_new_path` has no extension filter.
fn with_sql_extension(path: &Path) -> PathBuf {
    // T9 review NIT-3: THE extension rail, not a fourth spelling of it.
    if crate::scripts::is_sql_path(path) {
        path.to_path_buf()
    } else {
        let mut s = path.as_os_str().to_owned();
        s.push(".sql");
        PathBuf::from(s)
    }
}

/// Did the editor's binding move to a DIFFERENT target? This is what
/// `AppView::script_binding_generation` counts, and therefore what every
/// async open/save continuation asks before it touches the binding.
///
/// Refreshing `saved_text` for the SAME path (a successful save) is
/// deliberately not a change: the user did not move on, so a save that
/// lands afterwards must still be allowed to update the binding. Only a
/// different file — or unbinding, or binding where nothing was bound — is
/// „the user moved on".
fn script_binding_target_changed(old: Option<&Path>, new: Option<&Path>) -> bool {
    match (old, new) {
        // T9 review MINOR-1: folded, so a re-save of the SAME file reached
        // through a differently-cased root is not mistaken for the user
        // moving on (which would strand a phantom „ •" over a saved file).
        (Some(a), Some(b)) => !same_path_ci(a, b),
        (None, None) => false,
        _ => true,
    }
}

/// Case-INSENSITIVE, Unicode-aware path fold — the SAME rule
/// `dbc_state::fsutil` applies to names ([`dbc_state::fsutil::fold_name`],
/// never `eq_ignore_ascii_case`), lifted to whole paths and applied per
/// component so a separator can never fold into a name.
///
/// T10 carry-forward 6: this used to spell the fold itself, as
/// `to_lowercase`, while claiming in this very comment to be applying
/// `fsutil`'s rule — which is `to_uppercase`. Two folds coexisted in one
/// crate for one job, and the one here was the WRONG one: `to_lowercase`
/// implements Unicode's final-sigma context rule, so `…/ΟΔΟΣ.sql` and
/// `…/οδοσ.sql` fold APART although NTFS resolves them to a single file —
/// i.e. deleting the file would leave the binding standing, which is
/// precisely the failure the paragraph below says this function prevents.
/// It now calls the shared rail instead of re-spelling it.
///
/// T9 review MINOR-1. The binding's path has TWO producers that disagree
/// on casing: the tree resolves against `effective_scripts_root()`, which
/// is the CONFIGURED string and is never canonicalized, while save-as
/// receives the OS dialog's true on-disk casing (GPUI pushes a
/// `root.canonicalize()` through `SetFolder`). Byte-exact comparison then
/// makes a root configured `D:\ws\Scripts` over an on-disk `D:\ws\scripts`
/// answer FALSE for the very file being deleted: the binding survives, the
/// caption goes on naming a file that is gone, and the next Ctrl+S
/// silently recreates it — exactly what `finish_script_delete` says the
/// rule prevents. NTFS is case-insensitive across all of Unicode, which is
/// why the fold has to be too (`scripts::conflicting_name`'s rationale,
/// verbatim — this was the one place in the phase that had reverted to
/// `Path::eq`).
fn path_fold(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| dbc_state::fsutil::fold_name(&c.as_os_str().to_string_lossy()))
        .collect()
}

/// Are these the same path on a case-insensitive volume? (See `path_fold`.)
fn same_path_ci(a: &Path, b: &Path) -> bool {
    path_fold(a) == path_fold(b)
}

/// Is `a` at or below `b`? The `Path::starts_with` component semantics,
/// with `path_fold`'s casing rule.
fn path_starts_with_ci(a: &Path, b: &Path) -> bool {
    let (fa, fb) = (path_fold(a), path_fold(b));
    fa.len() >= fb.len() && fa[..fb.len()] == fb[..]
}

/// Part S §4: is the editor's binding touched by a mutation of `target`?
///
/// Exact hit, OR — when `target` is a FOLDER — anywhere beneath it. The
/// second arm is the one the plan text did not have and the Task 9 brief
/// demanded: `rename_entry` renames folders too, so „only an exact match
/// counts" would leave the binding pointing at a path that no longer
/// exists the moment a parent folder is renamed. `is_dir` gates it
/// because a FILE whose path happens to be a prefix of the binding's is
/// not an ancestor of it.
fn script_binding_affected(binding: &Path, target: &Path, is_dir: bool) -> bool {
    if same_path_ci(binding, target) {
        return true;
    }
    is_dir && path_starts_with_ci(binding, target)
}

/// „Does the editor's binding point AT this tree entry (or inside it)?" —
/// as a FREE fn over the three pieces of state it depends on, so the
/// question can be asked at any instant without an `AppView` and, above
/// all, so it can never be answered by a bool captured earlier.
///
/// FINAL-REVIEW MAJOR-1. `confirm_script_delete` used to compute
/// `was_bound` BEFORE dispatching the background delete and hand it to
/// `finish_script_delete`, which applied it blind. That is the phase's own
/// banned shape — a check performed before an `await` is a statement about
/// the PAST — and it lost data in the resurrection direction:
///
/// 1. Editor UNBOUND. Double-click `trzby.sql` → `read_script` dispatched.
/// 2. Right-click → Smazat → confirm. `was_bound == false`. Delete
///    dispatched.
/// 3. The read lands FIRST. `script_open_abort_reason` passes all three
///    legs (root unchanged; generation unchanged, because an unbound
///    editor never called `set_script_binding` so nothing bumped it;
///    buffer untouched) → `bind_script` binds the doomed `trzby.sql`.
/// 4. The delete lands. `was_bound == false`, so the binding is NOT
///    cleared: the caption still names a file that no longer exists.
/// 5. Ctrl+S — `script_save_allowed` passes, the modal is long closed —
///    and the irreversibly deleted file is silently back on disk.
///
/// The symmetric direction is milder but equally wrong: bound to `a.sql`,
/// an in-flight open of `b.sql` lands during a confirmed delete of
/// `a.sql`, `was_bound == true`, and the NEW `b.sql` binding is dropped so
/// the next Ctrl+S silently becomes a save-as.
///
/// Both are the same asymmetry T9 review MINOR-2 established between
/// `set_script_name_error` (synchronous, sound) and `land_script_name_error`
/// (post-await, must re-verify), and the fix is the same: the landing
/// re-ASKS. `retarget_binding_after_rename` — the delete's sibling — was
/// already written this way, which is why rename never had the bug.
fn binding_targets_entry(
    binding: Option<&Path>,
    root: Option<&Path>,
    rel: &str,
    is_dir: bool,
) -> bool {
    let (Some(binding), Some(root)) = (binding, root) else { return false };
    crate::scripts::resolve_entry_rel(root, rel)
        .is_ok_and(|p| script_binding_affected(binding, &p, is_dir))
}

/// Where the binding must MOVE to when `old` is renamed to `new`, or
/// `None` when the binding is not affected at all. For a folder rename the
/// binding's suffix below `old` is rebased onto `new`, so „rename the
/// folder containing the open script" keeps the caption honest instead of
/// silently stranding it on a dead path.
fn script_binding_retarget(
    binding: &Path,
    old: &Path,
    new: &Path,
    is_dir: bool,
) -> Option<PathBuf> {
    if !script_binding_affected(binding, old, is_dir) {
        return None;
    }
    // By component COUNT, not `strip_prefix`: the prefix may differ from
    // `old` in casing (that is the whole point of `path_fold`), and the
    // suffix must keep the casing it actually has on disk.
    let mut out = new.to_path_buf();
    for c in binding.components().skip(old.components().count()) {
        out.push(c.as_os_str());
    }
    Some(out)
}

/// T9 review MAJOR-1: the refusal when Ctrl+S arrives while a dialog owns
/// the screen. Like [`SCRIPT_SAVE_IN_FLIGHT`] it is deliberately NOT an
/// „error:" — nothing failed, and the way out is one Esc away.
pub(crate) const SCRIPT_SAVE_BLOCKED: &str = "nejprve zavřete otevřený dialog";

/// FINAL-REVIEW MAJOR-2 — the Ctrl+S guard, as a TYPE the compiler
/// enforces instead of a regex a reviewer can walk around.
///
/// `script_write_audit` pinned this rule textually and the reviewer beat
/// it with the most ordinary alternative call syntax in Rust: the needle
/// was `.save_script(` — with the leading dot, and its doc comment argued
/// at length for the dot — and UFCS puts a COLON there, so
/// `AppView::save_script(self, path, text, false, cx)` sailed past both
/// the audit and the zero-warning gate. That is the fifth time a text
/// audit in this phase has been defeated.
///
/// So the rule moved into the type system. [`SaveAllowed`] is a witness
/// with a PRIVATE field, and `save_script` demands one by value. Rust's
/// finest privacy granularity is the module, and `main.rs` is the crate
/// ROOT — a private field declared there would be visible crate-wide and
/// prove nothing — so the witness and its only mint live in this small
/// child module. A parent cannot see a child's private items, which makes
/// `SaveAllowed(..)` unspellable everywhere in `dbc-ui` except the lines
/// below. There is no receiver to rebind, no path spelling to vary and no
/// macro to hide it in.
///
/// **RE-VERIFY FAIL-2 — the first version was a VALUE, and this comment
/// claimed more than the type delivered.** It said a witness „cannot be
/// re-used after an await… stashing the first one across the picker would
/// not compile", resting that on `!Copy + !Clone`. Those forbid a second
/// USE of one value; they do not forbid a MOVE. The re-verifier gave
/// `save_script_as` the witness as a parameter, captured it in the
/// `async move` block, dropped the re-mint — clean build, 961 green — and
/// so restored T9 re-verify FAIL-1 whole: Ctrl+S → picker opens → user
/// deletes `trzby.sql` and confirms → picker completes naming
/// `trzby.sql` → the irreversibly deleted file is silently back.
///
/// The permission is therefore no longer a value handed out at all.
/// [`save_guard::with_save_permission`] is a SCOPE: the witness carries a
/// generative brand (`SaveAllowed<'brand>` over an INVARIANT
/// `PhantomData`), the closure is `for<'brand> FnOnce(..) -> R`, and `R`
/// is one type chosen before `'brand` exists — so the witness cannot be
/// returned from the scope, cannot be stored in anything that outlives
/// it, and above all cannot be captured by `cx.spawn`, whose future must
/// be `'static`. The closure is synchronous, so there is no await inside
/// the scope to hold it across either.
///
/// Stated precisely, because over-claiming is exactly what let the last
/// round through: this makes the permission unable to LEAVE the
/// synchronous scope in which the predicate was checked. That is the
/// property „a check before an await is a statement about the past"
/// actually needs.
///
/// The text audits are KEPT and were widened (belt and braces), but they
/// are not what holds this invariant up.
/// RE-VERIFY: the SECOND compiler-enforced rail, and the one the previous
/// round declined on the wrong grounds.
///
/// `SqlInput::replace_buffer` is the only mutating text API on the editor,
/// and Part S §5.5 says exactly one guard stands in front of it. That was
/// held up by `editor_clobber_audit` alone — a source-text audit — and the
/// last two rounds walked past it three separate ways: an aliased
/// fn-pointer, a module directory the walk pruned by prefix, and an
/// out-of-tree `#[path]` module. Every one of those clobbered a bound
/// script's unsaved changes with no undo.
///
/// The previous round declined a witness here, citing the Task 8 note on
/// `editor_clobber_audit`: a real rail would mean moving `AppView.sql`
/// behind a private accessor, splitting `impl AppView` across files and
/// dragging the autocomplete plumbing with it. That is a fair objection to
/// the shape Task 8 proposed and IRRELEVANT to this one. A scope needs no
/// accessor and no module move: the editor entity stays exactly where it
/// is, and what changes is that `replace_buffer` will not compile without
/// a permit only this module can mint.
///
/// The precondition is real, which is what separates this from the three
/// witnesses still declined (see the as-built note): the editor holds
/// nothing unsaved, OR the user has just answered „Zahodit" for the very
/// action being performed. `editor_load_guarded` already computes exactly
/// that; this module simply refuses to let anyone else decide it.
mod editor_guard {
    use crate::AppView;
    use gpui::Context;
    use std::marker::PhantomData;

    /// Permission to destroy the editor's buffer, valid only inside the
    /// [`with_editor_replaceable`] scope that produced it.
    ///
    /// Same generative invariant brand as `save_guard::SaveAllowed`, for
    /// the same reason and with the same guarantee: it cannot be returned,
    /// stored, or captured by a `'static` future, so a permission checked
    /// before an await cannot be spent after one. (Re-verify FAIL-2 is why
    /// that is spelled `fn(&'brand ()) -> &'brand ()` and not `&'brand ()`.)
    #[must_use = "this permit IS the permission to destroy unsaved editor text"]
    pub(crate) struct BufferReplace<'brand>(PhantomData<fn(&'brand ()) -> &'brand ()>);

    /// THE mint. `None` — the closure never runs — means the editor holds
    /// unsaved changes nobody has agreed to lose.
    ///
    /// Two ways to be allowed, and they are the two `editor_load_guarded`
    /// already distinguishes:
    ///
    /// * **Nothing is at stake.** `script_is_dirty` is a live read of the
    ///   buffer against `saved_text`, so this cannot be stale.
    /// * **The user said „Zahodit".** That is a fact about the past which
    ///   no later read can recover, so `on_discard_confirm_yes` records it
    ///   — STAMPED WITH THE GENERATION it was granted at, and consumed
    ///   once. Every path that moves the binding bumps that generation
    ///   (`set_script_binding`, `supersede_script_continuations`), so a
    ///   grant cannot be spent on a different editor state than the one
    ///   the user was asked about. That is the same reasoning
    ///   `script_open_abort_reason` applies to the read it guards.
    pub(crate) fn with_editor_replaceable<R>(
        view: &mut AppView,
        cx: &mut Context<AppView>,
        f: impl for<'brand> FnOnce(&mut AppView, &mut Context<AppView>, BufferReplace<'brand>) -> R,
    ) -> Option<R> {
        if !view.script_is_dirty(cx) {
            return Some(f(view, cx, BufferReplace(PhantomData)));
        }
        if view.editor_discard_grant == Some(view.script_binding_generation) {
            // One shot. A second replacement needs a second answer.
            view.editor_discard_grant = None;
            return Some(f(view, cx, BufferReplace(PhantomData)));
        }
        None
    }

    /// The second, UNCONDITIONAL mint — for rewriting the buffer into a
    /// transformation of ITS OWN CURRENT TEXT.
    ///
    /// What [`with_editor_replaceable`] protects is „content from somewhere
    /// else must not silently replace unsaved work". A self-rewrite is
    /// categorically not that: nothing arrives from outside, so there is no
    /// other content for the user's work to be lost TO, and prompting
    /// „zahodit neuložené změny?" before formatting the very text they are
    /// editing would be nonsense.
    ///
    /// The API is what keeps that honest: the caller never supplies text, it
    /// supplies `rewrite: &str -> String` and this function feeds it the
    /// live buffer. There is no parameter through which foreign content
    /// could enter, so this cannot be quietly repurposed into a clobber —
    /// which is why it is safe for it to skip the dirty check that the other
    /// mint exists to enforce.
    ///
    /// It leaves the buffer DIRTY on purpose: a format is an edit like any
    /// other, so the caption keeps its „ •" and Ctrl+S still has something
    /// to do.
    pub(crate) fn rewrite_buffer_in_place(
        view: &mut AppView,
        cx: &mut Context<AppView>,
        rewrite: impl FnOnce(&str) -> String,
    ) -> bool {
        let before = view.sql.read(cx).text();
        let after = rewrite(&before);
        if after == before {
            return false;
        }
        view.sql.update(cx, |input, cx| {
            input.replace_buffer(&after, cx, BufferReplace(PhantomData));
        });
        true
    }
}
use editor_guard::{rewrite_buffer_in_place, with_editor_replaceable};

mod save_guard {
    use crate::AppView;
    use gpui::Context;
    use std::marker::PhantomData;

    /// Proof that a save was permitted, valid ONLY inside the
    /// [`with_save_permission`] scope that produced it.
    ///
    /// `'brand` is generative and INVARIANT — `fn(&'brand ()) -> &'brand ()`
    /// rather than `&'brand ()`, so subtyping can neither shorten nor
    /// lengthen it. It is introduced by the `for<'brand>` bound on the
    /// scope's closure, so no type nameable outside that closure can
    /// mention it: the witness cannot be returned, stored in a field, or
    /// captured by a `'static` future.
    ///
    /// Still `!Copy` and `!Clone`, but that is now a detail rather than
    /// the argument. Re-verify FAIL-2 showed those forbid a second USE and
    /// not a MOVE, and a move was all the escape needed.
    #[must_use = "this witness IS the permission — dropping it and saving anyway is \
                  the bug it exists to prevent"]
    pub(crate) struct SaveAllowed<'brand>(PhantomData<fn(&'brand ()) -> &'brand ()>);

    /// T9 review MAJOR-1: may a Ctrl+S dispatch right now? A pure
    /// predicate so the rule is unit-pinned — `on_save_script` takes a
    /// `Window` and so has no test harness, which is exactly how it went
    /// unguarded in the first place. `SaveScript` is bound with context
    /// `None`, i.e. it fires straight through an open modal's
    /// `.occlude()`, so this is the ONLY thing standing between a habitual
    /// Ctrl+S and a write that races the rename/delete the user is
    /// currently confirming.
    pub(crate) fn script_save_allowed(
        modal_open: bool,
        apply_open: bool,
        discard_open: bool,
    ) -> bool {
        !(modal_open || apply_open || discard_open)
    }

    /// THE only mint of [`SaveAllowed`] — a SCOPE, not a value.
    ///
    /// Reading `&AppView` rather than taking three booleans is the
    /// difference between a rail and a formality: a caller handed
    /// `script_save_allowed(false, false, false)` could mint over a screen
    /// full of dialogs without lying about anything the compiler can see.
    /// Here there is nothing to lie WITH — the three facts come straight
    /// off the view. (This module is a DESCENDANT of the crate root, where
    /// `AppView` lives, so it can read fields private there; the reverse —
    /// the crate root reaching into this module's private tuple field — is
    /// what Rust forbids, and that asymmetry is the whole mechanism.)
    ///
    /// ALIASING THIS BUYS NOTHING, and that is the point of a scope over a
    /// bare token: however it is spelled — `use … as go;`, a fn-pointer
    /// binding, a macro — calling it RUNS the predicate. Re-verify FAIL-1
    /// walked past four name-based audits with exactly that trick; the
    /// writers that sit behind a real precondition were untouched by it.
    ///
    /// `None` — the closure never ran — is the refusal; callers report
    /// [`SCRIPT_SAVE_BLOCKED`](crate::SCRIPT_SAVE_BLOCKED).
    pub(crate) fn with_save_permission<R>(
        view: &mut AppView,
        cx: &mut Context<AppView>,
        f: impl for<'brand> FnOnce(&mut AppView, &mut Context<AppView>, SaveAllowed<'brand>) -> R,
    ) -> Option<R> {
        if !script_save_allowed(
            view.modal.is_some(),
            view.apply_dialog.is_some(),
            view.discard_confirm.is_some(),
        ) {
            return None;
        }
        Some(f(view, cx, SaveAllowed(PhantomData)))
    }
}
use save_guard::{with_save_permission, SaveAllowed};

/// T8 review MAJOR-2: the refusal when a save of this editor is already in
/// flight. Not an „error:" — nothing failed; the user's keystroke simply
/// arrived while the previous write was still fsyncing, and the „ •" stays
/// up so they can see the buffer is not yet on disk.
pub(crate) const SCRIPT_SAVE_IN_FLIGHT: &str = "ukládání skriptu už probíhá";

/// RE-VERIFY: the refusal when something tries to replace the editor's
/// buffer while it holds unsaved changes nobody agreed to lose.
///
/// Unreachable through `editor_load_guarded`, which is the point — this is
/// what a NEW path that forgot the guard now hits instead of silently
/// destroying the user's text. Not an „error:": nothing failed.
pub(crate) const SCRIPT_LOAD_BLOCKED: &str =
    "editor má neuložené změny — nejprve je uložte nebo zahoďte";

/// May an in-flight `open_script` still replace the editor's buffer?
///
/// `editor_load_guarded` answers „is it safe" at DISPATCH; `read_script`
/// then yields the UI thread, so by the time the text arrives the answer
/// may have expired. THREE independent things must still hold, and each
/// was a real hole:
///
/// * **The buffer** must be byte-identical to what the guard looked at
///   (T8 review BLOCKER-1). Not redundant with the generation:
///   `set_script_binding` bumps only when the bound PATH changes, so
///   typing — the exact thing the guard protects — leaves it untouched,
///   and `SqlInput` has no undo, so a replacement over fresh keystrokes
///   destroys them for good.
/// * **The binding** must not have moved (`script_binding_generation`),
///   or the open would clobber whatever the user moved on to.
/// * **The scripts root** must still be the one the `rel` was resolved
///   against (T8 re-verify NEW MAJOR, generalised). `apply_context`'s
///   unconditional `supersede_script_continuations` covers the workspace
///   swap, but the root also changes in profile mode via
///   `start_scripts_dir_pick` / `clear_scripts_dir` — and there the
///   generation deliberately stays put, because an in-flight SAVE holds an
///   absolute path and is not invalidated by a root change. Comparing the
///   root directly is exact where a shared counter would be a blunt proxy:
///   it supersedes the open and nothing else, and it needs no future
///   root-changing site to remember to bump anything.
/// `None` means „land it". Otherwise the Czech status naming WHICH of the
/// three moved — the `context_switch_refusal` idiom, for the same reason:
/// one refusal that covers three different causes teaches the user
/// nothing, and „editor se mezitím změnil" would be a lie for a swapped
/// workspace nobody typed into.
fn script_open_abort_reason(
    root_now: Option<&Path>,
    root_dispatched: &Path,
    binding_now: u64,
    binding_dispatched: u64,
    text_now: &str,
    text_dispatched: &str,
) -> Option<&'static str> {
    // FINAL-REVIEW NIT-3: `same_path_ci`, not `!=`. This was the ONE path
    // comparison in the crate still done by exact bytes while every other
    // one goes through `path_fold` — the exact shape T10 carry-forward 6
    // existed to eliminate. Harmless in practice today (the root is copied
    // out of the same `effective_scripts_root` on both sides, so a casing
    // difference needs a re-pick that spells the same folder differently),
    // but „harmless today" is how the last fold divergence started.
    if !root_now.is_some_and(|r| same_path_ci(r, root_dispatched)) {
        return Some("otevření skriptu zrušeno — složka skriptů se mezitím změnila");
    }
    if binding_now != binding_dispatched {
        return Some("otevření skriptu zrušeno — editor se mezitím změnil");
    }
    if text_now != text_dispatched {
        return Some("otevření skriptu zrušeno — mezitím jste psali do editoru");
    }
    None
}

/// The discard prompt's question line. `script_rel` is `Some` only for a
/// `PendingDiscard::Script` (Part S §5.5's copy); every other action keeps
/// the pre-existing staged-rows wording byte for byte, because for those
/// the count IS the information and „(1)" would be a downgrade.
fn discard_confirm_question(script_rel: Option<&str>, change_count: usize) -> String {
    match script_rel {
        Some(rel) => format!("Neuložené změny skriptu {rel} budou zahozeny."),
        None => format!("Neuložené změny ({change_count}) — zahodit?"),
    }
}

/// Part S §1.3: the app has no editor TABS (fact 0.1) — opening a script
/// binds the ONE global editor to a file. `path` is ABSOLUTE so the binding
/// survives a scripts-root change; the caption re-relativizes for display.
/// `saved_text` is what is on disk as far as this session knows — the
/// dirty flag is `sql.text() != saved_text`.
pub(crate) struct ScriptBinding {
    pub path: PathBuf,
    pub saved_text: String,
}

/// Part S §5.5: what a dirty binding is parked on. `LoadText` covers the
/// two pre-existing "load SQL into the editor" sites (the history panel row
/// and the palette's history item) — which today clobber the editor with NO
/// guard at all; this phase strictly improves that for BOUND scripts and
/// leaves unbound ad-hoc text exactly as (un)guarded as before.
#[derive(Clone)]
pub(crate) enum PendingScriptAction {
    /// Open the library-relative `rel` into the editor (never runs it).
    Open { rel: String },
    /// Drop the binding; the editor TEXT stays (§5.3).
    Unbind,
    /// Replace the editor text and drop the binding — the history sites.
    LoadText { sql: String },
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
    /// Sidebar rework (resolved deviation 11): the user confirmed dropping
    /// the staged admin edits — close the dirty admin tab, then perform
    /// the switch. Carries the dispatch's own `follow_up` (T5 review
    /// MAJOR 2): "Zrušit" drops this whole variant, follow-up included —
    /// a cancelled switch can never leave an armed action behind.
    SwitchDatabase { conn_id: String, db: Option<String>, follow_up: Option<PendingTreeAction> },
    /// Workspace T8 (Part S §5.5): the editor is bound to a script with
    /// unsaved changes and the user asked for something that would replace
    /// its text (or drop the binding). „Zahodit" performs the parked
    /// action via `perform_script_action`; „Zrušit" drops the whole
    /// variant, exactly like every other arm — nothing to undo, because
    /// nothing was applied before the prompt went up.
    Script(PendingScriptAction),
}

/// Sidebar rework (design §2.2): the one-shot action a cross-context
/// switch replays after success. T5 review MAJOR 2: NOT a shared AppView
/// field — each `switch_to_database` dispatch OWNS its action (parameter,
/// captured into the spawn closure), so a superseded dispatch's action
/// dies with it under the `switch_generation` guard, and the vault/confirm
/// detours carry it inside their pending payloads (dropped on cancel).
/// Single-variant by design (pinned by `switch_decision_tests`) — a second
/// queued kind needs its own design pass.
#[derive(Clone)]
pub(crate) enum PendingTreeAction {
    OpenPreview { schema: Option<String>, table: String },
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

/// G7 T7: the pending-fetch state `connections_ui::confirm_compare_dialog`
/// hands to `AppView::on_compare_schema_pair_ready` once `fetch_schema_pair`
/// resolves. The Compare tab (and its `CompareView` entity, in
/// `CompareLoadState::Loading`) is opened immediately at dispatch time —
/// `view` is that SAME entity, updated in place once the fetch resolves
/// (rather than a second entity being constructed here), so the tab
/// actually shows "Načítám schéma…" for the duration of the fetch instead
/// of only appearing once it's already done. `generation` mirrors
/// `sidebar_fetch_generation`'s guard shape — a newer dispatch's result
/// always wins over an older, still-in-flight one.
pub(crate) struct PendingCompare {
    pub view: Entity<compare::CompareView>,
    pub generation: u64,
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
    /// Design §W2: the ACTIVE workspace root, or `None` in profile mode.
    /// There is no third state — a broken pointer never reaches here (it
    /// is blocked at startup, §W4). Written ONLY by `apply_context`.
    workspace_root: Option<PathBuf>,
    /// T4 review MAJOR-1: bumped by EVERY context swap (`apply_context`),
    /// captured by each „Najít složku…" dispatch. A pick whose folder
    /// classification finished after the context already changed under it
    /// is inert — see `recovery_pick_may_commit`.
    workspace_pick_generation: u64,
    /// T5 review MINOR-2: the LAST folder-pick refusal, rendered inside the
    /// Settings „Pracovní prostor" block. `WORKSPACE_PICK_NONEMPTY` is a
    /// ~230-character explanation; the status bar is a single unwrapped
    /// flex row behind the modal backdrop, so its payload half (the
    /// interrupted-init hint this task exists to deliver) was physically
    /// unreadable there. The status bar keeps a SHORT sentinel
    /// (`WORKSPACE_PICK_FAILED_STATUS`); the prose lands here, in the panel
    /// the user is already looking at. Cleared when a new pick starts and
    /// when the modal closes (`close_modal`) — it belongs to one Settings
    /// session, not to the app.
    workspace_pick_error: Option<String>,
    /// T4 review NIT-11: tab stops for the `WorkspaceMissing` modal's three
    /// choices. A BLOCKING dialog whose only exit is a mouse click leaves a
    /// keyboard-only user with nothing but the window close button, so the
    /// buttons are real focus targets (`tab_index` requires a tracked
    /// handle in the pinned gpui). Enter/Space activate the FOCUSED button
    /// only; a bare Enter from the modal's own focus still reaches the
    /// `ModalConfirmKind::Ignore` policy, so §W4's "no default button"
    /// rule holds.
    ///
    /// T4 re-verify carry-forward: Enter only became true when the buttons
    /// gained the `WorkspaceChoice` key context and the `ActivateChoice`
    /// binding — a keymap binding is dispatched before any `on_key_down`
    /// listener, so the ancestor `ModalForm`'s `enter → ModalConfirm` was
    /// swallowing it. See `connections_ui::WORKSPACE_CHOICE_CONTEXT`.
    workspace_choice_focus: [FocusHandle; 3],
    /// T4 review NIT-11: the `WorkspaceMissing` panel's own focus handle,
    /// which makes the panel a gpui TAB GROUP. Focused when the modal
    /// opens (instead of the shared `modal_focus_handle`), so the first
    /// Tab descends into the three choices deterministically rather than
    /// into whatever else the window happens to expose — the pinned gpui
    /// documents exactly this "focus the container, then `focus_next`"
    /// contract on `InteractiveElement::tab_stop`. Focus lands on the
    /// CONTAINER, not on a button, so a bare Enter still reaches
    /// `ModalConfirmKind::Ignore`: §W4's "no default button" holds — the
    /// `WorkspaceChoice` key context that claims `enter` is only on the
    /// dispatch path once a choice has actually been tabbed to.
    workspace_panel_focus: FocusHandle,
    /// Unlocked vault, kept for the session once the user has entered the
    /// master password once (brief: prompt on first use, not at startup).
    vault: Option<Vault>,
    active_connection_id: Option<String>,
    /// Sidebar rework (design §2.2): the active database WITHIN
    /// `active_connection_id`. `None` = the saved config's `database` (the
    /// default). Always `None` when `active_connection_id` is `None` (the
    /// CLI path has no db switching) and always NORMALIZED — explicitly
    /// picking the default db stores `None`, so identity/store-key/label
    /// logic has a single canonical spelling (`switch_to_database` enforces
    /// this; until that lands in T5, nothing writes `Some` here).
    active_database: Option<String>,
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
    // --- G2 Task 6 / sidebar rework: multi-root sidebar panel ---
    /// Per-slot lazy-fetch state lives on the entity itself, driven by
    /// direct mutation from `start_db_list_fetch`/`start_schema_slot_fetch`
    /// (see schema_tree.rs's header comment).
    tree: Entity<SchemaTree>,
    /// Ctrl+B (`ToggleTree`, app action, binding context `None`). `false`
    /// means the panel isn't rendered at all (0 px), not just visually
    /// hidden.
    tree_visible: bool,
    /// Current sidebar width in logical px, seeded from
    /// `AppConfig::sidebar_width` at startup and clamped into
    /// `SIDEBAR_MIN_W..=SIDEBAR_MAX_W`.
    ///
    /// The clamp lives HERE and not in the setter, because the stored value
    /// is whatever the user dragged: a `config.toml` hand-edited to `5`, or
    /// written on a wide monitor and reopened on a narrow one, must not be
    /// able to leave the panel unreachably thin — there would be no handle
    /// left to grab to undo it.
    sidebar_width: f32,
    /// `(mouse x at mouse-down, sidebar width at mouse-down)` while a
    /// splitter drag is active — the same shape as `grid.rs`'s `resizing`,
    /// and for the same reason: the delta must be measured against where the
    /// drag STARTED, not against the previous move event, or rounding
    /// accumulates and the panel creeps away from the pointer.
    sidebar_resizing: Option<(f32, f32)>,
    /// A context-menu action that needs a `Window` (dialog focus), parked
    /// by the cx-only tree subscription and drained at the top of `render`.
    /// Same shape as the queued cross-context preview open.
    pending_menu_action: Option<TreeEvent>,
    /// Sidebar rework: bumped on every db-list/schema-slot fetch dispatch;
    /// a result only applies if the generation still matches
    /// (last-dispatched wins — the slot state machines in schema_tree.rs
    /// carry the captured generation and drop mismatches). Replaces the
    /// single-root `schema_fetch_generation`.
    sidebar_fetch_generation: u64,
    /// G7 T6: bumped on every `confirm_compare_dialog` dispatch; an
    /// `on_compare_schema_pair_ready` result only applies if the generation
    /// still matches — same last-dispatched-wins guard as
    /// `sidebar_fetch_generation`/`switch_generation`.
    compare_fetch_generation: u64,
    // --- G3 Task 3: history panel + query recording ---
    /// Opened at startup from the ACTIVE context's history path
    /// (`StartupContext::paths.history`, workspace T4) — which is
    /// `default_history_path()` in BOTH modes, because history is
    /// deliberately machine-local (§W5); `None` when the open
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
    /// Opened at startup from the ACTIVE context's views path
    /// (`StartupContext::paths.views`, workspace T4 — the profile path in
    /// profile mode, `<workspace>/views.toml` in workspace mode) and
    /// rebuilt by `apply_context` on every swap. `None` when the open
    /// failed (surfaced once in the startup status — see `main`) or when
    /// the start was BLOCKED (§W4 loads nothing — see `StartupLoads`), in
    /// which case the feature is simply off — no apply, no
    /// save — the rest of the app is fully functional either way, same
    /// "degrade gracefully" precedent as `history: Option<HistoryDb>`.
    view_prefs: Option<ViewPrefsStore>,
    // --- G6 Task 3: parametrized `:name` query values ---
    /// Opened at startup from the ACTIVE context's params path
    /// (`StartupContext::paths.params`, workspace T4) and rebuilt by
    /// `apply_context` on every swap; `None` on a load failure or a
    /// BLOCKED start (§W4 — see `StartupLoads`), same "degrade
    /// gracefully" posture as
    /// `view_prefs` — the values dialog still opens and runs queries, it
    /// just won't prefill/remember values across runs). Keyed by
    /// `(connection_id, param name)` — see `open_query_params_dialog`/
    /// `confirm_query_params`.
    param_values: Option<ParamValuesStore>,
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
    // --- Workspace T8: the script editor binding (Part S §5) ---
    /// The `.sql` file the ONE global editor is currently bound to, or
    /// `None` for ad-hoc text. Every mutation goes through
    /// `set_script_binding` so `script_binding_generation` cannot drift.
    script_binding: Option<ScriptBinding>,
    /// `script_is_dirty`'s answer, recomputed ONCE per frame at the top of
    /// `AppView::render` — the same lazy-poll idiom `refresh_autocomplete`
    /// and `history_search`/`last_history_query` already use (see
    /// history_panel.rs's module doc comment).
    ///
    /// It exists because `context_switch_blocked` takes no `cx` and so
    /// cannot read the editor entity. That makes it at most ONE FRAME
    /// stale, which is safe in the only direction that matters: every
    /// path that changes the editor text calls `cx.notify()`, so a frame
    /// is always drawn between an edit and the next click — the flag can
    /// linger `true` after an async save lands (the gate then refuses a
    /// switch it could have allowed, the conservative side) but cannot
    /// report `false` for text the user has already typed.
    script_dirty_flag: bool,
    /// Bumped by `set_script_binding` on EVERY binding change. Async
    /// continuations (`open_script`, `save_script`, `save_script_as`)
    /// capture it at dispatch and refuse to touch the binding if it moved
    /// — the phase's four-MAJOR "a stale background continuation applied
    /// after the user had moved on" class (`start_script_pick`,
    /// `start_csv_import`, `pick_workspace_for_recovery`,
    /// `open_workspace_confirm` are the precedents).
    script_binding_generation: u64,
    /// „The user answered „Zahodit" for the action about to run" — the one
    /// fact `editor_guard::with_editor_replaceable` cannot re-derive from
    /// live state, stamped with the `script_binding_generation` it was
    /// granted at and consumed once.
    ///
    /// Written by exactly two functions and read by one; pinned by
    /// `the_discard_grant_is_written_only_where_the_user_answered`. Any
    /// third writer is a way to fake the user's answer, which is why the
    /// grant is a generation rather than a bool: every path that moves the
    /// binding bumps it, so a stale grant expires on its own instead of
    /// waiting to be spent.
    editor_discard_grant: Option<u64>,
    /// T8 review MAJOR-2: is a `save_script` write still in flight? The
    /// shared `fsutil::write_atomic` rail derives ONE tmp path per target,
    /// so two overlapping writes to the same file corrupt each other's tmp
    /// and can leave the caption reading clean over contents the disk does
    /// not hold. OS key auto-repeat on a held Ctrl+S is enough to trigger
    /// it. One editor means one flag is enough; a second dispatch is
    /// refused out loud (`SCRIPT_SAVE_IN_FLIGHT`), never queued silently.
    script_save_in_flight: bool,
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
    // --- G6 Task 7: schema autocomplete popup ---
    /// `None` when the popup is closed — see `AutocompleteState`'s doc
    /// comment for the lazy-diff recompute idiom.
    autocomplete: Option<AutocompleteState>,
    /// The SQL text `autocomplete` was last computed from — compared
    /// against `self.sql`'s live text/cursor each render
    /// (`refresh_autocomplete`) to decide whether a recompute is needed.
    last_ac_text: String,
    last_ac_cursor: usize,
}

/// G5 Task 4 review fix (BLOCKER 1): sentinel `ResultTab::conn_identity`/
/// `AppView::current_conn_identity` use for the CLI-arg back-compat path
/// (no saved `ConnectionConfig`, hence no stable id to use instead).
const CLI_CONN_IDENTITY: &str = "cli";

/// The sidebar's built-in width, used until the user drags the splitter.
/// Was the hard-coded `260.` in `render`.
pub(crate) const SIDEBAR_DEFAULT_W: f32 = 260.0;
/// Narrow enough to be useful, wide enough that the 5 px splitter is still
/// grabbable. A panel dragged to 0 could not be dragged back — Ctrl+B hides
/// the panel, and that is the control for „I want it gone".
pub(crate) const SIDEBAR_MIN_W: f32 = 140.0;
/// Keeps the editor usable on a laptop screen; a user who wants the tree
/// huge can still hide the history panel with Ctrl+H.
pub(crate) const SIDEBAR_MAX_W: f32 = 640.0;

/// The ONE clamp. Applied on load, on every drag move, and before saving, so
/// no path can put a width outside the range into `self` or `config.toml`.
pub(crate) fn clamp_sidebar_width(w: f32) -> f32 {
    // `f32::clamp` PANICS on NaN. A NaN can only come from a corrupted drag
    // start, but a resize must not be able to take the process down.
    if w.is_nan() {
        return SIDEBAR_DEFAULT_W;
    }
    w.clamp(SIDEBAR_MIN_W, SIDEBAR_MAX_W)
}

/// `AppConfig::sidebar_width` (whole px, `None` = never resized) → the
/// working `f32`. Clamped here too, because the stored value may have been
/// hand-edited or written on a much wider monitor.
pub(crate) fn sidebar_width_from(stored: Option<u16>) -> f32 {
    match stored {
        Some(w) => clamp_sidebar_width(f32::from(w)),
        None => SIDEBAR_DEFAULT_W,
    }
}

/// Design §2.3: the widened connection identity. `\u{1F}` (unit separator)
/// joins id and database — the same convention dbc-state's
/// view_prefs/params `encode_key` already uses. Ids are app-generated
/// `conn-{hex}` and can never contain the separator; database names CAN
/// (design §7 CORRECTION: Postgres identifiers allow any character except
/// NUL), which is safe HERE because identities are compared atomically by
/// `conn_identity_matches` — never split for authorization — and never
/// rendered raw (`conn_name_for_identity` translates, display-only). The
/// store-bucket keys, which ARE compositional, go through dbc-state's
/// escaping `connection_scope_key` instead — see `store_scope_key`. The
/// CLI path keeps the plain `"cli"` sentinel (its URL bakes its own
/// database).
///
/// # T7 AUDIT RECORD (design §7, verified 2026-08-25 on the final code —
/// grep census recorded verbatim in the T7 commit message)
///
/// **29 `current_conn_identity()` sites** (main.rs + connections_ui.rs,
/// non-test), all funneling through this composition:
/// - 17 stamp sites: `run_query_with`, `run_many`, `start_script_pick`
///   (ScriptRun modal), `confirm_script_run` (progress tab),
///   `start_csv_import` (CsvImport modal), `confirm_csv_import` (progress
///   tab), `dispatch_plan_query`, `dispatch_mssql_plan`,
///   `on_confirm_analyze_write` (Plan tabs), `confirm_chart_picker`,
///   `open_admin_tab` (panel + tab), `open_monitor_tab`
///   (`"monitor:{identity}"` key + tab), `open_er_diagram`,
///   `on_er_diagram_event` (DDL child), `on_tree_event` (tree-DDL text
///   tab, inert), `confirm_compare_dialog` (compare tab, inert).
/// - 12 guard-"current" sides: the script pair (`start_script_pick`
///   continuation + `confirm_script_run`'s `script_run_dispatch_allowed`),
///   the CSV pair, `fetch_admin_catalog_into` (M2),
///   `on_open_apply_dialog`, `on_confirm_apply`,
///   `open_admin_apply_dialog`, `render_apply_bar` dim-out, PLUS the
///   three T5-added consumers: `push_admin_schemas_if_matching` (the
///   relocated schema-push M2 guard), `dirty_admin_change_count` (the
///   switch's dirty-admin confirm gate), and `switch_to_database`'s
///   no-op-switch equality (a census finding beyond the design table —
///   full-identity compare, so a same-connection different-db switch is
///   correctly NOT a no-op).
/// - `store_scope_key` deliberately does NOT appear (legacy bucket key,
///   T3).
///
/// **9 guard families, zero weakened** (11 physical
/// `conn_identity_matches` sites + `admin_open_decision`'s full-identity
/// `==`) — pinned by `identity_audit_tests`. **`ConnectSpec::Config`
/// constructors**: only `spec_for_database`, `ActiveConn::into_spec`
/// (the `resolve_active` projection), and the by-design explicit-config
/// paths (`run_backup_now`/`run_restore_now`, compare's swapped configs +
/// `CompareView`'s data-diff leg, the dialog's `test_connect_spec`) —
/// zero direct `active_connection_id`-based builds (T3's invariant
/// holds).
///
/// **Intentionally guard-free surfaces re-affirmed**: monitor (identity
/// is only a tab key; per-operation runner), backup/restore (explicit id
/// + existence check by design), compare (self-contained swapped
/// configs), read-only artifact tabs (stamped, never checked). The
/// sidebar's cross-context SNAPSHOT non-leak is pre-pinned by the T5 fix
/// round's fallback key-gate tests
/// (`fallback_slot_is_key_gated_no_cross_context_leak` and siblings,
/// schema_tree.rs) — referenced here, not duplicated.
pub(crate) fn conn_identity_for(conn_id: &str, database: &str) -> String {
    format!("{conn_id}\u{1F}{database}")
}

/// SECURITY (design §3.1): the derived spec inherits EVERYTHING from the
/// saved config except `database` — same id (⇒ same vault secret, same
/// favourites/prefs bucket root), same read_only (⇒ `open_config` still
/// applies default_transaction_read_only / file-engine read-only modes),
/// same ssh/timeout/auto_limit. No new secret storage, no new config
/// entry; this function moves a secret field it never reads.
pub(crate) fn spec_for_database(
    cfg: &ConnectionConfig,
    db: &str,
    secret: Option<String>,
) -> ConnectSpec {
    let mut cfg = cfg.clone();
    cfg.database = db.to_string();
    ConnectSpec::Config { cfg: Box::new(cfg), secret }
}

/// Design §2.4: the ONE resolved snapshot of the active `(connection,
/// database)` context. INVARIANT (design §2.4, doc'd here as the single
/// change point): no other code path may build a `ConnectSpec::Config`
/// from `active_connection_id` directly — `run_query_with`,
/// `resolve_spec_for_explain`, `apply_conn_spec` and `active_conn_spec`
/// are all thin projections of this. Compare and backup build specs from
/// EXPLICIT configs by design and are exempt (design §5 rows 6–7).
struct ActiveConn {
    /// `database` ALREADY swapped to the effective one.
    cfg: ConnectionConfig,
    secret: Option<String>,
    read_only: bool,
    engine: dbc_state::Engine,
    timeout_secs: Option<u64>,
    auto_limit: Option<u64>,
}

// T7 AUDIT VERDICT (the field `identity: String` this struct carried
// through T3–T6 is RETIRED here, per its allow's named-owner note): the
// census found ZERO stamp sites that want a snapshot-coupled identity —
// every stamp/guard site deliberately evaluates `current_conn_identity()`
// FRESH at its own stamp/recheck moment (that capture-then-recheck
// discipline is exactly what the guard families test), and
// `switch_to_database` computes its TARGET identity from explicit args
// before any state changes. An unread snapshot identity would only invite
// divergence from the freshly-read one; the coherence it pinned
// (`cfg.database` already swapped ⇒ the identity composes from the same
// snapshot) is asserted directly in `identity_widening_tests` via
// `conn_identity_for(&a.cfg.id, &a.cfg.database)`.

impl ActiveConn {
    fn into_spec(self) -> ConnectSpec {
        ConnectSpec::Config { cfg: Box::new(self.cfg), secret: self.secret }
    }
}

/// Pure core of `AppView::conn_name_for_identity` — free function so it is
/// testable without a GPUI context. NEVER renders the raw `\u{1F}` control
/// character (batch B review NIT 1): a hostile db name can itself contain
/// the separator (design §7 CORRECTION), so BOTH branches — deleted
/// connection AND found-connection non-default db — replace it with a
/// visible " / " before display.
fn conn_name_for_identity_from(connections: &[ConnectionConfig], identity: &str) -> String {
    if identity == CLI_CONN_IDENTITY {
        return "cli".to_string();
    }
    let (id, db) = match identity.split_once('\u{1F}') {
        Some((id, db)) => (id, Some(db)),
        None => (identity, None), // defensive: nothing stamps the bare shape any more
    };
    match connections.iter().find(|c| c.id == id) {
        // Deleted connection: never render the raw control character.
        None => identity.replace('\u{1F}', " / "),
        Some(c) => match db {
            Some(db) if db != c.database => {
                format!("{} / {}", c.name, db.replace('\u{1F}', " / "))
            }
            _ => c.name.clone(),
        },
    }
}

/// Pure core of `AppView::resolve_active` — free function so it is
/// testable without a GPUI context (this crate has no GPUI test harness).
fn resolve_active_from(
    config: &AppConfig,
    vault: Option<&Vault>,
    active_id: &str,
    active_db: Option<&str>,
) -> Option<ActiveConn> {
    let saved = config.connections.iter().find(|c| c.id == active_id)?;
    let mut cfg = saved.clone();
    if let Some(db) = active_db {
        cfg.database = db.to_string();
    }
    let secret = connect::resolve_secret_for_connect(vault, &cfg);
    Some(ActiveConn {
        read_only: cfg.read_only,
        engine: cfg.engine,
        timeout_secs: cfg.timeout_secs,
        auto_limit: cfg.auto_limit,
        secret,
        cfg,
    })
}

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

/// T5 review MINOR 3: pure entry-guard decision for `switch_to_database` —
/// a switch attempted under ANY open overlay (modal, apply dialog,
/// discard-confirm) is refused outright. Trivial by design (same posture
/// as `conn_identity_matches`): named + testable rather than an inline
/// `||` chain, because the failure mode it prevents is subtle — an open
/// discard-confirm from ANOTHER flow would otherwise have skipped the
/// dirty-admin confirmation entirely.
fn switch_blocked_by_overlay(modal_open: bool, apply_dialog_open: bool, discard_confirm_open: bool) -> bool {
    modal_open || apply_dialog_open || discard_confirm_open
}

/// G10 T4: what `open_admin_tab` should do given the current tab set — pure
/// (over `&Tabs`, GPUI-free plain data) so the singleton-per-connection
/// dedup/replace decision is directly testable. Same connection → activate
/// (re-focus, staged edits preserved — design §2 "re-focuses the existing
/// tab"); different connection → replace (stale staged admin edits must
/// never survive a connection switch — the same rationale as G5's
/// BLOCKER-1 `conn_identity` guard, see `tabs.rs::ResultTab::conn_identity`'s
/// doc comment); no admin tab open at all → open fresh.
#[derive(Debug, PartialEq, Eq)]
enum AdminOpenDecision {
    Activate(u64),
    Replace(u64),
    OpenFresh,
}

fn admin_open_decision(tabs: &Tabs, current_identity: &str) -> AdminOpenDecision {
    match tabs
        .iter()
        .find(|t| t.preview_key.as_deref() == Some(admin_panel::ADMIN_PREVIEW_KEY))
        .map(|t| (t.id, t.conn_identity == current_identity))
    {
        Some((id, true)) => AdminOpenDecision::Activate(id),
        Some((id, false)) => AdminOpenDecision::Replace(id),
        None => AdminOpenDecision::OpenFresh,
    }
}

impl AppView {
    fn on_run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.run_query(false, window, cx);
    }

    /// `Ctrl+Shift+Enter`: bypasses ONLY the auto-limit guard. Read-only
    /// enforcement is not a "per-run convenience" the way auto-limit is —
    /// it stays enforced regardless of how the query was launched.
    fn on_run_query_unlimited(
        &mut self,
        _: &RunQueryUnlimited,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_query(true, window, cx);
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
    ///
    /// G6 Task 3: also the single interception point for parametrized
    /// `:name` queries — every editor-typed-query trigger (`on_run_query`,
    /// `on_run_query_unlimited`, and the command palette's
    /// `PaletteAction::RunQuery`) funnels through this one call before ever
    /// reaching `run_query_with`, so intercepting here covers all three
    /// with one change rather than duplicating the check at each call site.
    fn run_query(&mut self, bypass_auto_limit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql.read(cx).text();
        if sql.trim().is_empty() {
            return;
        }
        match find_params(&sql) {
            Some(names) if !names.is_empty() => {
                self.open_query_params_dialog(sql, names, bypass_auto_limit, window, cx);
            }
            // Some(empty) or None (fail-closed scan failure) — proceed
            // exactly as before G6 Task 3, no behavior change.
            _ => self.run_query_with(sql, None, bypass_auto_limit, cx),
        }
    }

    /// Opens the `QueryParams` modal for `sql`'s distinct `:name`s, one
    /// `TextField` + NULL flag per name, prefilled from `self.param_values`
    /// (keyed by `store_scope_key()` — the legacy-for-default store bucket
    /// rule, sidebar rework design §7 items 4–5, covering both a saved
    /// connection and the CLI-arg `"cli"` sentinel). Refuses
    /// to open a second modal on top of an existing one (same
    /// single-modal-at-a-time invariant `run_query_with` itself enforces
    /// via its own `self.modal.is_some()` guard). Focuses the first param's
    /// `TextField` in the same update that sets `self.modal` — the same
    /// convention every other modal-opener follows (`open_connection_dialog`,
    /// the master-password prompts) — T3 review round 1 (finding 1): this
    /// was missing, so the dialog opened with nothing focused.
    fn open_query_params_dialog(
        &mut self,
        sql: String,
        names: Vec<String>,
        bypass_auto_limit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            return;
        }
        let conn_id = self.store_scope_key();
        let mut inputs = Vec::with_capacity(names.len());
        let mut null_flags = Vec::with_capacity(names.len());
        for name in &names {
            let stored = self.param_values.as_ref().and_then(|s| s.get(&conn_id, name));
            let prefill = stored.filter(|v| !v.is_null).map(|v| v.text.clone()).unwrap_or_default();
            null_flags.push(stored.map(|v| v.is_null).unwrap_or(false));
            inputs.push(cx.new(|cx| {
                let mut f = connections_ui::TextField::form_field(cx, "", false);
                f.set_text(&prefill, cx);
                f
            }));
        }
        let first_focus = inputs.first().map(|f| f.focus_handle(cx));
        self.modal = Some(connections_ui::ModalState::QueryParams {
            names,
            inputs,
            null_flags,
            sql_template: sql,
            bypass_auto_limit,
            error: None,
        });
        if let Some(focus) = first_focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// "Spustit" click — reads every input's live text + its `null_flags`
    /// entry, substitutes via `build_param_sql` (which also runs the
    /// CURATION-mandated post-substitution rescan, design §5). On `Ok`:
    /// persists every value to `self.param_values`, surfacing any
    /// `store.set` error to `self.status` (same posture
    /// `save_view_prefs_for_grid` takes, main.rs — NOT silently swallowed;
    /// T3 review round 1 (finding 2) caught this doc comment's earlier
    /// "best-effort, degrades silently" claim as wrong), closes the modal,
    /// and runs the final SQL with the caller's original
    /// `bypass_auto_limit`. A save failure on one param doesn't stop the
    /// rest from being attempted — the loop keeps going, so the LAST error
    /// (if any) is what ends up in `self.status`. On `Err` from
    /// `build_param_sql`: sets the modal's `error` (shown in the dialog)
    /// and does NOT close the modal, run anything, or persist any value —
    /// persistence only ever happens on the `Ok` branch, i.e. only on an
    /// actual confirmed run.
    fn confirm_query_params(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::QueryParams {
            names,
            inputs,
            null_flags,
            sql_template,
            bypass_auto_limit,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        let values: Vec<(String, bool)> = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| {
                let text = input.read(cx).text();
                let is_null = null_flags.get(i).copied().unwrap_or(false);
                (text, is_null)
            })
            .collect();

        match build_param_sql(&sql_template, &names, &values) {
            Ok(final_sql) => {
                let conn_id = self.store_scope_key();
                if let Some(store) = &mut self.param_values {
                    for (name, (text, is_null)) in names.iter().zip(values.iter()) {
                        if let Err(e) = store.set(
                            &conn_id,
                            name,
                            ParamValue { text: text.clone(), is_null: *is_null },
                        ) {
                            // Keep saving the remaining params even after a
                            // failure — a save failure on one param name
                            // shouldn't stop the others from persisting.
                            // Last error wins in `self.status` (matches
                            // `save_view_prefs_for_grid`'s posture).
                            self.status = format!("error ukládání parametrů: {}", e.message);
                        }
                    }
                }
                self.modal = None;
                self.run_query_with(final_sql, None, bypass_auto_limit, cx);
            }
            Err(msg) => {
                if let Some(connections_ui::ModalState::QueryParams { error, .. }) = &mut self.modal {
                    *error = Some(msg);
                }
                cx.notify();
            }
        }
    }

    /// Esc / "Zrušit" — closes the dialog without running anything or
    /// persisting any value ("Esc cancels — no run, no persistence").
    fn cancel_query_params(&mut self, cx: &mut Context<Self>) {
        self.close_modal(cx);
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

        let spec = if self.active_connection_id.is_some() {
            // Sidebar rework: `resolve_active` is the single spec-resolution
            // site (see `ActiveConn`'s invariant) — it keeps G15 T8 HARD
            // GATE ITEM 2 (`connect::resolve_secret_for_connect`, not a raw
            // `vault.get_secret`) inside `resolve_active_from`.
            let Some(a) = self.resolve_active() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return;
            };
            // G5 Task 3: captured before the cfg moves into the spec —
            // `Started`'s `Editable` detection needs both facts (see
            // `detect_editable_pk`).
            let conn_meta = Some((a.read_only, a.engine));
            (a.read_only, a.auto_limit, a.timeout_secs, conn_meta, a.into_spec())
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

        // G12 T5: multi-statement unlock. Params were already substituted
        // upstream (`run_query`, G6) — CURATION-fixed order: params ->
        // split -> per-statement guards/auto-limit -> dispatch. A preview
        // run (`preview.is_some()`) never carries more than one statement
        // (`preview_sql`'s own output), so it always falls through
        // unchanged. When the split yields 0 or 1 statements, this also
        // falls through to the existing single-statement pipeline below
        // (Guard 1 read-only on the full text, Guard 2 auto-limit),
        // byte-for-byte unchanged.
        if preview.is_none() {
            if let Some(dialect) = conn_meta.map(|(_, e)| e).and_then(dialect_for_engine) {
                match dbc_core::split_sql(&sql, dialect) {
                    Err(e) => {
                        self.status =
                            format!("error: SQL nelze rozdělit na příkazy: {}", split_error_message(e));
                        cx.notify();
                        return;
                    }
                    Ok(stmts) if stmts.len() > 1 => {
                        let (stmts, limited) =
                            auto_limit_each(stmts, auto_limit, bypass_auto_limit, dialect);
                        self.run_many(spec, sql, stmts, limited, dialect, timeout_secs, cx);
                        return;
                    }
                    Ok(_) => {}
                }
            }
        }

        // Resolved once, above every guard that needs it (batch C review
        // BLOCKER 2): `conn_meta`'s engine -> `dbc_core::Dialect`, same
        // expression Guard 2 used to compute separately (now hoisted so
        // Guard 1 can use it too, and both guards agree on the SAME
        // resolution — no risk of the two guards seeing different dialects
        // for the same run).
        let dialect = conn_meta.map(|(_, e)| sql_dialect(e)).unwrap_or(dbc_core::Dialect::Postgres);

        // Guard 1: read-only — rejected client-side without connecting.
        // (Server-side enforcement lives in connect::open_config: Postgres
        // `default_transaction_read_only=on`, SQLite `SQLITE_OPEN_READ_ONLY`
        // — this check is the fast, no-connection-needed first line, not the
        // only line — EXCEPT MSSQL, which has no server-side read-only mode
        // (driver integration note 5): for MSSQL this client-side check IS
        // the only line, so it MUST be dialect-aware — a bracket-quoted
        // reserved word like `[Delete]` must not false-reject a genuine
        // read via `is_read_statement_d` (batch C review BLOCKER 2; was
        // the plain pg-only `is_read_statement` here, falsifying this exact
        // comment's own stated invariant). Decision extracted to
        // `read_only_guard_rejects` so it's directly unit-testable without a
        // GPUI `Context`.
        if read_only_guard_rejects(&sql, read_only, dialect) {
            let err = QueryError::msg("connection is read-only");
            self.status = format!("error: {err}");
            cx.notify();
            return;
        }

        // Guard 2: auto-limit (single-statement fallback — the
        // multi-statement branch above already applied `auto_limit_each`
        // and returned). G15 §2d: dialect-correct rewrite vocabulary — an
        // MSSQL connection must never reach the `LIMIT`-appending form on
        // any branch-intermediate state.
        let mut sql = sql;
        let mut limit_suffix = String::new();
        if !bypass_auto_limit {
            if let Some(n) = auto_limit {
                let (rewritten, changed) = apply_auto_limit_d(&sql, n, dialect);
                if changed {
                    sql = rewritten;
                    limit_suffix = match dialect {
                        dbc_core::Dialect::Mssql => format!(" · auto-TOP {n}"),
                        _ => format!(" · auto-LIMIT {n}"),
                    };
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
        // Phase-3 follow-up I2: threaded into the loop below so a spill
        // write (see `QueryEvent::Batch` handling) can hop onto the tokio
        // runtime's blocking pool via `push_async` instead of blocking this
        // GPUI foreground-executor task inline.
        let handle = self.runner.handle();
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
                // Phase-3 follow-up I2: a batch's spill write (if any) is
                // performed HERE, in the async loop body, before entering
                // `this.update`'s synchronous closure — that closure can't
                // itself `.await` a background write, and it runs on GPUI's
                // foreground executor (the UI thread), which is exactly the
                // thread a spill write must not block. `this.update` below
                // then only applies this already-known outcome; every other
                // event variant is still handled entirely inside it,
                // unchanged. (`ev` can't be matched twice — a `Batch`'s
                // `RecordBatch` payload is moved out here, so this arm
                // `continue`s rather than falling through to the second
                // match below, which stubs `Batch` as unreachable.)
                if let QueryEvent::Batch(b) = ev {
                    let push_result = if errored.is_some() {
                        None // already failed and cancelled — drop the batch
                    } else if let Some(buf) = buffer.as_ref() {
                        // td-security fix round, BLOCKER B1: was
                        // `buf.borrow_mut().push_async(b, &handle).await`,
                        // which holds the `RefMut` temporary across the
                        // `.await` (method-call desugaring keeps it alive to
                        // the end of the full expression) — a `BorrowMutError`
                        // panic waiting to happen the moment a spilling batch
                        // suspended here while `cx.notify()` scheduled a grid
                        // paint that reads the same `RefCell` on this thread.
                        // `push_async_shared` never holds a borrow across an
                        // await; see its doc comment (dbc-buffer/src/lib.rs).
                        Some(dbc_buffer::push_async_shared(buf, b, &handle).await)
                    } else {
                        None
                    };
                    let stop = this
                        .update(cx, |view, cx| {
                            let mut stop = false;
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
                            } else if let Some(Err(e)) = push_result {
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
                            cx.notify();
                            stop
                        })
                        .unwrap_or(false);
                    if stop {
                        break;
                    }
                    continue;
                }
                let stop = this
                    .update(cx, |view, cx| {
                        // I2: `Batch` — the only arm that used to set this
                        // to `true` (tab-closed-mid-stream) — is now handled
                        // above, before this closure; every remaining arm
                        // (`Started`/`Finished`/`Failed`) always continues
                        // the loop, relying on `rx.recv()` returning `None`
                        // once the driver's one terminal event closes the
                        // channel.
                        let stop = false;
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
                                    // G15 T8 whole-branch review M3 fix: set
                                    // once, right after creation — see
                                    // `ResultGrid::dialect`'s doc comment.
                                    g.set_dialect(
                                        conn_meta
                                            .map(|(_, e)| sql_dialect(e))
                                            .unwrap_or(dbc_core::Dialect::Postgres),
                                    );
                                    // G4 Task 4: a preview tab knows its
                                    // source table (used as the `INSERT
                                    // INTO` target for exports) — an
                                    // ad-hoc SQL-editor run doesn't, and
                                    // keeps `set_buffer`'s "export"
                                    // placeholder.
                                    if let Some(p) = &preview {
                                        g.set_table_name(p.table.clone());
                                        g.set_preview_context(p.schema.clone(), p.key.clone(), p.title.clone());
                                        // G12 T4: entry-gate half of
                                        // CURATION item 4(b) — the "Import
                                        // CSV" toolbar button only exists on
                                        // a preview tab whose connection is
                                        // NOT read-only (`conn_meta` is
                                        // `None` only when neither a saved
                                        // connection nor a CLI-arg URL
                                        // resolved, which never reaches
                                        // `Started` in practice — treated as
                                        // not-enabled defensively).
                                        g.csv_import_enabled =
                                            conn_meta.is_some_and(|(read_only, _)| !read_only);
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
                            // I2: `Batch` is handled above, before this
                            // `this.update` call, so its push can `.await` a
                            // background spill write — see the `if let
                            // QueryEvent::Batch(b) = ev { ... continue; }`
                            // block right before this loop iteration's
                            // `this.update`. Unreachable here.
                            QueryEvent::Batch(_) => unreachable!("Batch handled before this match"),
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
                            // G15 T8 whole-branch review B2 fix: the
                            // WRITE-dispatch sibling of `Finished` —
                            // `runner::stream_query`'s MSSQL-write branch
                            // sends this INSTEAD of `Started`/`Batch`*/
                            // `Finished` (no result set exists, so no
                            // buffer/tab ever opens for this run — `errored`
                            // can never be set here, since it's only latched
                            // by a `Batch` buffer-push failure and no
                            // `Batch` is ever sent on this path). Reports
                            // the driver's real affected-row count (not a
                            // buffer row_count read, since there is no
                            // buffer) and records history exactly like a
                            // successful read does.
                            QueryEvent::WriteFinished { affected, elapsed } => {
                                view.status = format!("{affected} rows affected in {elapsed:.2?}");
                                view.record_history(
                                    &sql_for_title,
                                    &history_conn_name,
                                    history_started_at,
                                    Some(elapsed.as_millis() as i64),
                                    Some(affected as i64),
                                    None,
                                    cx,
                                );
                                view.cancel = None;
                            }
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
                                            TabContent::Monitor { .. } => None,
                                            TabContent::Plan { .. } => None,
                                            TabContent::Diagram { .. } => None,
                                            TabContent::Compare { .. } => None,
                                            TabContent::Chart { .. } => None,
                                            TabContent::ScriptRun { .. } => None,
                                            TabContent::Admin { .. } => None,
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
                                                let dialect = conn_meta
                                                    .map(|(_, e)| sql_dialect(e))
                                                    .unwrap_or(dbc_core::Dialect::Postgres);
                                                let sql = fk_join::build_join_sql(
                                                    dialect,
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

    // -----------------------------------------------------------------
    // G12 T5: editor multi-statement unlock.
    // -----------------------------------------------------------------

    /// The AD-HOC subset of `run_query_with`'s own `QueryEvent::Started`
    /// arm (buffer, FK metadata for the ☰ menu, grid entity, subscription,
    /// tab open) — extracted for `run_many`'s per-row-producing-statement
    /// tabs ONLY. Deliberately NOT used to refactor the single-run
    /// `Started` arm above (leave working code untouched; the duplication
    /// is deliberate and documented, same precedent `history_panel::
    /// collapse_sql` sets against `tabs::collapse_title`). No preview
    /// context, no editability, no per-table view-prefs — a multi-statement
    /// run's tabs are always plain ad-hoc results, same as today's
    /// single-statement ad-hoc path.
    fn open_adhoc_result_tab(
        &mut self,
        columns: SchemaRef,
        title_sql: &str,
        conn_identity: &str,
        dialect: dbc_core::Dialect,
        cx: &mut Context<Self>,
    ) -> (u64, Rc<RefCell<ResultBuffer>>) {
        let buf = Rc::new(RefCell::new(ResultBuffer::new(columns)));
        let result_cols: Vec<String> =
            buf.borrow().schema().fields().iter().map(|f| f.name().to_string()).collect();
        let (fk_info, ref_cols) = self.fk_info_for_adhoc(&result_cols, cx);
        let grid = cx.new(ResultGrid::new);
        grid.update(cx, |g, cx| {
            g.set_buffer(buf.clone(), cx);
            g.set_fk_info(fk_info, ref_cols);
            // G15 T8 whole-branch review M3 fix: set once, right after
            // creation — see `ResultGrid::dialect`'s doc comment.
            g.set_dialect(dialect);
        });
        cx.subscribe(&grid, AppView::on_grid_event).detach();
        let id = self.tabs.open(ResultTab {
            id: 0,
            title: collapse_title(title_sql),
            pinned: false,
            preview_key: None,
            conn_identity: conn_identity.to_string(),
            content: TabContent::Grid { grid, buffer: buf.clone() },
        });
        (id, buf)
    }

    /// `run_query_with`'s multi-statement dispatch (>1 statement after
    /// `split_sql`) — single-flight guards already ran in the caller; sets
    /// `cancel`/`run_generation`/`started_at`/status the same way, then
    /// consumes `runner::connect_and_run_many`, opening one
    /// `open_adhoc_result_tab` per row-producing statement and recording
    /// ONE history entry for the whole run (`sql` = the original full
    /// post-params editor text, `row_count` = returned rows + affected sum
    /// — design silent on the combined metric, flagged as a judgment call).
    fn run_many(
        &mut self,
        spec: ConnectSpec,
        sql: String,
        statements: Vec<String>,
        limited: bool,
        dialect: dbc_core::Dialect,
        timeout_secs: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let limit_suffix = if limited { " · auto-LIMIT".to_string() } else { String::new() };
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.started_at = Some(std::time::Instant::now());
        self.run_generation += 1;
        let my_generation = self.run_generation;
        self.status = format!("connecting…{limit_suffix}");
        cx.notify();

        let history_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let history_conn_name = self.active_connection_name_for_history();
        let conn_identity = self.current_conn_identity();
        let total_statements = statements.len();
        let mut rx = self.runner.connect_and_run_many(spec, statements.clone(), cancel, timeout_secs);
        // Phase-3 follow-up I2: see `run_query_with`'s identical `handle`
        // capture — lets `MultiQueryEvent::Batch` push through `push_async`
        // below instead of blocking this foreground-executor task inline.
        let handle = self.runner.handle();
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<Rc<RefCell<ResultBuffer>>> = None;
            let mut tab_id: Option<u64> = None;
            let mut errored: Option<String> = None;
            let mut rows_returned: u64 = 0;
            let mut total_affected: u64 = 0;
            let mut with_rows: usize = 0;
            let mut writes: usize = 0;

            while let Some(ev) = rx.recv().await {
                // I2: same "push before `this.update`, `continue`" shape as
                // `run_query_with` — see that loop's comment for why `ev`
                // can't be matched a second time below.
                if let MultiQueryEvent::Batch(b) = ev {
                    let push_result = if errored.is_some() {
                        None // already failed — drop further batches
                    } else if let Some(buf) = buffer.as_ref() {
                        // td-security fix round, BLOCKER B1: see
                        // `run_query_with`'s identical call site above for
                        // why this must NOT be
                        // `buf.borrow_mut().push_async(...).await`.
                        Some(dbc_buffer::push_async_shared(buf, b, &handle).await)
                    } else {
                        None
                    };
                    let stop = this
                        .update(cx, |view, cx| {
                            let mut stop = false;
                            if errored.is_some() {
                                // Already failed — drop further batches.
                            } else if tab_id.is_some_and(|id| view.tabs.iter().all(|t| t.id != id)) {
                                stop = true;
                                if let Some(token) = view.cancel.take() {
                                    token.cancel();
                                }
                                view.status = "zrušeno (tab zavřen)".into();
                            } else if let Some(Err(e)) = push_result {
                                let err_text = e.to_string();
                                view.status = format!("error: {err_text}");
                                errored = Some(err_text);
                                if let Some(token) = view.cancel.take() {
                                    token.cancel();
                                }
                            } else if let Some(id) = tab_id {
                                if let Some(TabContent::Grid { grid, .. }) =
                                    view.tabs.iter().find(|t| t.id == id).map(|t| &t.content)
                                {
                                    grid.update(cx, |g, _| g.on_batch_grown());
                                }
                            }
                            cx.notify();
                            stop
                        })
                        .unwrap_or(false);
                    if stop {
                        break;
                    }
                    continue;
                }
                let stop = this
                    .update(cx, |view, cx| {
                        // I2: same reasoning as `run_query_with`'s identical
                        // comment — `Batch` (the only arm that used to set
                        // this) is handled above, before this closure.
                        let stop = false;
                        match ev {
                            MultiQueryEvent::StatementStarted { index, total, columns: Some(cols) } => {
                                let title_sql =
                                    statements.get(index).map(String::as_str).unwrap_or("");
                                let (id, buf) = view.open_adhoc_result_tab(
                                    cols,
                                    title_sql,
                                    &conn_identity,
                                    dialect,
                                    cx,
                                );
                                tab_id = Some(id);
                                buffer = Some(buf);
                                with_rows += 1;
                                view.status = format!("příkaz {}/{total}…{limit_suffix}", index + 1);
                            }
                            MultiQueryEvent::StatementStarted { index, total, columns: None } => {
                                view.status = format!("příkaz {}/{total}…{limit_suffix}", index + 1);
                            }
                            // I2: `Batch` handled above, before this
                            // `this.update` call — see the `if let
                            // MultiQueryEvent::Batch(b) = ev { ... continue;
                            // }` block earlier in this loop iteration.
                            MultiQueryEvent::Batch(_) => unreachable!("Batch handled before this match"),
                            MultiQueryEvent::StatementFinished { index, affected: Some(n), elapsed } => {
                                total_affected += n;
                                writes += 1;
                                view.status = format!(
                                    "příkaz {} dokončen ({n} řádků, {elapsed:.2?}){limit_suffix}",
                                    index + 1
                                );
                            }
                            MultiQueryEvent::StatementFinished { index, affected: None, elapsed } => {
                                // Accumulated, not overwritten — more than one
                                // read statement in the same run each gets its
                                // own fresh tab/buffer (`StatementStarted`
                                // above), so this fires once per read with
                                // THAT statement's own total, never double-
                                // counting the same buffer twice.
                                let rows = buffer.as_ref().map_or(0, |b| b.borrow().row_count());
                                rows_returned += rows as u64;
                                if let Some(id) = tab_id {
                                    if let Some(TabContent::Grid { grid, .. }) =
                                        view.tabs.iter().find(|t| t.id == id).map(|t| &t.content)
                                    {
                                        grid.update(cx, |g, _| {
                                            g.on_stream_finished();
                                        });
                                    }
                                }
                                view.status = format!(
                                    "příkaz {} dokončen ({rows} řádků, {elapsed:.2?}){limit_suffix}",
                                    index + 1
                                );
                            }
                            MultiQueryEvent::StatementFailed { index, error } => {
                                match &errored {
                                    None => {
                                        view.status = format!("selhalo na příkazu #{}: {error}", index + 1);
                                        let err_text = error.to_string();
                                        view.record_history(
                                            &sql,
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
                                            &sql,
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
                            MultiQueryEvent::RunFinished => {
                                let elapsed_ms = view
                                    .started_at
                                    .map(|t| t.elapsed().as_millis() as i64)
                                    .unwrap_or(0);
                                match &errored {
                                    None => {
                                        view.status = format!(
                                            "{total_statements} příkazů, {with_rows} s výsledky, \
                                             {writes} zápisů ({total_affected} řádků) — hotovo{limit_suffix}"
                                        );
                                        let row_count = rows_returned as i64 + total_affected as i64;
                                        view.record_history(
                                            &sql,
                                            &history_conn_name,
                                            history_started_at,
                                            Some(elapsed_ms),
                                            Some(row_count),
                                            None,
                                            cx,
                                        );
                                    }
                                    Some(err_text) => {
                                        view.record_history(
                                            &sql,
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
                if view.run_generation == my_generation {
                    view.cancel = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    // -----------------------------------------------------------------
    // G12 T3: script runner UI — file/folder pickers, pre-scan, confirm
    // modal, live progress tab. Wires up `runner::run_script` (Task 1,
    // unreachable from `main` until this task — every `#[allow(dead_code)]`
    // on its types is removed as part of this task).
    // -----------------------------------------------------------------

    /// Palette „Spustit SQL soubor…“/„Spustit SQL složku…“ entry point.
    /// Guards mirror `run_query_with`'s single-flight guard (one modal/run
    /// at a time). Resolves the active connection's dialect via the SAME
    /// `resolve_spec_for_explain`/`dialect_for_engine` pair Task 2's editor
    /// unlock and G13 T6's Explain/Analyze already use — an engine without a
    /// dialect (MSSQL — CURATION item 2's explicit non-goal) refuses with a
    /// status note rather than offering a picker that could never run
    /// anything.
    fn start_script_pick(&mut self, folder: bool, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.cancel.is_some() {
            return;
        }
        let Some((read_only, timeout_secs, engine, _spec)) = self.resolve_spec_for_explain(cx) else {
            return; // resolve_spec_for_explain already set self.status
        };
        let Some(dialect) = dialect_for_engine(engine) else {
            self.status = "error: skripty nejsou podporovány pro tento engine".to_string();
            cx.notify();
            return;
        };
        let conn_label = self.current_connection_label();
        // Review fix (MAJOR 1, same pattern as `start_csv_import`'s
        // `conn_identity` — see 46f6fc1): captured HERE, before the file
        // picker + background pre-scan pass — the connection dropdown stays
        // clickable through both, so `confirm_script_run` must re-verify
        // this identity against whatever is active AT CONFIRM TIME before
        // dispatching anything.
        let conn_identity = self.current_conn_identity();

        self.status = "výběr souboru…".to_string();
        cx.notify();
        // Grounding (design §7 spike, RESOLVED — no extension-filter API
        // exists at the pinned rev: `PathPromptOptions` has no filter field,
        // the Windows `file_open_dialog` never calls `SetFileTypes`).
        // Client-side `.sql` validation happens below instead.
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: !folder,
            directories: folder,
            multiple: false,
            prompt: Some("Spustit".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "výběr zrušen".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog selhal: {e}");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };

            // Off the UI thread: pre-scan (second sequential read past the
            // dialog's own stat — accepted per design §3, the count label
            // says "odhad" nowhere the count is 100% exact anyway since a
            // concurrent edit could change it before the actual run).
            let result: Result<(String, Vec<PathBuf>, Vec<usize>), String> = cx
                .background_spawn(async move {
                    if folder {
                        let files = list_sql_files(&picked)?;
                        if files.is_empty() {
                            return Err("složka neobsahuje žádné .sql soubory".to_string());
                        }
                        let mut counts = Vec::with_capacity(files.len());
                        for f in &files {
                            counts.push(count_statements_in_file(f, dialect)?);
                        }
                        let name = picked
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| picked.display().to_string());
                        let label = format!("{name}/ ({} souborů)", files.len());
                        Ok((label, files, counts))
                    } else {
                        if !crate::scripts::is_sql_path(&picked) {
                            return Err("vyberte soubor .sql".to_string());
                        }
                        let count = count_statements_in_file(&picked, dialect)?;
                        let name = picked
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| picked.display().to_string());
                        Ok((name, vec![picked], vec![count]))
                    }
                })
                .await;

            let _ = this.update(cx, |view, cx| match result {
                Ok((source_label, files, file_counts)) => view.open_script_run_modal(
                    source_label,
                    files,
                    file_counts,
                    conn_label,
                    conn_identity,
                    read_only,
                    timeout_secs,
                    cx,
                ),
                Err(e) => {
                    view.status = format!("error: {e}");
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Part S §6 step 3: the SHARED post-pre-scan continuation of the G12
    /// script-run flow. Both the ad-hoc picker (`start_script_pick`) and the
    /// library's `▶` (`run_script_from_library`) end here, so there is
    /// exactly ONE place that decides the modal races and the connection
    /// identity re-check. Moving a single line of this into a caller forks
    /// the confirm policy — that is the defect this factoring prevents.
    #[allow(clippy::too_many_arguments)]
    fn open_script_run_modal(
        &mut self,
        source_label: String,
        files: Vec<PathBuf>,
        file_counts: Vec<usize>,
        conn_label: String,
        conn_identity: String,
        read_only: bool,
        timeout_secs: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        // Review fix (MINOR 4), carried verbatim: a modal the user opened
        // WHILE the pick/pre-scan was in flight wins — don't clobber it
        // with a stale script-run pick.
        if self.modal.is_some() {
            self.status = "výběr skriptu zahozen — je otevřený jiný dialog".to_string();
            cx.notify();
            return;
        }
        // Review fix (MAJOR 1), carried verbatim (same posture as CSV's
        // `start_csv_import`): the pick + pre-scan didn't block the
        // connection dropdown — if it already changed, don't even open the
        // modal with a stale selection. `confirm_script_run` re-checks this
        // same identity again regardless (the actual guard), so this is
        // purely the faster/friendlier refusal.
        if !conn_identity_matches(&conn_identity, &self.current_conn_identity()) {
            self.status = "připojení se během výběru změnilo — spuštění zrušeno".to_string();
            cx.notify();
            return;
        }
        // T9 review MINOR-3. Computed HERE, in the SHARED continuation, so
        // both run paths disclose it: `▶` on the bound-and-dirty script,
        // and the ad-hoc picker when the user happens to pick that same
        // file. Folded comparison for the reason `path_fold` documents.
        let dirty_bound = self.script_dirty_flag
            && self
                .script_binding
                .as_ref()
                .is_some_and(|b| files.iter().any(|f| same_path_ci(f, &b.path)));
        self.status = String::new();
        self.modal = Some(connections_ui::ModalState::ScriptRun {
            files,
            file_counts,
            tx_scope: runner::TxScope::PerFile,
            error_policy: runner::ErrorPolicy::Stop,
            source_label,
            conn_label,
            read_only,
            timeout_secs,
            dirty_bound,
            conn_identity,
        });
        // UX-polish §1.4: no-input modal, cx-only continuation — defer
        // focus to `AppView::render` via `modal_needs_focus`.
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// Part S §6: the library's „▶". Same entry gates as
    /// `start_script_pick`, same `conn_identity` captured BEFORE the
    /// pre-scan, then the SHARED continuation — so the scripts library
    /// reuses the G12 confirm policy rather than forking it, and
    /// everything downstream (`confirm_script_run`'s re-checks,
    /// `script_run_dispatch_allowed`, the tx/error radios, the runner's
    /// per-statement read-only gate, the progress tab, history's
    /// `[skript]` entry) is untouched by construction.
    ///
    /// Runs the file ON DISK — never the editor buffer, and never a save
    /// first (§1.3: auto-saving before a run would be a silent write the
    /// user never asked for). A dirty binding means editor and disk
    /// differ; the „ •" is what discloses that, and the confirm modal's
    /// statement count is the from-disk truth.
    fn run_script_from_library(&mut self, rel: String, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.cancel.is_some() {
            return;
        }
        let Some(root) = self.effective_scripts_root() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let Some((read_only, timeout_secs, engine, _spec)) = self.resolve_spec_for_explain(cx)
        else {
            return; // resolve_spec_for_explain already set self.status
        };
        let Some(dialect) = dialect_for_engine(engine) else {
            self.status = "error: skripty nejsou podporovány pro tento engine".to_string();
            cx.notify();
            return;
        };
        let conn_label = self.current_connection_label();
        // Captured HERE, before the background pre-scan — the connection
        // dropdown stays clickable throughout (see `current_conn_identity`).
        let conn_identity = self.current_conn_identity();
        cx.spawn(async move |this, cx| {
            let result: Result<(String, PathBuf, usize), String> = cx
                .background_spawn(async move {
                    let path = crate::scripts::resolve_rel(&root, &rel)?;
                    if !crate::scripts::is_sql_path(&path) {
                        return Err("vyberte soubor .sql".to_string());
                    }
                    // A stale tree (an external delete since the last scan)
                    // is a Czech error plus a rescan, never a corruption.
                    if !path.is_file() {
                        return Err("soubor už neexistuje".to_string());
                    }
                    let count = count_statements_in_file(&path, dialect)?;
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    Ok((name, path, count))
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok((label, path, count)) => view.open_script_run_modal(
                    label,
                    vec![path],
                    vec![count],
                    conn_label,
                    conn_identity,
                    read_only,
                    timeout_secs,
                    cx,
                ),
                Err(e) => {
                    view.status = format!("error: {e}");
                    view.start_scripts_scan(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The modal's „Transakce“ radio — a click on an option that would
    /// violate `script_options_valid` (whole-run scope + continue policy)
    /// is a structural no-op, per the design §2 matrix's UI rule.
    fn set_script_tx_scope(&mut self, scope: runner::TxScope, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ScriptRun { tx_scope, error_policy, .. }) = &mut self.modal {
            if script_options_valid(scope, *error_policy) {
                *tx_scope = scope;
            }
        }
        cx.notify();
    }

    /// The modal's „Při chybě“ radio — same no-op-on-invalid-combination
    /// rule as `set_script_tx_scope`.
    fn set_script_error_policy(&mut self, policy: runner::ErrorPolicy, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ScriptRun { tx_scope, error_policy, .. }) = &mut self.modal {
            if script_options_valid(*tx_scope, policy) {
                *error_policy = policy;
            }
        }
        cx.notify();
    }

    /// „Spustit“ — closes the modal, opens the `TabContent::ScriptRun`
    /// progress tab, and drains `runner::run_script`'s event stream into
    /// `ScriptRunState`.
    ///
    /// §3-novela / carry-forward review note (sql_preview credential
    /// handling): `ScriptEvent`'s `sql_preview` field (and the log lines
    /// built from it below) is display-safe-capped (200 chars, single-line,
    /// `runner::sql_preview`) but NOT secret-redacted — a script containing
    /// e.g. `ALTER USER x PASSWORD 'y'` shows that literal text in the
    /// progress tab's log, exactly like the app's EXISTING convention for
    /// every other run: `record_history`/`AppView::run_query_with` already
    /// store the full literal editor SQL in the history DB unredacted (see
    /// `history_panel.rs`). The log here is even MORE conservative than
    /// that existing convention: it only ever holds `sql_preview`'s capped
    /// text (never full statement text, never file contents), and the
    /// history entry this run records is a SYNTHETIC description
    /// (`script_history_sql`) — never the SQL at all. So this matches (and
    /// narrows) the app's existing "history/log stores literal SQL"
    /// posture rather than inventing a new, inconsistent redaction rule
    /// for scripts alone.
    fn confirm_script_run(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptRun {
            files,
            file_counts,
            tx_scope,
            error_policy,
            source_label,
            conn_identity,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        // Review fix (MINOR 3): a query started DURING the picker/pre-scan
        // window can still be streaming — confirming now would start a
        // second concurrent run and silently orphan the first token
        // (`self.cancel` clobbered). Refuse and keep the modal open (don't
        // clear `self.modal`) so the user can retry once the other run
        // finishes.
        if self.cancel.is_some() {
            self.status = "jiný dotaz stále běží — počkejte na dokončení".to_string();
            cx.notify();
            return;
        }
        // Review fix (MAJOR 1): re-verify the connection identity captured
        // at `start_script_pick` time against whatever is active NOW — same
        // pattern as `confirm_csv_import`'s guard (46f6fc1). On mismatch:
        // close the modal and refuse — no spec is ever resolved,
        // `self.runner.run_script` is never reached.
        if !script_run_dispatch_allowed(&conn_identity, &self.current_conn_identity()) {
            self.modal = None;
            self.status = "připojení se během výběru změnilo — spuštění zrušeno".to_string();
            cx.notify();
            return;
        }
        let Some((_, timeout_secs, engine, spec)) = self.resolve_spec_for_explain(cx) else {
            self.modal = None;
            return;
        };
        let Some(dialect) = dialect_for_engine(engine) else {
            self.status = "error: skripty nejsou podporovány pro tento engine".to_string();
            self.modal = None;
            cx.notify();
            return;
        };
        self.modal = None;

        let opts =
            ScriptRunOptions { tx_scope, error_policy, dialect, statement_timeout_secs: timeout_secs };

        let file_rows: Vec<ScriptFileRow> = files
            .iter()
            .zip(file_counts.iter())
            .map(|(p, _)| ScriptFileRow {
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.display().to_string()),
                status: ScriptFileStatus::Pending,
                statements_run: 0,
                statements_failed: 0,
            })
            .collect();
        let total_statements: usize = file_counts.iter().sum();
        let state = Rc::new(RefCell::new(ScriptRunState {
            files: file_rows,
            total_statements,
            statements_run: 0,
            statements_failed: 0,
            total_affected: 0,
            progress_rows: None,
            log: std::collections::VecDeque::new(),
            outcome: ScriptRunOutcome::Running,
            started_at: std::time::Instant::now(),
            elapsed: None,
        }));

        let conn_identity = self.current_conn_identity();
        self.tabs.open(ResultTab {
            id: 0,
            title: format!("Skript: {source_label}"),
            pinned: false,
            preview_key: None,
            conn_identity,
            content: TabContent::ScriptRun { state: state.clone() },
        });

        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.run_generation += 1;
        let my_generation = self.run_generation;
        self.status = format!("skript {source_label}…");
        cx.notify();

        let history_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let history_conn_name = self.active_connection_name_for_history();
        let files_for_history: Vec<(PathBuf, usize)> = files.iter().cloned().zip(file_counts).collect();
        let run_cancel = cancel.clone();
        let mut rx = self.runner.run_script(spec, files, opts, run_cancel);

        cx.spawn(async move |this, cx| {
            let mut current_file_ix: Option<usize> = None;
            let mut last_preview = String::new();
            while let Some(ev) = rx.recv().await {
                let stop = this
                    .update(cx, |view, cx| {
                        match ev {
                            ScriptEvent::FileStarted { path, index, total_files } => {
                                current_file_ix = Some(index);
                                let mut s = state.borrow_mut();
                                if let Some(row) = s.files.get_mut(index) {
                                    row.status = ScriptFileStatus::Running;
                                }
                                let name = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path.display().to_string());
                                s.push_log(format!("▶ soubor {}/{total_files}: {name}", index + 1));
                            }
                            ScriptEvent::StatementStarted { stmt_index, sql_preview } => {
                                last_preview = sql_preview;
                                view.status = format!("skript {source_label}: příkaz {}…", stmt_index + 1);
                            }
                            ScriptEvent::StatementFinished { stmt_index, affected, elapsed } => {
                                let mut s = state.borrow_mut();
                                s.statements_run += 1;
                                if let Some(n) = affected {
                                    s.total_affected += n;
                                }
                                if let Some(ix) = current_file_ix {
                                    if let Some(row) = s.files.get_mut(ix) {
                                        row.statements_run += 1;
                                    }
                                }
                                let rows_note =
                                    affected.map(|n| format!(", {n} řádků")).unwrap_or_default();
                                s.push_log(format!(
                                    "✓ #{} {last_preview} ({} ms{rows_note})",
                                    stmt_index + 1,
                                    elapsed.as_millis()
                                ));
                            }
                            ScriptEvent::StatementFailed { stmt_index, error } => {
                                let mut s = state.borrow_mut();
                                s.statements_failed += 1;
                                if let Some(ix) = current_file_ix {
                                    if let Some(row) = s.files.get_mut(ix) {
                                        row.statements_failed += 1;
                                    }
                                }
                                s.push_log(format!(
                                    "✗ #{} {last_preview} — chyba: {error}",
                                    stmt_index + 1
                                ));
                            }
                            ScriptEvent::FileFinished { path, statements_run, statements_failed, elapsed } => {
                                let mut s = state.borrow_mut();
                                if let Some(ix) = current_file_ix {
                                    if let Some(row) = s.files.get_mut(ix) {
                                        row.status = if statements_failed > 0 {
                                            ScriptFileStatus::Failed
                                        } else {
                                            ScriptFileStatus::Done
                                        };
                                    }
                                }
                                let name = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path.display().to_string());
                                s.push_log(format!(
                                    "— {name} dokončen: {statements_run} OK, {statements_failed} chyb ({} ms)",
                                    elapsed.as_millis()
                                ));
                            }
                            ScriptEvent::RunFinished {
                                files_run,
                                statements_run,
                                statements_failed,
                                elapsed,
                                aborted,
                            } => {
                                {
                                    let mut s = state.borrow_mut();
                                    if aborted {
                                        for row in s.files.iter_mut() {
                                            if matches!(
                                                row.status,
                                                ScriptFileStatus::Pending | ScriptFileStatus::Running
                                            ) {
                                                row.status = ScriptFileStatus::Skipped;
                                            }
                                        }
                                    }
                                    s.elapsed = Some(elapsed);
                                    s.outcome = if !aborted {
                                        ScriptRunOutcome::Done
                                    } else if cancel.is_cancelled() {
                                        ScriptRunOutcome::Cancelled
                                    } else {
                                        ScriptRunOutcome::Failed
                                    };
                                }
                                let hist_sql = script_history_sql(
                                    &files_for_history,
                                    statements_run,
                                    statements_failed,
                                );
                                let err_opt: Option<String> =
                                    if aborted { Some("běh přerušen".to_string()) } else { None };
                                view.record_history(
                                    &hist_sql,
                                    &history_conn_name,
                                    history_started_at,
                                    Some(elapsed.as_millis() as i64),
                                    Some(statements_run as i64),
                                    err_opt.as_deref(),
                                    cx,
                                );
                                view.status = if aborted {
                                    format!(
                                        "skript {source_label}: přerušeno ({files_run} souborů, {statements_run}/{total_statements} příkazů)"
                                    )
                                } else {
                                    format!(
                                        "skript {source_label}: hotovo ({files_run} souborů, {statements_run} příkazů, {statements_failed} chyb)"
                                    )
                                };
                                if view.run_generation == my_generation {
                                    view.cancel = None;
                                }
                            }
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(false);
                if stop {
                    break;
                }
            }
            let _ = this.update(cx, |view, cx| {
                if view.run_generation == my_generation {
                    view.cancel = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    // -----------------------------------------------------------------
    // G12 T4: CSV import UI — file picker, header/row pre-count peek,
    // column-mapping modal, batched-execute via `runner::run_csv_import`
    // (Task 1/T7's sanctioned write path). Reuses Task 3's
    // `TabContent::ScriptRun` progress-tab kind (`progress_rows` drives the
    // honest rows-done/rows-total display CSV import needs and script runs
    // don't). Entry points: `TreeEvent::ImportCsv` (schema tree "⇪") and
    // `GridEvent::ImportCsvRequested` (preview toolbar "Import CSV") — both
    // gated read-only at the UI layer (tree icon absent / grid button
    // absent) AND re-checked here AND by `run_csv_import`'s own shared
    // guard (CURATION item 4(b), all three layers).
    // -----------------------------------------------------------------

    /// Schema-tree "⇪" / preview-toolbar "Import CSV" entry point.
    fn start_csv_import(&mut self, schema: Option<String>, table: String, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.cancel.is_some() {
            return;
        }
        if self.active_read_only() {
            self.status = "error: připojení je jen pro čtení".to_string();
            cx.notify();
            return;
        }
        let Some(snapshot) = self.tree.read(cx).snapshot() else {
            self.status = "error: schéma není načteno".to_string();
            cx.notify();
            return;
        };
        let Some(t) = snapshot.tables.iter().find(|t| t.schema == schema && t.name == table) else {
            self.status = "error: tabulka nenalezena ve schématu".to_string();
            cx.notify();
            return;
        };
        let columns: Vec<csv_import::TargetColumn> = t
            .columns
            .iter()
            .map(|c| csv_import::TargetColumn {
                name: c.name.clone(),
                numeric: csv_import::is_numeric_type_name(&c.data_type),
            })
            .collect();

        // Review fix (BLOCKER): captured HERE, before the (non-blocking)
        // file picker + background pre-count pass — the connection dropdown
        // stays clickable through both, so `confirm_csv_import` must
        // re-verify this identity against whatever is active AT CONFIRM
        // TIME before dispatching anything (see `ModalState::CsvImport`'s
        // doc comment). `conn_identity` is the STABLE value the guard
        // actually compares; `conn_label` is display-only.
        let conn_identity = self.current_conn_identity();
        let conn_label = self.current_connection_label();
        // Batch C review BLOCKER 1: captured alongside `conn_identity` (same
        // rationale — the connection dropdown stays clickable through the
        // picker/pre-count pass) so the sample SQL shown below is built for
        // the SAME connection `conn_identity` refers to; `confirm_csv_import`
        // re-resolves the engine itself for the actual execution, but only
        // ever proceeds when `conn_identity` still matches — display/exec
        // parity holds because both resolve from the same connection.
        let dialect = self.active_engine().map(sql_dialect).unwrap_or(dbc_core::Dialect::Postgres);

        self.status = "výběr CSV souboru…".to_string();
        cx.notify();
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "výběr zrušen".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog selhal: {e}");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let is_csv = picked
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("csv"));
            if !is_csv {
                let _ = this.update(cx, |view, cx| {
                    view.status = "error: vyberte soubor .csv".to_string();
                    cx.notify();
                });
                return;
            }

            let peek_path = picked.clone();
            let peek: Result<(Vec<String>, usize, Vec<csv_import::CsvRow>), String> = cx
                .background_spawn(async move {
                    let mut reader = csv::Reader::from_path(&peek_path)
                        .map_err(|e| format!("{}: {e}", peek_path.display()))?;
                    let headers: Vec<String> = reader
                        .headers()
                        .map_err(|e| format!("{}: {e}", peek_path.display()))?
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    let mut row_count = 0usize;
                    let mut first_rows: Vec<csv_import::CsvRow> = Vec::new();
                    for rec in reader.records() {
                        let rec = rec.map_err(|e| format!("{}: {e}", peek_path.display()))?;
                        if first_rows.len() < csv_import::CSV_IMPORT_BATCH_SIZE {
                            first_rows.push(rec.iter().map(csv_field_to_value).collect());
                        }
                        row_count += 1;
                    }
                    Ok((headers, row_count, first_rows))
                })
                .await;

            let _ = this.update(cx, |view, cx| match peek {
                Ok((headers, row_count, first_rows)) => {
                    let mapping = default_csv_mapping(&headers, &columns);
                    // Batch C review BLOCKER 1: dialect-aware sibling —
                    // `dialect` resolved above alongside `conn_identity`, the
                    // same connection the actual import will run against.
                    let sample_sql = csv_import::generate_insert_batches_d(
                        dialect,
                        schema.as_deref(),
                        &table,
                        &columns,
                        &mapping,
                        &first_rows,
                    );
                    let (sample_sql, error) = match sample_sql {
                        Ok(stmts) => (stmts.into_iter().next().map(cap_sql_sample), None),
                        Err(msg) => (None, Some(msg)),
                    };
                    // Review fix (MINOR 4): a modal the user opened WHILE
                    // this picker/peek was in flight wins — don't clobber
                    // it with a stale CSV-import pick.
                    if view.modal.is_some() {
                        view.status =
                            "výběr CSV zahozen — je otevřený jiný dialog".to_string();
                        cx.notify();
                        return;
                    }
                    // Review fix, defense in depth (optional per the
                    // review, cheap here): the picker + this background
                    // pre-count pass didn't block the connection dropdown —
                    // if it already changed, don't even open the modal with
                    // stale schema/columns; `confirm_csv_import` re-checks
                    // this same identity again regardless (the actual
                    // BLOCKER fix), so this is purely a faster/friendlier
                    // refusal, not the enforcement point.
                    if !conn_identity_matches(&conn_identity, &view.current_conn_identity()) {
                        view.status =
                            "připojení se během importu změnilo — import zrušen".to_string();
                        cx.notify();
                        return;
                    }
                    view.status = String::new();
                    view.modal = Some(connections_ui::ModalState::CsvImport {
                        path: picked,
                        schema,
                        table,
                        headers,
                        columns,
                        targets: mapping.targets,
                        row_count,
                        first_rows,
                        sample_sql,
                        error,
                        conn_identity,
                        conn_label,
                    });
                    // UX-polish §1.4: no-input modal, cx-only continuation —
                    // defer focus to `AppView::render` via `modal_needs_focus`.
                    view.modal_needs_focus = true;
                    cx.notify();
                }
                Err(e) => {
                    view.status = format!("error: {e}");
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The mapping modal's per-header cycle-button — `(přeskočit)` -> each
    /// target column in order, wrapping back to `(přeskočit)`.
    fn cycle_csv_target(&mut self, header_ix: usize, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::CsvImport { targets, columns, .. }) = &mut self.modal
        {
            if let Some(t) = targets.get_mut(header_ix) {
                *t = match *t {
                    None if columns.is_empty() => None,
                    None => Some(0),
                    Some(i) if i + 1 < columns.len() => Some(i + 1),
                    Some(_) => None,
                };
            }
        }
        self.recompute_csv_sample(cx);
    }

    /// Recomputes `sample_sql`/`error` from the REAL first batch on every
    /// mapping change (never a synthetic example) — an `Err` (duplicate
    /// target) fills `error` and disables "Spustit import" (see
    /// `render_csv_import_panel`'s `can_run` gate).
    fn recompute_csv_sample(&mut self, cx: &mut Context<Self>) {
        // Batch C review BLOCKER 1: resolved BEFORE the `&mut self.modal`
        // borrow below (needs `&self`) — same resolution
        // (`active_engine` -> `sql_dialect`) `start_csv_import` captured for
        // the initial sample; execution only ever proceeds when the active
        // connection still matches the modal's `conn_identity`
        // (`confirm_csv_import`'s guard), so this stays in parity with what
        // will actually run.
        let dialect = self.active_engine().map(sql_dialect).unwrap_or(dbc_core::Dialect::Postgres);
        if let Some(connections_ui::ModalState::CsvImport {
            schema,
            table,
            columns,
            targets,
            first_rows,
            sample_sql,
            error,
            ..
        }) = &mut self.modal
        {
            let mapping = csv_import::ColumnMapping { targets: targets.clone() };
            match csv_import::generate_insert_batches_d(
                dialect,
                schema.as_deref(),
                table,
                columns.as_slice(),
                &mapping,
                first_rows.as_slice(),
            ) {
                Ok(stmts) => {
                    *sample_sql = stmts.into_iter().next().map(cap_sql_sample);
                    *error = if sample_sql.is_none() {
                        Some("žádný sloupec není namapován".to_string())
                    } else {
                        None
                    };
                }
                Err(msg) => {
                    *sample_sql = None;
                    *error = Some(msg);
                }
            }
        }
        cx.notify();
    }

    /// „Spustit import“ — closes the modal, opens the `TabContent::ScriptRun`
    /// progress tab (`progress_rows: Some((0, row_count))`), and drains
    /// `runner::run_csv_import`'s event stream.
    ///
    /// Review fix (BLOCKER): the FIRST thing this does, before resolving a
    /// fresh spec or touching `columns`/`schema`/`table` at all, is
    /// re-verify the connection identity captured at `start_csv_import`
    /// time against whatever is active NOW — see `csv_import_dispatch_allowed`
    /// and `ModalState::CsvImport`'s doc comment for why this is needed
    /// (the picker + pre-count pass don't block the connection dropdown).
    /// On mismatch: close the modal and refuse — no `CsvImportJob` is ever
    /// built, `resolve_spec_for_explain`/`self.runner.run_csv_import` are
    /// never reached.
    fn confirm_csv_import(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::CsvImport {
            path,
            schema,
            table,
            columns,
            targets,
            row_count,
            error,
            conn_identity,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        if error.is_some() {
            return; // "Spustit import" is rendered disabled in this state too.
        }
        // Review fix (MINOR 3): same guard as `confirm_script_run` — a
        // query started DURING the picker/peek window can still be
        // streaming; confirming now would start a second concurrent run
        // and silently orphan the first token. Refuse and keep the modal
        // open.
        if self.cancel.is_some() {
            self.status = "jiný dotaz stále běží — počkejte na dokončení".to_string();
            cx.notify();
            return;
        }
        if !csv_import_dispatch_allowed(&conn_identity, &self.current_conn_identity()) {
            self.modal = None;
            self.status = "připojení se během importu změnilo — import zrušen".to_string();
            cx.notify();
            return;
        }
        let Some((_, timeout_secs, _, spec)) = self.resolve_spec_for_explain(cx) else {
            self.modal = None;
            return;
        };
        self.modal = None;

        let mapping = csv_import::ColumnMapping { targets };
        let job = CsvImportJob { path: path.clone(), schema, table: table.clone(), columns, mapping };

        let batch_count = row_count.div_ceil(csv_import::CSV_IMPORT_BATCH_SIZE);
        let state = Rc::new(RefCell::new(ScriptRunState {
            files: Vec::new(),
            total_statements: batch_count,
            statements_run: 0,
            statements_failed: 0,
            total_affected: 0,
            progress_rows: Some((0, row_count as u64)),
            log: std::collections::VecDeque::new(),
            outcome: ScriptRunOutcome::Running,
            started_at: std::time::Instant::now(),
            elapsed: None,
        }));

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let conn_identity = self.current_conn_identity();
        self.tabs.open(ResultTab {
            id: 0,
            title: format!("CSV import: {file_name}"),
            pinned: false,
            preview_key: None,
            conn_identity,
            content: TabContent::ScriptRun { state: state.clone() },
        });

        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.run_generation += 1;
        let my_generation = self.run_generation;
        self.status = format!("CSV import {file_name}…");
        cx.notify();

        let history_started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let history_conn_name = self.active_connection_name_for_history();
        let run_cancel = cancel.clone();
        let mut rx = self.runner.run_csv_import(spec, job, run_cancel, timeout_secs);

        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let stop = this
                    .update(cx, |view, cx| {
                        match ev {
                            CsvImportEvent::BatchStarted { batch_index, rows_in_batch } => {
                                let mut s = state.borrow_mut();
                                s.push_log(format!("▶ dávka #{} ({rows_in_batch} řádků)", batch_index + 1));
                            }
                            CsvImportEvent::BatchFinished { batch_index, rows_committed_so_far } => {
                                let mut s = state.borrow_mut();
                                s.statements_run += 1;
                                s.progress_rows = Some((rows_committed_so_far, row_count as u64));
                                s.push_log(format!(
                                    "✓ dávka #{} — celkem {rows_committed_so_far} řádků",
                                    batch_index + 1
                                ));
                            }
                            CsvImportEvent::Failed { error } => {
                                {
                                    let mut s = state.borrow_mut();
                                    s.statements_failed += 1;
                                    s.outcome = if cancel.is_cancelled() {
                                        ScriptRunOutcome::Cancelled
                                    } else {
                                        ScriptRunOutcome::Failed
                                    };
                                    s.push_log(format!(
                                        "✗ chyba: {error} — import zrušen, žádná data nezapsána"
                                    ));
                                }
                                let err_text = error.to_string();
                                view.record_history(
                                    &format!("[CSV import] {} → {table}", path.display()),
                                    &history_conn_name,
                                    history_started_at,
                                    None,
                                    Some(0),
                                    Some(&err_text),
                                    cx,
                                );
                                view.status = format!("CSV import selhal: {error}");
                                if view.run_generation == my_generation {
                                    view.cancel = None;
                                }
                            }
                            CsvImportEvent::Finished { rows_imported, elapsed } => {
                                {
                                    let mut s = state.borrow_mut();
                                    s.outcome = ScriptRunOutcome::Done;
                                    s.elapsed = Some(elapsed);
                                    s.progress_rows = Some((rows_imported, row_count as u64));
                                }
                                let hist_sql = format!(
                                    "[CSV import] {} → {table} ({rows_imported} řádků, dávka {})",
                                    path.display(),
                                    csv_import::CSV_IMPORT_BATCH_SIZE
                                );
                                view.record_history(
                                    &hist_sql,
                                    &history_conn_name,
                                    history_started_at,
                                    Some(elapsed.as_millis() as i64),
                                    Some(rows_imported as i64),
                                    None,
                                    cx,
                                );
                                view.status = format!("CSV import hotovo: {rows_imported} řádků");
                                if view.run_generation == my_generation {
                                    view.cancel = None;
                                }
                            }
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(false);
                if stop {
                    break;
                }
            }
            let _ = this.update(cx, |view, cx| {
                if view.run_generation == my_generation {
                    view.cancel = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    // -----------------------------------------------------------------
    // G13 T6: "Vysvětlit"/"Analyzovat" status-bar buttons — dispatch,
    // three-case write gate (design §5), and the resulting `TabContent::Plan`
    // tab. See `plan.rs`'s module doc comment / the design's §3-novela
    // note for the ANALYZE-on-a-write sequence's write-path status.
    // -----------------------------------------------------------------

    /// Shared spec-resolution slice of `run_query_with`'s own block, factored
    /// out for `run_explain`/`on_confirm_analyze_write` to reuse without
    /// duplicating the connection-lookup/CLI-url branching. Returns `None`
    /// (status already set) on no active connection.
    fn resolve_spec_for_explain(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(bool, Option<u64>, dbc_state::Engine, ConnectSpec)> {
        if self.active_connection_id.is_some() {
            let Some(a) = self.resolve_active() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return None;
            };
            Some((a.read_only, a.timeout_secs, a.engine, a.into_spec()))
        } else if let Some(url) = self.conn_url.clone() {
            Some((false, None, engine_from_url(&url), ConnectSpec::Url(url)))
        } else {
            self.status = "Bez připojení — vyberte připojení nahoře.".into();
            cx.notify();
            None
        }
    }

    /// Dispatch for both buttons: `is_analyze == false` is §5's ALWAYS-safe
    /// estimated path (no gate, ever, on any engine/connection); `true`
    /// routes through `plan::analyze_gate`'s three-case dispatch (Run /
    /// Blocked / NeedsConfirm) decided from the RAW pre-wrap SQL, mirroring
    /// `run_query_with`'s Guard 1 read-only check.
    ///
    /// G15 T7: MSSQL routes to `dispatch_mssql_plan` (session preludes via
    /// `query_with_session`) BEFORE either the generic estimated dispatch
    /// OR `analyze_gate`'s match — `plan::explain_sql`/`explain_analyze_sql`
    /// no longer produce a runnable MSSQL string at all (see their doc
    /// comments), so MSSQL must never reach `dispatch_plan_query`. Gating
    /// itself is UNCHANGED for the analyze path: `analyze_gate`'s three
    /// cases still decide Run/Blocked/NeedsConfirm by SQL classification,
    /// not by engine — only WHERE the eventual "Run" lands differs.
    fn run_explain(&mut self, is_analyze: bool, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.cancel.is_some() {
            return;
        }
        let sql = self.sql.read(cx).text().to_string();
        if sql.trim().is_empty() {
            return;
        }

        let Some((read_only, timeout_secs, engine, spec)) = self.resolve_spec_for_explain(cx) else {
            return; // resolve_spec_for_explain already set self.status on failure
        };

        if !is_analyze {
            if engine == dbc_state::Engine::Mssql {
                if !plan::mssql_plan_dispatch_available() {
                    self.status = "plán pro MSSQL zatím není k dispozici".to_string();
                    cx.notify();
                    return;
                }
                self.dispatch_mssql_plan(spec, sql, false, timeout_secs, cx);
                return;
            }
            // §5: Explain is ALWAYS safe — no gate, dispatch immediately.
            self.dispatch_plan_query(spec, plan::explain_sql(engine, &sql), engine, false, timeout_secs, cx);
            return;
        }

        match plan::analyze_gate(&sql, read_only, sql_dialect(engine)) {
            plan::AnalyzeGate::Run => {
                if engine == dbc_state::Engine::Mssql {
                    if !plan::mssql_plan_dispatch_available() {
                        self.status = "plán pro MSSQL zatím není k dispozici".to_string();
                        cx.notify();
                        return;
                    }
                    self.dispatch_mssql_plan(spec, sql, true, timeout_secs, cx);
                    return;
                }
                let Some(explain_sql) = plan::explain_analyze_sql(engine, &sql) else { return }; // SQLite: button hidden, unreachable
                self.dispatch_plan_query(spec, explain_sql, engine, true, timeout_secs, cx);
            }
            plan::AnalyzeGate::Blocked => {
                self.status = "error: připojení je jen pro čtení".to_string();
                cx.notify();
            }
            plan::AnalyzeGate::NeedsConfirm => {
                if engine == dbc_state::Engine::Duckdb {
                    // G16 T5 (resolved design gap): the DuckDB driver's
                    // query() sessions are independent clones off the shared
                    // root, invisible to execute()'s persistent exec_conn —
                    // the same structural property runner.rs's
                    // analyze_write_tests document for sqlite. So
                    // run_analyze_write's BEGIN → EXPLAIN (ANALYZE …) →
                    // ROLLBACK CANNOT actually wrap the analyzed write in a
                    // transaction: the write would durably COMMIT while the
                    // UI claims "změny vráceny zpět". Refuse honestly.
                    // Pinned by runner.rs::duckdb_query_sessions_do_not_see_execute_transactions.
                    self.status = "error: EXPLAIN ANALYZE zápisu není pro DuckDB podporováno — analyzovaný zápis nelze bezpečně vrátit".to_string();
                    cx.notify();
                    return;
                }
                self.modal = Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                    sql,
                    engine,
                    running: false,
                    error: None,
                });
                // UX-polish §1.4: no-input modal, cx-only opener — defer
                // focus to `AppView::render` via `modal_needs_focus`.
                self.modal_needs_focus = true;
                cx.notify();
            }
        }
    }

    /// The estimated-path AND the read-case-of-Analyze path both go through
    /// the NORMAL `connect_and_run` (no write gating needed for either —
    /// design §5: a plain read, or `EXPLAIN` itself, never writes) —
    /// draining exactly like an ad-hoc tab but capturing text/rows into a
    /// `plan::PlanResult` instead of opening a live grid.
    fn dispatch_plan_query(
        &mut self,
        spec: ConnectSpec,
        wrapped_sql: String,
        engine: dbc_state::Engine,
        is_analyze: bool,
        timeout_secs: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.run_generation += 1;
        let my_generation = self.run_generation;
        self.status = if is_analyze { "analyzuji plán…".to_string() } else { "vysvětluji plán…".to_string() };
        cx.notify();

        let sql_title = format!("Plán: {}", collapse_title(&wrapped_sql));
        let conn_identity = self.current_conn_identity();
        let mut rx = self.runner.connect_and_run(spec, wrapped_sql, cancel, timeout_secs);
        // Phase-3 follow-up I2: EXPLAIN output is always tiny (one row/JSON
        // blob) so this buffer realistically never spills, but the push is
        // routed through `push_async` anyway for consistency with the other
        // two `QueryEvent`/`MultiQueryEvent` loops and to not leave a
        // synchronous spill-write path on the foreground executor at all.
        let handle = self.runner.handle();
        cx.spawn(async move |this, cx| {
            let mut buffer: Option<ResultBuffer> = None;
            let mut failed: Option<QueryError> = None;
            while let Some(ev) = rx.recv().await {
                // I2: same "push before `this.update`, `continue`" shape as
                // `run_query_with` — see that loop's comment for why `ev`
                // can't be matched a second time below.
                if let QueryEvent::Batch(b) = ev {
                    if let Some(buf) = buffer.as_mut() {
                        if let Err(e) = buf.push_async(b, &handle).await {
                            failed = Some(QueryError::msg(e.to_string()));
                        }
                    }
                    continue;
                }
                let stop = this
                    .update(cx, |_view, _cx| match ev {
                        QueryEvent::Started { columns } => {
                            buffer = Some(ResultBuffer::new(columns));
                            false
                        }
                        // I2: `Batch` handled above, before this
                        // `this.update` call.
                        QueryEvent::Batch(_) => unreachable!("Batch handled before this match"),
                        QueryEvent::Finished { .. } => true,
                        // G15 T8 whole-branch review B2 fix: `stream_query`
                        // only sends this for a WRITE on an MSSQL spec, and
                        // this function is only ever reached with the
                        // `explain_sql`/`explain_analyze_sql` output of a
                        // READ (`run_explain`'s MSSQL arm routes to
                        // `dispatch_mssql_plan` instead, before this
                        // function is ever called) — defensively handled
                        // (not `unreachable!()`) rather than assumed away.
                        QueryEvent::WriteFinished { .. } => {
                            failed = Some(QueryError::msg(
                                "interní chyba: plán vrátil zápis místo výsledku",
                            ));
                            true
                        }
                        QueryEvent::Failed(e) => {
                            failed = Some(e);
                            true
                        }
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }

            let _ = this.update(cx, move |view, cx| {
                if view.run_generation != my_generation {
                    return; // a newer run superseded this one — don't clobber its state
                }
                view.cancel = None;
                if let Some(e) = failed {
                    view.status = format!("error: {e}");
                    cx.notify();
                    return;
                }
                let Some(mut buf) = buffer else {
                    view.status = "prázdná odpověď EXPLAIN".to_string();
                    cx.notify();
                    return;
                };

                let parsed = if engine == dbc_state::Engine::Sqlite {
                    let mut rows: Vec<(i64, i64, String)> = Vec::new();
                    let mut raw_lines: Vec<String> = Vec::new();
                    for r in 0..buf.row_count() {
                        let id = buf.cell_text(r, 0).parse().unwrap_or(0);
                        let parent = buf.cell_text(r, 1).parse().unwrap_or(0);
                        let detail = buf.cell_text(r, 3); // columns: id, parent, notused, detail
                        raw_lines.push(format!("{id}\t{parent}\t{detail}"));
                        rows.push((id, parent, detail));
                    }
                    Ok(plan::PlanResult {
                        root: plan::parse_sqlite_rows(&rows),
                        is_analyze,
                        engine,
                        total_planning_time_ms: None,
                        total_execution_time_ms: None,
                        top_level_hints: Vec::new(),
                        raw_text: raw_lines.join("\n"),
                    })
                } else {
                    // G16 T5: DuckDB's EXPLAIN result set is
                    // (explain_key, explain_value) — the payload sits in
                    // the SECOND column (capture-pinned); pg stays 0.
                    let payload_col = plan::plan_payload_col(engine);
                    let raw_text = if buf.row_count() == 0 || buf.cell_is_null(0, payload_col) {
                        Err("EXPLAIN nevrátil žádný řádek".to_string())
                    } else {
                        Ok(buf.cell_text(0, payload_col))
                    };
                    raw_text.and_then(|t| plan::parse_plan(engine, is_analyze, &t))
                };

                match parsed {
                    Ok(result) => {
                        let result = Rc::new(result);
                        let view_entity = cx.new(|cx| plan::PlanView::new(result, cx));
                        let tab = ResultTab {
                            id: 0,
                            title: sql_title,
                            pinned: false,
                            preview_key: None,
                            conn_identity,
                            content: TabContent::Plan { view: view_entity },
                        };
                        view.tabs.open(tab);
                        view.status = "hotovo".to_string();
                    }
                    Err(e) => {
                        view.status = format!("error parsování plánu: {e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// G15 T7: dispatches `QueryRunner::run_mssql_plan` — the MSSQL face
    /// of BOTH `run_explain`'s estimated/`AnalyzeGate::Run`-case dispatch
    /// (no modal ever involved) and `on_confirm_analyze_write`'s
    /// confirmed-write dispatch (the `AnalyzeWriteConfirm` modal stays
    /// open, mutated in place, for the WHOLE duration — same "Escape is a
    /// structural no-op against this modal" invariant
    /// `on_confirm_analyze_write`'s own doc comment already documents), so
    /// checking `self.modal`'s shape at COMPLETION time (`via_confirm_modal`
    /// below) — not a value captured before the await — reliably tells the
    /// two callers apart with no race. Mirrors `dispatch_plan_query`'s
    /// status-text/tab-opening plumbing (no `ResultBuffer` streaming
    /// needed here — `run_mssql_plan` already returns the whole plan text
    /// as one `String`) and `on_confirm_analyze_write`'s modal-aware
    /// completion handling.
    fn dispatch_mssql_plan(
        &mut self,
        spec: ConnectSpec,
        sql: String,
        is_analyze: bool,
        timeout_secs: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        self.status =
            if is_analyze { "analyzuji plán…".to_string() } else { "vysvětluji plán…".to_string() };
        cx.notify();

        let sql_title = format!("Plán: {}", collapse_title(&sql));
        let conn_identity = self.current_conn_identity();
        let rx = self.runner.run_mssql_plan(spec, sql.clone(), is_analyze, timeout_secs);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, move |view, cx| {
                let via_confirm_modal =
                    matches!(view.modal, Some(connections_ui::ModalState::AnalyzeWriteConfirm { .. }));
                match result {
                    Ok(Ok(raw_text)) => {
                        match plan::parse_plan(dbc_state::Engine::Mssql, is_analyze, &raw_text) {
                            Ok(parsed) => {
                                if via_confirm_modal {
                                    view.modal = None;
                                }
                                let parsed = Rc::new(parsed);
                                let view_entity = cx.new(|cx| plan::PlanView::new(parsed, cx));
                                view.tabs.open(ResultTab {
                                    id: 0,
                                    title: sql_title,
                                    pinned: false,
                                    preview_key: None,
                                    conn_identity,
                                    content: TabContent::Plan { view: view_entity },
                                });
                                view.status = if via_confirm_modal {
                                    "hotovo (změny vráceny zpět)".to_string()
                                } else {
                                    "hotovo".to_string()
                                };
                            }
                            Err(e) => {
                                view.status = format!("error parsování plánu: {e}");
                                if let Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                                    running,
                                    error,
                                    ..
                                }) = &mut view.modal
                                {
                                    *running = false;
                                    *error = Some(format!("error parsování plánu: {e}"));
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        view.status = format!("error: {e}");
                        if let Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                            running,
                            error,
                            ..
                        }) = &mut view.modal
                        {
                            *running = false;
                            *error = Some(e.to_string());
                        }
                    }
                    Err(_canceled) => {
                        view.status = "error: plán zrušen".to_string();
                        if let Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                            running,
                            error,
                            ..
                        }) = &mut view.modal
                        {
                            *running = false;
                            *error = Some("analýza zrušena".to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Dispatches `QueryRunner::run_analyze_write` (the runner-owned,
    /// dedicated-connection BEGIN…ROLLBACK sequence), called from the
    /// `ModalState::AnalyzeWriteConfirm` dialog's "Analyzovat" button
    /// (connections_ui.rs).
    ///
    /// Review fix (MAJOR, adversarial review of commit 0bab655): the first
    /// version of this method used `self.cancel = Some(CancelToken::new())`
    /// as a busy-guard, but that token was never threaded into
    /// `QueryRunner::run_analyze_write` (which builds its own internal
    /// token in `run_analyze_write_inner`) — Escape while "analyzuji
    /// plán…" showed would clear `self.cancel`, print a false
    /// "cancelling…" status, and re-enable every other busy-guard that
    /// checks `self.cancel.is_none()`, letting a second query/Explain/
    /// Analyze dispatch start while the original BEGIN…EXPLAIN ANALYZE…
    /// ROLLBACK was still running server-side (which would then land its
    /// result and silently clobber whatever the second dispatch had just
    /// shown). Fixed by mirroring `on_confirm_apply`'s pattern instead
    /// (`ApplyDialogState::running`): `self.modal` stays `Some(..)` —
    /// mutated in place, never `.take()`n — for the WHOLE duration of the
    /// analyze (not just cleared and re-substituted by a cancel token), so
    /// the SAME `self.modal.is_some()` checks `run_query`/`run_query_with`/
    /// `run_explain` already use to refuse a second dispatch cover this
    /// path for free, and (per `on_cancel_query`'s `closable` match, which
    /// only allow-lists `ConnectionDialog`/`QueryParams` — every other
    /// modal, `AnalyzeWriteConfirm` included, falls into its `_ => false`
    /// arm) Escape is now a structural no-op against this dialog, exactly
    /// like it already was against `KillConfirm`. `self.cancel`/
    /// `self.run_generation` are never touched by this method — same as
    /// `on_confirm_apply`.
    fn on_confirm_analyze_write(
        &mut self,
        engine: dbc_state::Engine,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Pure guard (unit-tested directly, connections_ui.rs): `None` for
        // no modal, a DIFFERENT modal, or a re-click while `running` is
        // already `true` — see its doc comment for why this is the actual
        // mechanism (not `self.cancel`) that makes a second dispatch a
        // structural no-op.
        let Some(sql) = connections_ui::analyze_write_dispatch_sql(&self.modal) else { return };

        let Some((_, timeout_secs, _, spec)) = self.resolve_spec_for_explain(cx) else { return };

        if let Some(connections_ui::ModalState::AnalyzeWriteConfirm { running, error, .. }) =
            &mut self.modal
        {
            *running = true;
            *error = None;
        }
        cx.notify();

        // G15 T7: MSSQL routes to `dispatch_mssql_plan` (session preludes)
        // instead — `plan::explain_analyze_sql(Mssql, ..)` is `None` now
        // (see its doc comment), so the generic path below would bail out
        // immediately for MSSQL if reached; this check MUST come before
        // that `let Some(explain_sql) = ...` line, after the busy-guard
        // `running = true` flip above (so the confirm modal's spinner
        // shows for MSSQL too, same as every other engine).
        if engine == dbc_state::Engine::Mssql {
            if !plan::mssql_plan_dispatch_available() {
                if let Some(connections_ui::ModalState::AnalyzeWriteConfirm { running, error, .. }) =
                    &mut self.modal
                {
                    *running = false;
                    *error = Some("plán pro MSSQL zatím není k dispozici".to_string());
                }
                cx.notify();
                return;
            }
            self.dispatch_mssql_plan(spec, sql, true, timeout_secs, cx);
            return;
        }

        let Some(explain_sql) = plan::explain_analyze_sql(engine, &sql) else { return };

        let sql_title = format!("Plán: {}", collapse_title(&sql));
        let conn_identity = self.current_conn_identity();
        let rx = self.runner.run_analyze_write(spec, explain_sql, timeout_secs);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, move |view, cx| {
                match result {
                    Ok(Ok(raw_text)) => match plan::parse_plan(engine, true, &raw_text) {
                        Ok(parsed) => {
                            // Brief-mirroring `on_confirm_apply`'s success
                            // shape: close the dialog, open the result tab,
                            // global status takes over from here.
                            view.modal = None;
                            let parsed = Rc::new(parsed);
                            let view_entity = cx.new(|cx| plan::PlanView::new(parsed, cx));
                            view.tabs.open(ResultTab {
                                id: 0,
                                title: sql_title,
                                pinned: false,
                                preview_key: None,
                                conn_identity,
                                content: TabContent::Plan { view: view_entity },
                            });
                            view.status = "hotovo (změny vráceny zpět)".to_string();
                        }
                        Err(e) => {
                            if let Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                                running,
                                error,
                                ..
                            }) = &mut view.modal
                            {
                                *running = false;
                                *error = Some(format!("error parsování plánu: {e}"));
                            }
                        }
                    },
                    Ok(Err(e)) => {
                        if let Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                            running,
                            error,
                            ..
                        }) = &mut view.modal
                        {
                            *running = false;
                            *error = Some(e.to_string());
                        }
                    }
                    Err(_canceled) => {
                        if let Some(connections_ui::ModalState::AnalyzeWriteConfirm {
                            running,
                            error,
                            ..
                        }) = &mut view.modal
                        {
                            *running = false;
                            *error = Some("analýza zrušena".to_string());
                        }
                    }
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
            // §W4 (T4 review MINOR-4): a BLOCKING modal is never
            // Esc-closable, whatever its contents — checked before the
            // per-variant rules below, which all ask the narrower "is there
            // unsaved secret state / a running job?" question. The property
            // lives in `connections_ui::modal_is_blocking` so it can be
            // pinned by a unit test; this handler cannot be.
            if connections_ui::modal_is_blocking(&modal) {
                return;
            }
            let closable = match &modal {
                connections_ui::ModalState::ConnectionDialog(ui) => ui.password.read(cx).text().is_empty(),
                // G6 Task 3: no password/unsaved-secret concern here — Esc
                // always cancels the values dialog (no run, no persistence,
                // same contract as its "Zrušit" button/`cancel_query_params`).
                connections_ui::ModalState::QueryParams { .. } => true,
                // G11 T6: not closable while a backup/restore is actually
                // running (design: Esc must never abandon a running
                // pg_dump/pg_restore/psql child or an in-flight MSSQL/
                // SQLite write silently) — closable once it reaches a
                // terminal state or is still only `Confirming` (nothing
                // dispatched yet).
                connections_ui::ModalState::BackupRestore(session) => !session.is_running(),
                // G12 T3/T4: no run has started yet (that only happens on
                // "Spustit"/"Spustit import" — see
                // `confirm_script_run`/`confirm_csv_import`) and neither
                // holds unsaved secret state, so Esc closing them is safe —
                // same reasoning `QueryParams` documents above.
                connections_ui::ModalState::ScriptRun { .. } => true,
                connections_ui::ModalState::CsvImport { .. } => true,
                // G14 T10: no secret/unsaved-run state at all — a benign
                // display-only panel, same reasoning as `QueryParams` above.
                connections_ui::ModalState::Settings => true,
                // G14 T11: no secret/unsaved-run state — a pick-then-confirm
                // dialog, same reasoning as `QueryParams`/`Settings` above.
                connections_ui::ModalState::ChartPicker { .. } => true,
                // pwchange (spec §2): zavíratelný jen bez rozepsaného hesla
                // (kterékoli maskované pole) a bez běžící změny — stejná
                // „no accidental dismissal while a password is typed"
                // úvaha jako ConnectionDialog výše; pravdivostní tabulka
                // je pwchange::esc_closable.
                connections_ui::ModalState::ChangeServerPassword {
                    new1, new2, admin_password, running, ..
                } => {
                    let empty = new1.read(cx).text().is_empty()
                        && new2.read(cx).text().is_empty()
                        && admin_password.read(cx).text().is_empty();
                    pwchange::esc_closable(empty, *running)
                }
                // §W3.2: nothing is dispatched until the button is
                // clicked, and nothing secret is typed here — so Esc
                // cancels freely, BUT never mid-init (`running`), the same
                // reasoning as `BackupRestore`'s `!session.is_running()`
                // above. The truth table is
                // `connections_ui::workspace_confirm_esc_closable`.
                connections_ui::ModalState::WorkspaceConfirm { running, .. } => {
                    connections_ui::workspace_confirm_esc_closable(*running)
                }
                // T9: nothing secret is typed into either scripts dialog
                // and nothing is dispatched until its button is clicked —
                // so Esc cancels freely, BUT never while the background
                // op is in flight, the same reasoning as `BackupRestore`'s
                // `!session.is_running()`. Truth table:
                // `connections_ui::script_modal_esc_closable`.
                connections_ui::ModalState::ScriptName { running, .. }
                | connections_ui::ModalState::ScriptDeleteConfirm { running, .. } => {
                    connections_ui::script_modal_esc_closable(*running)
                }
                // `WorkspaceMissing` never reaches this match — the
                // `modal_is_blocking` guard above returns first.
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

    /// Ctrl+Shift+F / the „Formátovat" button.
    ///
    /// Dialect comes from the ACTIVE connection's engine, so the same text
    /// formats as T-SQL against MSSQL and as Postgres against pg — `[a b]`
    /// is one identifier in the first and a subscript in the second. With no
    /// active connection there is nothing to be right about, so it refuses
    /// rather than guessing a dialect and reflowing the user's SQL by the
    /// wrong rules.
    fn on_format_sql(&mut self, _: &FormatSql, _window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let Some(engine) = self.active_engine() else {
            self.status = "error: formátování potřebuje aktivní připojení (určuje dialekt)".into();
            cx.notify();
            return;
        };
        let dialect = sql_dialect(engine);
        let changed = rewrite_buffer_in_place(self, cx, |sql| dbc_core::format::format_sql(sql, dialect));
        self.status =
            if changed { "SQL naformátováno".into() } else { "SQL už je naformátované".into() };
        cx.notify();
    }

    /// Flips the schema tree between „by schema" and „by object kind" and
    /// persists the choice.
    ///
    /// No fetch: both shapes are the SAME `SchemaSnapshot` flattened
    /// differently, so this is a re-render, not a round trip — which is why
    /// it can be a one-click header icon rather than something guarded
    /// behind a confirm.
    ///
    /// The tree does not own this value. It is global config, so the flip,
    /// the save and the push back into the tree all happen here; letting the
    /// tree flip its own copy would let the two disagree whenever the save
    /// is refused.
    fn toggle_tree_grouping(&mut self, cx: &mut Context<Self>) {
        let next = match self.config.tree_grouping {
            dbc_state::TreeGrouping::Schema => dbc_state::TreeGrouping::Kind,
            dbc_state::TreeGrouping::Kind => dbc_state::TreeGrouping::Schema,
        };
        self.config.tree_grouping = next;
        self.tree.update(cx, |t, cx| t.set_grouping(next, cx));
        self.status = match next {
            dbc_state::TreeGrouping::Schema => "strom: podle schémat".to_string(),
            dbc_state::TreeGrouping::Kind => "strom: podle typu objektu".to_string(),
        };
        // Same posture as `set_theme` and `end_sidebar_resize`: the guard
        // gates the WRITE only, the session already switched above, and a
        // refusal leaves its own status in place.
        if let Some(guard) = self.guard_corrupt_config(cx) {
            if let Err(e) = self.config.save(&self.config_path, &guard) {
                self.status = format!("error: režim stromu se nepodařilo uložit: {e}");
            }
        }
        cx.notify();
    }

    /// Ends a splitter drag and persists the result.
    ///
    /// Persisting happens HERE — at the drag END — and never in the move
    /// handler. A move fires per mouse event, so saving there would rewrite
    /// `config.toml` (tmp + `sync_all` + rename, over every connection and
    /// favourite) dozens of times per second for a single gesture. Same
    /// contract as `grid.rs`'s column widths, which emit `ViewChanged` only
    /// on `on_mouse_up`.
    ///
    /// The equality check is not an optimisation: a plain CLICK on the
    /// splitter — no movement at all — is a complete down/up pair, so
    /// without it every stray click would write `config.toml`.
    fn end_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        self.sidebar_resizing = None;
        let width = clamp_sidebar_width(self.sidebar_width) as u16;
        if self.config.sidebar_width == Some(width) {
            cx.notify();
            return;
        }
        self.config.sidebar_width = Some(width);
        // Same posture as `set_theme`: the guard gates the WRITE only. The
        // in-session width already changed and stays changed — a save
        // failure degrades to session-only plus the guard's own status.
        if let Some(guard) = self.guard_corrupt_config(cx) {
            if let Err(e) = self.config.save(&self.config_path, &guard) {
                self.status = format!("error: šířku panelu se nepodařilo uložit: {e}");
            }
        }
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

        let monitor_available = self.active_engine().is_some_and(monitor::monitor_available);
        let admin_entry = admin_panel::admin_entry_state(self.active_engine(), self.active_read_only());
        // G14 T11: absent-not-disabled, same posture as `monitor_available`
        // — listed only while the active tab is a Grid (design §2.1's entry
        // gate: a chart needs an existing result buffer to draw from).
        let chart_available =
            matches!(self.tabs.active(), Some(ResultTab { content: TabContent::Grid { .. }, .. }));
        // App-wide master password UX design §2/§3: "Odemknout trezor" only
        // when there's actually a vault file to unlock and it's currently
        // locked; "Zamknout trezor" only while unlocked.
        let vault_unlockable = self.vault.is_none() && Vault::exists(&self.vault_path);
        let vault_lockable = self.vault.is_some();
        palette::rank_items(
            query,
            &tables,
            &history,
            &connections,
            monitor_available,
            admin_entry,
            30,
            self.active_connection_id.is_some(),
            chart_available,
            vault_unlockable,
            vault_lockable,
        )
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
                //
                // Workspace T8: LEGACY CLOBBER SITE 1 of 2. Until now this
                // replaced the editor's text unconditionally; with a bound
                // script that silently destroyed the user's unsaved file
                // edits. It now routes through THE guard (Part S §5.5).
                // Unbound ad-hoc text is deliberately as (un)guarded as it
                // has always been — zero behavioural regression surface.
                self.editor_load_guarded(PendingScriptAction::LoadText { sql }, cx);
                // Focus the editor only if the load actually happened. When
                // the guard parked it, the discard prompt is what owns
                // focus (`modal_needs_focus`) and stealing it back would
                // strand the prompt un-dismissable by keyboard.
                if self.discard_confirm.is_none() {
                    let editor_focus = self.sql.focus_handle(cx);
                    window.focus(&editor_focus, cx);
                }
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
                PaletteAction::RunQuery => self.run_query(false, window, cx),
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
                    // Exactly `on_tree_event`'s `TreeEvent::RefreshRequested`
                    // arm (sidebar rework: the ACTIVE slot).
                    if let Some(id) = self.active_connection_id.clone() {
                        if let Some(db) = self.effective_database() {
                            self.start_schema_slot_fetch(id, db, cx);
                        }
                    } else if self.conn_url.is_some() {
                        self.start_schema_slot_fetch(
                            CLI_CONN_IDENTITY.to_string(),
                            String::new(),
                            cx,
                        );
                    } else {
                        self.refresh_tree_context(cx);
                    }
                }
                PaletteAction::OpenMonitor => self.open_monitor_tab(cx),
                PaletteAction::ShowErDiagram => {
                    let target = self.resolve_er_diagram_schema(cx);
                    match target {
                        Some(schema) => self.open_er_diagram(schema, cx),
                        None => {
                            self.status =
                                "Vyberte schéma ve stromu (klikněte na ikonu vedle schématu)"
                                    .to_string();
                            cx.notify();
                        }
                    }
                }
                PaletteAction::ShowLog => self.open_log_tab(cx),
                PaletteAction::OpenCompare => self.open_compare_dialog(cx),
                PaletteAction::BackupDatabase => {
                    if let Some(id) = self.active_connection_id.clone() {
                        self.open_backup_dialog(id, window, cx);
                    }
                }
                PaletteAction::RestoreDatabase => {
                    if let Some(id) = self.active_connection_id.clone() {
                        self.open_restore_dialog(id, window, cx);
                    }
                }
                PaletteAction::RunSqlFile => self.start_script_pick(false, cx),
                PaletteAction::RunSqlFolder => self.start_script_pick(true, cx),
                // Part S §8: the same entry point Ctrl+S and the caption
                // strip's „Uložit" use — no second save path.
                PaletteAction::SaveScript => self.on_save_script(&SaveScript, window, cx),
                PaletteAction::OpenServerAdmin => self.open_admin_tab(cx),
                PaletteAction::ToggleTheme => self.toggle_theme(cx),
                PaletteAction::OpenChart => self.open_chart_picker(None, cx),
                PaletteAction::UnlockVault => self.open_unlock_vault_prompt(window, cx),
                PaletteAction::LockVault => self.lock_vault(cx),
            },
        }
        cx.notify();
    }

    /// G14 T10: single write-through path for both toggle surfaces (design
    /// §1.5) — the settings-modal radio buttons AND the palette's "Přepnout
    /// motiv" action both call this. A config-save failure still switches
    /// the SESSION theme (the live switch must never be hostage to a
    /// read-only disk) — the error is surfaced in the status line instead,
    /// same "save failure degrades to session-only + status message" shape
    /// as `on_tree_event`'s `ToggleFavourite` arm above.
    fn set_theme(&mut self, mode: dbc_state::ThemeMode, cx: &mut Context<Self>) {
        if self.config.theme != mode {
            self.config.theme = mode;
            // T7 review MAJOR-1: the SAME corrupt-config gate every other
            // `config.toml` writer passes (`finish_save`, the two ★
            // toggles, the scripts-dir pair). Without it, a `config.toml`
            // that failed to parse at startup — reported only as a status
            // string, with the UI fully usable and `self.config` replaced
            // by `AppConfig::default()` — is destroyed by a REFLEX: one
            // click on the light-theme radio (or the palette's „Přepnout
            // motiv") writes a file holding `theme = "light"` and nothing
            // else over every connection, favourite and vault key id.
            // `AppConfig::save` is tmp + `sync_all` + rename with no
            // backup; `guard_corrupt_config` is the only thing that keeps
            // the original, as `config.toml.corrupt-bak`.
            //
            // It gates the WRITE only. The session switch below still runs
            // unconditionally — "a save failure degrades to session-only +
            // a status message" is this function's documented posture, and
            // a refusal to persist is a save failure like any other.
            // `guard_corrupt_config` sets its own status when it refuses,
            // so nothing here overwrites it.
            if let Some(guard) = self.guard_corrupt_config(cx) {
                self.status = match self.config.save(&self.config_path, &guard) {
                    Ok(()) => format!(
                        "motiv: {}",
                        match mode {
                            dbc_state::ThemeMode::Dark => "tmavý",
                            dbc_state::ThemeMode::Light => "světlý",
                        }
                    ),
                    Err(e) => format!("error: motiv se nepodařilo uložit ({e})"),
                };
            }
        }
        cx.set_global(theme::Theme::from_mode(mode));
        // Re-highlight the editor with the new syntax palette (Task 6's
        // spans were computed against the old one — a switch must re-kick
        // it, otherwise the SQL editor keeps stale colors until the next
        // keystroke), then repaint everything.
        self.sql.update(cx, |sql, cx| sql.kick_highlight(cx));
        cx.refresh_windows(); // NOT cx.refresh() — doesn't exist at rev 907ed09
        cx.notify();
    }

    /// G14 T10: dark<->light, dispatched from both the palette action and
    /// the settings-modal gear/topbar entry point.
    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        let next = match self.config.theme {
            dbc_state::ThemeMode::Dark => dbc_state::ThemeMode::Light,
            dbc_state::ThemeMode::Light => dbc_state::ThemeMode::Dark,
        };
        self.set_theme(next, cx);
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

        let theme = *cx.theme();
        let mut list = div().id("palette-list").flex().flex_col().flex_1().overflow_hidden();
        for (ix, item) in items.into_iter().enumerate() {
            let label = palette::display_label(&item);
            let is_selected = ix == selected;
            let bg = if is_selected { theme.bg_selected } else { theme.bg_panel };
            list = list.child(
                div()
                    .id(("palette-item", ix))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .bg(bg)
                    .text_color(theme.text_primary)
                    .hover(|s| s.bg(theme.bg_hover))
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
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_palette_up))
            .on_action(cx.listener(Self::on_palette_down))
            .on_action(cx.listener(Self::on_palette_confirm))
            .on_action(cx.listener(Self::on_palette_close))
            .child(div().px_2().py_2().border_b_1().border_color(theme.border).child(input))
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
                .bg(theme.bg_backdrop)
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    /// G6 T7 (review round 3, MAJOR 1): unconditionally closes the
    /// autocomplete popup — called from every place the ACTIVE connection's
    /// identity or schema snapshot changes underneath it
    /// (`switch_to_database`'s success arm, AND
    /// `start_schema_slot_fetch`'s active-slot success arm), not just
    /// from `on_ac_escape`. Without this, `refresh_autocomplete`'s
    /// text/cursor/focus-based lazy-diff has no signal that the SCHEMA
    /// changed (the SQL editor's own text/cursor/focus are untouched by a
    /// connection switch), so a popup opened against the OLD connection's
    /// schema could survive the switch and, if accepted, insert a
    /// table/column name from the wrong database. No-op (and no
    /// `cx.notify()`) when already closed, so call sites don't need their
    /// own guard.
    pub(crate) fn close_autocomplete(&mut self, cx: &mut Context<Self>) {
        if self.autocomplete.is_none() {
            return;
        }
        self.autocomplete = None;
        self.sql.update(cx, |s, _| s.set_autocomplete_active(false));
        cx.notify();
    }

    /// G6 T7: force-trigger (`Ctrl+Space`, global binding, design §2) — opens
    /// the popup with the FULL candidate set (empty prefix, bypassing
    /// whatever partial identifier/qualifier the cursor happens to sit in),
    /// regardless of the typing-trigger gating `refresh_autocomplete` does.
    /// No-ops while a modal is open, same posture as `on_open_palette`.
    fn on_open_autocomplete(&mut self, _: &OpenAutocomplete, _window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let text = self.sql.read(cx).text();
        let cursor = self.sql.read(cx).cursor();
        let suppressed = self.sql.read(cx).cursor_in_suppressed_span();
        let snapshot = self.tree.read(cx).snapshot();
        let had_snapshot = snapshot.is_some();
        let candidates = autocomplete::candidates(&text, cursor, snapshot, true, suppressed);
        // Ctrl+Space is an EXPLICIT ask, so silence is the wrong answer:
        // every reason the popup stays shut is invisible from the outside
        // (cursor inside a string, no schema loaded yet), which is exactly
        // how „autocomplete is broken" gets reported for something that is
        // working as designed. The typing trigger stays silent — a status
        // line rewritten on every keystroke would be noise.
        if candidates.is_empty() {
            self.status = if suppressed {
                "napovídání: kurzor je v řetězci nebo komentáři".into()
            } else if !had_snapshot {
                "napovídání: schéma není načtené — rozbalte databázi v panelu vlevo".into()
            } else {
                "napovídání: nic nevyhovuje".into()
            };
        }
        self.autocomplete =
            (!candidates.is_empty()).then(|| AutocompleteState { candidates, selected: 0 });
        // Keep the lazy-diff cache in sync so the SAME render's
        // `refresh_autocomplete` (which runs before this popup is drawn,
        // but AFTER this handler since actions dispatch before re-render)
        // doesn't immediately reconsider — text/cursor are unchanged by a
        // force-trigger, so this is a no-op compare either way, but staying
        // explicit here avoids relying on that coincidence.
        self.last_ac_text = text;
        self.last_ac_cursor = cursor;
        cx.notify();
    }

    /// G6 T7: the typing-trigger lazy-diff recompute (design §2 / plan T7
    /// grounding — same idiom as `history_search`/`last_history_query`),
    /// called at the top of every `Render::render`, BEFORE the popup is
    /// drawn. Also the one place responsible for every "close the popup"
    /// condition that isn't `Escape`/accept: losing focus, a params (or any
    /// other) modal opening, the cursor leaving a completable position
    /// (typing a space/most punctuation, arrow-key/mouse cursor movement
    /// that isn't popup navigation) — all of these change what
    /// `self.sql`'s focus/text/cursor look like, which this function reads
    /// fresh every render.
    fn refresh_autocomplete(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.sql.focus_handle(cx).is_focused(window) || self.modal.is_some() {
            self.autocomplete = None;
            return;
        }

        let text = self.sql.read(cx).text();
        let cursor = self.sql.read(cx).cursor();
        if text == self.last_ac_text && cursor == self.last_ac_cursor {
            return;
        }
        self.last_ac_text = text.clone();
        self.last_ac_cursor = cursor;

        let suppressed = self.sql.read(cx).cursor_in_suppressed_span();
        let ctx = autocomplete::cursor_context(&text, cursor);
        if suppressed || (ctx.prefix.is_empty() && ctx.qualifier.is_none()) {
            self.autocomplete = None;
            return;
        }

        let snapshot = self.tree.read(cx).snapshot();
        let candidates = autocomplete::candidates(&text, cursor, snapshot, false, suppressed);
        self.autocomplete =
            (!candidates.is_empty()).then(|| AutocompleteState { candidates, selected: 0 });
    }

    fn on_ac_up(&mut self, _: &sql_input::Up, _window: &mut Window, cx: &mut Context<Self>) {
        // Review round 3, BLOCKER follow-up: every one of these
        // wrapper-div handlers only runs at all when `SqlInput`'s own
        // handler propagated (i.e. `autocomplete_active` was true at
        // dispatch time), but the defensive `self.autocomplete` re-check
        // below can still fail on a same-frame race (plan T7 step 3, item
        // 5) — when it does, this handler must `cx.propagate()`, not
        // silently swallow the keystroke (see `on_ac_escape`'s doc comment
        // for why this matters most for `Escape`; applied uniformly here
        // for consistency/hygiene).
        if !autocomplete_handles_action(self.autocomplete.is_some()) {
            cx.propagate();
            return;
        }
        let ac = self.autocomplete.as_mut().unwrap();
        ac.selected = move_selection(ac.selected, ac.candidates.len(), -1);
        cx.notify();
    }

    fn on_ac_down(&mut self, _: &sql_input::Down, _window: &mut Window, cx: &mut Context<Self>) {
        if !autocomplete_handles_action(self.autocomplete.is_some()) {
            cx.propagate();
            return;
        }
        let ac = self.autocomplete.as_mut().unwrap();
        ac.selected = move_selection(ac.selected, ac.candidates.len(), 1);
        cx.notify();
    }

    /// Shared accept path for both `Newline` (Enter) and `Tab` — see
    /// `on_ac_confirm`/`on_ac_confirm_tab`. Inserts the selected candidate's
    /// `text` via `SqlInput::accept_completion`, using `completion_edit`'s
    /// pure range computation for the prefix length (design §2's "Enter/Tab
    /// accept").
    fn accept_selected_completion(&mut self, cx: &mut Context<Self>) {
        // Callers (`on_ac_confirm`/`on_ac_confirm_tab`) already guard on
        // `autocomplete_handles_action` before calling this, but `ac`'s
        // `selected` index could in principle be stale (e.g. a candidate
        // list that shrank between the last nav and this accept) — the
        // `.get` below stays defensive rather than assuming it's always
        // in-bounds.
        let Some(ac) = &self.autocomplete else { return };
        let Some(candidate) = ac.candidates.get(ac.selected) else { return };
        let insert = candidate.text.clone();
        // RE-VERIFY: the range is no longer computed HERE and handed over.
        // `accept_completion` derives it from its own buffer through the
        // same `autocomplete::cursor_context` rail `completion_edit` uses,
        // so the span it deletes is bounded by one identifier prefix by
        // construction and a caller cannot widen it into a buffer clobber.
        self.sql.update(cx, |s, cx| s.accept_completion(&insert, cx));
        self.autocomplete = None;
        self.sql.update(cx, |s, _| s.set_autocomplete_active(false));
        // Sync the lazy-diff cache to the post-accept text/cursor so the
        // very next render's `refresh_autocomplete` doesn't compare against
        // stale pre-accept values.
        self.last_ac_text = self.sql.read(cx).text();
        self.last_ac_cursor = self.sql.read(cx).cursor();
        cx.notify();
    }

    fn on_ac_confirm(&mut self, _: &sql_input::Newline, _window: &mut Window, cx: &mut Context<Self>) {
        if !autocomplete_handles_action(self.autocomplete.is_some()) {
            cx.propagate();
            return;
        }
        self.accept_selected_completion(cx);
    }

    /// Review round 3, hygiene follow-up to the BLOCKER below: `Tab` is
    /// unconditionally propagated by `SqlInput::on_tab` regardless of
    /// `autocomplete_active` (unlike `Up`/`Down`/`Newline`, which `SqlInput`
    /// itself only propagates while the popup is open), so this handler
    /// runs on EVERY Tab press with editor focus, not just while the popup
    /// is open. Currently harmless (no other ancestor binds `Tab`, so a
    /// swallowed propagate has no observable effect), but propagating here
    /// keeps that from becoming a silent trap if a `Tab` binding is ever
    /// added elsewhere.
    fn on_ac_confirm_tab(&mut self, _: &sql_input::Tab, _window: &mut Window, cx: &mut Context<Self>) {
        if !autocomplete_handles_action(self.autocomplete.is_some()) {
            cx.propagate();
            return;
        }
        self.accept_selected_completion(cx);
    }

    /// Review round 3, BLOCKER: `SqlInput::on_escape` ALWAYS propagates
    /// (open or closed popup alike — see its own doc comment), specifically
    /// so `Escape` can reach the global `"escape" -> CancelQuery` binding
    /// when the popup is closed. This handler sits directly on that bubble
    /// path (bound on the SAME wrapper div, SAME action type), so it must
    /// mirror that and propagate too whenever it doesn't actually close a
    /// popup — the previous version returned early WITHOUT propagating,
    /// which silently consumed the action and made `CancelQuery`
    /// unreachable via Escape for as long as the SQL editor had focus (a
    /// user-visible regression: no way to cancel a running query from the
    /// keyboard).
    fn on_ac_escape(&mut self, _: &sql_input::Escape, _window: &mut Window, cx: &mut Context<Self>) {
        if !autocomplete_handles_action(self.autocomplete.is_some()) {
            cx.propagate();
            return;
        }
        self.close_autocomplete(cx);
    }

    /// G6 T7: floating popup, anchored just below the cursor via
    /// `SqlInput::cursor_screen_bounds()` (design §2). `uniform_list` —
    /// same mechanism `schema_tree.rs`/`history_panel.rs`/`grid.rs` use for
    /// their scrollable rows — capped to 8 visible rows (design: "Max 8
    /// visible rows, scrollable"); `autocomplete::candidates` itself already
    /// caps the underlying set at 20. `None` (renders nothing) when closed,
    /// when there's no live cursor position to anchor to (e.g. scrolled out
    /// of view — `cursor_screen_bounds`'s own documented degradation), or
    /// while a modal is open (belt and suspenders alongside
    /// `refresh_autocomplete`'s own guard).
    fn render_autocomplete_popup(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.modal.is_some() {
            return None;
        }
        let ac = self.autocomplete.as_ref()?;
        let candidates = ac.candidates.clone();
        let selected = ac.selected;
        let bounds = self.sql.read(cx).cursor_screen_bounds()?;
        let theme = *cx.theme();

        const ROW_H: gpui::Pixels = px(22.);
        let visible_rows = candidates.len().min(8);

        let list = uniform_list(
            "autocomplete-list",
            candidates.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                let theme = *cx.theme();
                let mut items = Vec::with_capacity(range.len());
                for ix in range {
                    let c = candidates[ix].clone();
                    let is_selected = ix == selected;
                    let bg = if is_selected { theme.bg_selected } else { theme.bg_panel };
                    let (kind_label, kind_color) = match c.kind {
                        autocomplete::CandidateKind::Keyword => ("K", theme.accent),
                        autocomplete::CandidateKind::Table => ("T", theme.success),
                        autocomplete::CandidateKind::Column => ("C", theme.warn),
                    };
                    let label = c.label.clone();
                    items.push(
                        div()
                            .id(("ac-item", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .h(ROW_H)
                            .px_2()
                            .cursor_pointer()
                            .bg(bg)
                            .text_color(theme.text_primary)
                            .hover(|s| s.bg(theme.bg_hover))
                            .child(div().w(px(10.)).text_size(px(11.)).text_color(kind_color).child(kind_label))
                            .child(div().flex_1().overflow_hidden().child(label))
                            .on_click(cx.listener(move |view, _, _window, cx| {
                                if let Some(a) = &mut view.autocomplete {
                                    a.selected = ix;
                                }
                                view.accept_selected_completion(cx);
                            })),
                    );
                }
                items
            }),
        )
        .h(ROW_H * visible_rows);

        Some(
            div()
                .absolute()
                .left(bounds.left())
                .top(bounds.top() + bounds.size.height)
                .w(px(280.))
                .max_h(ROW_H * 8)
                .bg(theme.bg_panel)
                .border_1()
                .border_color(theme.border)
                .rounded_md()
                .occlude()
                .child(list)
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
        if self.active_connection_id.is_some() {
            self.resolve_active().map(ActiveConn::into_spec)
        } else {
            self.conn_url.clone().map(ConnectSpec::Url)
        }
    }

    /// The single site where "the database the app talks to" is decided —
    /// see `ActiveConn`'s doc comment for the invariant. `None` = no active
    /// saved connection OR the connection was deleted; the CLI-arg URL path
    /// is handled by callers as today.
    fn resolve_active(&self) -> Option<ActiveConn> {
        let id = self.active_connection_id.as_deref()?;
        resolve_active_from(&self.config, self.vault.as_ref(), id, self.active_database.as_deref())
    }

    // -----------------------------------------------------------------
    // Sidebar rework (T5): the (connection, database) context switch and
    // the per-slot sidebar fetches.
    // -----------------------------------------------------------------

    /// Design §2.2: THE context switch. `db == None` targets the saved
    /// default database (dropdown/palette/tree-connection-row semantics);
    /// `Some(db)` a tree-selected one. Success is the ONLY writer of
    /// `active_database`. A failed test_connect leaves the previous
    /// context untouched — same contract as the pre-rework switch.
    ///
    /// `follow_up` (T5 review MAJOR 2): the one-shot action THIS dispatch
    /// replays on success — owned by the dispatch (captured into the spawn
    /// closure), never shared state, so a superseding switch retires it via
    /// the `switch_generation` guard and a vault/confirm cancel drops it
    /// with its pending payload.
    pub(crate) fn switch_to_database(
        &mut self,
        id: &str,
        db: Option<String>,
        follow_up: Option<PendingTreeAction>,
        cx: &mut Context<Self>,
    ) {
        // T5 review MINOR 3 (house convention, cf. `run_query_with`):
        // refuse outright under ANY open overlay — in particular an open
        // discard-confirm from ANOTHER flow must not let the switch skip
        // the dirty-admin confirmation below.
        if switch_blocked_by_overlay(
            self.modal.is_some(),
            self.apply_dialog.is_some(),
            self.discard_confirm.is_some(),
        ) {
            return;
        }
        self.cancel_active_backup_if_running();
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else { return };
        // Canonical spelling: explicitly picking the default == None
        // (pinned by `db_choice_normalizes_default_to_none` — the whole
        // legacy-store-key contract rests on this line).
        let db = db.filter(|d| d != &cfg.database);
        let target_identity = conn_identity_for(id, db.as_deref().unwrap_or(&cfg.database));
        if target_identity == self.current_conn_identity() {
            // Already there — still worth a re-validate? No: match the old
            // dropdown behaviour (clicking the active item re-tested)?
            // Deliberate change: a no-op switch is a no-op; the ⟳ button
            // owns re-validation. Keeps double-click idempotent. A
            // follow-up (unreachable from the cross-context arm, which
            // implies a different identity — kept defensively) targets the
            // already-active context, so it runs directly.
            if let Some(PendingTreeAction::OpenPreview { schema, table }) = follow_up {
                self.open_table_preview(schema, table, cx);
            }
            return;
        }
        // Resolved deviation 11 (risk list "release-note + confirm"): a
        // context switch makes a dirty admin tab's staged edits
        // permanently inapplicable (the identity guard will refuse them
        // and the next admin open Replaces the tab) — confirm first. Dirty
        // sandbox GRID edits deliberately get NO prompt: they are not
        // dropped — the tab stays, the apply bar dims via the identity
        // guard, and switching back to the same (conn, db) re-enables it.
        // (The entry guard above already refused a foreign open confirm;
        // the Yes-arm re-enters with `discard_confirm` taken.)
        if let Some(count) = self.dirty_admin_change_count(cx) {
            self.discard_confirm = Some(DiscardConfirmState {
                change_count: count,
                action: PendingDiscard::SwitchDatabase {
                    conn_id: id.to_string(),
                    db,
                    follow_up,
                },
            });
            self.modal_needs_focus = true;
            cx.notify();
            return;
        }
        // Vault gate (design §1.3/§4.4) — same three-boolean predicate as
        // the dropdown's.
        let needs_secret = !connections_ui::engine_is_file_based(cfg.engine);
        if connections_ui::connect_needs_vault_prompt(
            needs_secret,
            self.vault.is_some(),
            Vault::exists(&self.vault_path),
        ) {
            self.open_vault_prompt(
                connections_ui::PendingAfterUnlock::SwitchDatabase {
                    conn_id: id.to_string(),
                    db,
                    follow_up,
                },
                cx,
            );
            return;
        }
        let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
        let engine_lbl = connections_ui::engine_label(cfg.engine);
        let effective = db.clone().unwrap_or_else(|| cfg.database.clone());
        let spec = spec_for_database(&cfg, &effective, secret);
        let target_id = cfg.id.clone();
        // pwchange (spec §1): detekce v failure armu níže potřebuje engine
        // + přihlašovací jméno z configu.
        let engine = cfg.engine;
        let conn_user = cfg.user.clone();
        self.dropdown_open = false;
        self.status = "connecting…".into();
        self.switch_generation += 1;
        let my_generation = self.switch_generation;
        cx.notify();
        let rx = self.runner.test_connect(spec);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                if view.switch_generation != my_generation {
                    return; // superseded — last-dispatched wins
                }
                match result {
                    Ok(Ok(())) => {
                        view.status = format!("Připojeno ({engine_lbl})");
                        view.active_connection_id = Some(target_id.clone());
                        view.active_database = db.clone();
                        view.conn_url = None;
                        // G6 T7 review round 3, MAJOR 1 (carried forward):
                        // close any open autocomplete at the moment the
                        // identity changes — don't wait for the schema
                        // fetch below to land.
                        view.close_autocomplete(cx);
                        view.refresh_tree_context(cx);
                        view.start_schema_slot_fetch(target_id.clone(), effective.clone(), cx);
                        // T5 review MAJOR 2: the follow-up is THIS
                        // dispatch's own (closure-captured) — a superseded
                        // dispatch never reaches here (generation guard
                        // above), so a stale action can never replay
                        // against the wrong database.
                        if let Some(PendingTreeAction::OpenPreview { schema, table }) = follow_up {
                            view.open_table_preview(schema, table, cx);
                        }
                    }
                    // Failure arms: the closure-owned follow-up simply
                    // drops with them — nothing shared to clear.
                    Ok(Err(e)) => {
                        view.status = format!("error: {e}");
                        // pwchange (spec §1): nabídka změny hesla — nikdy
                        // auto-změna, dialog má Zrušit/Esc; při jiném
                        // otevřeném modalu zůstane jen status výše
                        // (single-modal invariant v open_pw_change_dialog).
                        if let Some(kind) = pwchange::detect(engine, &e) {
                            view.open_pw_change_dialog(
                                target_id.clone(),
                                conn_user.clone(),
                                kind,
                                db.clone(),
                                cx,
                            );
                        }
                    }
                    Err(_) => {
                        view.status = "error: connect zrušen".into();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Some(change_count) when an open admin tab is stamped with the
    /// CURRENT identity and has staged edits (roles/memberships/matrix).
    fn dirty_admin_change_count(&self, cx: &Context<Self>) -> Option<usize> {
        let current = self.current_conn_identity();
        self.tabs.iter().find_map(|t| match &t.content {
            TabContent::Admin { view } => {
                let p = view.read(cx);
                let n = p.change_count();
                (p.conn_identity() == current && n > 0).then_some(n)
            }
            _ => None,
        })
    }

    /// Opens the master-password prompt from a cx-only context (tree
    /// subscribe callbacks have no `&mut Window`) — deferred focus lands on
    /// the prompt's own input via the render-top hook (see
    /// `AppView::render`'s `modal_needs_focus` block).
    /// pwchange (spec §1): otevírá nabídku změny hesla po detekovaném
    /// connect selhání. Nikdy nevytlačí existující modal (single-modal
    /// invariant — v tom případě zůstane jen chybový status, který
    /// volající už nastavil). Deferred-focus vzor `open_vault_prompt`
    /// níže: z async callbacku není `&mut Window`, fokus dodá render.
    pub(crate) fn open_pw_change_dialog(
        &mut self,
        conn_id: String,
        user: String,
        kind: pwchange::PwChangeKind,
        retry_db: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            return;
        }
        let new1 = cx.new(|cx| connections_ui::TextField::form_field(cx, "nové heslo", true));
        let new2 = cx.new(|cx| connections_ui::TextField::form_field(cx, "nové heslo znovu", true));
        let admin_user = cx.new(|cx| connections_ui::TextField::form_field(cx, "postgres", false));
        let admin_password =
            cx.new(|cx| connections_ui::TextField::form_field(cx, "heslo administrátora", true));
        self.modal = Some(connections_ui::ModalState::ChangeServerPassword {
            conn_id,
            kind,
            user,
            retry_db,
            new1,
            new2,
            admin_user,
            admin_password,
            error: None,
            running: false,
        });
        self.dropdown_open = false;
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// pwchange (spec §3): Enter/„Změnit heslo". Self-guarding (validace
    /// nahoře, chyby zůstávají v dialogu) — `on_modal_confirm` nepřidává
    /// žádnou autoritu. MSSQL: staré heslo Z TREZORU (18488 implikuje, že
    /// bylo správné ⇒ trezor byl při connectu odemčený), změna driver-level
    /// přes sankcionovaný `QueryRunner::change_mssql_password`. PG: admin
    /// credentials z dialogu, existující `run_write_transaction`, zápis do
    /// historie kind "admin" (display_sql, nikdy exec_sql). Po úspěchu
    /// OBOU větví: `finish_pw_change_success` (trezor + retry).
    fn confirm_pw_change(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ChangeServerPassword {
            conn_id,
            kind,
            retry_db,
            new1,
            new2,
            admin_user,
            admin_password,
            running,
            ..
        }) = self.modal.clone()
        else {
            return;
        };
        if running {
            return;
        }
        let new1_text = zeroize::Zeroizing::new(new1.read(cx).text());
        let new2_text = zeroize::Zeroizing::new(new2.read(cx).text());
        if let Err(m) = pwchange::validate_new_password(&new1_text, &new2_text) {
            self.pw_change_set_error(m, cx);
            return;
        }
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == conn_id).cloned() else {
            self.pw_change_set_error("připojení nenalezeno".to_string(), cx);
            return;
        };
        if self.vault.is_none() {
            // Defenzivní: detekce implikuje odemčený trezor (spec §4);
            // kdyby přesto nebyl, neměníme heslo, které bychom neuměli
            // hned uložit.
            self.pw_change_set_error(
                "trezor není odemčený — odemkněte ho a zkuste znovu".to_string(),
                cx,
            );
            return;
        }
        match kind {
            pwchange::PwChangeKind::MssqlMustChange => {
                let Some(old) = self.vault.as_ref().and_then(|v| v.get_secret(&conn_id)) else {
                    self.pw_change_set_error(
                        "současné heslo není v trezoru — uložte ho v dialogu připojení".to_string(),
                        cx,
                    );
                    return;
                };
                if let Some(connections_ui::ModalState::ChangeServerPassword {
                    running, error, ..
                }) = &mut self.modal
                {
                    *running = true;
                    *error = None;
                }
                cx.notify();
                let rx = self.runner.change_mssql_password(
                    Box::new(cfg),
                    zeroize::Zeroizing::new(old),
                    zeroize::Zeroizing::new(new1_text.to_string()),
                );
                let new_password = zeroize::Zeroizing::new(new1_text.to_string());
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        match result {
                            Ok(Ok(())) => view.finish_pw_change_success(
                                &conn_id,
                                &new_password,
                                retry_db.clone(),
                                cx,
                            ),
                            Ok(Err(e)) => view.pw_change_set_error(e.to_string(), cx),
                            Err(_) => {
                                view.pw_change_set_error("změna hesla zrušena".to_string(), cx)
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            pwchange::PwChangeKind::PgMaybeExpired => {
                let admin_user_text = admin_user.read(cx).text();
                if let Err(m) = pwchange::validate_pg_admin(&admin_user_text) {
                    self.pw_change_set_error(m, cx);
                    return;
                }
                let admin_password_text = zeroize::Zeroizing::new(admin_password.read(cx).text());
                let stmt = admin_sql::alter_password_rescue_pg(&cfg.user, &new1_text);
                let sql_text = stmt.display_sql.clone();
                let mut rescue_cfg = cfg.clone();
                rescue_cfg.user = admin_user_text;
                // Server-side by default_transaction_read_only ALTER stejně
                // odmítl; explicitně potvrzená credential operace, ne zápis
                // dat (spec §3).
                rescue_cfg.read_only = false;
                if let Some(db) = &retry_db {
                    rescue_cfg.database = db.clone();
                }
                if let Some(connections_ui::ModalState::ChangeServerPassword {
                    running, error, ..
                }) = &mut self.modal
                {
                    *running = true;
                    *error = None;
                }
                cx.notify();
                let history_conn_name = cfg.name.clone();
                let history_started_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let started = std::time::Instant::now();
                let spec = ConnectSpec::Config {
                    cfg: Box::new(rescue_cfg),
                    secret: Some(admin_password_text.to_string()),
                };
                let rx = self.runner.run_write_transaction(spec, vec![stmt], Some(60));
                let new_password = zeroize::Zeroizing::new(new1_text.to_string());
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        match result {
                            Ok(Ok(_affected)) => {
                                let elapsed_ms = started.elapsed().as_millis() as i64;
                                // ALTER ROLE nemá smysluplný affected count
                                // (drive_write_sequence vrací 0) → None.
                                view.record_history_with_kind(
                                    &sql_text,
                                    &history_conn_name,
                                    history_started_at,
                                    Some(elapsed_ms),
                                    None,
                                    None,
                                    "admin",
                                    cx,
                                );
                                view.finish_pw_change_success(
                                    &conn_id,
                                    &new_password,
                                    retry_db.clone(),
                                    cx,
                                );
                            }
                            Ok(Err(e)) => view.pw_change_set_error(e.to_string(), cx),
                            Err(_) => {
                                view.pw_change_set_error("změna hesla zrušena".to_string(), cx)
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    /// Společný úspěchový konec obou pwchange větví: heslo OKAMŽITĚ do
    /// trezoru (spec §4), zavřít dialog, opakovat původní přepnutí.
    /// Selhání zápisu do trezoru se NESMÍ tvářit jako selhání změny —
    /// heslo na serveru UŽ je změněné; dialog zůstává s poctivou instrukcí.
    fn finish_pw_change_success(
        &mut self,
        conn_id: &str,
        new_password: &str,
        retry_db: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let saved = match self.vault.as_mut() {
            Some(v) => v.set_secret(conn_id, new_password).map_err(|e| e.message),
            None => Err("trezor není odemčený".to_string()),
        };
        match saved {
            Ok(()) => {
                self.modal = None;
                self.status = "heslo změněno a uloženo do trezoru".to_string();
                self.switch_to_database(conn_id, retry_db, None, cx);
            }
            Err(m) => self.pw_change_set_error(
                format!(
                    "heslo na serveru ZMĚNĚNO, ale uložení do trezoru selhalo: {m} — \
                     uložte nové heslo v dialogu připojení"
                ),
                cx,
            ),
        }
    }

    /// Chyba zpět do otevřeného pwchange dialogu (a shodit `running`);
    /// když už dialog nestojí (defenzivní — Esc je při running blokovaný),
    /// spadne do statusu.
    fn pw_change_set_error(&mut self, msg: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ChangeServerPassword { error, running, .. }) =
            &mut self.modal
        {
            *error = Some(msg);
            *running = false;
        } else {
            self.status = format!("error: {msg}");
        }
        cx.notify();
    }

    fn open_vault_prompt(&mut self, pending: connections_ui::PendingAfterUnlock, cx: &mut Context<Self>) {
        let input = cx.new(|cx| connections_ui::TextField::form_field(cx, "Heslo", true));
        self.modal = Some(connections_ui::ModalState::MasterPasswordPrompt {
            input,
            error: None,
            pending,
        });
        self.dropdown_open = false;
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// Whether `(conn_id, db)` IS the active context — the CLI sentinel
    /// pair (`CLI_CONN_IDENTITY`, any db) answers for the CLI-arg session.
    fn scope_is_active(&self, conn_id: &str, db: &str) -> bool {
        if conn_id == CLI_CONN_IDENTITY {
            return self.active_connection_id.is_none() && self.conn_url.is_some();
        }
        self.active_connection_id.as_deref() == Some(conn_id)
            && self.effective_database().as_deref() == Some(db)
    }

    /// Pushes the active `(connection, database)` scope + CLI url into the
    /// tree — the sidebar's ● indicator, icon gating and favourites
    /// filtering all derive from this one push.
    fn push_active_scope_to_tree(&mut self, cx: &mut Context<Self>) {
        let scope = self.active_connection_id.as_ref().and_then(|id| {
            let cfg = self.config.connections.iter().find(|c| &c.id == id)?;
            Some(schema_tree::ActiveScope {
                conn_id: id.clone(),
                db: self.active_database.clone().unwrap_or_else(|| cfg.database.clone()),
                default_db: cfg.database.clone(),
            })
        });
        let cli = self.conn_url.clone();
        self.tree.update(cx, |t, cx| {
            t.set_active_scope(scope, cx);
            // Switch success sets conn_url = None → the CLI root
            // disappears (design §3.4).
            t.set_cli(cli, cx);
        });
    }

    /// Consolidated tree-context push (sidebar rework): favourites +
    /// read_only + admin entry + active scope, called from the slot-fetch
    /// success arm, the ★ toggle, the switch success arm, and startup.
    fn refresh_tree_context(&mut self, cx: &mut Context<Self>) {
        let favourites = self.config.favourite_objects.clone();
        let read_only = self.active_read_only();
        let admin_entry = admin_panel::admin_entry_state(self.active_engine(), read_only);
        self.tree.update(cx, |t, cx| {
            t.set_favourites(favourites, cx);
            t.set_read_only(read_only, cx);
            t.set_admin_entry(admin_entry, cx);
        });
        // The editor's keyword colouring is dialect-specific, so it rides
        // the same context refresh as everything else that depends on the
        // active connection — one place where „the context changed" is
        // acted on, rather than a second hook that could be forgotten on a
        // future switch path.
        let dialect = self.active_engine().map(sql_dialect);
        self.sql.update(cx, |input, cx| input.set_dialect(dialect, cx));
        self.tree.update(cx, |t, cx| t.set_dialect(dialect, cx));
        self.push_active_scope_to_tree(cx);
        // Workspace T7: „is there a scripts root at all" travels with the
        // rest of the tree context. Pushing it never scans by itself —
        // dispatch is `start_scripts_scan`'s job.
        let configured = self.effective_scripts_root().is_some();
        self.tree.update(cx, |t, cx| t.set_scripts_configured(configured, cx));
    }

    /// The scripts library's root for the ACTIVE context — see
    /// `scripts_root_for`. Every scan and every fs op in Tasks 8/9 starts
    /// here; there is no second resolver.
    pub(crate) fn effective_scripts_root(&self) -> Option<PathBuf> {
        scripts_root_for(self.workspace_root.as_deref(), self.config.scripts_dir.as_deref())
    }

    /// Dispatches a bounded background scan into the tree's scripts slot.
    /// A missing root is NOT an error here — the section renders its
    /// „složka skriptů není nastavena" pointer row instead (Part S §1.4),
    /// and in workspace mode `configured` is always true, so a deleted
    /// `<workspace>/scripts` surfaces honestly as the scan's own error row
    /// plus its retry click.
    ///
    /// The scan itself NEVER executes anything: `scan_scripts` is a bounded
    /// `read_dir` walk (Part S §7); opening or running a script is a
    /// separate, explicit user action.
    pub(crate) fn start_scripts_scan(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.effective_scripts_root() else {
            self.tree.update(cx, |t, cx| {
                t.set_scripts_configured(false, cx);
                t.reset_scripts(cx);
            });
            return;
        };
        self.tree.update(cx, |t, cx| t.set_scripts_configured(true, cx));
        // The generation comes from `begin_scripts_scan`'s RETURN value —
        // `ScriptsListState::Loading` is a unit variant and deliberately
        // carries no copy (see its doc comment). `finish_scripts_scan`
        // compares against the tree's own counter, so a result that lands
        // after a context swap or a newer dispatch is DROPPED, never
        // applied to the wrong root.
        let generation = self.tree.update(cx, |t, cx| t.begin_scripts_scan(cx));
        let tree = self.tree.clone();
        cx.spawn(async move |_this, cx| {
            let result =
                cx.background_spawn(async move { crate::scripts::scan_scripts(&root) }).await;
            let _ = tree.update(cx, |t, cx| t.finish_scripts_scan(generation, result, cx));
        })
        .detach();
    }

    /// Part S §2 — PROFILE mode only (the caller only renders the button
    /// there). Stores the absolute path, saves the config, rescans.
    fn start_scripts_dir_pick(&mut self, cx: &mut Context<Self>) {
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Vybrat".into()),
        });
        cx.spawn(async move |this, cx| {
            // DEVIATION from the plan snippet's `let … else { return }`: a
            // swallowed dialog failure is exactly the silence this phase
            // keeps banning. Same three arms as `start_csv_import` /
            // `start_workspace_pick`, same strings.
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "výběr zrušen".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog selhal: {e}");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |view, cx| {
                // §W8: `scripts_dir` is inert in workspace mode, so the app
                // must never WRITE it there either.
                //
                // T7 review MINOR-2, decided: this stays, but it now
                // REPORTS. Whether a swap can actually interleave between
                // the native dialog closing and this continuation is not
                // something this code can assert across three platforms, so
                // the guard is kept — and a guard that can fire must not
                // fire silently. A user who just picked a folder and got
                // nothing back is the exact shape commit 4c06379 removed
                // from `start_workspace_pick`.
                //
                // No generation guard here, deliberately, and NOT by
                // analogy-failure with `start_workspace_pick`: that one
                // needs `workspace_pick_generation` because its
                // continuation contains a SECONDS-LONG background
                // classification during which the window stays interactive
                // and the user can reach a different explicit decision.
                // This continuation has no background step — it resumes
                // straight onto the UI thread — and the condition that
                // actually matters here is not "did anything change" but
                // "are we in workspace mode", which is exactly what the
                // check below tests. A generation would be a weaker proxy
                // for an invariant we can read directly.
                if view.workspace_root.is_some() {
                    view.status =
                        connections_ui::SCRIPTS_PICK_DISCARDED_WORKSPACE.to_string();
                    cx.notify();
                    return;
                }
                // T7 review MINOR-4: store the path only if it round-trips.
                // `dbc_state::workspace::write_pointer` refuses a lossy path
                // at the rail rather than papering over it with
                // `display().to_string()`; a `scripts_dir` is read back the
                // same way and deserves the same honesty. Substituting
                // U+FFFD and reporting SUCCESS would put a path that does
                // not exist in `config.toml` and then blame the scan for it.
                let Some(picked_str) = picked.to_str() else {
                    view.status = format!(
                        "error: cesta ke složce skriptů obsahuje znaky, které nelze uložit: {}",
                        picked.display()
                    );
                    cx.notify();
                    return;
                };
                // Same corrupt-config gate the connection savers use
                // (`finish_save`, the ★ toggle): with a poisoned
                // `config.toml` the in-memory `config` is `default()`, so an
                // unguarded save would write a file with a `scripts_dir` and
                // NO connections over the user's real one.
                let Some(guard) = view.guard_corrupt_config(cx) else { return };
                view.config.scripts_dir = Some(picked_str.to_string());
                view.status = match view.config.save(&view.config_path, &guard) {
                    Ok(()) => format!("složka skriptů: {picked_str}"),
                    Err(e) => format!("error: nastavení se nepodařilo uložit ({e})"),
                };
                // T7 review MINOR-3: the ROOT just changed (A -> B). The
                // per-folder expand keys are `OuterId::ScriptFolder(rel)`
                // values naming paths under the OLD root — `reset_scripts`'
                // own doc says they "name paths that no longer exist" — and
                // a `reporting/` that happens to exist under B would render
                // pre-expanded without the user ever opening it.
                // `apply_context` and „Odebrat" already reset; the re-pick
                // was the uncovered third context change.
                //
                // It is deliberately NOT inside `start_scripts_scan`: that
                // is also the ⟳/retry path, and a refresh of the SAME root
                // must PRESERVE expansion (the schema-slot refresh contract,
                // resolved deviation 13).
                view.tree.update(cx, |t, cx| t.reset_scripts(cx));
                view.start_scripts_scan(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Part S §2's „Odebrat": clears the setting and the tree state. It
    /// deliberately does NOT touch a script binding — the binding holds an
    /// ABSOLUTE path, so „Uložit" in the caption strip keeps working; the
    /// resolved §2 note says so explicitly, and there is no guard here.
    fn clear_scripts_dir(&mut self, cx: &mut Context<Self>) {
        if self.workspace_root.is_some() {
            return;
        }
        // See `start_scripts_dir_pick` — same corrupt-config gate.
        let Some(guard) = self.guard_corrupt_config(cx) else { return };
        self.config.scripts_dir = None;
        self.status = match self.config.save(&self.config_path, &guard) {
            Ok(()) => "složka skriptů odebrána".to_string(),
            Err(e) => format!("error: nastavení se nepodařilo uložit ({e})"),
        };
        self.start_scripts_scan(cx); // no root => resets the tree slot
        cx.notify();
    }

    // -----------------------------------------------------------------
    // Workspace T8 — the script editor binding (Part S §5).
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // Workspace T9 — the scripts library's fs mutations (Part S §4).
    // Every op goes through `crate::scripts`, off the UI thread; the
    // component validator, the ONE Unicode-aware collision probe and the
    // empty-rel mutation rail all live inside those ops and are never
    // re-implemented here.
    // -----------------------------------------------------------------

    /// Part S §4: is the CURRENT binding touched by a mutation of `rel`?
    /// The fixup predicate for rename and delete, and what decides whether
    /// the delete confirm carries §4's second line. `resolve_entry_rel`
    /// (not `resolve_rel`) deliberately: this asks about a MUTATION
    /// target, and the library root is never one.
    ///
    /// A thin wrapper over the free [`binding_targets_entry`] so the answer
    /// is always computed from state read AT THE MOMENT OF THE QUESTION.
    /// See that function for the data-loss bug (final-review MAJOR-1) that
    /// caching this answer across an await caused.
    fn binding_targets(&self, rel: &str, is_dir: bool) -> bool {
        binding_targets_entry(
            self.script_binding.as_ref().map(|b| b.path.as_path()),
            self.effective_scripts_root().as_deref(),
            rel,
            is_dir,
        )
    }

    /// Part S §4: opens the ONE name dialog (new script / new folder /
    /// rename). Same single-modal invariant every other opener applies.
    fn open_script_name_modal(
        &mut self,
        mode: connections_ui::ScriptNameMode,
        parent_rel: String,
        target_rel: String,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.effective_scripts_root().is_none() {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        }
        // Rename prefills the CURRENT name, extension included: the
        // effective name is what `validate_script_name` will see, so
        // showing anything less would make „trzby.sql" → „trzby-2025"
        // look like it drops the suffix when it does not.
        let prefill = match mode {
            connections_ui::ScriptNameMode::Rename => {
                target_rel.rsplit('/').next().unwrap_or("").to_string()
            }
            _ => String::new(),
        };
        let field = cx.new(|cx| {
            let mut f = connections_ui::TextField::form_field(cx, "např. trzby", false);
            f.set_text(&prefill, cx);
            f
        });
        self.modal = Some(connections_ui::ModalState::ScriptName {
            mode,
            parent_rel,
            target_rel,
            is_dir,
            field,
            error: None,
            running: false,
        });
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// Part S §4/§7.9. `dirty_bound` is computed HERE, at open time, from
    /// the SAME predicate the fixup uses, so the modal's second line
    /// describes the binding by the same rule that will later drop it.
    ///
    /// It is a WARNING, not a decision: `finish_script_delete` asks the
    /// predicate AGAIN when the delete lands (final-review MAJOR-1),
    /// because the binding can move while the background op runs. This
    /// line is an honest statement about the moment the user is looking
    /// at it, which is all a confirm dialog can ever be.
    fn open_script_delete_modal(&mut self, rel: String, is_dir: bool, cx: &mut Context<Self>) {
        if self.modal.is_some() || self.apply_dialog.is_some() || self.discard_confirm.is_some() {
            return;
        }
        if self.effective_scripts_root().is_none() {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        }
        let dirty_bound = self.binding_targets(&rel, is_dir) && self.script_dirty_flag;
        self.modal = Some(connections_ui::ModalState::ScriptDeleteConfirm {
            rel,
            is_dir,
            dirty_bound,
            error: None,
            running: false,
        });
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// The Skript/Složka radio. Inert for `Rename` (an entry's kind cannot
    /// change) and while `running`, the same structural-no-op rule the
    /// ScriptRun radios follow.
    pub(crate) fn set_script_name_mode(
        &mut self,
        value: connections_ui::ScriptNameMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(connections_ui::ModalState::ScriptName { mode, is_dir, running, .. }) =
            &mut self.modal
        {
            if *running || *mode == connections_ui::ScriptNameMode::Rename {
                return;
            }
            *mode = value;
            *is_dir = value == connections_ui::ScriptNameMode::NewFolder;
            cx.notify();
        }
    }

    /// „Zrušit" on either scripts modal — inert while the background fs op
    /// is in flight, exactly as Esc is (`script_modal_esc_closable`), so a
    /// click cannot abandon a modal whose continuation is about to close
    /// it and fix the editor binding up.
    pub(crate) fn cancel_script_modal(&mut self, cx: &mut Context<Self>) {
        let running = match &self.modal {
            Some(connections_ui::ModalState::ScriptName { running, .. })
            | Some(connections_ui::ModalState::ScriptDeleteConfirm { running, .. }) => *running,
            _ => return,
        };
        if !connections_ui::script_modal_esc_closable(running) {
            return;
        }
        self.close_modal(cx);
    }

    /// Is `self.modal` still the SAME latched name dialog this
    /// continuation was dispatched from? THE identity rule — one source,
    /// used by both the success and the failure landing (T9 review
    /// MINOR-2: they used to disagree, and the failure side matched on the
    /// VARIANT alone).
    fn owns_script_name_modal(
        &self,
        mode: connections_ui::ScriptNameMode,
        target_rel: &str,
    ) -> bool {
        matches!(
            &self.modal,
            Some(connections_ui::ModalState::ScriptName {
                mode: m, target_rel: t, running: true, ..
            }) if *m == mode && *t == target_rel
        )
    }

    /// The delete confirm's equivalent.
    fn owns_script_delete_modal(&self, rel: &str) -> bool {
        matches!(
            &self.modal,
            Some(connections_ui::ModalState::ScriptDeleteConfirm { rel: r, running: true, .. })
                if *r == rel
        )
    }

    /// Continuation-side failure landing for the name dialog.
    ///
    /// T9 review MINOR-2. `set_script_name_error` below is sound where it
    /// is used — synchronously inside `confirm_script_name`, with no await
    /// between destructuring the modal and stamping it. This path is not:
    /// it resumes AFTER a background step, so it must re-verify identity
    /// for exactly the reason `finish_script_name` does. Stamping a
    /// different dialog of the same kind would not merely misplace a
    /// message — it would clear THAT dialog's `running` latch, which is
    /// the one thing making a double-dispatch into a shared `<path>.tmp`
    /// unreachable.
    fn land_script_name_error(
        &mut self,
        mode: connections_ui::ScriptNameMode,
        target_rel: &str,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.owns_script_name_modal(mode, target_rel) {
            self.set_script_name_error(message, cx);
        } else {
            self.status = format!("error: {message}");
            cx.notify();
        }
    }

    /// The delete confirm's equivalent — same asymmetry, same fix.
    fn land_script_delete_error(&mut self, rel: &str, message: String, cx: &mut Context<Self>) {
        if self.owns_script_delete_modal(rel) {
            self.set_script_delete_error(message, cx);
        } else {
            self.status = format!("error: {message}");
            cx.notify();
        }
    }

    /// A failed name op: the message goes back INTO the dialog (the field
    /// keeps its text, so the user edits rather than retypes) and clears
    /// `running` so they can retry. If the dialog is somehow no longer
    /// there, the message still reaches the status line — never nowhere.
    ///
    /// SYNCHRONOUS callers only (`confirm_script_name`'s pre-dispatch
    /// refusals): the variant-only match is sound there because nothing
    /// can have replaced the modal between the destructure and this call.
    /// A continuation must use `land_script_name_error` instead.
    fn set_script_name_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ScriptName { error, running, .. }) = &mut self.modal
        {
            *running = false;
            *error = Some(message);
        } else {
            self.status = format!("error: {message}");
        }
        cx.notify();
    }

    /// The delete confirm's equivalent — „složka není prázdná — smažte
    /// nejdřív její obsah" has to be readable against the folder named
    /// right above it, so it stays in the modal too.
    fn set_script_delete_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ScriptDeleteConfirm { error, running, .. }) =
            &mut self.modal
        {
            *running = false;
            *error = Some(message);
        } else {
            self.status = format!("error: {message}");
        }
        cx.notify();
    }

    /// „Vytvořit"/„Přejmenovat" (and Enter — policy clause (a)).
    pub(crate) fn confirm_script_name(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptName {
            mode,
            parent_rel,
            target_rel,
            is_dir,
            field,
            running,
            ..
        }) = &self.modal
        else {
            return;
        };
        if *running {
            return;
        }
        let (mode, parent_rel, target_rel, is_dir, field) =
            (*mode, parent_rel.clone(), target_rel.clone(), *is_dir, field.clone());
        let name = field.read(cx).text();
        let Some(root) = self.effective_scripts_root() else {
            self.set_script_name_error("nastavte složku skriptů v Nastavení".to_string(), cx);
            return;
        };
        // SINGLE WRITER PER PATH (T8's `fsutil::write_atomic` contract):
        // `create_script` writes through the very same fixed-`<path>.tmp`
        // rail Ctrl+S uses, and a rename can move a file out from under an
        // in-flight save. Serialize against the editor's save exactly the
        // way `save_script` serializes against itself — this is a refusal
        // the user can retry, not an error, so it reuses that wording.
        if self.script_save_in_flight {
            self.set_script_name_error(SCRIPT_SAVE_IN_FLIGHT.to_string(), cx);
            return;
        }
        if let Some(connections_ui::ModalState::ScriptName { running, error, .. }) = &mut self.modal
        {
            *running = true;
            *error = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let job = (root, parent_rel, target_rel.clone(), name);
            let result: Result<String, String> = cx
                .background_spawn(async move {
                    let (root, parent_rel, target_rel, name) = job;
                    match mode {
                        connections_ui::ScriptNameMode::NewScript => {
                            crate::scripts::create_script(&root, &parent_rel, &name)
                        }
                        connections_ui::ScriptNameMode::NewFolder => {
                            crate::scripts::create_folder(&root, &parent_rel, &name)
                        }
                        connections_ui::ScriptNameMode::Rename => {
                            crate::scripts::rename_entry(&root, &target_rel, &name, is_dir)
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(new_rel) => view.finish_script_name(mode, target_rel, is_dir, new_rel, cx),
                Err(e) => view.land_script_name_error(mode, &target_rel, e, cx),
            });
        })
        .detach();
    }

    /// The name op landed. RE-CHECKS that the modal it is about to close
    /// is still ITS OWN dialog: the op ran in the background, so closing
    /// „the modal" blindly would close whatever the user has since opened
    /// — the silent-context-change class this phase has already paid for
    /// five times. `running` makes that unreachable in practice (Esc, both
    /// buttons and the radio are all inert while it holds), but a
    /// continuation must re-verify its promise, not assume it. If the
    /// dialog IS gone, the op still happened, so the status still says so.
    fn finish_script_name(
        &mut self,
        mode: connections_ui::ScriptNameMode,
        target_rel: String,
        is_dir: bool,
        new_rel: String,
        cx: &mut Context<Self>,
    ) {
        let mine = self.owns_script_name_modal(mode, &target_rel);
        if mode == connections_ui::ScriptNameMode::Rename {
            self.retarget_binding_after_rename(&target_rel, &new_rel, is_dir);
        }
        let name = new_rel.rsplit('/').next().unwrap_or(&new_rel).to_string();
        // DEVIATION from the plan snippet, recorded: it statused
        // „skript vytvořen: {name}" for BOTH create modes, which says
        // „script" over a folder. A false noun in the one line confirming
        // a filesystem mutation is not a nit.
        self.status = match mode {
            connections_ui::ScriptNameMode::Rename => format!("přejmenováno: {name}"),
            connections_ui::ScriptNameMode::NewFolder => format!("složka vytvořena: {name}"),
            connections_ui::ScriptNameMode::NewScript => format!("skript vytvořen: {name}"),
        };
        if mine {
            self.close_modal(cx);
        }
        self.start_scripts_scan(cx);
        cx.notify();
    }

    /// §4's rename fixup. A rename moves the file the editor is bound to —
    /// or the FOLDER above it — so the binding must follow it, or the
    /// caption names a path that no longer exists and the next Ctrl+S
    /// silently recreates the old file.
    ///
    /// Routed through `set_script_binding`, THE only writer of the field
    /// (DEVIATION from the plan snippet, which poked `binding.path`
    /// directly): the generation bump is exactly what tells an in-flight
    /// save/open that its target moved. `saved_text` is carried across
    /// unchanged, so a clean binding stays clean — the caption follows the
    /// rename without sprouting a „ •".
    fn retarget_binding_after_rename(&mut self, old_rel: &str, new_rel: &str, is_dir: bool) {
        let Some(root) = self.effective_scripts_root() else { return };
        let Some(b) = self.script_binding.as_ref() else { return };
        let (Ok(old), Ok(new)) = (
            crate::scripts::resolve_entry_rel(&root, old_rel),
            crate::scripts::resolve_entry_rel(&root, new_rel),
        ) else {
            return;
        };
        let Some(moved) = script_binding_retarget(&b.path, &old, &new, is_dir) else { return };
        let saved_text = b.saved_text.clone();
        self.set_script_binding(Some(ScriptBinding { path: moved, saved_text }));
    }

    /// „Smazat". Enter never reaches here (`ModalConfirmKind::Ignore`) —
    /// the button is the last gate before an unrecoverable disk delete.
    pub(crate) fn confirm_script_delete(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ScriptDeleteConfirm { rel, is_dir, running, .. }) =
            &self.modal
        else {
            return;
        };
        if *running {
            return;
        }
        let (rel, is_dir) = (rel.clone(), *is_dir);
        let Some(root) = self.effective_scripts_root() else {
            self.set_script_delete_error("nastavte složku skriptů v Nastavení".to_string(), cx);
            return;
        };
        // Same single-writer serialization as the name dialog: removing a
        // file whose atomic save is still fsyncing would race the rename
        // half of that write and could resurrect it a moment later.
        if self.script_save_in_flight {
            self.set_script_delete_error(SCRIPT_SAVE_IN_FLIGHT.to_string(), cx);
            return;
        }
        // FINAL-REVIEW MAJOR-1: the binding question is deliberately NOT
        // asked here. It used to be, and the captured bool was applied
        // blind at the landing — see `binding_targets_entry` for the
        // resurrection scenario that cost. `is_dir` travels instead (it is
        // an immutable property of the confirmed target, not a statement
        // about the editor) and the landing asks for itself.
        if let Some(connections_ui::ModalState::ScriptDeleteConfirm { running, error, .. }) =
            &mut self.modal
        {
            *running = true;
            *error = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let job = (root, rel.clone());
            let result = cx
                .background_spawn(async move {
                    let (root, rel) = job;
                    crate::scripts::delete_entry(&root, &rel, is_dir)
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(()) => view.finish_script_delete(rel, is_dir, cx),
                Err(e) => view.land_script_delete_error(&rel, e, cx),
            });
        })
        .detach();
    }

    /// The delete landed — same own-modal re-check as `finish_script_name`,
    /// and (final-review MAJOR-1) the same re-ASK of the binding.
    ///
    /// `is_dir` is threaded from the dispatch because it describes the
    /// entry that was deleted, which cannot change while the delete runs.
    /// The BINDING can, so it is read here and nowhere else.
    fn finish_script_delete(&mut self, rel: String, is_dir: bool, cx: &mut Context<Self>) {
        let mine = self.owns_script_delete_modal(&rel);
        // RE-VERIFY MINOR-A. UNCONDITIONAL, and it must stay that way.
        //
        // Re-asking the binding above closed MAJOR-1's ordering (open
        // lands, THEN delete lands). The mirror survived: with the editor
        // UNBOUND at the landing, `binding_targets` is false, nothing is
        // called, and `script_binding_generation` is therefore never
        // bumped — so an `open_script` dispatched BEFORE this delete lands
        // AFTER it, passes all three legs of `script_open_abort_reason`
        // (root unmoved, generation unmoved, buffer untouched) and binds
        // the file that was just irreversibly deleted. Windows opens the
        // read with `FILE_SHARE_DELETE`, so the read completes across the
        // delete and there is not even an error to notice. The next Ctrl+S
        // recreates the file.
        //
        // The fix is the lesson `apply_context` already learned and wrote
        // down: supersede in-flight continuations ALWAYS, not only when
        // the state you happen to be looking at changed. A conditional
        // bump is a bump that is missing precisely in the case where
        // nothing local looks wrong.
        self.supersede_script_continuations();
        if self.binding_targets(&rel, is_dir) {
            // §4: the bound file is gone — drop the binding. The editor
            // TEXT stays (the user may still want it, exactly as „Zavřít"
            // has always left it); what must not survive is a caption and
            // a „ •" claiming a file that no longer exists, and a Ctrl+S
            // that would silently RECREATE it.
            self.set_script_binding(None);
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        self.status = format!("smazáno: {name}");
        if mine {
            self.close_modal(cx);
        }
        self.start_scripts_scan(cx);
        cx.notify();
    }

    /// Part S §5: does the editor differ from what is on disk? `false`
    /// whenever nothing is bound — the guard NEVER protects unbound ad-hoc
    /// text (§5.5, deliberate: identical exposure to today).
    pub(crate) fn script_is_dirty(&self, cx: &App) -> bool {
        self.script_binding
            .as_ref()
            .is_some_and(|b| script_text_is_dirty(&self.sql.read(cx).text(), &b.saved_text))
    }

    /// The bound script's label, relative to the CURRENT scripts root.
    pub(crate) fn binding_rel(&self) -> Option<String> {
        let b = self.script_binding.as_ref()?;
        Some(script_caption_rel(&b.path, self.effective_scripts_root().as_deref()))
    }

    /// THE only writer of `script_binding` — it bumps
    /// `script_binding_generation` so an in-flight open/save continuation
    /// can tell that the user moved on. Writing the field directly
    /// anywhere else would silently reintroduce the stale-continuation
    /// class this phase has already paid for four times.
    ///
    /// The bump is keyed on the bound PATH, not on every call: refreshing
    /// `saved_text` for the same file (what a successful save does) is not
    /// „the user moved on", and counting it as one would make two quick
    /// Ctrl+S presses report a spurious „editor se mezitím změnil" and
    /// leave a phantom „ •" over a file that is in fact saved.
    ///
    /// That heuristic is about SAVES, and it is not a general „did anything
    /// invalidate the in-flight work" test — see
    /// `supersede_script_continuations` for the case where it is wrong.
    fn set_script_binding(&mut self, binding: Option<ScriptBinding>) {
        let changed = script_binding_target_changed(
            self.script_binding.as_ref().map(|b| b.path.as_path()),
            binding.as_ref().map(|b| b.path.as_path()),
        );
        self.script_binding = binding;
        if changed {
            self.supersede_script_continuations();
        }
    }

    /// Invalidates EVERY in-flight script continuation, unconditionally.
    ///
    /// T8 re-verify NEW MAJOR. `set_script_binding`'s path-changed
    /// heuristic bumps nothing for a `None -> None` transition, so
    /// `apply_context`'s unbind was a no-op whenever the editor was
    /// UNBOUND — and an unbound editor is `script_is_dirty == false` by
    /// construction, so `context_switch_blocked` waves the swap through.
    /// The hole: open `trzby.sql`, the read goes to the background
    /// executor (seconds on a OneDrive/network-backed root), switch
    /// workspace, the read lands with the generation and the buffer both
    /// unchanged — both gates pass — and `bind_script` silently installs
    /// the OLD workspace's file under a path beneath the OLD root, with
    /// the status cleared. The next Ctrl+S then writes into the old
    /// workspace folder: precisely the cross-context leak the swap-unbind
    /// was added to close. Same shape in `save_script_as`.
    ///
    /// A context swap is not „did the binding change" — it is „everything
    /// dispatched before this instant belongs to a context that no longer
    /// exists". So it bumps directly rather than going through the
    /// heuristic. Still the ONE counter and the ONE meaning; only the
    /// trigger is broader.
    fn supersede_script_continuations(&mut self) {
        self.script_binding_generation = self.script_binding_generation.wrapping_add(1);
    }

    /// Sets the editor text AND the binding in one place, so the two can
    /// never drift (a `set_text` without a matching `saved_text` update is
    /// exactly how a phantom „ •" appears).
    pub(crate) fn bind_script(&mut self, path: PathBuf, text: String, cx: &mut Context<Self>) {
        // RE-VERIFY: the buffer replacement is behind the compiler now.
        // `editor_load_guarded` said this was safe at DISPATCH and
        // `script_open_abort_reason` has just re-checked that root,
        // binding and buffer all stood still — so the permission asked for
        // here is the same one, asked again, at the instant it is spent.
        let permitted = with_editor_replaceable(self, cx, |view, cx, permit| {
            view.sql.update(cx, |s, cx| s.replace_buffer(&text, cx, permit));
            view.set_script_binding(Some(ScriptBinding { path, saved_text: text }));
            view.status = String::new();
        });
        if permitted.is_none() {
            // Unreachable through the guard, and a silent no-op is the
            // shape this phase keeps banning, so it says so.
            self.status = SCRIPT_LOAD_BLOCKED.to_string();
        }
        cx.notify();
    }

    /// THE guard (Part S §5.5). Every site that would replace the editor's
    /// text — or drop the binding under it — routes through here; there is
    /// no second dirty check anywhere.
    pub(crate) fn editor_load_guarded(
        &mut self,
        action: PendingScriptAction,
        cx: &mut Context<Self>,
    ) {
        // T8 review MINOR-1: this used to read
        // `if dirty && discard_confirm.is_none() { park }`, which fell
        // THROUGH to performing the destructive action whenever a prompt
        // was already up — the fail-safe direction inverted. Unreachable
        // today (the overlay is a full-screen `.occlude()` that also takes
        // focus, and `on_open_palette` refuses while one is open), but
        // „another question is already unanswered" must mean REFUSE, never
        // „do it anyway". Refusing out loud, because a silent no-op is the
        // other thing this phase keeps banning.
        if self.discard_confirm.is_some() {
            self.status = "nejprve dokončete rozpracované úpravy".to_string();
            cx.notify();
            return;
        }
        if self.script_is_dirty(cx) {
            self.discard_confirm = Some(DiscardConfirmState {
                // Scripts are text, not staged rows — the count is „one
                // file", and `discard_confirm_question`'s script branch is
                // what actually names it, so the number is never rendered.
                change_count: 1,
                action: PendingDiscard::Script(action),
            });
            // UX-polish §1.4: no-input prompt, cx-only site — defer focus
            // to `AppView::render` via `modal_needs_focus`, exactly like
            // the three pre-existing discard sites.
            self.modal_needs_focus = true;
            cx.notify();
            return;
        }
        self.perform_script_action(action, cx);
    }

    /// The parked action, performed — either straight away (clean editor)
    /// or from „Zahodit".
    fn perform_script_action(&mut self, action: PendingScriptAction, cx: &mut Context<Self>) {
        match action {
            PendingScriptAction::Open { rel } => self.open_script(rel, cx),
            PendingScriptAction::Unbind => {
                // §5.3: the text STAYS — it is simply no longer bound.
                self.set_script_binding(None);
                self.status = String::new();
                cx.notify();
            }
            PendingScriptAction::LoadText { sql } => {
                let permitted = with_editor_replaceable(self, cx, |view, cx, permit| {
                    view.sql.update(cx, |s, cx| s.replace_buffer(&sql, cx, permit));
                    view.set_script_binding(None);
                });
                if permitted.is_none() {
                    self.status = SCRIPT_LOAD_BLOCKED.to_string();
                }
                cx.notify();
            }
        }
    }

    /// Part S §5.1. Opening NEVER runs anything (the brief's binding rule:
    /// script files are user content). The stat + 1 MiB cap + symlink +
    /// UTF-8 decisions all live in `scripts::read_script` — one rail, not a
    /// second size probe here.
    fn open_script(&mut self, rel: String, cx: &mut Context<Self>) {
        let Some(root) = self.effective_scripts_root() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let path = match crate::scripts::resolve_rel(&root, &rel) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("error: {e}");
                cx.notify();
                return;
            }
        };
        let dispatched = self.script_binding_generation;
        // T8 review BLOCKER-1. `editor_load_guarded` ran at DISPATCH; the
        // read below then yields the UI thread, and the buffer is not
        // replaced until it comes back. The generation rail alone is
        // structurally BLIND to the one thing the guard exists to protect:
        // `set_script_binding` bumps only when the bound PATH changes, and
        // TYPING changes no binding. So a clean (or unbound) editor, a
        // double-click, a few keystrokes while a OneDrive/network-backed
        // root answers, and the read landed on top of them — permanently
        // (`SqlInput` has no undo) and silently (`bind_script` even clears
        // the status). The buffer gets the same treatment as the binding.
        let dispatched_text = self.sql.read(cx).text();
        cx.spawn(async move |this, cx| {
            let job = path.clone();
            let result =
                cx.background_spawn(async move { crate::scripts::read_script(&job) }).await;
            let _ = this.update(cx, |view, cx| {
                // The read yielded the UI thread, so the user may have
                // opened something else, closed the binding, or typed
                // meanwhile. Landing now would be a silent context change —
                // and it would clobber whatever they moved on to.
                if let Some(reason) = script_open_abort_reason(
                    view.effective_scripts_root().as_deref(),
                    &root,
                    view.script_binding_generation,
                    dispatched,
                    &view.sql.read(cx).text(),
                    &dispatched_text,
                ) {
                    view.status = reason.to_string();
                    cx.notify();
                    return;
                }
                match result {
                    Ok(text) => view.bind_script(path.clone(), text, cx),
                    Err(e) => {
                        view.status = format!("error: {e}");
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Part S §5.2. Atomic (the shared `fsutil::write_atomic` rail, via
    /// `scripts::write_script`). Last-writer-wins on external edits — by
    /// the user's own model git is the history layer; the app does not
    /// diff or version.
    ///
    /// `rescan` is `true` only for save-as. DEVIATION from the plan
    /// snippet, recorded: it dispatched the rescan next to `save_script`
    /// on the UI thread, i.e. BEFORE the background write had landed, so
    /// the freshly created file was routinely missing from the tree.
    /// Rescanning in the success arm is the only ordering that can show it.
    ///
    /// SERIALIZED (T8 review MAJOR-2). `fsutil::write_atomic` derives a
    /// FIXED `<path>.tmp`, and `sync_all` holds that window open for tens
    /// of milliseconds — long enough for OS key auto-repeat on a held
    /// Ctrl+S to dispatch a second write into the SAME tmp file. That
    /// races three ways: a truncated tmp mid-`write_all` (a byte-interleaved
    /// `.sql`), an ENOENT rename reported as „uložení selhalo" over a file
    /// that is fine, and — worst — a phantom-clean caption, where the
    /// later-completing update sets `saved_text` to text the disk does not
    /// hold and the „ •" disappears over an unsaved file.
    ///
    /// The fix is here rather than in the shared rail, and the reason is
    /// NOT the one this comment first gave (T8 re-verify MINOR — that
    /// version argued from a stale copy of the `.gitignore` template in
    /// the spec draft; the shipped `GITIGNORE_TEMPLATE` is a blanket
    /// `*.tmp` and would cover a nonce perfectly well). The real reason:
    /// a unique tmp name fixes the first two failures above but NOT the
    /// third. With two writes in flight the `rename` that wins on disk
    /// need not belong to the continuation that runs last, so phantom-clean
    /// survives a nonce untouched. Only per-path serialization closes it —
    /// and once the caller has that, a nonce buys nothing. `write_atomic`'s
    /// doc comment now states the contract for every other writer over a
    /// user folder.
    ///
    /// FINAL-REVIEW MAJOR-2: `_allowed` is a [`SaveAllowed`] witness,
    /// mintable ONLY by `save_guard::with_save_permission`. It is unused at
    /// runtime and load-bearing at compile time — no caller can reach this
    /// writer, by any call syntax, without having asked the predicate. The
    /// reviewer's UFCS bypass (`AppView::save_script(self, p, t, false, cx)`)
    /// no longer type-checks.
    fn save_script(
        &mut self,
        path: PathBuf,
        text: String,
        rescan: bool,
        _allowed: SaveAllowed<'_>,
        cx: &mut Context<Self>,
    ) {
        if self.script_save_in_flight {
            self.status = SCRIPT_SAVE_IN_FLIGHT.to_string();
            cx.notify();
            return;
        }
        self.script_save_in_flight = true;
        let dispatched = self.script_binding_generation;
        cx.spawn(async move |this, cx| {
            let (job_path, job_text) = (path.clone(), text.clone());
            let result = cx
                .background_spawn(async move { crate::scripts::write_script(&job_path, &job_text) })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.script_save_in_flight = false;
                let name =
                    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                match result {
                    Ok(()) => {
                        // The WRITE is honoured either way — the user asked
                        // for this file to hold this text and it now does.
                        // What must not happen is re-binding the editor to
                        // it after the user moved on: that would strand a
                        // different buffer under this file's caption and
                        // make the next Ctrl+S overwrite the wrong script.
                        if view.script_binding_generation == dispatched {
                            view.set_script_binding(Some(ScriptBinding {
                                path: path.clone(),
                                saved_text: text.clone(),
                            }));
                            view.status = format!("skript uložen: {name}");
                        } else {
                            view.status =
                                format!("skript uložen: {name} — editor se mezitím změnil");
                        }
                        // T8 review MINOR-2: the rescan is INDEPENDENT of
                        // the re-bind decision and lives outside it. A
                        // save-as that lands after the binding moved still
                        // created a file in the library; leaving it out of
                        // the tree, with a status that talks only about the
                        // binding, is a silently incomplete result.
                        if rescan
                            && view
                                .effective_scripts_root()
                                .is_some_and(|r| path_starts_with_ci(&path, &r))
                        {
                            view.start_scripts_scan(cx);
                        }
                    }
                    Err(e) => view.status = format!("error: {e}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Part S §5.4 — Ctrl+S with no binding.
    fn save_script_as(&mut self, cx: &mut Context<Self>) {
        let text = self.sql.read(cx).text();
        if text.trim().is_empty() {
            self.status = "editor je prázdný".to_string();
            cx.notify();
            return;
        }
        let Some(root) = self.effective_scripts_root() else {
            self.status = "error: nastavte složku skriptů v Nastavení".to_string();
            cx.notify();
            return;
        };
        let dispatched = self.script_binding_generation;
        // Verified in the pinned GPUI rev (907ed09,
        // `gpui_windows/src/platform.rs::file_save_dialog`): a non-empty
        // `directory` is canonicalized and pushed through
        // `IFileSaveDialog::SetFolder` — the FORCING call, not
        // `SetDefaultFolder` — so this really does open in the library,
        // with `dotaz.sql` prefilled via `SetFileName`. The same function
        // sets `SetFileTypes` to „All files"/`*.*`, which is why
        // `with_sql_extension` below is both correct and necessary.
        //
        // KNOWN, UNOBSERVABLE FALLBACK: if `root.canonicalize()` fails —
        // the scripts folder deleted or unmounted between the scan and
        // this click — GPUI `log_err()`s and opens the dialog at the
        // PLATFORM DEFAULT instead. The app cannot see that happen, so the
        // user can land in their home folder with no explanation. Not
        // fixable from here without duplicating the canonicalize (and then
        // racing it); recorded so it is not mistaken for our own silence.
        let dialog = cx.prompt_for_new_path(&root, Some("dotaz.sql"));
        cx.spawn(async move |this, cx| {
            // Same three arms as the backup save picker: a swallowed dialog
            // failure is exactly the silence this phase keeps banning.
            let picked = match dialog.await {
                Ok(Ok(Some(p))) => p,
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "uložení zrušeno".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog pro uložení selhal ({e})");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog pro uložení není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let path = with_sql_extension(&picked);
            let _ = this.update(cx, |view, cx| {
                // The picker is not modal to the whole app on every
                // platform, and the buffer this save-as was started for is
                // the one the user saw. If the editor has since been bound
                // to a script (or reloaded from history), writing the OLD
                // captured text to a NEW path — and then binding to it —
                // would be a silent context change on both ends.
                if view.script_binding_generation != dispatched {
                    view.status = "uložení zrušeno — editor se mezitím změnil".to_string();
                    cx.notify();
                    return;
                }
                // FINAL-REVIEW NIT-1, the third leg. `open_script` re-asks
                // root + generation + BUFFER TEXT
                // (`script_open_abort_reason`); this path asked only the
                // first two. The generation is structurally blind to
                // typing — `set_script_binding` bumps on a PATH change and
                // nothing else — and the picker is not app-modal on every
                // platform, so keystrokes during it are invisible here.
                //
                // The consequence is milder than the open's (no data is
                // destroyed: the file gets the pre-picker text, the „ •"
                // stays up and a second Ctrl+S fixes it) but it is still a
                // silent divergence — `saved_text` would be bound to text
                // the user can no longer see anywhere. Refuse instead, in
                // the same words the open uses, so the two paths do not
                // teach the user two different stories about the same
                // event. Nothing is lost: the editor is untouched, no file
                // is created, and Ctrl+S re-opens the picker.
                if view.sql.read(cx).text() != text {
                    view.status = "uložení zrušeno — mezitím jste psali do editoru".to_string();
                    cx.notify();
                    return;
                }
                // T9 RE-VERIFY FAIL-1. The generation check above is NOT
                // enough, and MAJOR-1's own scenario walks straight through
                // the gap: `on_save_script` asked `script_save_allowed`
                // BEFORE the picker opened, and the picker is not app-modal
                // on every platform, so the whole dialog-open window is
                // unguarded on this branch.
                //
                // 1. Editor UNBOUND and dirty. Ctrl+S → guard passes →
                //    `save_script_as` → the picker opens.
                // 2. The user deletes `trzby.sql` from the tree and
                //    confirms. `script_save_in_flight` is still false (no
                //    write was dispatched), so the serialization check does
                //    not fire; the delete lands. `finish_script_delete`
                //    runs with `was_bound == false` — the editor is
                //    unbound, that is why we are in save-AS — so
                //    `set_script_binding` is never called and the
                //    generation is NEVER BUMPED.
                // 3. The user completes the picker naming `trzby.sql`. The
                //    generation check passes, the write lands, and the
                //    irreversibly deleted file is silently back.
                //
                // So the predicate is re-asked HERE, continuation-side,
                // exactly the asymmetry T9 review MINOR-2 established
                // between `set_script_name_error` (synchronous, sound) and
                // `land_script_name_error` (post-await, must re-verify): a
                // check performed before an await is a check about the past.
                //
                // FINAL-REVIEW MAJOR-2 + RE-VERIFY FAIL-2: the SCOPE is
                // what makes this structural. `on_save_script`'s
                // permission is branded to its own synchronous scope, so
                // it cannot be captured by this `'static` spawned future
                // even deliberately — the re-verifier's carried-witness
                // refactor no longer compiles. The only way to reach
                // `save_script` from here is to ask again, here.
                //
                // Rescan when the save lands INSIDE the library; outside is
                // allowed (it is the user's disk) but the tree honestly
                // won't show it.
                let text = text.clone();
                if with_save_permission(view, cx, move |view, cx, allowed| {
                    view.save_script(path, text, true, allowed, cx);
                })
                .is_none()
                {
                    view.status = SCRIPT_SAVE_BLOCKED.to_string();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Ctrl+S / the caption strip's „Uložit" / the palette's „Uložit
    /// skript" — one entry point for all three (Part S §5.2/§5.4).
    fn on_save_script(&mut self, _: &SaveScript, _window: &mut Window, cx: &mut Context<Self>) {
        // T9 review MAJOR-1. `.occlude()` blocks CLICKS, not KEYS — the
        // trap `run_query_with`'s guard already documents — and `ctrl-s` is
        // registered with context `None`, so nothing shadows it while a
        // dialog is up. Unguarded, a Ctrl+S pressed out of habit while
        // „Pracuji…" shows RACES the rename/delete running underneath it:
        // `script_save_in_flight` is one-directional (it is read at
        // dispatch, and the UI thread is free for the whole background op),
        // so the delete lands, the binding is cleared, the rescan finishes
        // — and THEN the save recreates the file the user just
        // irreversibly deleted, invisibly, because the tree already
        // refreshed without it. A rename can leave BOTH names on disk.
        //
        // Refused OUT LOUD (a silent `return` is the other thing this phase
        // bans) and not as an „error:" — nothing failed; the keystroke
        // simply arrived while another decision was still on screen.
        let bound = self.script_binding.as_ref().map(|b| b.path.clone());
        let permitted = with_save_permission(self, cx, |view, cx, allowed| match bound {
            Some(path) => {
                let text = view.sql.read(cx).text();
                view.save_script(path, text, false, allowed, cx);
            }
            // The permission ENDS with this scope, deliberately.
            // `save_script_as` resumes after a file picker, so this answer
            // will be stale by the time it has a path to write, and it
            // opens its own scope there. Re-verify FAIL-2: handing the
            // witness over used to COMPILE (a move is not a re-use), which
            // restored the delete/save-as race whole; the generative brand
            // is what makes it impossible rather than merely discouraged.
            None => view.save_script_as(cx),
        });
        if permitted.is_none() {
            self.status = SCRIPT_SAVE_BLOCKED.to_string();
            cx.notify();
        }
    }

    // -----------------------------------------------------------------
    // Workspace T4 — the §W4 recovery flow + the §W3.4 context swap.
    // -----------------------------------------------------------------

    /// Design §W4. Opened only from `main()`'s startup wiring — there is no
    /// other way to reach a broken resolution, and no guard is needed
    /// (nothing else can be open one frame after the window appears).
    fn open_workspace_missing_modal(
        &mut self,
        root: Option<PathBuf>,
        reason: String,
        cx: &mut Context<Self>,
    ) {
        self.modal = Some(connections_ui::ModalState::WorkspaceMissing {
            root,
            reason,
            error: None,
            pending: None,
        });
        // UX-polish §1.4: no-input modal, cx-only opener.
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// „Najít složku…" — THE WORKSPACE MOVED, not "make a new one": only a
    /// folder carrying a valid `dbc-workspace.toml` marker is accepted here
    /// (design §W4). An empty folder is refused with the same honesty as a
    /// non-workspace one — initialization is a Settings decision with its
    /// own confirm + security warning (T5), never a recovery side effect.
    fn pick_workspace_for_recovery(&mut self, cx: &mut Context<Self>) {
        // T4 review MAJOR-1: capture the generation this pick belongs to.
        // The picker and the classification below are both slow (a folder
        // dialog, then a `read_dir` that can take seconds on a network
        // share) and the window stays interactive throughout, so the user
        // can reach a DIFFERENT decision — „Použít lokální profil" — while
        // this task is still running.
        let my_generation = self.workspace_pick_generation;
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Otevřít".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => return, // cancelled: the modal stays up
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_missing_error(format!("dialog selhal: {e}"), cx);
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_missing_error("dialog není dostupný".into(), cx);
                    });
                    return;
                }
            };
            // Off the UI thread: classification ONLY. The pointer write
            // deliberately does NOT happen here (T4 review MAJOR-1): a
            // stale task that had already rewritten the pointer would leave
            // the NEXT launch in workspace mode even after being refused
            // here, and undoing a write after the fact is its own race.
            // Nothing is persisted until the UI-thread guard below passes.
            let classified = cx
                .background_spawn({
                    let picked = picked.clone();
                    async move {
                        match dbc_state::workspace::classify(&picked) {
                            dbc_state::workspace::Classification::Workspace => Ok(()),
                            dbc_state::workspace::Classification::FutureFormat(f) => Err(format!(
                                "pracovní prostor vyžaduje novější verzi aplikace (formát {f})"
                            )),
                            dbc_state::workspace::Classification::Unreadable(m) => Err(m),
                            _ => Err(
                                "vybraná složka není pracovní prostor dbc — vyberte složku s dbc-workspace.toml"
                                    .to_string(),
                            ),
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if let Err(e) = classified {
                    view.set_workspace_missing_error(e, cx);
                    return;
                }
                // THE GUARD (T4 review MAJOR-1). Both halves are pinned by
                // `recovery_pick_may_commit`'s tests.
                let modal_open = matches!(
                    view.modal,
                    Some(connections_ui::ModalState::WorkspaceMissing { .. })
                );
                if !recovery_pick_may_commit(
                    modal_open,
                    my_generation,
                    view.workspace_pick_generation,
                ) {
                    // Superseded by the user's own explicit choice. Say
                    // nothing and change nothing: the pointer was never
                    // written, so there is no state to undo, and stealing
                    // the context back now is exactly the silent override
                    // this guard exists to stop.
                    return;
                }
                // FINAL-REVIEW MINOR-1: still nothing is persisted here.
                // The folder is a valid workspace, but pointing the app at
                // a folder it did not previously own is an ADOPT, and
                // Settings' adopt states the two things this one did not —
                // that the trezor there has its OWN master password, and
                // §W6.3's git warning — BEFORE it commits. So the pick
                // moves the blocking modal to its confirm STATE and the
                // user decides; `confirm_workspace_recovery` does the
                // write, synchronously, with no await anywhere near it.
                if let Some(connections_ui::ModalState::WorkspaceMissing { pending, error, .. }) =
                    &mut view.modal
                {
                    *pending = Some(picked);
                    // A previous refusal's message is about the previous
                    // folder; carrying it onto this screen would read as a
                    // complaint about the one just picked.
                    *error = None;
                }
                // The confirm state's buttons are new elements, so the
                // panel must take focus again for Tab to reach them.
                view.modal_needs_focus = true;
                cx.notify();
            });
        })
        .detach();
    }

    /// The confirm button of the recovery modal's adopt state
    /// (final-review MINOR-1).
    ///
    /// Synchronous end to end — `write_pointer` is a small atomic TOML
    /// write, the same deliberately-synchronous posture `apply_context`
    /// takes with `AppConfig::load` — so there is no await between reading
    /// `pending` and acting on it, and therefore nothing to re-verify.
    /// That is the whole reason the confirm lives here rather than in
    /// another background task.
    fn confirm_workspace_recovery(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::WorkspaceMissing { pending: Some(root), .. }) =
            &self.modal
        else {
            return;
        };
        let root = root.clone();
        if let Err(e) =
            dbc_state::workspace::write_pointer(&dbc_state::workspace::pointer_path(), &root)
        {
            // Back to the confirm screen with the failure in place: the
            // pointer is untouched, so „Zpět" and „Použít lokální profil"
            // both still mean exactly what they did.
            self.set_workspace_missing_error(e.message, cx);
            return;
        }
        self.close_modal(cx);
        self.apply_context(Some(root), cx);
    }

    /// „Zpět" on the recovery modal's adopt state — returns to the three
    /// choices. NOT a close: `WorkspaceMissing` is the one modal the user
    /// cannot dismiss (§W4), and the whole point of putting the confirm
    /// INSIDE it rather than handing the screen to an Esc-closable
    /// `WorkspaceConfirm` is that cancelling can never leave the app with
    /// no context and no dialog. The picked folder is dropped; nothing was
    /// written, so there is nothing to undo.
    fn cancel_workspace_recovery(&mut self, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::WorkspaceMissing { pending, error, .. }) =
            &mut self.modal
        {
            *pending = None;
            *error = None;
        }
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// Replaces the `WorkspaceMissing` modal's in-place error line. A no-op
    /// if the modal is gone (the user quit, or an earlier re-pick already
    /// succeeded) — never reopens it, never writes over another modal.
    fn set_workspace_missing_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::WorkspaceMissing { error, .. }) = &mut self.modal {
            *error = Some(message);
        }
        cx.notify();
    }

    /// Design §W3.1 — THE common gate in front of EVERY context change
    /// (init, adopt, „Přejít na lokální profil"). A context replacement
    /// demands a quiet app: the same gate style as `start_script_pick`.
    /// Returns the Czech refusal to show, or `None` to proceed.
    ///
    /// This function reads the live state; the DECISION (and its
    /// precedence) lives in the pure `connections_ui::context_switch_refusal`
    /// so it can be unit-pinned without a GPUI window — the
    /// `modal_is_blocking` / `pwchange::esc_closable` precedent. The two
    /// together are ONE gate, not two: nothing else in the app may answer
    /// „is it safe to switch".
    ///
    /// Workspace T8 filled the EXTENSION POINT this comment used to
    /// reserve: the dirty-`script_binding` arm (Part S §5.5's guard) is now
    /// the second parameter below. There remains exactly ONE gate — a
    /// second „is it safe to switch" predicate is a review-blocking defect.
    ///
    /// `script_dirty_flag` (not `script_is_dirty(cx)`) because this
    /// function takes no `cx` by design — see that field's doc comment for
    /// why one frame of staleness is safe in the only direction that
    /// matters.
    pub(crate) fn context_switch_blocked(&self) -> Option<String> {
        connections_ui::context_switch_refusal(
            self.cancel.is_some(),
            self.script_binding.is_some() && self.script_dirty_flag,
            self.apply_dialog.is_some() || self.discard_confirm.is_some(),
            // The switch flow's OWN modals (Settings, and the confirm
            // itself) do not count as "some other dialog" — see
            // `modal_blocks_context_switch`. Every other open dialog does
            // (single-modal invariant, app-wide).
            connections_ui::modal_blocks_context_switch(self.modal.as_ref()),
        )
        .map(str::to_string)
    }

    /// §W3: „Použít složku…". Gates first, then picks, then classifies in
    /// the background, then opens the confirm modal. NOTHING is written
    /// before the user clicks the confirm button — the classification is a
    /// read-only `read_dir` (§W6.4: nothing under `.git/` is ever opened).
    fn start_workspace_pick(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.context_switch_blocked() {
            self.status = format!("error: {reason}");
            cx.notify();
            return;
        }
        // A new attempt supersedes the previous one's refusal text.
        self.workspace_pick_error = None;
        // T5 review MAJOR-1/MINOR-3: capture the generation this pick
        // belongs to, exactly as `pick_workspace_for_recovery` does. The
        // picker is modal to the app but the `classify()` below is NOT —
        // it yields the UI thread, and Settings is Esc-closable, so every
        // continuation arm from here on is a potential STALE write.
        let my_generation = self.workspace_pick_generation;
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Použít".into()),
        });
        cx.spawn(async move |this, cx| {
            let picked = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_pick_status(my_generation, "výběr zrušen".into(), cx);
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_pick_status(
                            my_generation,
                            format!("error: dialog selhal: {e}"),
                            cx,
                        );
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.set_workspace_pick_status(
                            my_generation,
                            "error: dialog není dostupný".into(),
                            cx,
                        );
                    });
                    return;
                }
            };
            let probe = picked.clone();
            let outcome = cx
                .background_spawn(async move {
                    connections_ui::workspace_pick_outcome(dbc_state::workspace::classify(&probe))
                })
                .await;
            let _ = this.update(cx, |view, cx| match outcome {
                Ok(mode) => view.open_workspace_confirm(mode, Some(picked), my_generation, cx),
                // T5 review MINOR-2: the prose goes into the Settings
                // panel, the status bar keeps a short sentinel.
                Err(e) => {
                    if view.workspace_pick_generation != my_generation {
                        return; // superseded — silent, like the Ok arm
                    }
                    view.workspace_pick_error = Some(e);
                    view.status = connections_ui::WORKSPACE_PICK_FAILED_STATUS.to_string();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A status write from `start_workspace_pick`'s continuation
    /// (T5 review MINOR-3). Silent when superseded: a context swap means
    /// the user has already reached a newer decision, and „výběr zrušen"
    /// landing over „pracovní prostor: D:\ws" would be a stale write over
    /// it. Same posture as `set_workspace_missing_error`'s no-op arm.
    fn set_workspace_pick_status(
        &mut self,
        dispatched_generation: u64,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if self.workspace_pick_generation != dispatched_generation {
            return;
        }
        self.status = message;
        cx.notify();
    }

    /// §W3.4's reverse switch — same gate, same confirm shape. Nothing in
    /// the workspace folder is read or written: only the pointer goes.
    fn start_leave_workspace(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.context_switch_blocked() {
            self.status = format!("error: {reason}");
            cx.notify();
            return;
        }
        // Synchronous from the Settings click — nothing can have gone
        // stale, so this pick's generation IS the current one.
        let my_generation = self.workspace_pick_generation;
        self.open_workspace_confirm(
            connections_ui::WorkspaceConfirmMode::ToProfile,
            None,
            my_generation,
            cx,
        );
    }

    /// Opens the confirm modal OVER the Settings modal it was started
    /// from — behind the `workspace_pick_verdict` guard (T5 review
    /// MAJOR-1). The precondition this function's older comment merely
    /// ASSERTED is now CHECKED: the raw `self.modal = Some(..)` below is a
    /// destructive write, and a stale classification landing on a
    /// `WorkspaceConfirm { running: true }`, a `BackupRestore`, or a
    /// half-typed `ConnectionDialog` would silently destroy it.
    fn open_workspace_confirm(
        &mut self,
        mode: connections_ui::WorkspaceConfirmMode,
        root: Option<PathBuf>,
        dispatched_generation: u64,
        cx: &mut Context<Self>,
    ) {
        let settings_open = matches!(self.modal, Some(connections_ui::ModalState::Settings));
        match workspace_pick_verdict(
            settings_open,
            dispatched_generation,
            self.workspace_pick_generation,
        ) {
            WorkspacePickVerdict::Open => {}
            // The user has already reached a newer, explicit decision.
            WorkspacePickVerdict::Superseded => return,
            WorkspacePickVerdict::OtherDialog => {
                self.status = connections_ui::WORKSPACE_PICK_DISCARDED.to_string();
                cx.notify();
                return;
            }
        }
        // Now verified: exactly one modal is open and it is Settings, the
        // one this flow started from — replacing it in place keeps the
        // single-modal invariant.
        self.modal = Some(connections_ui::ModalState::WorkspaceConfirm {
            mode,
            root,
            error: None,
            running: false,
        });
        // UX-polish §1.4: no-input modal, cx-only opener.
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// The confirm button of `ModalState::WorkspaceConfirm`. The order is
    /// the design's, and the order MATTERS: files first, marker last
    /// (inside `init_workspace`), pointer only after that returns `Ok`,
    /// live swap only after the pointer is on disk. A failure at any step
    /// leaves the PREVIOUS context fully intact and the error in the modal.
    fn confirm_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::WorkspaceConfirm { mode, root, running, .. }) =
            &mut self.modal
        else {
            return;
        };
        if *running {
            return; // double-click guard, `KillConfirm::dispatched`'s role
        }
        let (mode, root) = (*mode, root.clone());
        // Re-run the gate: the pick + classification did not block the app,
        // so a query or a dialog may have started in the meantime.
        if let Some(reason) = self.context_switch_blocked() {
            self.set_workspace_confirm_error(reason, cx);
            return;
        }
        if let Some(connections_ui::ModalState::WorkspaceConfirm { running, .. }) = &mut self.modal
        {
            *running = true;
        }
        cx.notify();
        // §W3.2 step 1: init copies from the PROFILE, always — workspace
        // mode offers no picker (§W3), so the profile is the only possible
        // origin. `from` is captured here, on the UI thread, and moved into
        // the background job.
        let from = dbc_state::workspace::profile_paths();
        let pointer = dbc_state::workspace::pointer_path();
        cx.spawn(async move |this, cx| {
            let job_root = root.clone();
            let result: Result<(), String> = cx
                .background_spawn(async move {
                    match mode {
                        connections_ui::WorkspaceConfirmMode::Init => {
                            let root = job_root.ok_or("chybí cílová složka")?;
                            // Copies + scripts/ + .gitignore + MARKER LAST.
                            // Every write inside goes through the shared
                            // rails (`fsutil::write_atomic` /
                            // `join_component` / `entry_exists_ci`) — this
                            // call site must NEVER grow its own copy loop.
                            dbc_state::workspace::init_workspace(&root, &from)
                                .map_err(|e| e.message)?;
                            dbc_state::workspace::write_pointer(&pointer, &root)
                                .map_err(|e| e.message)
                        }
                        connections_ui::WorkspaceConfirmMode::Adopt => {
                            let root = job_root.ok_or("chybí cílová složka")?;
                            // §W3.3: NOTHING is written but the pointer.
                            dbc_state::workspace::write_pointer(&pointer, &root)
                                .map_err(|e| e.message)
                        }
                        connections_ui::WorkspaceConfirmMode::ToProfile => {
                            // §W3.4: the folder is not touched in any way.
                            dbc_state::workspace::clear_pointer(&pointer).map_err(|e| e.message)
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(()) => {
                    view.close_modal(cx);
                    // THE single seam (§W3.4). It bumps
                    // `workspace_pick_generation`, so any „Najít složku…"
                    // continuation still in flight is superseded by this.
                    view.apply_context(root.clone(), cx);
                }
                Err(e) => {
                    // The previous context is untouched: `apply_context`
                    // was never reached, and nothing partial was pointed
                    // at (the pointer is written LAST inside the job).
                    view.set_workspace_confirm_error(e, cx);
                }
            });
        })
        .detach();
    }

    /// „Zrušit" on `ModalState::WorkspaceConfirm`. Refuses while the
    /// init/pointer write is in flight, for the same reason Esc does
    /// (`connections_ui::workspace_confirm_esc_closable`): the background
    /// job's success arm calls `apply_context`, so letting the modal close
    /// mid-write would change the whole working context AFTER the user
    /// asked to cancel — a silent context change, which this design bans.
    /// Nothing is lost by waiting: the write is a handful of file copies,
    /// and a FAILED one leaves the previous context fully intact.
    fn cancel_workspace_confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::WorkspaceConfirm { running, .. }) = &self.modal {
            if !connections_ui::workspace_confirm_esc_closable(*running) {
                return;
            }
        }
        self.close_modal(cx);
    }

    /// Replaces the `WorkspaceConfirm` modal's in-place error line and
    /// releases its `running` guard. A no-op if the modal is gone (the user
    /// cancelled while the write was still pending) — never reopens it,
    /// never writes over another modal.
    fn set_workspace_confirm_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::WorkspaceConfirm { error, running, .. }) =
            &mut self.modal
        {
            *running = false;
            *error = Some(message);
        }
        cx.notify();
    }

    /// „Použít lokální profil" — the EXPLICIT user action design §W4
    /// contrasts with a silent fallback: it deletes the pointer (so the
    /// next start is plain profile mode) and swaps the live context. The
    /// workspace folder itself is not touched in any way.
    fn use_local_profile(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = dbc_state::workspace::clear_pointer(&dbc_state::workspace::pointer_path()) {
            self.set_workspace_missing_error(e.message, cx);
            return;
        }
        self.close_modal(cx);
        self.apply_context(None, cx);
    }

    /// Design §W3.4 — the live, in-place context swap. THE single seam:
    /// „Najít složku…" (§W4), „Použít lokální profil" (§W4), init (§W3.2),
    /// adopt (§W3.3) and „Přejít na lokální profil" all end here.
    ///
    /// PRECONDITIONS the caller owns: the §W3.1 gates have passed (no run
    /// in flight, no pending apply/discard, no dirty script — T5's
    /// `context_switch_blocked`) and the pointer file has already been
    /// written or cleared. This fn performs no I/O beyond loading the NEW
    /// context's stores, and never deletes, moves, or rewrites anything in
    /// the OLD one (never-destructive rail).
    ///
    /// `AppConfig::load` runs on the UI thread here, exactly as it does in
    /// `fn main()` — a small TOML read, deliberately synchronous so the
    /// swap is atomic from the user's point of view (no frame in which the
    /// paths are new but the connections are still the old ones).
    pub(crate) fn apply_context(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        let paths = match &root {
            Some(r) => dbc_state::workspace::workspace_paths(r),
            None => dbc_state::workspace::profile_paths(),
        };
        // §W3.1: the connection list itself is about to change — keeping a
        // session from the OLD context alive under the NEW config is
        // exactly the silent context mixing this design bans.
        self.clear_active_connection(cx);
        // §W3.4: a workspace vault is a DIFFERENT file; the session unlock
        // must not carry over. The existing lazy prompt re-fires on the
        // next secret use, at most once per run.
        self.vault = None;
        self.config_path = paths.config.clone();
        self.vault_path = paths.vault.clone();
        let (config, config_load_error) = match AppConfig::load(&paths.config) {
            Ok(c) => (c, None),
            Err(e) => (AppConfig::default(), Some(e.to_string())),
        };
        self.config = config;
        self.config_load_error = config_load_error;
        // Existing degrade-to-None postures, unchanged.
        self.view_prefs = ViewPrefsStore::load(&paths.views).ok();
        self.param_values = ParamValuesStore::load(&paths.params).ok();
        // history: NOT touched — machine-local in both modes (§W5).
        self.workspace_root = root.clone();
        // T4 review MAJOR-1: every swap supersedes any „Najít složku…"
        // continuation still in flight, so a stale pick can never override
        // the choice the user just made (`recovery_pick_may_commit`).
        self.workspace_pick_generation = self.workspace_pick_generation.wrapping_add(1);
        // T4 review MINOR-6 (as-built addendum to §W3.4, which listed only
        // the scripts tree): drop the OLD context's fetched database lists
        // and schema snapshots BEFORE `sync_connections` re-seeds the map.
        // `sync_connections` keeps every cached entry whose connection id
        // still exists — and because §W3.2 initialises a workspace by
        // copying `config.toml` verbatim, the two contexts share ids by
        // construction, so without this the sidebar would render context
        // A's databases and schemas under context B's identically-id'd
        // connection. Display-only (queries rebuild from the new config),
        // but a wrong-context display is still a wrong context.
        self.tree.update(cx, |t, cx| t.reset_fetched_context(cx));
        self.refresh_grouped_cache(cx);
        self.refresh_tree_context(cx);
        // §W3.4: the scripts root just changed under us. Clear to
        // `NotLoaded` (dropping stale expand keys and any in-flight scan of
        // the OLD root — `reset_scripts` bumps the generation), then
        // rescan the new one.
        self.tree.update(cx, |t, cx| t.reset_scripts(cx));
        self.start_scripts_scan(cx);
        // T8 review MINOR-3: the script binding is context state too, and
        // nothing else drops it. `context_switch_blocked` refuses a swap
        // only while the binding is DIRTY, so a CLEAN one would ride into
        // the new workspace: the caption would silently degrade from
        // `prod/trzby.sql` to a bare `trzby.sql` (the path no longer sits
        // under the new root) and the next Ctrl+S would write back into the
        // OLD workspace folder — precisely the cross-context leak §W3.4
        // exists to prevent. The editor TEXT stays, exactly as „Zavřít"
        // leaves it (§5.3): it is the binding that belongs to the old
        // context, not the buffer.
        //
        // T8 re-verify NEW MAJOR: the unbind alone was NOT enough, and the
        // claim that it "supersedes any in-flight open/save" was false
        // whenever there was nothing bound — `set_script_binding`'s
        // path-changed heuristic bumps nothing for `None -> None`, and an
        // unbound editor is never dirty, so the gate lets the swap through
        // in exactly that state. An `open_script` dispatched against the
        // OLD root would then land, pass both re-checks unchanged, and
        // bind the old workspace's file. The bump is therefore explicit and
        // unconditional; see `supersede_script_continuations`.
        self.set_script_binding(None);
        self.supersede_script_continuations();
        self.status = match &root {
            Some(r) => format!("pracovní prostor: {}", r.display()),
            None => "lokální profil obnoven".to_string(),
        };
        if let Some(detail) = self.config_load_error.clone() {
            self.status =
                format!("error: config.toml je poškozený – oprav nebo smaž soubor ({detail})");
        }
        cx.notify();
    }

    /// §W3.1's „Aktivní připojení bude odpojeno." made real. The app keeps
    /// no persistent session (the runner is per-operation — sidebar design
    /// fact 0.1), so disconnecting IS dropping the active identity and
    /// bumping `switch_generation` so an in-flight switch's result can
    /// never land in the NEW context. The CLI-arg root goes too: it belongs
    /// to the old context and, per the sidebar design, cannot come back.
    fn clear_active_connection(&mut self, cx: &mut Context<Self>) {
        self.active_connection_id = None;
        self.active_database = None;
        self.conn_url = None;
        self.switch_generation = self.switch_generation.wrapping_add(1);
        self.dropdown_open = false;
        cx.notify();
    }

    /// The old trigger_schema_fetch success-arm's M2-guarded admin-schema
    /// push, verbatim (AUDIT SITE — design §7 guard list: "only push into
    /// an admin panel whose OWN stamped identity still matches the
    /// CURRENTLY active connection").
    fn push_admin_schemas_if_matching(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.tree.read(cx).snapshot() else { return };
        let schemas = admin_panel::distinct_schemas(snapshot);
        if let Some(panel) = self.tabs.iter().find_map(|t| match &t.content {
            TabContent::Admin { view } => Some(view.clone()),
            _ => None,
        }) {
            let current_identity = self.current_conn_identity();
            if conn_identity_matches(panel.read(cx).conn_identity(), &current_identity) {
                panel.update(cx, |p, cx| p.set_schemas(schemas, cx));
            }
        }
    }

    /// Expand of a Connection row (or its error-row retry / vault resume).
    /// Design §1.2: NOT eager — one bounded metadata fetch over one
    /// short-lived connection to the DEFAULT database; no other connection
    /// is touched, no schema is fetched yet.
    fn start_db_list_fetch(&mut self, conn_id: String, cx: &mut Context<Self>) {
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == conn_id).cloned() else {
            return;
        };
        let needs_secret = !connections_ui::engine_is_file_based(cfg.engine);
        if connections_ui::connect_needs_vault_prompt(
            needs_secret,
            self.vault.is_some(),
            Vault::exists(&self.vault_path),
        ) {
            self.open_vault_prompt(connections_ui::PendingAfterUnlock::ExpandConnection(conn_id), cx);
            return;
        }
        let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
        self.sidebar_fetch_generation += 1;
        let my_generation = self.sidebar_fetch_generation;
        let default_db = cfg.database.clone();
        self.tree.update(cx, |t, cx| t.begin_db_list(&conn_id, my_generation, cx));
        let rx = self.runner.fetch_database_list(spec_for_database(&cfg, &cfg.database, secret));
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                let result = match result {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("výpis databází zrušen".to_string()),
                };
                view.tree.update(cx, |t, cx| {
                    t.finish_db_list(&conn_id, my_generation, result, &default_db, cx)
                });
            });
        })
        .detach();
    }

    /// Expand of a Database row / ⟳ refresh of the active slot / the
    /// switch success arm. CLI slot: `conn_id == CLI_CONN_IDENTITY`, db "".
    fn start_schema_slot_fetch(&mut self, conn_id: String, db: String, cx: &mut Context<Self>) {
        self.sidebar_fetch_generation += 1;
        let my_generation = self.sidebar_fetch_generation;
        let spec = if conn_id == CLI_CONN_IDENTITY {
            let Some(url) = self.conn_url.clone() else { return };
            ConnectSpec::Url(url)
        } else {
            let Some(cfg) = self.config.connections.iter().find(|c| c.id == conn_id).cloned() else {
                return;
            };
            let needs_secret = !connections_ui::engine_is_file_based(cfg.engine);
            if connections_ui::connect_needs_vault_prompt(
                needs_secret,
                self.vault.is_some(),
                Vault::exists(&self.vault_path),
            ) {
                // Design §4.4 + resolved deviation 9: the vault can lock
                // BETWEEN expanding a connection and expanding one of its
                // databases — never fetch with an empty secret fallback.
                self.open_vault_prompt(
                    connections_ui::PendingAfterUnlock::LoadDbSchema { conn_id, db },
                    cx,
                );
                return;
            }
            let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
            spec_for_database(&cfg, &db, secret)
        };
        self.tree.update(cx, |t, cx| t.begin_schema(&conn_id, &db, my_generation, cx));
        let rx = self.runner.fetch_schema(spec);
        let started = std::time::Instant::now();
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                let result = match result {
                    Ok(Ok(s)) => Ok(s),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("fetch zrušen".to_string()),
                };
                let ok = result.is_ok();
                {
                    use dbc_state::applog::{log, Event};
                    match &result {
                        Ok(s) => log(Event::SchemaLoaded {
                            conn: conn_id.clone(),
                            db: Some(db.clone()),
                            tables: s.tables.len(),
                            ms: started.elapsed().as_millis() as u64,
                        }),
                        Err(e) => log(Event::SchemaFailed {
                            conn: conn_id.clone(),
                            db: Some(db.clone()),
                            error: e.clone(),
                        }),
                    }
                }
                view.tree
                    .update(cx, |t, cx| t.finish_schema(&conn_id, &db, my_generation, result, cx));
                // The old trigger_schema_fetch success-arm side effects,
                // ACTIVE slot only:
                if ok && view.scope_is_active(&conn_id, &db) {
                    view.refresh_tree_context(cx);
                    view.push_admin_schemas_if_matching(cx); // M2 guard preserved verbatim (audit site!)
                    // Review round 3, MAJOR 1 (carried forward): a new
                    // snapshot invalidates an open autocomplete popup's
                    // candidates.
                    view.close_autocomplete(cx);
                }
            });
        })
        .detach();
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
        let conn_id = dbc_state::connection_scope_key(&conn_id, self.active_database.as_deref());
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
        let conn_id = dbc_state::connection_scope_key(&conn_id, self.active_database.as_deref());
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
                let dialect = self.active_engine().map(sql_dialect).unwrap_or(dbc_core::Dialect::Postgres);
                let sql = fk_join::build_join_sql(dialect, schema.as_deref(), table, joins);
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
                    // UX-polish §1.4: no-input prompt, cx-only site — defer
                    // focus to `AppView::render` via `modal_needs_focus`.
                    self.modal_needs_focus = true;
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
            GridEvent::ImportCsvRequested { schema, table } => {
                // CURATION item 4(b): belt-and-braces re-check above the
                // button's own gating (`csv_import_enabled` set only for a
                // non-read-only preview) AND the runner's own up-front
                // guard in `run_csv_import`.
                if self.active_read_only() {
                    self.status = "error: připojení je jen pro čtení".to_string();
                    cx.notify();
                    return;
                }
                self.start_csv_import(schema.clone(), table.clone(), cx);
            }
            GridEvent::OpenChart => self.open_chart_picker(Some(emitter.clone()), cx),
        }
    }

    // -----------------------------------------------------------------
    // G14 T11: chart tab wiring (design §2.1/§2.4). Read-only over an
    // already-materialized `ResultBuffer` snapshot — no execute() surface.
    // -----------------------------------------------------------------

    /// Opens the axis picker — from the grid toolbar's "Graf" button
    /// (`from_grid = Some(emitter)`) or the palette's "Graf z výsledku"
    /// (`from_grid = None`, uses the active tab). Single-modal invariant,
    /// same guard every other opener in `connections_ui.rs` applies.
    fn open_chart_picker(&mut self, from_grid: Option<Entity<ResultGrid>>, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            self.status = "zavřete nejprve otevřený dialog".into();
            cx.notify();
            return;
        }
        // Resolve the source tab: the one owning the emitting grid Entity, or
        // the active tab (palette path). Entity<T> is comparable by identity.
        let source = self
            .tabs
            .iter()
            .find(|t| match (&t.content, &from_grid) {
                (TabContent::Grid { grid, .. }, Some(g)) => grid == g,
                (TabContent::Grid { .. }, None) => Some(t.id) == self.tabs.active().map(|a| a.id),
                _ => false,
            })
            .map(|t| {
                (
                    t.title.clone(),
                    match &t.content {
                        TabContent::Grid { buffer, .. } => buffer.clone(),
                        _ => unreachable!("matched Grid above"),
                    },
                )
            });
        let Some((source_title, buffer)) = source else {
            self.status = "graf lze vytvořit jen z výsledkové mřížky".into();
            cx.notify();
            return;
        };
        // design §2.1: the exact is_numeric scan the preview-editability
        // path already uses above (`numeric_cols`).
        let columns: Vec<(String, bool)> = buffer
            .borrow()
            .schema()
            .fields()
            .iter()
            .map(|f| (f.name().clone(), f.data_type().is_numeric()))
            .collect();
        if !columns.iter().any(|(_, numeric)| *numeric) {
            self.status = "výsledek nemá žádný číselný sloupec — graf nelze vytvořit".into();
            cx.notify();
            return;
        }
        let n = columns.len();
        // default: first numeric column pre-checked as Y, column 0 as X.
        let mut y_selected = vec![false; n];
        if let Some(i) = columns.iter().position(|(_, num)| *num) {
            y_selected[i] = true;
        }
        self.modal = Some(connections_ui::ModalState::ChartPicker {
            source_title,
            buffer,
            columns,
            kind: chart_data::ChartKind::Bar,
            x_col: 0,
            y_selected,
            edit_tab: None,
        });
        // UX-polish §1.4: no-input modal, cx-only opener — defer focus to
        // `AppView::render` via `modal_needs_focus`.
        self.modal_needs_focus = true;
        cx.notify();
    }

    /// "Vytvořit graf"/"Použít" — validates BEFORE taking the modal (an
    /// invalid pick leaves the dialog open untouched with a status nudge,
    /// never a half-configured chart), then either opens a new Chart tab or
    /// reconfigures the tab named by `edit_tab` in place (design §2.4).
    fn confirm_chart_picker(&mut self, cx: &mut Context<Self>) {
        let valid = matches!(
            &self.modal,
            Some(connections_ui::ModalState::ChartPicker { y_selected, .. })
                if y_selected.iter().any(|on| *on)
        );
        if !valid {
            self.status = "vyberte alespoň jeden číselný sloupec pro osu Y".into();
            cx.notify();
            return;
        }
        let Some(connections_ui::ModalState::ChartPicker {
            source_title,
            buffer,
            columns: _,
            kind,
            x_col,
            y_selected,
            edit_tab,
        }) = self.modal.take()
        else {
            return;
        };
        let y_cols: Vec<usize> =
            y_selected.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect();
        match edit_tab {
            Some(id) => {
                // re-pick: reconfigure the existing tab's view in place (§2.4)
                let view = self.tabs.iter().find_map(|t| {
                    (t.id == id).then_some(()).and_then(|()| match &t.content {
                        TabContent::Chart { view } => Some(view.clone()),
                        _ => None,
                    })
                });
                if let Some(view) = view {
                    view.update(cx, |v, cx| v.reconfigure(kind, x_col, y_cols, cx));
                }
            }
            None => {
                let view = cx.new(|_| {
                    chart_view::ChartView::new(buffer, kind, x_col, y_cols, source_title.clone())
                });
                cx.subscribe(&view, Self::on_chart_view_event).detach();
                let conn_identity = self.current_conn_identity();
                self.tabs.open(ResultTab {
                    id: 0, // Tabs::open assigns
                    title: tabs::collapse_title(&format!("Graf: {source_title}")),
                    pinned: false,
                    preview_key: None, // stacked like ad-hoc tabs, Plan precedent
                    conn_identity,
                    content: TabContent::Chart { view },
                });
            }
        }
        cx.notify();
    }

    /// `ChartView`'s only event — "Upravit…" clicked. Reopens the picker
    /// seeded from that view's current pick, editing that tab's `ChartView`
    /// in place on confirm (design §2.4's only interaction).
    fn on_chart_view_event(
        &mut self,
        emitter: Entity<chart_view::ChartView>,
        _event: &chart_view::ChartViewEvent,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            self.status = "zavřete nejprve otevřený dialog".into();
            cx.notify();
            return;
        }
        let Some(tab_id) = self
            .tabs
            .iter()
            .find(|t| matches!(&t.content, TabContent::Chart { view } if view == &emitter))
            .map(|t| t.id)
        else {
            return;
        };
        let (kind, x_col, y_cols) = emitter.read(cx).picker_seed();
        let (source_title, buffer) = {
            let v = emitter.read(cx);
            (v.source_title().to_string(), v.buffer_handle())
        };
        let columns: Vec<(String, bool)> = buffer
            .borrow()
            .schema()
            .fields()
            .iter()
            .map(|f| (f.name().clone(), f.data_type().is_numeric()))
            .collect();
        let mut y_selected = vec![false; columns.len()];
        for c in &y_cols {
            if let Some(flag) = y_selected.get_mut(*c) {
                *flag = true;
            }
        }
        self.modal = Some(connections_ui::ModalState::ChartPicker {
            source_title,
            buffer,
            columns,
            kind,
            x_col,
            y_selected,
            edit_tab: Some(tab_id),
        });
        // UX-polish §1.4: no-input modal, cx-only opener — defer focus to
        // `AppView::render` via `modal_needs_focus`.
        self.modal_needs_focus = true;
        cx.notify();
    }

    // -----------------------------------------------------------------
    // G5 Task 4: dirty-edit discard guard (folded T3 review issue 2).
    // -----------------------------------------------------------------

    /// Row-granular staged-change count for `tab`, if it has any — `None`
    /// for a `Text`/other non-editable tab or a clean `Grid`/`Admin` tab
    /// (both are safe to proceed past without a confirm prompt). Shared by
    /// the two lookup helpers below.
    ///
    /// `Admin` reuses `AdminPanel::change_count` verbatim — the SAME
    /// dirtiness definition that already drives the panel's own Apply bar
    /// and sub-nav discard-confirm prompt, not a second one invented here
    /// (review finding: closing a dirty admin tab via "✕" used to silently
    /// discard staged writes, since this match had no `Admin` arm at all).
    fn grid_dirty_change_count(tab: &ResultTab, cx: &Context<Self>) -> Option<usize> {
        let n = match &tab.content {
            TabContent::Grid { grid, .. } => grid.read(cx).edit_state.change_count(),
            TabContent::Admin { view } => view.read(cx).change_count(),
            _ => return None,
        };
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
        let dialect = self.active_engine().map(sql_dialect).unwrap_or(dbc_core::Dialect::Postgres);
        let sql = preview_sql(dialect, schema.as_deref(), &table);
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
            // UX-polish §1.4: no-input prompt, cx-only site — defer focus
            // to `AppView::render` via `modal_needs_focus`.
            self.modal_needs_focus = true;
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
            // Sidebar rework (resolved deviation 11): closing the dirty
            // admin tab IS the "drop staged edits" the user just confirmed
            // (matches `AdminOpenDecision::Replace`'s posture) — and it is
            // what lets the re-entered `switch_to_database` sail past its
            // own `dirty_admin_change_count` check without looping.
            PendingDiscard::SwitchDatabase { conn_id, db, follow_up } => {
                let admin_tab_id = self
                    .tabs
                    .iter()
                    .find_map(|t| matches!(&t.content, TabContent::Admin { .. }).then_some(t.id));
                if let Some(id) = admin_tab_id {
                    self.tabs.close(id);
                }
                self.switch_to_database(&conn_id, db, follow_up, cx);
            }
            // Workspace T8 (Part S §5.5): the user confirmed dropping the
            // script's unsaved changes — perform what was parked.
            // RE-VERIFY: the user has just agreed to lose the script's
            // unsaved changes. That answer is the one thing
            // `editor_guard::with_editor_replaceable` cannot read off live
            // state, so it is recorded here — stamped with the generation
            // it was given at, so it expires the moment the binding moves.
            PendingDiscard::Script(action) => {
                self.editor_discard_grant = Some(self.script_binding_generation);
                self.perform_script_action(action, cx);
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
    /// Sidebar rework: composes via `conn_identity_for` — a database switch
    /// on the SAME connection now changes the identity, which is the
    /// audit's headline fix (design §7).
    fn current_conn_identity(&self) -> String {
        match &self.active_connection_id {
            None => CLI_CONN_IDENTITY.to_string(),
            Some(id) => {
                // A deleted-while-active connection (rare, transient) falls
                // back to the empty db component — still a stable, unequal-
                // to-everything-real identity, same posture as the old raw
                // id fallback.
                let db = self.effective_database().unwrap_or_default();
                conn_identity_for(id, &db)
            }
        }
    }

    /// The database the active context points at: `active_database`, or
    /// the saved config's default. `None` = no active saved connection.
    fn effective_database(&self) -> Option<String> {
        let id = self.active_connection_id.as_ref()?;
        if let Some(db) = &self.active_database {
            return Some(db.clone());
        }
        self.config.connections.iter().find(|c| &c.id == id).map(|c| c.database.clone())
    }

    /// Store bucket key for view_prefs/params (design §7 items 4–5):
    /// LEGACY bare id for the default database — existing views.toml/
    /// params.toml entries keep working byte-for-byte — one more `\u{1F}`
    /// component only for a non-default db; `"cli"` sentinel for the CLI
    /// path. Deliberately NOT `current_conn_identity()`: embedding the
    /// composite identity would orphan every pre-phase stored value.
    fn store_scope_key(&self) -> String {
        match &self.active_connection_id {
            Some(id) => dbc_state::connection_scope_key(id, self.active_database.as_deref()),
            None => CLI_CONN_IDENTITY.to_string(),
        }
    }

    /// Human-readable name for a `ResultTab::conn_identity` value — used
    /// only in the Apply flow's mismatch error text ("changes came from
    /// connection X"). Falls back to the raw identity string itself if the
    /// connection has since been deleted (rare, but must never panic or
    /// silently say "cli" for a real connection that's simply gone).
    /// Sidebar rework: splits on `\u{1F}`; the db segment renders only when
    /// ≠ the connection's current default. Delegates to the pure
    /// `conn_name_for_identity_from` (testable without a GPUI context).
    fn conn_name_for_identity(&self, identity: &str) -> String {
        conn_name_for_identity_from(&self.config.connections, identity)
    }

    // -----------------------------------------------------------------
    // G9 T6: server-monitor tab — open action, timer loop, kill-dialog
    // bridge (design §4/§6/§7).
    // -----------------------------------------------------------------

    /// The ACTIVE connection's engine: saved config's `cfg.engine`, or
    /// `engine_from_url` for the CLI-arg back-compat path, `None` with no
    /// active connection at all (design §7's three-way gating input).
    fn active_engine(&self) -> Option<dbc_state::Engine> {
        if let Some(id) = &self.active_connection_id {
            return self.config.connections.iter().find(|c| &c.id == id).map(|c| c.engine);
        }
        self.conn_url.as_deref().map(engine_from_url)
    }

    /// G12 T4: `cfg.read_only` for the active saved connection, or `false`
    /// for the CLI-arg URL path (no read-only concept there — same
    /// convention `run_query_with`'s own spec resolution applies) and for
    /// "no active connection at all" (nothing to gate CSV import against
    /// yet). Feeds `SchemaTree::set_read_only` (the tree's ⇪ affordance)
    /// and `grid.rs`'s `csv_import_enabled` flag.
    fn active_read_only(&self) -> bool {
        if let Some(id) = &self.active_connection_id {
            return self.config.connections.iter().find(|c| &c.id == id).is_some_and(|c| c.read_only);
        }
        false
    }

    // -----------------------------------------------------------------
    // G10 T4: "Správa serveru" admin tab — open/singleton-dedup, catalog
    // fetch, panel-event dispatch (design §2/§5).
    // -----------------------------------------------------------------

    /// Tree row click (`TreeEvent::OpenAdmin`), palette action
    /// (`PaletteAction::OpenServerAdmin`) — both funnel through here.
    /// Re-checks `admin_entry_state` itself (belt-and-braces with the
    /// runner's shared `guard_not_read_only`, and with the tree/palette's
    /// own gating that should already prevent reaching this with a
    /// Hidden/Disabled entry) before doing anything. Singleton-per-
    /// connection dedup via `admin_open_decision` (pure, unit-tested): same
    /// connection → re-focus the existing tab (staged edits preserved);
    /// different connection → close the stale tab first (its staged admin
    /// edits must never survive a connection switch) and open fresh.
    fn open_admin_tab(&mut self, cx: &mut Context<Self>) {
        let engine = self.active_engine();
        let read_only = self.active_read_only();
        if admin_panel::admin_entry_state(engine, read_only) != admin_panel::AdminEntry::Enabled {
            self.status = "správa serveru není pro toto připojení dostupná".to_string();
            cx.notify();
            return;
        }
        let engine = engine.expect("Enabled implies an engine");
        let identity = self.current_conn_identity();
        match admin_open_decision(&self.tabs, &identity) {
            AdminOpenDecision::Activate(id) => {
                self.tabs.activate(id);
                cx.notify();
            }
            AdminOpenDecision::Replace(id) => {
                self.tabs.close(id);
                self.open_fresh_admin_tab(engine, identity, cx);
            }
            AdminOpenDecision::OpenFresh => {
                self.open_fresh_admin_tab(engine, identity, cx);
            }
        }
    }

    /// The "open a brand-new admin tab" half of `open_admin_tab`, shared by
    /// its `Replace`/`OpenFresh` arms — subscribes to the new panel's
    /// `AdminEvent`s and kicks off its first catalog fetch (Roles, the
    /// sub-view every panel opens on).
    fn open_fresh_admin_tab(&mut self, engine: dbc_state::Engine, identity: String, cx: &mut Context<Self>) {
        let panel = cx.new(|cx| admin_panel::AdminPanel::new(engine, identity.clone(), cx));
        cx.subscribe(&panel, Self::on_admin_event).detach();
        self.tabs.open(ResultTab {
            id: 0,
            title: "Správa serveru".to_string(),
            pinned: false,
            preview_key: Some(admin_panel::ADMIN_PREVIEW_KEY.to_string()),
            conn_identity: identity,
            content: TabContent::Admin { view: panel.clone() },
        });
        // G10 T5: seeds the Privileges sub-view's schema selector from
        // whatever's already in the tree's SchemaSnapshot — same source
        // `push_admin_schemas_if_matching` re-pushes on every subsequent
        // active-slot refresh (`start_schema_slot_fetch`'s success arm).
        let schemas = self.tree.read(cx).snapshot().map(admin_panel::distinct_schemas).unwrap_or_default();
        panel.update(cx, |p, cx| p.set_schemas(schemas, cx));
        self.fetch_admin_catalog_into(panel, admin_sql::roles_catalog(engine), cx);
        cx.notify();
    }

    /// `AdminPanel` → `AppView` (the panel doesn't own the runner or the
    /// confirm dialog — same "view emits, AppView owns the I/O" shape
    /// `CompareView`/`MonitorView` already use).
    fn on_admin_event(
        &mut self,
        panel: Entity<admin_panel::AdminPanel>,
        event: &admin_panel::AdminEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            admin_panel::AdminEvent::FetchCatalog { queries } => {
                self.fetch_admin_catalog_into(panel, queries.clone(), cx);
            }
            admin_panel::AdminEvent::RequestApply { statements, warning } => {
                self.open_admin_apply_dialog(panel, statements.clone(), warning.clone(), cx);
            }
        }
    }

    /// Dispatches `runner.fetch_admin_catalog(spec, queries)` off the UI
    /// thread and routes the result into `panel` — same one-shot
    /// "dispatch, `cx.spawn`, update the entity when it resolves" shape
    /// `start_schema_slot_fetch`/`fetch_lookup` already use. No read-only
    /// guard here (design: catalog reads are never gated) and no
    /// generation counter (unlike schema fetches, a stale admin-catalog
    /// result landing after a newer one is a non-issue: the same sub-view's
    /// re-fetch just overwrites the same parsed fields, and switching
    /// sub-views clears staged state first via `switch_sub_view`).
    ///
    /// Review finding M2: `apply_conn_spec()` alone always resolves against
    /// the CURRENTLY active connection — with no check against `panel`'s
    /// OWN stamped identity, opening the admin tab for connection A, then
    /// switching to B, then clicking a sub-nav tab (which re-emits
    /// `FetchCatalog` — see `switch_sub_view`) would fetch B's catalog and
    /// render it inside a panel still labeled/stamped A. Writes were always
    /// safe (`open_admin_apply_dialog` already re-checks `conn_identity`
    /// before dispatching `run_write_transaction`); this closes the
    /// display-only gap using the SAME `conn_identity_matches` predicate
    /// (already unit-tested in `conn_identity_matches_tests`) the write
    /// path uses — no new decision logic to test separately.
    fn fetch_admin_catalog_into(
        &mut self,
        panel: Entity<admin_panel::AdminPanel>,
        queries: Vec<(&'static str, String)>,
        cx: &mut Context<Self>,
    ) {
        let panel_conn_identity = panel.read(cx).conn_identity().to_string();
        let current_identity = self.current_conn_identity();
        if !conn_identity_matches(&panel_conn_identity, &current_identity) {
            let from = self.conn_name_for_identity(&panel_conn_identity);
            panel.update(cx, |p, cx| {
                p.set_error(&format!("data pocházejí z jiného připojení ({from}) — přepni se zpět"), cx)
            });
            return;
        }
        let Some((spec, _timeout)) = self.apply_conn_spec() else {
            panel.update(cx, |p, cx| p.set_error("Bez připojení — vyberte připojení nahoře.", cx));
            return;
        };
        panel.update(cx, |p, cx| p.set_loading(cx));
        let rx = self.runner.fetch_admin_catalog(spec, queries);
        cx.spawn(async move |_this, cx| {
            let result = rx.await;
            let _ = panel.update(cx, |p, cx| match result {
                Ok(Ok(rows)) => p.apply_catalog(rows, cx),
                Ok(Err(e)) => p.set_error(&e.to_string(), cx),
                Err(_) => p.set_error("dotaz zrušen", cx),
            });
        })
        .detach();
    }

    /// Opens (or re-activates) the monitor tab for the active connection.
    /// `preview_key = "monitor:{conn_identity}"` gives one-monitor-per-
    /// connection: unlike table previews (`close_by_preview_key` replaces),
    /// reopening just ACTIVATES the existing tab (design §5).
    fn open_monitor_tab(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.active_engine() else {
            self.status = "Bez připojení — vyberte připojení nahoře.".into();
            cx.notify();
            return;
        };
        if !monitor::monitor_available(engine) {
            // The palette already hides the entry (build_palette_items);
            // this is the belt for any other entry point.
            self.status = "monitor serveru není pro tento engine k dispozici".into();
            cx.notify();
            return;
        }
        let key = format!("monitor:{}", self.current_conn_identity());
        let existing_id =
            self.tabs.iter().find(|t| t.preview_key.as_deref() == Some(key.as_str())).map(|t| t.id);
        if let Some(id) = existing_id {
            self.tabs.activate(id);
            cx.notify();
            return;
        }
        let Some(spec) = self.active_conn_spec() else {
            self.status = "Bez připojení — vyberte připojení nahoře.".into();
            cx.notify();
            return;
        };
        // Same read-only resolution runner::spec_is_read_only applies:
        // config flag, or always-writable for the CLI-arg URL path.
        let read_only = match &spec {
            ConnectSpec::Config { cfg, .. } => cfg.read_only,
            ConnectSpec::Url(_) => false,
        };
        let (cmd_tx, event_rx) = self.runner.open_monitor(spec, read_only, engine);
        let view = cx.new(|cx| monitor_view::MonitorView::new(cx, cmd_tx, event_rx, read_only, engine));
        let title = collapse_title(&format!("Monitor: {}", self.current_connection_label()));
        let conn_identity = self.current_conn_identity();
        let tab_id = self.tabs.open(ResultTab {
            id: 0,
            title,
            pinned: false,
            preview_key: Some(key),
            conn_identity,
            content: TabContent::Monitor { view: view.clone() },
        });
        cx.subscribe(&view, move |this, _emitter, event, cx| {
            this.on_monitor_view_event(tab_id, event, cx);
        })
        .detach();
        self.spawn_monitor_timer(tab_id, cx);
        cx.notify();
    }

    // -----------------------------------------------------------------
    // G8 T6/T7: ER diagram tab — schema-tree icon + palette entry points,
    // large-schema truncation (design §3).
    // -----------------------------------------------------------------

    /// design §3 CURATION: the entry action always operates on ONE schema.
    /// `schema` is `None` for an engine/snapshot with no schema concept
    /// (SQLite) — matches every other `Option<String>` schema field in this
    /// codebase (strict, no "public" guessing).
    /// Opens the tail of the diagnostic log as a text tab.
    ///
    /// The tail rather than the whole file: the cap is 2 MiB and the answer
    /// to „what just happened" is always at the end. The path is the first
    /// line so the file can be opened outside the app too — which is the
    /// only way to read it after a crash.
    fn open_log_tab(&mut self, cx: &mut Context<Self>) {
        const TAIL_BYTES: usize = 64 * 1024;
        let where_ = dbc_state::applog::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(log není k dispozici)".to_string());
        let tail = dbc_state::applog::tail(TAIL_BYTES);
        let body = if tail.trim().is_empty() {
            format!("{where_}

(zatím prázdný)")
        } else {
            format!("{where_}

{tail}")
        };
        self.tabs.open(ResultTab {
            id: 0,
            title: "Log".to_string(),
            pinned: false,
            // Keyed, so repeatedly opening it reuses one tab instead of
            // stacking copies of a file that changes under you.
            preview_key: Some("applog".to_string()),
            conn_identity: self.current_conn_identity(),
            content: TabContent::Text { text: body, scroll_lines: 0 },
        });
        self.status = format!("Log: {where_}");
        cx.notify();
    }

    fn open_er_diagram(&mut self, schema: Option<String>, cx: &mut Context<Self>) {
        let Some(snapshot) = self.tree.read(cx).snapshot() else {
            self.status = "Nejprve načtěte schéma".to_string();
            dbc_state::applog::log(dbc_state::applog::Event::Refused {
                what: "er_diagram".into(),
                reason: "no schema snapshot loaded".into(),
            });
            cx.notify();
            return;
        };
        let scoped: Vec<TableInfo> =
            snapshot.tables.iter().filter(|t| t.schema == schema).cloned().collect();
        // A diagram of nothing is a blank tab, and a blank tab reads as „the
        // click did nothing" (user report, 2026-08-29). Say so instead.
        if scoped.is_empty() {
            dbc_state::applog::log(dbc_state::applog::Event::Refused {
                what: "er_diagram".into(),
                reason: format!(
                    "no tables in schema {:?} ({} in snapshot)",
                    schema,
                    snapshot.tables.len()
                ),
            });
            self.status = match &schema {
                Some(s) => format!("Schéma {s} nemá žádné tabulky — není co nakreslit"),
                None => "Snímek schématu nemá žádné tabulky — není co nakreslit".to_string(),
            };
            cx.notify();
            return;
        }
        let (scoped, hidden) = er_diagram_view::cap_tables(scoped, er_diagram_view::DIAGRAM_TABLE_CAP);
        let truncated_notice = hidden.map(|hidden| {
            format!(
                "Schéma má {} tabulek — zobrazeno prvních {} podle názvu; použijte filtr.",
                hidden + er_diagram_view::DIAGRAM_TABLE_CAP,
                er_diagram_view::DIAGRAM_TABLE_CAP
            )
        });
        let label = schema.clone().unwrap_or_else(|| "(bez schématu)".to_string());
        let graph = dbc_core::erd::build_graph(&scoped);
        let layout = dbc_core::erd::layout::compute_layout(&graph);
        let view = cx.new(|_cx| {
            let mut v = er_diagram_view::ErDiagramView::new(layout, scoped, label.clone());
            v.truncated_notice = truncated_notice;
            v
        });
        cx.subscribe(&view, Self::on_er_diagram_event).detach();
        self.tabs.open(ResultTab {
            id: 0,
            title: format!("ER: {label}"),
            pinned: false,
            preview_key: None,
            conn_identity: self.current_conn_identity(),
            content: TabContent::Diagram { view },
        });
        self.status = "ER diagram otevřen".to_string();
        cx.notify();
    }

    /// `ErDiagramView` reuses `TreeEvent` verbatim (only ever emits
    /// `OpenDdl`) — this handler mirrors `on_tree_event`'s `OpenDdl` arm
    /// exactly rather than duplicating tab-open logic a third time.
    fn on_er_diagram_event(
        &mut self,
        _emitter: Entity<er_diagram_view::ErDiagramView>,
        event: &TreeEvent,
        cx: &mut Context<Self>,
    ) {
        if let TreeEvent::OpenDdl { title, ddl } = event {
            self.tabs.open(ResultTab {
                id: 0,
                title: format!("DDL: {title}"),
                pinned: false,
                preview_key: None,
                conn_identity: self.current_conn_identity(),
                content: TabContent::Text { text: ddl.clone(), scroll_lines: 0 },
            });
            self.status = format!("DDL otevřeno: {title}");
            cx.notify();
        }
    }

    /// `PaletteAction::ShowErDiagram`'s zero-argument -> one-schema
    /// resolution: exactly one distinct schema in the snapshot wins
    /// outright; otherwise `None` (caller shows the Czech refusal status
    /// text pointing at the schema-tree icon, the primary entry point).
    fn resolve_er_diagram_schema(&self, cx: &Context<Self>) -> Option<Option<String>> {
        let snapshot = self.tree.read(cx).snapshot()?;
        let mut schemas: Vec<Option<String>> = snapshot.tables.iter().map(|t| t.schema.clone()).collect();
        schemas.sort();
        schemas.dedup();
        if schemas.len() == 1 {
            return schemas.into_iter().next();
        }
        None
    }

    /// One timer loop per open monitor tab (design §4), on the SAME
    /// `cx.background_executor().timer` primitive `grid.rs`'s export
    /// chunking uses. Hidden-tab gating is automatic: a tick only reaches
    /// `tick_if_idle` when this tab is the active one; pause/awaiting are
    /// checked inside `MonitorView`. The loop BREAKS (never a forever-no-op)
    /// when the tab or the `AppView` is gone.
    fn spawn_monitor_timer(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                // Re-read the CURRENT interval each lap — backoff can
                // change it between ticks. None = tab closed.
                let interval = match this.update(cx, |view, cx| {
                    view.monitor_view_for_tab(tab_id).map(|m| m.read(cx).interval_secs())
                }) {
                    Ok(Some(secs)) => secs,
                    Ok(None) | Err(_) => break,
                };
                cx.background_executor().timer(std::time::Duration::from_secs(interval)).await;
                let tick = this.update(cx, |view, cx| {
                    let visible = view.tabs.active().is_some_and(|t| t.id == tab_id);
                    if visible {
                        if let Some(m) = view.monitor_view_for_tab(tab_id) {
                            m.update(cx, |m, cx| m.tick_if_idle(cx));
                        }
                    }
                });
                if tick.is_err() {
                    break; // AppView released
                }
            }
        })
        .detach();
    }

    /// `MonitorView` -> `AppView` event bridge (subscription wired in
    /// `open_monitor_tab`). `KillRequested` opens the confirm dialog;
    /// `KillFinished` resolves it (design §6's success/failure UX).
    fn on_monitor_view_event(
        &mut self,
        tab_id: u64,
        event: &monitor_view::MonitorViewEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            monitor_view::MonitorViewEvent::KillRequested { pid, label, sql } => {
                if self.modal.is_some() {
                    return; // single-modal invariant, same as every dialog opener
                }
                self.modal = Some(connections_ui::ModalState::KillConfirm {
                    pid: *pid,
                    label: label.clone(),
                    sql: sql.clone(),
                    tab_id,
                    error: None,
                    dispatched: false,
                });
                // UX-polish §1.4: no-input modal, cx-only opener — defer
                // focus to `AppView::render` via `modal_needs_focus`.
                self.modal_needs_focus = true;
                cx.notify();
            }
            monitor_view::MonitorViewEvent::KillFinished { pid, result } => {
                // MAJOR review fix: only touch `self.modal` when it's STILL
                // the KillConfirm dialog THIS event belongs to (same pid AND
                // same originating tab) — otherwise a stale/cancelled kill's
                // outcome can land in an unrelated, currently-open dialog
                // (same tab, different pid; or a different monitor tab
                // entirely) and either overwrite its error or silently
                // close it out from under the user. `tab_id` here is this
                // handler's own parameter — fixed per-subscription to the
                // tab whose MonitorView emitted the event (see
                // `open_monitor_tab`'s `cx.subscribe`), so it IS the
                // event's true origin, no extra plumbing needed.
                let matches_open_dialog = connections_ui::kill_confirm_matches(&self.modal, tab_id, *pid);
                match result {
                    Ok(()) => {
                        if matches_open_dialog {
                            self.modal = None;
                        }
                        // pg reports Ok even when the pid already exited
                        // (the function returns false, not an error) — the
                        // out-of-cycle refresh MonitorView already
                        // dispatched shows the truth momentarily (design
                        // §6).
                        self.status = format!("proces {pid} ukončen");
                    }
                    Err(msg) => {
                        if matches_open_dialog {
                            // Dialog stays open with the error; NEW MINOR
                            // review fix: also resets `dispatched` so a
                            // genuine failure can be retried (see
                            // `apply_kill_error_to_modal`'s doc comment).
                            connections_ui::apply_kill_error_to_modal(&mut self.modal, tab_id, *pid, msg);
                        } else {
                            self.status = format!("error: {msg}");
                        }
                    }
                }
                cx.notify();
            }
        }
    }

    fn apply_conn_spec(&self) -> Option<(ConnectSpec, Option<u64>)> {
        if self.active_connection_id.is_some() {
            self.resolve_active().map(|a| {
                let timeout_secs = a.timeout_secs;
                (a.into_spec(), timeout_secs)
            })
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
            TabContent::Monitor { .. } => return,
            TabContent::Plan { .. } => return,
            TabContent::Diagram { .. } => return,
            TabContent::Compare { .. } => return,
            TabContent::Chart { .. } => return,
            TabContent::ScriptRun { .. } => return,
            // G10 T4: admin Apply goes through `open_admin_apply_dialog`
            // (the panel's own "Aplikovat" click emits `AdminEvent::
            // RequestApply`), not this generic sandbox-grid path.
            TabContent::Admin { .. } => return,
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
        let dialect = self.active_engine().map(sql_dialect).unwrap_or(dbc_core::Dialect::Postgres);
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
                dialect,
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
        // G10 T3/T4: sandbox tuples convert via the blanket `From` impl —
        // exec_sql == display_sql for these, so this is a no-op
        // behaviourally, just the type the shared write path now takes.
        let statements: Vec<admin_sql::WriteStatement> =
            statements.into_iter().map(admin_sql::WriteStatement::from).collect();
        let sql_text = statements.iter().map(|s| s.display_sql.as_str()).collect::<Vec<_>>().join("\n");
        let focus_handle = cx.focus_handle();
        self.apply_dialog = Some(ApplyDialogState {
            target: ApplyTarget::SandboxTab { tab_id, preview_identity },
            statements,
            sql_text,
            warning: None,
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
        let target = ad.target.clone();
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
        // §3-novela's single choke point: `run_write_transaction` already
        // takes `Vec<admin_sql::WriteStatement>` (T3) — both callers'
        // statements were built as such at dialog-open time (T4), so this
        // is the one and only dispatch, unchanged in shape whether `target`
        // is a sandbox tab or the admin panel.
        let rx = self.runner.run_write_transaction(spec, statements, timeout_secs);
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(Ok(total)) => {
                        // Brief contract #3, in order: close modal, run the
                        // target-specific success cleanup, status, record
                        // ONE history entry (display_sql only, per CURATION
                        // items 3/4 — `sql_text` is already '***'-redacted
                        // where it matters, built once at dialog-open time).
                        view.apply_dialog = None;
                        // G10 N2 (final review): captured before `target` is
                        // moved into the match below — drives which
                        // `record_history*` call runs after it, so an admin
                        // write shows up in the History panel tagged kind
                        // `"admin"` (its own 🛡 badge) instead of
                        // masquerading as a plain `"query"` entry, same
                        // pattern as G11's `"backup"`/`"restore"` kinds
                        // (see `record_backup_restore_history` and
                        // `history_panel::badge_for_kind`).
                        let is_admin = matches!(target, ApplyTarget::Admin { .. });
                        match target {
                            ApplyTarget::SandboxTab { tab_id, preview_identity } => {
                                if let Some(tab) = view.tabs.iter().find(|t| t.id == tab_id) {
                                    if let TabContent::Grid { grid, .. } = &tab.content {
                                        grid.clone().update(cx, |g, cx| g.clear_edits(cx));
                                    }
                                }
                                view.status = format!("aplikováno ({n_statements} příkazů)");
                                // Re-run the preview via the EXISTING
                                // pipeline (brief: "preserves joins via
                                // from_join_change=false machinery" —
                                // `apply_view_prefs_to_grid`'s saved-fk-join
                                // auto-retrigger picks the active joins back
                                // up from this table's persisted view prefs
                                // once this run's own `Started` lands,
                                // exactly like a plain preview re-open
                                // does). This immediately overwrites
                                // `view.status` above with its own
                                // "connecting…" / progress text — expected:
                                // the "aplikováno (…)" status is a transient
                                // confirmation, the refreshed preview's own
                                // status (ending in "N rows in …") takes
                                // over next, same as every other status
                                // transition in this file.
                                let (schema, table) = preview_identity;
                                let dialect =
                                    view.active_engine().map(sql_dialect).unwrap_or(dbc_core::Dialect::Postgres);
                                let sql = preview_sql(dialect, schema.as_deref(), &table);
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
                            }
                            ApplyTarget::Tree => {
                                view.status = "provedeno".into();
                                // The dropped/emptied object must leave the
                                // tree, or the next click acts on something
                                // that no longer exists.
                                if let (Some(id), Some(db)) =
                                    (view.active_connection_id.clone(), view.effective_database())
                                {
                                    view.start_schema_slot_fetch(id, db, cx);
                                }
                            }
                            ApplyTarget::Admin { panel } => {
                                // G10 T4: clears the panel's staged sets and
                                // re-requests the active sub-view's catalog
                                // — the admin equivalent of "re-run the
                                // preview".
                                panel.clone().update(cx, |p, cx| p.on_apply_success(cx));
                                view.status = format!("aplikováno ({n_statements} příkazů)");
                            }
                        }
                        // Record ONE history entry for the write itself
                        // (brief contract #3's final step) — the sandbox
                        // re-run's own SELECT gets its OWN separate history
                        // entry once ITS `Finished`/`Failed` lands, same as
                        // any other preview; the admin path has no re-run at
                        // all, just this one entry.
                        if is_admin {
                            view.record_history_with_kind(
                                &sql_text,
                                &history_conn_name,
                                history_started_at,
                                Some(started.elapsed().as_millis() as i64),
                                Some(total as i64),
                                None,
                                "admin",
                                cx,
                            );
                        } else {
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

    /// G10 T4: the admin panel's own "Aplikovat" (`AdminEvent::RequestApply`)
    /// — opens the SAME generalized Apply confirm dialog `on_open_apply_dialog`
    /// does (§3-novela: one confirm modal for both callers), just built from
    /// the panel's already-staged `Vec<admin_sql::WriteStatement>` instead of
    /// `sandbox::generate_statements`' tuples. Called from a `cx.subscribe`
    /// callback (`on_admin_event`), which — unlike a click listener — has no
    /// `Window` access, so (like `on_monitor_view_event`'s `KillRequested`)
    /// this does NOT `window.focus` the dialog; the overlay still renders
    /// and is fully clickable, just without an explicit keyboard-focus
    /// hand-off.
    fn open_admin_apply_dialog(
        &mut self,
        panel: Entity<admin_panel::AdminPanel>,
        statements: Vec<admin_sql::WriteStatement>,
        warning: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() || self.discard_confirm.is_some() || self.apply_dialog.is_some() {
            return;
        }
        if statements.is_empty() {
            return;
        }
        let panel_conn_identity = panel.read(cx).conn_identity().to_string();
        let current_identity = self.current_conn_identity();
        if !conn_identity_matches(&panel_conn_identity, &current_identity) {
            let from = self.conn_name_for_identity(&panel_conn_identity);
            self.status = format!("změny pocházejí z jiného připojení ({from}) — přepni se zpět");
            cx.notify();
            return;
        }
        let sql_text = statements.iter().map(|s| s.display_sql.as_str()).collect::<Vec<_>>().join("\n");
        let focus_handle = cx.focus_handle();
        self.apply_dialog = Some(ApplyDialogState {
            target: ApplyTarget::Admin { panel },
            statements,
            sql_text,
            warning,
            conn_identity: panel_conn_identity,
            running: false,
            error: None,
            focus_handle,
        });
        cx.notify();
    }

    /// G7 T7: computes `mode` from the two connections' engines, runs
    /// `dbc_diff::schema_diff::diff_schema`, and updates the ALREADY-OPEN
    /// Compare tab's `CompareView` entity (`pending.view`, created and
    /// opened by `connections_ui::confirm_compare_dialog` in
    /// `CompareLoadState::Loading` at dispatch time — design §3) in place.
    /// `result`'s `Err` case (the oneshot channel closing — the runner task
    /// panicked/dropped, which never happens in normal operation, but is
    /// still a `Result`, not an `unwrap`, same posture `start_schema_slot_fetch`
    /// takes on its own oneshot) surfaces as a `CompareLoadState::Error` on
    /// BOTH legs rather than leaving the tab stuck on "Načítám schéma…".
    pub(crate) fn on_compare_schema_pair_ready(
        &mut self,
        pending: PendingCompare,
        result: Result<(Result<SchemaSnapshot, QueryError>, Result<SchemaSnapshot, QueryError>), tokio::sync::oneshot::error::RecvError>,
        cx: &mut Context<Self>,
    ) {
        let (result_a, result_b) = result.unwrap_or_else(|_| {
            let cancelled = || Err(QueryError::msg("fetch zrušen".to_string()));
            (cancelled(), cancelled())
        });
        let (engine_a, engine_b) = pending.view.read(cx).engines();
        let mode = if engine_a == engine_b {
            dbc_diff::schema_diff::CompareMode::SameEngine
        } else {
            dbc_diff::schema_diff::CompareMode::CrossEngine
        };
        let state = match (result_a, result_b) {
            (Ok(snap_a), Ok(snap_b)) => {
                let diff = dbc_diff::schema_diff::diff_schema(&snap_a, &snap_b, mode);
                compare::CompareLoadState::Ready { diff, mode }
            }
            (a, b) => compare::CompareLoadState::Error { a: a.err(), b: b.err() },
        };
        pending.view.update(cx, |v, cx| {
            v.state = state;
            cx.notify();
        });
        self.status = "Porovnání schématu dokončeno".to_string();
        cx.notify();
    }

    /// G7 T8: `CompareView`'s `CompareViewEvent` subscription (wired by
    /// `connections_ui::confirm_compare_dialog` at tab-open time) —
    /// `CompareView` doesn't own a `QueryRunner` (no tab-content entity in
    /// this codebase does, see `MonitorView`'s `KillRequested` for the same
    /// shape), so the actual `fetch_diff_side` dispatch for "Porovnat data"
    /// happens here, reading the CURRENT diff/selection straight off the
    /// entity before calling back into `CompareView::start_data_diff`
    /// (which owns the generation guard and the `cx.spawn` completion).
    pub(crate) fn on_compare_view_event(
        &mut self,
        view: Entity<compare::CompareView>,
        event: &compare::CompareViewEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            compare::CompareViewEvent::DataDiffRequested => {
                let runner = &self.runner;
                view.update(cx, |v, cx| {
                    if let compare::CompareLoadState::Ready { diff, .. } = v.state.clone() {
                        v.start_data_diff(&diff, runner, cx);
                    }
                });
            }
        }
    }

    /// `SchemaTree`'s `TreeEvent` subscription (wired in `main`). G2 Task 7:
    /// `OpenPreview` builds the SQL via `preview_sql` and runs it through the
    /// normal guarded pipeline (`run_query_with`, `bypass_auto_limit = true`
    /// — the SQL already carries its own `LIMIT 1000`) without touching the
    /// editor's text; `OpenDdl` (double-click on a routine/trigger, or the
    /// tree header's "DDL" button via `SchemaTree::handle_generate_ddl`)
    /// just opens a read-only `Text` tab — no DB round-trip either way.
    fn on_tree_event(&mut self, _emitter: Entity<SchemaTree>, event: &TreeEvent, cx: &mut Context<Self>) {
        // One line per tree/context-menu action, before it runs. This is
        // the entry that answers „did my click even arrive?" — the question
        // the log was added for. Only the variant NAME is recorded; the
        // payload can carry generated SQL.
        dbc_state::applog::log(dbc_state::applog::Event::Action {
            action: crate::schema_tree::event_name(event),
            target: String::new(),
        });
        match event {
            // Sidebar rework (design §5 row 1): scope-checked — an
            // active-scope open runs directly (including its dirty-preview
            // discard-confirm gate inside `open_table_preview`); a
            // cross-context double-click switches FIRST and opens after
            // (one-shot queue; cleared on failure/supersede — §2.2). The
            // queued replay goes through the SAME `open_table_preview`, so
            // it also passes the dirty gate — a queued open must never
            // silently drop staged edits either.
            TreeEvent::OpenPreview { conn_id, db, schema, table } => {
                if self.scope_is_active(conn_id, db) {
                    self.open_table_preview(schema.clone(), table.clone(), cx);
                } else {
                    self.switch_to_database(
                        conn_id,
                        Some(db.clone()),
                        Some(PendingTreeAction::OpenPreview {
                            schema: schema.clone(),
                            table: table.clone(),
                        }),
                        cx,
                    );
                }
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
            // Sidebar rework: ⟳ refreshes the ACTIVE slot (a `Loading`
            // transition carries the slot's expand-set forward — resolved
            // deviation 13). Nothing active → just re-push the (empty)
            // context; there is no whole-panel state to clear any more.
            TreeEvent::ToggleGroupingRequested => {
                self.toggle_tree_grouping(cx);
            }
            TreeEvent::RefreshRequested => {
                if let Some(id) = self.active_connection_id.clone() {
                    if let Some(db) = self.effective_database() {
                        self.start_schema_slot_fetch(id, db, cx);
                    }
                } else if self.conn_url.is_some() {
                    self.start_schema_slot_fetch(CLI_CONN_IDENTITY.to_string(), String::new(), cx);
                } else {
                    self.refresh_tree_context(cx);
                }
            }
            // G3 Task 4: a row's ★/☆ toggle (a table/view/routine/trigger/
            // sequence in the schema tree proper, or an item already listed
            // under the "Oblíbené" section) — mirrors
            // `connections_ui::AppView::toggle_connection_favourite`'s
            // guarded-save shape for the dropdown's connection stars.
            TreeEvent::ToggleFavourite(fav) => {
                let Some(guard) = self.guard_corrupt_config(cx) else { return };
                // Full-struct equality in `toggle_favourite` means the same
                // table in two databases is two distinct favourites (T1's
                // `toggle_favourite_distinguishes_databases` pin).
                self.config.toggle_favourite(fav.clone());
                self.status = match self.config.save(&self.config_path, &guard) {
                    Ok(()) => "Uloženo".to_string(),
                    Err(e) => format!("error saving config: {}", e.message),
                };
                self.refresh_tree_context(cx);
                cx.notify();
            }
            TreeEvent::OpenErDiagram { schema } => {
                self.open_er_diagram(schema.clone(), cx);
            }
            TreeEvent::ImportCsv { schema, table } => {
                self.start_csv_import(schema.clone(), table.clone(), cx);
            }
            TreeEvent::OpenAdmin => {
                self.open_admin_tab(cx);
            }
            TreeEvent::LoadDatabases { conn_id } => self.start_db_list_fetch(conn_id.clone(), cx),
            TreeEvent::LoadSchema { conn_id, db } => {
                self.start_schema_slot_fetch(conn_id.clone(), db.clone(), cx)
            }
            TreeEvent::SwitchToDatabase { conn_id, db } => {
                self.switch_to_database(conn_id, db.clone(), None, cx)
            }
            TreeEvent::ScriptsRefresh => self.start_scripts_scan(cx),
            TreeEvent::OpenScriptsSettings => self.open_settings(cx),
            // Part S §5.1: opening binds the ONE global editor to the file
            // and NEVER runs it. Guarded, so a dirty binding is never
            // silently clobbered.
            TreeEvent::ScriptOpen { rel } => {
                self.editor_load_guarded(PendingScriptAction::Open { rel: rel.clone() }, cx)
            }
            // Part S §6: „▶" runs the file ON DISK through the SHARED G12
            // confirm continuation — never the editor buffer, never a save
            // first.
            TreeEvent::ScriptRunFile { rel } => self.run_script_from_library(rel.clone(), cx),
            // Part S §4: the ONE name dialog, three flavours. Nothing is
            // written until its confirm; `validate_script_name`, the
            // Unicode-aware collision probe and the empty-rel mutation
            // rail all live inside the `scripts.rs` ops it dispatches.
            TreeEvent::ScriptCreate { parent_rel } => self.open_script_name_modal(
                connections_ui::ScriptNameMode::NewScript,
                parent_rel.clone(),
                String::new(),
                false,
                cx,
            ),
            TreeEvent::ScriptRename { rel, is_dir } => self.open_script_name_modal(
                connections_ui::ScriptNameMode::Rename,
                String::new(),
                rel.clone(),
                *is_dir,
                cx,
            ),
            TreeEvent::ScriptDelete { rel, is_dir } => {
                self.open_script_delete_modal(rel.clone(), *is_dir, cx)
            }

            // --- Context menu (2026-08-29) ---
            TreeEvent::CopyText { what, text } => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                self.status = format!("{what} zkopírováno");
                cx.notify();
            }
            TreeEvent::InsertAtCursor { text } => {
                let text = text.clone();
                self.sql.update(cx, |input, cx| input.insert_text(&text, cx));
                self.status = "vloženo do editoru".into();
                cx.notify();
            }
            TreeEvent::GenerateSql { kind, sql } => {
                let sql = sql.clone();
                let changed = rewrite_buffer_in_place(self, cx, move |_| sql);
                self.status = match kind {
                    schema_tree::GenKind::Select if changed => "SELECT vygenerován".into(),
                    schema_tree::GenKind::Insert if changed => "INSERT vygenerován".into(),
                    schema_tree::GenKind::Update if changed => "UPDATE vygenerován".into(),
                    _ => "editor už obsahuje tento dotaz".into(),
                };
                cx.notify();
            }
            TreeEvent::OpenPreviewHere { schema, table } => {
                self.open_table_preview(schema.clone(), table.clone(), cx)
            }
            TreeEvent::CountRows { schema, table } => self.run_count_rows(schema.clone(), table.clone(), cx),
            TreeEvent::OpenMonitorFor { .. } => self.open_monitor_tab(cx),
            TreeEvent::OpenCompareFor { .. } => self.open_compare_dialog(cx),
            TreeEvent::ExportCsv { schema, table } => {
                // Export runs off a RESULT, not off a table name, so the
                // honest thing is to open the data first — the grid's own
                // „Export" is then one click away and exports exactly what
                // is on screen, rather than a second, silently different
                // extraction path.
                self.open_table_preview(schema.clone(), table.clone(), cx);
                self.status = "data otevřena — export je v panelu výsledku".into();
                cx.notify();
            }
            // These need a `Window` (dialog focus) and this subscription is
            // cx-only, so they are parked and drained at the top of
            // `render`, the same shape the queued cross-context open uses.
            TreeEvent::BackupFor { .. }
            | TreeEvent::RestoreFor { .. }
            | TreeEvent::EditConnection { .. }
            | TreeEvent::DropObject { .. }
            | TreeEvent::TruncateTable { .. } => {
                self.pending_menu_action = Some(event.clone());
                cx.notify();
            }
        }
    }

    /// „Počet řádků" — runs `SELECT COUNT(*)` through the normal guarded
    /// pipeline WITHOUT touching the editor's text, the same posture as a
    /// table preview. `bypass_auto_limit` because a count returns one row
    /// and an appended `LIMIT` would be noise in the history entry.
    fn run_count_rows(&mut self, schema: Option<String>, table: String, cx: &mut Context<Self>) {
        let Some(engine) = self.active_engine() else {
            self.status = "error: není aktivní připojení".into();
            cx.notify();
            return;
        };
        let target =
            dbc_core::quote_qualified_d(sql_dialect(engine), schema.as_deref(), &table);
        self.run_query_with(format!("SELECT COUNT(*) FROM {target}"), None, true, cx);
    }

    /// Drains [`AppView::pending_menu_action`]. Called from `render`, which
    /// is where a `Window` exists.
    fn perform_pending_menu_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(event) = self.pending_menu_action.take() else {
            return;
        };
        match event {
            TreeEvent::BackupFor { conn_id, .. } => self.open_backup_dialog(conn_id, window, cx),
            TreeEvent::RestoreFor { conn_id, .. } => self.open_restore_dialog(conn_id, window, cx),
            TreeEvent::EditConnection { conn_id } => {
                let cfg = self.config.connections.iter().find(|c| c.id == conn_id).cloned();
                match cfg {
                    Some(cfg) => self.open_connection_dialog(Some(cfg), window, cx),
                    // The row was built from this list, so a miss means the
                    // config changed under the open menu.
                    None => {
                        self.status = "error: připojení už neexistuje".into();
                        cx.notify();
                    }
                }
            }
            TreeEvent::DropObject { kind, schema, name } => {
                self.open_destructive_confirm(
                    |d| tree_menu::drop_sql(kind, d, schema.as_deref(), &name),
                    format!("Objekt {name} bude nenávratně odstraněn."),
                    window,
                    cx,
                );
            }
            TreeEvent::TruncateTable { schema, table } => {
                self.open_destructive_confirm(
                    |d| tree_menu::truncate_sql(d, schema.as_deref(), &table),
                    format!("Všechna data v tabulce {table} budou nenávratně smazána."),
                    window,
                    cx,
                );
            }
            // Every other variant is handled synchronously in
            // `on_tree_event` and never parked.
            _ => {}
        }
    }

    /// Opens the SHARED Apply confirm dialog for a destructive statement.
    ///
    /// This is the whole safety story for the menu's `DROP`/`TRUNCATE`
    /// items: they do not execute, they stage exactly one statement into the
    /// dialog that already exists for sandbox and admin writes, which shows
    /// the SQL verbatim and runs it through `run_write_transaction` — the
    /// path that carries the read-only guard and the transaction
    /// discipline. There is no second execution route.
    ///
    /// `build` takes the dialect rather than a finished string so the SQL
    /// cannot be built before we know there IS an active connection to
    /// build it for.
    fn open_destructive_confirm(
        &mut self,
        build: impl FnOnce(dbc_core::Dialect) -> String,
        warning: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.active_engine() else {
            self.status = "error: není aktivní připojení".into();
            cx.notify();
            return;
        };
        if self.active_read_only() {
            // Belt-and-braces: `tree_menu` already omits these items on a
            // read-only connection, so reaching here means the flag changed
            // between the menu opening and the click.
            self.status = "error: připojení je jen pro čtení".into();
            cx.notify();
            return;
        }
        let sql = build(sql_dialect(engine));
        let statements = vec![admin_sql::WriteStatement::from((sql.clone(), None))];
        let focus_handle = cx.focus_handle();
        self.apply_dialog = Some(ApplyDialogState {
            target: ApplyTarget::Tree,
            statements,
            sql_text: sql,
            warning: Some(warning),
            conn_identity: self.current_conn_identity(),
            running: false,
            error: None,
            focus_handle: focus_handle.clone(),
        });
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    /// Tab strip between the SQL editor and result content: title +
    /// row-count badge (`Grid` tabs read `buffer.row_count()` fresh at
    /// render time rather than caching it on the tab) + pin toggle + close.
    /// Click activates. Active tab bg 0x313244, inactive 0x181825. Only
    /// called when there's at least one open tab (see `Render::render`).
    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
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
                    TabContent::Monitor { .. } => (0, false),
                    TabContent::Plan { .. } => (0, false),
                    TabContent::Diagram { .. } => (0, false),
                    TabContent::Compare { .. } => (0, false),
                    TabContent::Chart { .. } => (0, false),
                    TabContent::ScriptRun { .. } => (0, false),
                    // Review finding: this used to hardcode `false`
                    // regardless of staged admin edits, so a dirty admin
                    // tab never got the " •" suffix either. Reuses
                    // `AdminPanel::change_count` — the same dirtiness
                    // definition `grid_dirty_change_count`'s `Admin` arm
                    // (the close-tab guard) already reads.
                    TabContent::Admin { view } => (0, view.read(cx).change_count() > 0),
                };
                // G5 Task 3, brief contract #7: dirty (unapplied staged
                // edits) tabs get a " •" title suffix — the apply bar
                // itself is a later task, but the indicator is wired now.
                let title = if dirty { format!("{} •", t.title) } else { t.title.clone() };
                (t.id, title, t.pinned, row_count)
            })
            .collect();

        let mut strip = div().id("tab-strip").flex().flex_row().h(px(28.)).bg(theme.bg_app);
        for (id, title, pinned, row_count) in rows {
            let is_active = Some(id) == active_id;
            let bg = if is_active { theme.bg_hover } else { theme.bg_app };
            let pin_color = if pinned { theme.warn } else { theme.text_disabled };
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
                    .text_color(theme.text_primary)
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
                                    // UX-polish §1.4: no-input prompt,
                                    // cx-only site — defer focus to
                                    // `AppView::render` via `modal_needs_focus`.
                                    view.modal_needs_focus = true;
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
        let theme = *cx.theme();
        let Some(active) = self.tabs.active() else {
            return div().flex_1().bg(theme.bg_panel).into_any_element();
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
                    .text_color(theme.text_primary)
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
                    .bg(theme.bg_panel)
                    .child(
                        div().flex().flex_row().justify_end().p_1().child(
                            div()
                                .id("tab-copy")
                                .cursor_pointer()
                                .bg(theme.bg_hover)
                                .text_color(theme.text_primary)
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
            TabContent::Monitor { view } => view.clone().into_any_element(),
            TabContent::Plan { view } => view.clone().into_any_element(),
            TabContent::Diagram { view } => {
                // G8 T6: mirrors `ResultGrid`'s `status_note` idiom above —
                // taken once so the export flow's status text surfaces in
                // `AppView::status` exactly once, not stuck forever.
                if let Some(note) = view.update(cx, |v, _| v.status_note.take()) {
                    self.status = note;
                }
                view.clone().into_any_element()
            }
            TabContent::Compare { view } => view.clone().into_any_element(),
            TabContent::Chart { view } => view.clone().into_any_element(),
            // G12 T3/T4: a free function (not a method) — it never touches
            // `self` directly, only `state` (cloned out of `active`) and
            // `cx` (for the "Zrušit" listener) — calling a `&mut self`
            // method here instead would conflict with `active`'s still-live
            // borrow of `self.tabs` (the same reason every other arm above
            // either avoids `self` or touches only a named field like
            // `self.status`, never an opaque method call).
            TabContent::ScriptRun { state } => render_script_run_tab(state.clone(), cx),
            TabContent::Admin { view } => view.clone().into_any_element(),
        }
    }

    /// The `MonitorView` entity behind an open Monitor tab, by tab id —
    /// used by the kill-confirm dialog (T5) and the per-tab timer loop
    /// (T6).
    fn monitor_view_for_tab(&self, tab_id: u64) -> Option<Entity<monitor_view::MonitorView>> {
        self.tabs.iter().find(|t| t.id == tab_id).and_then(|t| match &t.content {
            TabContent::Monitor { view } => Some(view.clone()),
            _ => None,
        })
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
        let theme = *cx.theme();
        Some(
            div()
                .id("apply-bar")
                .h(px(28.))
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .bg(theme.bg_warn_banner)
                .text_color(theme.warn)
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
                        .bg(theme.bg_selected)
                        .text_color(if identity_ok { theme.success } else { theme.text_disabled })
                        .child("Aplikovat"),
                )
                .when(!identity_ok, |d| {
                    d.child(
                        div()
                            .text_color(theme.text_disabled)
                            .child("(jiné připojení — přepni se zpět)"),
                    )
                })
                .child(
                    div()
                        .id("apply-bar-discard")
                        .cursor_pointer()
                        .px_2()
                        .rounded_md()
                        .bg(theme.bg_selected)
                        .text_color(theme.danger)
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
        let (n, is_script) = {
            let dc = self.discard_confirm.as_ref()?;
            (dc.change_count, matches!(dc.action, PendingDiscard::Script(_)))
        };
        // Workspace T8 (Part S §5.5): a parked script action names the file
        // it would discard. `binding_rel` is read HERE (not captured when
        // the prompt went up) so a scripts-root change while the prompt is
        // open cannot leave a stale label on screen; `None` — a binding
        // that vanished under an open prompt, which nothing can currently
        // do — degrades to the generic wording rather than to a blank name.
        let question =
            discard_confirm_question(if is_script { self.binding_rel() } else { None }.as_deref(), n);
        let theme = *cx.theme();

        let panel = div()
            .id("discard-confirm-panel")
            .w(px(420.))
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .text_color(theme.text_primary)
            .child(question)
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
                            .bg(theme.bg_hover)
                            .text_color(theme.danger)
                            .px_2()
                            .rounded_md()
                            .child("Zahodit")
                            .on_click(cx.listener(|view, _, _, cx| view.on_discard_confirm_yes(cx))),
                    )
                    .child(
                        div()
                            .id("discard-confirm-no")
                            .cursor_pointer()
                            .bg(theme.bg_hover)
                            .text_color(theme.text_primary)
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
                .bg(theme.bg_backdrop)
                // UX-polish §1.4: same shared focus target as
                // `render_modal_overlay` — holds keyboard focus so stray
                // typing can't reach the SQL editor underneath. NO key
                // context here or ever: Enter must stay structurally inert
                // on discard-confirm (§3-novela / Global Constraints).
                .track_focus(&self.modal_focus_handle)
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
        let warning = ad.warning.clone();
        let focus_handle = ad.focus_handle.clone();
        // G10 T3/T4 (CURATION items 3/4): display_sql ONLY — the confirm
        // modal is the ONE place a user ever sees this SQL, and it must
        // never be exec_sql (the real password on a password-bearing
        // statement).
        let lines: Vec<String> = ad.statements.iter().map(|s| s.display_sql.clone()).collect();
        let theme = *cx.theme();

        let mut body = div()
            .id("apply-dialog-body")
            .font_family("Consolas")
            .flex()
            .flex_col()
            .max_h(px(280.))
            .overflow_hidden()
            .p_2()
            .bg(theme.bg_app)
            .rounded_md()
            .text_color(theme.text_primary);
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
            .bg(theme.bg_panel)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .text_color(theme.text_primary)
            .child(format!("Aplikovat {} příkazů", lines.len()))
            .child(body);

        // G10 T6: the CASCADE-drop red warning line, above the buttons —
        // `None` for every non-T6 caller.
        if let Some(w) = &warning {
            panel = panel.child(div().text_color(theme.danger).child(w.clone()));
        }
        if running {
            panel = panel.child(div().text_color(theme.warn).child("aplikuji…"));
        }
        if let Some(err) = &error {
            panel = panel.child(div().text_color(theme.danger).child(format!("error: {err}")));
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
                        .bg(theme.bg_hover)
                        .text_color(if running { theme.text_disabled } else { theme.success })
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
                        .bg(theme.bg_hover)
                        .text_color(if running { theme.text_disabled } else { theme.text_primary })
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
                .bg(theme.bg_backdrop)
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    // -----------------------------------------------------------------
    // G11 T6: backup/restore dispatch.
    //
    // Flow: `open_backup_dialog`/`open_restore_dialog` (dropdown 🗄/♻ icons
    // and the palette's two new actions) resolve the target connection,
    // open the platform file dialog, and on a picked path re-verify the
    // connection is still there (`backup::backup_dispatch_allowed` —
    // binding carry-forward #3: an OS file dialog can take arbitrarily
    // long, and the connection list is mutable state the user could have
    // edited/deleted meanwhile) before EVER resolving a spec. Backup
    // dispatches immediately (`run_backup_now` — no typed-confirm step,
    // design §2 vs §3); Restore stops at `BackupStatus::Confirming` first
    // (`begin_restore_confirm`) and only reaches `run_restore_now` once
    // "Obnovit" is clicked with the typed database name matching AND the
    // read-only gate re-passes (`confirm_restore` — the THIRD independent
    // read-only check, after the dropdown icon's dimming and this method's
    // own pre-dialog check).
    //
    // Every `BackupHandle` (Postgres external process) this dispatches is
    // reachable for `cancel()` from THREE places: the session's own
    // "Zrušit" button (`cancel_backup_restore`), `close_modal`
    // (connections_ui.rs — covers every other way the modal closes, e.g. a
    // future code path that calls it directly), and the app-quit hook
    // (`main()`, below) — see `cancel_active_backup_if_running`'s doc
    // comment for the full accounting. MSSQL/SQLite runs go through
    // `Connection::execute`/plain file I/O inside a `tokio` task with no
    // OS child process to leak; `cancel_now()` is a documented no-op for
    // those two engines (see `BackupSession::cancel`'s doc comment) — only
    // the UI-visible status flips to `Cancelled` so a late completion
    // doesn't silently overwrite it (`finish_backup_restore`'s own guard).

    /// Looks up a `ConnectionConfig` + its vault secret (if any) by id —
    /// shared by every backup/restore entry point. `None` when the
    /// connection has since been deleted.
    fn resolve_conn_for_backup(&self, id: &str) -> Option<(dbc_state::ConnectionConfig, Option<String>)> {
        let cfg = self.config.connections.iter().find(|c| c.id == id)?.clone();
        let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
        Some((cfg, secret))
    }

    /// Builds a fresh `BackupSession`, stashes it as `self.modal`, and
    /// returns the `log`/`status`/`cancel` handles the caller needs to wire
    /// up the actual dispatch (or, for a Restore `Confirming` session,
    /// nothing further — dispatch happens later from `confirm_restore`).
    /// Called ONCE per state transition (Backup: once, straight to
    /// `Running`; Restore: once for `Confirming`, then again — with a
    /// brand-new set of handles, nothing shared with the aborted
    /// `Confirming` attempt — for `Running` once "Obnovit" is confirmed).
    fn start_backup_session(
        &mut self,
        kind: backup::BackupKind,
        cfg: &dbc_state::ConnectionConfig,
        target_path: &str,
        command_line: String,
        expected_name: String,
        confirm_input: Option<Entity<connections_ui::TextField>>,
        status: backup::BackupStatus,
        cx: &mut Context<Self>,
    ) -> (backup::BackupLog, Rc<RefCell<backup::BackupStatus>>, backup::CancelSlot) {
        let log: backup::BackupLog = Rc::new(RefCell::new(backup::BackupLogState::default()));
        let status_cell = Rc::new(RefCell::new(status));
        let cancel_slot: backup::CancelSlot = Rc::new(RefCell::new(None));
        // UX-polish §1.4: computed BEFORE the session struct consumes
        // `confirm_input` below. Backup-kind (and the Restore Running
        // re-session) has no input field, so it needs the shared
        // `modal_focus_handle` fallback; a Restore Confirming session
        // carries its own typed-name field and is focused directly by
        // `begin_restore_confirm` instead (unchanged).
        let needs_focus = confirm_input.is_none();
        let session = backup::BackupSession {
            kind,
            engine: cfg.engine,
            connection_id: cfg.id.clone(),
            connection_name: cfg.name.clone(),
            database: cfg.database.clone(),
            log: log.clone(),
            status: status_cell.clone(),
            started_at: std::time::Instant::now(),
            cancel: cancel_slot.clone(),
            confirm_input,
            expected_name,
            command_line,
            target_path: target_path.to_string(),
        };
        self.modal = Some(connections_ui::ModalState::BackupRestore(session));
        if needs_focus {
            self.modal_needs_focus = true;
        }
        cx.notify();
        (log, status_cell, cancel_slot)
    }

    /// Terminal-event handler shared by every engine's dispatch loop below.
    /// Guards against a late-arriving `Finished`/`Failed`/`Ok`/`Err` that
    /// lands AFTER the user already clicked "Zrušit" (which sets `status`
    /// to `Cancelled` immediately, synchronously — see
    /// `cancel_backup_restore`): once `status` is anything other than
    /// `Running`, this is a no-op other than repainting, so a cancelled run
    /// never gets silently overwritten back to Succeeded/Failed, and never
    /// double-records a history entry for the same run.
    fn finish_backup_restore(
        &mut self,
        status: &Rc<RefCell<backup::BackupStatus>>,
        kind: backup::BackupKind,
        connection_name: &str,
        database: &str,
        path: &str,
        started_at_unix: i64,
        elapsed_ms: i64,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Deliberately checks `status` only — NOT `self.modal` — so a
        // non-cancellable (MSSQL/SQLite) session's real outcome is still
        // recorded even after the user has closed/switched away from its
        // modal (`should_cancel_on_teardown` guarantees `status` was never
        // wrongly flipped to `Cancelled` for those two engines in the
        // meantime — see both functions' doc comments, review MAJOR fix).
        let is_running = matches!(*status.borrow(), backup::BackupStatus::Running);
        if !backup::should_record_terminal_event(is_running) {
            cx.notify();
            return;
        }
        *status.borrow_mut() = match &error {
            None => backup::BackupStatus::Succeeded,
            Some(e) => backup::BackupStatus::Failed(e.clone()),
        };
        self.record_backup_restore_history(
            kind,
            connection_name,
            database,
            path,
            started_at_unix,
            elapsed_ms,
            error.as_deref(),
            cx,
        );
        cx.notify();
    }

    /// Synthetic, secret-free history description (Global Constraints:
    /// "never the command line's raw form, never a password") — `path` is a
    /// local filesystem path, never argv, never a connection string.
    /// `record_history_with_kind` (G11 T7) is what makes this run show up
    /// in the History panel with its 🗄 badge instead of as a plain
    /// `"query"` entry.
    fn record_backup_restore_history(
        &mut self,
        kind: backup::BackupKind,
        connection_name: &str,
        database: &str,
        path: &str,
        started_at_unix: i64,
        elapsed_ms: i64,
        error: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let (verb, arrow) = match kind {
            backup::BackupKind::Backup => ("BACKUP", "->"),
            backup::BackupKind::Restore => ("RESTORE", "<-"),
        };
        let description = format!("-- {verb} {database} {arrow} {path}");
        let kind_str = match kind {
            backup::BackupKind::Backup => "backup",
            backup::BackupKind::Restore => "restore",
        };
        self.record_history_with_kind(
            &description,
            connection_name,
            started_at_unix,
            Some(elapsed_ms),
            None,
            error,
            kind_str,
            cx,
        );
    }

    /// "🗄" dropdown icon / palette "Zálohovat databázi…" — opens the SAVE
    /// dialog. Backup is the ONE documented read-only exemption (design
    /// CURATION item 2) — no read-only check anywhere in this method, on
    /// purpose.
    fn open_backup_dialog(&mut self, connection_id: String, _window: &mut Window, cx: &mut Context<Self>) {
        // Single-modal invariant — same guard every dialog opener in this
        // codebase already applies (see `on_monitor_view_event`'s
        // `KillRequested` arm) — also what makes "starting a new operation
        // while one runs" refuse rather than abandon the first one's handle.
        if self.modal.is_some() {
            return;
        }
        let Some((cfg, _secret)) = self.resolve_conn_for_backup(&connection_id) else {
            self.status = "error: připojení nenalezeno".to_string();
            cx.notify();
            return;
        };
        // G15 T8 HARD GATE ITEM 1: the dropdown icon is already hidden for
        // MSSQL (`connections_ui::dropdown_item`'s `.when` guard), but the
        // command palette's `PaletteAction::BackupDatabase` dispatches here
        // directly for whatever connection is active — this is the single
        // source of truth both paths must respect. See
        // `backup::backup_restore_available`'s doc comment for why.
        if !backup::backup_restore_available(cfg.engine) {
            self.status = "zálohování pro MSSQL zatím není k dispozici".to_string();
            cx.notify();
            return;
        }
        let ext = backup_file_ext(cfg.engine);
        let suggested_name = format!("{}-{}.{ext}", cfg.database, backup_timestamp());
        self.status = "volím cíl zálohy…".to_string();
        cx.notify();
        let dialog = cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name));
        cx.spawn(async move |this, cx| {
            let path = match dialog.await {
                Ok(Ok(Some(p))) => p,
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "záloha zrušena".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog pro uložení selhal ({e})");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog pro uložení není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |view, cx| {
                // Review fix (MINOR 1, final whole-branch review): a modal
                // the user opened WHILE this save dialog was in flight wins
                // — same idiom G12's script-run picker continuation uses
                // (main.rs, `start_script_pick`'s post-pick `this.update`
                // arm) — don't let `run_backup_now`/`start_backup_session`
                // clobber it by unconditionally overwriting `self.modal`.
                if view.modal.is_some() {
                    view.status = "záloha zahozena — je otevřený jiný dialog".to_string();
                    cx.notify();
                    return;
                }
                // Binding carry-forward #3: re-verify the connection still
                // exists RIGHT HERE, first, before resolving anything else
                // — the save dialog's async window may have taken
                // arbitrarily long.
                let current_ids: Vec<String> = view.config.connections.iter().map(|c| c.id.clone()).collect();
                if !backup::backup_dispatch_allowed(&connection_id, &current_ids) {
                    view.status = "připojení se během výběru změnilo — akce zrušena".to_string();
                    cx.notify();
                    return;
                }
                let Some((cfg, secret)) = view.resolve_conn_for_backup(&connection_id) else {
                    view.status = "error: připojení nenalezeno".to_string();
                    cx.notify();
                    return;
                };
                let dest_path = path.to_string_lossy().to_string();
                view.run_backup_now(cfg, secret, dest_path, cx);
            });
        })
        .detach();
    }

    /// Dispatches a backup run for `cfg`'s engine — builds the confirm/log
    /// panel (`start_backup_session`, status `Running` immediately — Backup
    /// has no typed-confirm step) then spawns the actual work.
    fn run_backup_now(
        &mut self,
        cfg: dbc_state::ConnectionConfig,
        secret: Option<String>,
        dest_path: String,
        cx: &mut Context<Self>,
    ) {
        let connection_name = cfg.name.clone();
        let database = cfg.database.clone();
        let started_at_unix = unix_now();

        match cfg.engine {
            dbc_state::Engine::Postgres => {
                // Scope reduction (documented, not silent — see this
                // phase's final report): SSH-tunneled Postgres connections
                // aren't tunneled for the EXTERNAL pg_dump/pg_restore/psql
                // path (unlike the normal driver connection, which already
                // tunnels via `connect::open_config`) — refusing outright
                // is safer than either stalling the UI thread opening a
                // tunnel inline or silently dialing the untunneled host
                // with the real password in the child's env.
                if cfg.ssh.is_some() {
                    self.status =
                        "error: zálohování přes SSH tunel zatím není podporováno pro tento engine — použij přímé připojení"
                            .to_string();
                    cx.notify();
                    return;
                }
                let opts = backup::PgBackupOptions { format: backup::PgDumpFormat::Custom, compress: 6 };
                let args =
                    match backup::build_pg_dump_args(&cfg, &cfg.host, cfg.port.unwrap_or(5432), &opts, &dest_path) {
                        Ok(a) => a,
                        Err(e) => {
                            self.status = format!("error: {e}");
                            cx.notify();
                            return;
                        }
                    };
                let program = match runner::resolve_tool_path(self.config.tool_paths.pg_dump.as_deref(), "pg_dump") {
                    Ok(p) => p,
                    Err(e) => {
                        self.status = format!("error: {e}");
                        cx.notify();
                        return;
                    }
                };
                let command_line = backup::display_command_line(&program, &args, secret.as_deref());
                let (log, status, cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Backup,
                    &cfg,
                    &dest_path,
                    command_line,
                    String::new(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let (mut rx, handle) = self.runner.run_external_tool(program, args, secret.clone());
                // BackupHandle wired into the session's cancel slot RIGHT
                // HERE, before this method returns — every teardown path
                // that can observe `self.modal` after this point can also
                // reach this handle's `cancel()`.
                *cancel_slot.borrow_mut() = Some(Rc::new(move || handle.cancel()));
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    while let Some(ev) = rx.recv().await {
                        match ev {
                            backup::BackupEvent::Log(line) => {
                                let ok = this
                                    .update(cx, |_view, cx| {
                                        backup::push_backup_log(&log, line);
                                        cx.notify();
                                    })
                                    .is_ok();
                                if !ok {
                                    return;
                                }
                            }
                            backup::BackupEvent::Finished => {
                                let _ = this.update(cx, |view, cx| {
                                    view.finish_backup_restore(
                                        &status,
                                        backup::BackupKind::Backup,
                                        &connection_name,
                                        &database,
                                        &dest_path,
                                        started_at_unix,
                                        started.elapsed().as_millis() as i64,
                                        None,
                                        cx,
                                    );
                                });
                                return;
                            }
                            backup::BackupEvent::Failed(msg) => {
                                let _ = this.update(cx, |view, cx| {
                                    view.finish_backup_restore(
                                        &status,
                                        backup::BackupKind::Backup,
                                        &connection_name,
                                        &database,
                                        &dest_path,
                                        started_at_unix,
                                        started.elapsed().as_millis() as i64,
                                        Some(msg),
                                        cx,
                                    );
                                });
                                return;
                            }
                        }
                    }
                })
                .detach();
            }
            dbc_state::Engine::Mssql => {
                let command_line = backup::build_backup_sql(&database, &dest_path);
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Backup,
                    &cfg,
                    &dest_path,
                    command_line,
                    String::new(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let spec = ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: secret.clone() };
                let rx = self.runner.run_mssql_backup(spec, database.clone(), dest_path.clone());
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("backup task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Backup,
                            &connection_name,
                            &database,
                            &dest_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
            dbc_state::Engine::Sqlite => {
                let command_line = backup::build_vacuum_into_sql(&dest_path);
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Backup,
                    &cfg,
                    &dest_path,
                    command_line,
                    String::new(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let spec = ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: secret.clone() };
                let rx = self.runner.run_sqlite_backup(spec, dest_path.clone());
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("backup task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Backup,
                            &connection_name,
                            &database,
                            &dest_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
            dbc_state::Engine::Duckdb => {
                // G16 T4 (live since T6 flipped
                // `backup::backup_restore_available(Duckdb)`). Display-only
                // preview of the source db name: DuckDB names a file
                // database after its file stem; execution re-derives it
                // from the engine (SELECT current_database()) — pinned in
                // duckdb_backup_command_line_preview_matches_engine_name.
                let display_src = std::path::Path::new(&cfg.database)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cfg.database.clone());
                let command_line = backup::build_duckdb_backup_sql(&display_src, &dest_path).join("\n");
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Backup,
                    &cfg,
                    &dest_path,
                    command_line,
                    String::new(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let spec = ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: None };
                let rx = self.runner.run_duckdb_backup(spec, dest_path.clone());
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("backup task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Backup,
                            &connection_name,
                            &database,
                            &dest_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
        }
    }

    /// "♻" dropdown icon / palette "Obnovit databázi ze zálohy…" — opens the
    /// OPEN dialog (single file pick). Layer 1 of the 3-layer read-only
    /// posture: refused right here, before even opening the dialog, if
    /// `cfg.read_only` — Restore is NEVER exempt (design CURATION item 2).
    /// Review MINOR A fix: uses `cx.spawn_in(window, ...)` (not the plain
    /// `cx.spawn`) so the async continuation can still reach `window` once
    /// the file dialog resolves — `begin_restore_confirm` needs it to focus
    /// the typed-name field, same as every other modal opener in this
    /// codebase already does for its own first focusable field.
    fn open_restore_dialog(&mut self, connection_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        let Some((cfg, _secret)) = self.resolve_conn_for_backup(&connection_id) else {
            self.status = "error: připojení nenalezeno".to_string();
            cx.notify();
            return;
        };
        if cfg.read_only {
            self.status = "error: připojení je pouze pro čtení — obnovu nelze spustit".to_string();
            cx.notify();
            return;
        }
        // G15 T8 HARD GATE ITEM 1: same single source of truth
        // `open_backup_dialog` checks — see `backup::backup_restore_available`'s
        // doc comment.
        if !backup::backup_restore_available(cfg.engine) {
            self.status = "obnova pro MSSQL zatím není k dispozici".to_string();
            cx.notify();
            return;
        }
        // Review NIT fix: same cheap early-refusal `begin_restore_confirm`
        // performs below — no reason to make the user pick a file for a
        // Postgres/SSH combination that's guaranteed to be refused once
        // they actually confirm.
        if cfg.engine == dbc_state::Engine::Postgres && cfg.ssh.is_some() {
            self.status =
                "error: obnova přes SSH tunel zatím není podporována pro tento engine — použij přímé připojení"
                    .to_string();
            cx.notify();
            return;
        }
        self.status = "volím zdroj obnovy…".to_string();
        cx.notify();
        let dialog = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Obnovit ze zálohy".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let path = match dialog.await {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.remove(0),
                Ok(Ok(_)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "obnova zrušena".to_string();
                        cx.notify();
                    });
                    return;
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = format!("error: dialog pro výběr souboru selhal ({e})");
                        cx.notify();
                    });
                    return;
                }
                Err(_canceled) => {
                    let _ = this.update(cx, |view, cx| {
                        view.status = "error: dialog pro výběr souboru není dostupný".to_string();
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |view, window, cx| {
                view.begin_restore_confirm(connection_id.clone(), path, window, cx);
            });
        })
        .detach();
    }

    /// Second layer of the 3-layer read-only posture (dialog-open-level) +
    /// binding carry-forward #3's identity re-check — both done RIGHT HERE,
    /// before ever building a confirm panel, since the file dialog above is
    /// the async window this connection's config could have changed under.
    /// Builds the `Confirming` session (typed-name field, no dispatch yet)
    /// and focuses it (review MINOR A fix — every sibling modal opener in
    /// this codebase already focuses its own first field the same way).
    fn begin_restore_confirm(
        &mut self,
        connection_id: String,
        path: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Review fix (MINOR 1, final whole-branch review): a modal the user
        // opened WHILE the open-file dialog was in flight wins — same
        // idiom G12's script-run picker continuation uses (main.rs,
        // `start_script_pick`'s post-pick `this.update` arm) — don't let
        // `start_backup_session` clobber it by unconditionally overwriting
        // `self.modal`.
        if self.modal.is_some() {
            self.status = "obnova zahozena — je otevřený jiný dialog".to_string();
            cx.notify();
            return;
        }
        let current_ids: Vec<String> = self.config.connections.iter().map(|c| c.id.clone()).collect();
        if !backup::backup_dispatch_allowed(&connection_id, &current_ids) {
            self.status = "připojení se během výběru změnilo — akce zrušena".to_string();
            cx.notify();
            return;
        }
        let Some((cfg, secret)) = self.resolve_conn_for_backup(&connection_id) else {
            self.status = "error: připojení nenalezeno".to_string();
            cx.notify();
            return;
        };
        if cfg.read_only {
            self.status = "error: připojení je pouze pro čtení — obnovu nelze spustit".to_string();
            cx.notify();
            return;
        }
        // Review NIT fix: refuse an SSH-tunneled Postgres connection HERE,
        // before ever building a preview — `run_restore_now` refuses the
        // exact same case at actual dispatch time (see that method), and
        // showing a full `pg_restore`/`psql` command preview for a run that
        // is guaranteed to be refused later is misleading.
        if cfg.engine == dbc_state::Engine::Postgres && cfg.ssh.is_some() {
            self.status =
                "error: obnova přes SSH tunel zatím není podporována pro tento engine — použij přímé připojení"
                    .to_string();
            cx.notify();
            return;
        }
        let source_path = path.to_string_lossy().to_string();

        let command_line = match plan_restore(&cfg, &source_path) {
            Ok(RestorePlan::PgTool { tool_name, args }) => {
                backup::display_command_line(&tool_name, &args, secret.as_deref())
            }
            Ok(RestorePlan::Mssql) => format!(
                "{}\n{}\n{}",
                backup::build_single_user_sql(&cfg.database, false),
                backup::build_restore_sql(&cfg.database, &source_path),
                backup::build_single_user_sql(&cfg.database, true),
            ),
            Ok(RestorePlan::Sqlite) => format!("copy {source_path} -> {}", cfg.database),
            // G16 T4: same sniff-and-copy preview as Sqlite — the DUCK
            // magic check is runner-level, identical division of labor.
            Ok(RestorePlan::Duckdb) => format!("copy {source_path} -> {}", cfg.database),
            Err(e) => {
                self.status = format!("error: {e}");
                cx.notify();
                return;
            }
        };

        let expected_name = cfg.database.clone();
        let input = cx.new(|cx| connections_ui::TextField::form_field(cx, &expected_name, false));
        // Review MINOR A fix: focus the typed-name field — every sibling
        // modal opener in this codebase already focuses its own first
        // field (`open_connection_dialog`, `on_dropdown_item_click`'s
        // master-password prompt, ...); this one was missing it.
        let input_focus = input.focus_handle(cx);
        self.start_backup_session(
            backup::BackupKind::Restore,
            &cfg,
            &source_path,
            command_line,
            expected_name,
            Some(input),
            backup::BackupStatus::Confirming,
            cx,
        );
        window.focus(&input_focus, cx);
    }

    /// "Obnovit" button (`ModalState::BackupRestore` while `Confirming`) —
    /// THIRD independent read-only check (belt-and-braces, after the
    /// dropdown icon's dim and `open_restore_dialog`'s own refusal), plus a
    /// second `backup_dispatch_allowed` re-check (the typed-name wait is
    /// itself another async-ish window a user could spend arbitrarily long
    /// in). A no-op if the typed name doesn't match `expected_name` — the
    /// button is rendered non-interactive in that case anyway
    /// (`connections_ui::render_backup_restore_panel`), this is defense in
    /// depth.
    fn confirm_restore(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::BackupRestore(session)) = self.modal.clone() else { return };
        if session.kind != backup::BackupKind::Restore
            || !matches!(*session.status.borrow(), backup::BackupStatus::Confirming)
        {
            return;
        }
        let typed = session.confirm_input.as_ref().map(|f| f.read(cx).text()).unwrap_or_default();
        if !backup::confirm_matches(&typed, &session.expected_name) {
            return;
        }

        let current_ids: Vec<String> = self.config.connections.iter().map(|c| c.id.clone()).collect();
        if !backup::backup_dispatch_allowed(&session.connection_id, &current_ids) {
            self.status = "připojení se během výběru změnilo — akce zrušena".to_string();
            self.close_modal(cx);
            cx.notify();
            return;
        }
        let Some((cfg, secret)) = self.resolve_conn_for_backup(&session.connection_id) else {
            self.status = "error: připojení nenalezeno".to_string();
            self.close_modal(cx);
            cx.notify();
            return;
        };
        if let Err(msg) = backup::guard_backup_restore_read_only(backup::BackupOp::Restore, cfg.read_only) {
            self.status = format!("error: {msg}");
            self.close_modal(cx);
            cx.notify();
            return;
        }

        let source_path = session.target_path.clone();
        self.run_restore_now(cfg, secret, source_path, cx);
    }

    /// Dispatches the actual restore work — replaces `self.modal` with a
    /// brand-new `Running` session (fresh `log`/`status`/`cancel` handles;
    /// nothing shared with the just-abandoned `Confirming` one, which never
    /// had anything to cancel).
    fn run_restore_now(
        &mut self,
        cfg: dbc_state::ConnectionConfig,
        secret: Option<String>,
        source_path: String,
        cx: &mut Context<Self>,
    ) {
        let connection_name = cfg.name.clone();
        let database = cfg.database.clone();
        let started_at_unix = unix_now();

        let plan = match plan_restore(&cfg, &source_path) {
            Ok(p) => p,
            Err(e) => {
                self.status = format!("error: {e}");
                cx.notify();
                return;
            }
        };

        match plan {
            RestorePlan::PgTool { tool_name, args } => {
                if cfg.ssh.is_some() {
                    self.status =
                        "error: obnova přes SSH tunel zatím není podporována pro tento engine — použij přímé připojení"
                            .to_string();
                    cx.notify();
                    return;
                }
                let configured = match tool_name.as_str() {
                    "pg_restore" => self.config.tool_paths.pg_restore.as_deref(),
                    _ => self.config.tool_paths.psql.as_deref(),
                };
                let program = match runner::resolve_tool_path(configured, &tool_name) {
                    Ok(p) => p,
                    Err(e) => {
                        self.status = format!("error: {e}");
                        cx.notify();
                        return;
                    }
                };
                let command_line = backup::display_command_line(&program, &args, secret.as_deref());
                let (log, status, cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Restore,
                    &cfg,
                    &source_path,
                    command_line,
                    database.clone(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let (mut rx, handle) = self.runner.run_external_tool(program, args, secret.clone());
                *cancel_slot.borrow_mut() = Some(Rc::new(move || handle.cancel()));
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    while let Some(ev) = rx.recv().await {
                        match ev {
                            backup::BackupEvent::Log(line) => {
                                let ok = this
                                    .update(cx, |_view, cx| {
                                        backup::push_backup_log(&log, line);
                                        cx.notify();
                                    })
                                    .is_ok();
                                if !ok {
                                    return;
                                }
                            }
                            backup::BackupEvent::Finished => {
                                let _ = this.update(cx, |view, cx| {
                                    view.finish_backup_restore(
                                        &status,
                                        backup::BackupKind::Restore,
                                        &connection_name,
                                        &database,
                                        &source_path,
                                        started_at_unix,
                                        started.elapsed().as_millis() as i64,
                                        None,
                                        cx,
                                    );
                                });
                                return;
                            }
                            backup::BackupEvent::Failed(msg) => {
                                let _ = this.update(cx, |view, cx| {
                                    view.finish_backup_restore(
                                        &status,
                                        backup::BackupKind::Restore,
                                        &connection_name,
                                        &database,
                                        &source_path,
                                        started_at_unix,
                                        started.elapsed().as_millis() as i64,
                                        Some(msg),
                                        cx,
                                    );
                                });
                                return;
                            }
                        }
                    }
                })
                .detach();
            }
            RestorePlan::Mssql => {
                let command_line = format!(
                    "{}\n{}\n{}",
                    backup::build_single_user_sql(&database, false),
                    backup::build_restore_sql(&database, &source_path),
                    backup::build_single_user_sql(&database, true),
                );
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Restore,
                    &cfg,
                    &source_path,
                    command_line,
                    database.clone(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let spec = ConnectSpec::Config { cfg: Box::new(cfg.clone()), secret: secret.clone() };
                let rx = self.runner.run_mssql_restore(spec, database.clone(), source_path.clone());
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("restore task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Restore,
                            &connection_name,
                            &database,
                            &source_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
            RestorePlan::Sqlite => {
                let command_line = format!("copy {source_path} -> {database}");
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Restore,
                    &cfg,
                    &source_path,
                    command_line,
                    database.clone(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let db_path = database.clone();
                let rx = self.runner.run_sqlite_restore(db_path, source_path.clone(), cfg.read_only);
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("restore task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Restore,
                            &connection_name,
                            &database,
                            &source_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
            RestorePlan::Duckdb => {
                // G16 T4: byte-for-byte the Sqlite arm above except the
                // runner call — sniff-and-copy is runner-level.
                let command_line = format!("copy {source_path} -> {database}");
                let (_log, status, _cancel_slot) = self.start_backup_session(
                    backup::BackupKind::Restore,
                    &cfg,
                    &source_path,
                    command_line,
                    database.clone(),
                    None,
                    backup::BackupStatus::Running,
                    cx,
                );
                let db_path = database.clone();
                let rx = self.runner.run_duckdb_restore(db_path, source_path.clone(), cfg.read_only);
                let started = std::time::Instant::now();
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        let err = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.message),
                            Err(_) => Some("restore task panicked".to_string()),
                        };
                        view.finish_backup_restore(
                            &status,
                            backup::BackupKind::Restore,
                            &connection_name,
                            &database,
                            &source_path,
                            started_at_unix,
                            started.elapsed().as_millis() as i64,
                            err,
                            cx,
                        );
                    });
                })
                .detach();
            }
        }
    }

    /// "Zrušit" on `ModalState::BackupRestore` — while `Confirming` (nothing
    /// dispatched yet) this is a plain close; while `Running` it only
    /// reaches for the real kill switch (`BackupSession::cancel_now`) and
    /// flips the UI-visible status to `Cancelled` when
    /// `backup::should_cancel_on_teardown` says so — i.e. a REAL cancel
    /// hook is installed (Postgres). Review MAJOR fix: for MSSQL/SQLite
    /// (`session.can_cancel() == false` — no OS child process, only a
    /// `tokio` task driving `Connection::execute`/`fs::copy`, and T4's
    /// runner methods for them expose no cancel hook), this is now a no-op
    /// other than repainting — the panel itself renders "Zrušit" as
    /// non-interactive for that case (`render_backup_restore_panel`), so
    /// this handler shouldn't even be reachable there; kept as a defensive
    /// no-op rather than lying about a cancellation that didn't happen (see
    /// `should_cancel_on_teardown`'s doc comment for the full consequence
    /// chain a wrongly-flipped status used to cause).
    fn cancel_backup_restore(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::BackupRestore(session)) = &self.modal else { return };
        if matches!(*session.status.borrow(), backup::BackupStatus::Confirming) {
            self.close_modal(cx);
            return;
        }
        if backup::should_cancel_on_teardown(session.can_cancel(), session.is_running()) {
            session.cancel_now();
            *session.status.borrow_mut() = backup::BackupStatus::Cancelled;
        }
        cx.notify();
    }

    /// G11 T6 binding carry-forward (BackupHandle has no Drop/kill-on-drop):
    /// the ONE place every teardown path funnels through to guarantee a
    /// still-`Running` backup/restore's handle is cancelled before it could
    /// otherwise be abandoned. Called from:
    /// - `connections_ui::AppView::close_modal` — covers every UI path that
    ///   closes the modal (the panel's own "Zavřít"/"Zrušit" buttons already
    ///   call `cancel_backup_restore` directly for the interactive case;
    ///   this is the backstop for any OTHER code path that closes the modal
    ///   without going through that button, e.g. a future feature).
    /// - `AppView::switch_to_connection` — defensive: `on_open_palette`
    ///   already refuses to open while `self.modal.is_some()` and the
    ///   dropdown overlay itself doesn't render while a modal is open
    ///   (`main.rs`'s `render`: `if self.dropdown_open && self.modal.is_none()`),
    ///   so this path is not reachable through today's UI — kept anyway,
    ///   matching this codebase's "each layer holds on its own" posture
    ///   (`guards.rs`), in case a future entry point calls
    ///   `switch_to_connection` directly.
    /// - the app-quit hook (`main()`, below) — window close.
    /// A no-op when no modal is open, the open modal isn't `BackupRestore`,
    /// its status isn't `Running`, or (review MAJOR fix) there is no REAL
    /// cancel hook installed (`session.can_cancel()` — MSSQL/SQLite have
    /// none, see `backup::should_cancel_on_teardown`'s doc comment). For
    /// those two engines closing the modal here is still safe to let
    /// proceed — there is no OS child process to leak, only a `tokio` task
    /// that will run to completion on its own — but `status` must NOT be
    /// wrongly flipped to `Cancelled`: `finish_backup_restore` doesn't
    /// consult `self.modal` at all, so the real outcome (and its history
    /// record) still lands correctly once that task actually finishes,
    /// PROVIDED `status` was left as `Running` for it to find.
    pub(crate) fn cancel_active_backup_if_running(&mut self) {
        if let Some(connections_ui::ModalState::BackupRestore(session)) = &self.modal {
            if backup::should_cancel_on_teardown(session.can_cancel(), session.is_running()) {
                session.cancel_now();
                *session.status.borrow_mut() = backup::BackupStatus::Cancelled;
            }
        }
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // UX-polish §1.4: deferred focus for overlay openers without a
        // `&mut Window` (see `modal_needs_focus`). Guarded: if the overlay
        // already closed again before this frame, just clear the flag.
        if self.modal_needs_focus {
            self.modal_needs_focus = false;
            if let Some(connections_ui::ModalState::MasterPasswordPrompt { input, .. }) = &self.modal
            {
                // Sidebar rework: the tree's expand/switch vault gate opens
                // this input-owning prompt from a cx-only subscribe callback
                // — focus its field, same end state as the window-having
                // openers (dropdown/test).
                let focus = input.focus_handle(cx);
                window.focus(&focus, cx);
            } else if let Some(connections_ui::ModalState::ScriptName { field, .. }) = &self.modal {
                // T9: an input-owning dialog opened from a cx-only tree
                // subscription — focus its name field, same end state as
                // the window-having openers.
                let focus = field.focus_handle(cx);
                window.focus(&focus, cx);
            } else if matches!(
                self.modal,
                Some(connections_ui::ModalState::WorkspaceMissing { .. })
            ) {
                // T4 review NIT-11: focus the §W4 panel (a tab group), not
                // the shared overlay handle, so Tab descends into its three
                // choices. Still a container, not a button — Enter stays
                // inert (`ModalConfirmKind::Ignore`).
                let focus = self.workspace_panel_focus.clone();
                window.focus(&focus, cx);
            } else if self.modal.is_some() || self.discard_confirm.is_some() {
                window.focus(&self.modal_focus_handle, cx);
            }
        }
        // Context-menu actions that need a `Window` were parked by the
        // cx-only tree subscription; this is the first point in the frame
        // where one exists.
        self.perform_pending_menu_action(window, cx);

        // G6 T7: lazy-diff typing-trigger recompute, BEFORE the popup is
        // drawn below (design §2 grounding) — then sync the flag T5's
        // `SqlInput::up`/`down`/`newline` check to decide whether to
        // consume or propagate (keyboard precedence, plan T7 step 3).
        self.refresh_autocomplete(window, cx);
        let ac_active = self.autocomplete.is_some();
        self.sql.update(cx, |s, _| s.set_autocomplete_active(ac_active));
        // Workspace T8: the ONE per-frame dirtiness recompute (same lazy-
        // poll idiom as the line above). It feeds BOTH the caption strip
        // below and `context_switch_blocked`, which has no `cx` — see
        // `script_dirty_flag`'s doc comment.
        self.script_dirty_flag = self.script_is_dirty(cx);
        let theme = *cx.theme();

        // The SQL editor + tab strip + tab content column, unchanged from
        // pre-Task-6 except that it's now one column in a horizontal row
        // rather than filling the whole window body.
        let mut column = div().flex().flex_col().flex_1().min_w_0();

        // Workspace T8 (Part S §5): the caption strip — rendered ONLY while
        // a script is bound, immediately above the editor, so an unbound
        // (today's) app is pixel-identical to before.
        if let Some(rel) = self.binding_rel() {
            let dirty = self.script_dirty_flag;
            column = column.child(
                div()
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .bg(theme.bg_app)
                    .text_color(theme.text_muted)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(script_caption(&rel, dirty)),
                    )
                    .child(
                        div()
                            .id("script-save")
                            .px_1()
                            .cursor_pointer()
                            // Dim when clean — the save is a no-op then.
                            .text_color(if dirty { theme.text_primary } else { theme.border })
                            .child("Uložit")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_save_script(&SaveScript, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .id("script-unbind")
                            .px_1()
                            .cursor_pointer()
                            .child("Zavřít")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.editor_load_guarded(PendingScriptAction::Unbind, cx)
                            })),
                    ),
            );
        }

        column = column
            .child(
                // Fixed height of 8 lines (SqlInput's own line_height is
                // px(20.), see sql_input.rs render()); the input scrolls
                // internally once the buffer grows past that. The
                // Up/Down/Newline/Escape/Tab handlers below only ever act
                // when `autocomplete_active` — see each's own doc comment
                // — so this is a no-op addition to this div's existing
                // behavior whenever the popup is closed (plan T7 step 3,
                // keyboard precedence item 3).
                div()
                    .h(px(20. * 8. + 4. * 2.))
                    // T8 review: `.h()` is a flex BASE, and the default
                    // `flex-shrink: 1` lets it collapse below eight lines
                    // when the column is tight — likelier now that the
                    // 22px caption strip is a sibling. Pin the height.
                    .flex_shrink_0()
                    .px_2()
                    .bg(theme.bg_app)
                    .on_action(cx.listener(Self::on_ac_up))
                    .on_action(cx.listener(Self::on_ac_down))
                    .on_action(cx.listener(Self::on_ac_confirm))
                    .on_action(cx.listener(Self::on_ac_confirm_tab))
                    .on_action(cx.listener(Self::on_ac_escape))
                    .child(self.sql.clone()),
            );

        // Tab strip only renders when there's at least one open tab (brief
        // contract #2); with none, `render_tab_content` fills the area with
        // a neutral placeholder instead.
        if self.tabs.iter().next().is_some() {
            column = column.child(self.render_tab_strip(cx));
        }
        column = column.child(self.render_tab_content(cx));

        // G2 Task 6: the schema tree panel sits LEFT of `column`,
        // collapsible via Ctrl+B (`ToggleTree`) — collapsed means not
        // rendered at all (width 0), not just visually hidden. The width was
        // a fixed 260 px until the user asked for a draggable splitter
        // (2026-08-28); it is now `sidebar_width`, persisted on drag END.
        let mut body = div().flex().flex_row().flex_1().min_h_0();
        if self.tree_visible {
            body = body.child(
                div()
                    .relative()
                    .w(px(self.sidebar_width))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(theme.border)
                    .child(self.tree.clone())
                    .child(
                        // The splitter. A sibling of the tree rather than
                        // part of its border, so grabbing it can never also
                        // hit a tree row underneath — the same
                        // `.occlude()` reasoning as `grid.rs`'s column
                        // resize handle, which learned it from a bug where
                        // a drag also toggled the sort.
                        div()
                            .id("sidebar-splitter")
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(5.))
                            .h_full()
                            .occlude()
                            .cursor_col_resize()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|view, e: &gpui::MouseDownEvent, _w, cx| {
                                    view.sidebar_resizing =
                                        Some((f32::from(e.position.x), view.sidebar_width));
                                    cx.notify();
                                }),
                            ),
                    ),
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
            .bg(theme.bg_panel)
            .on_action(cx.listener(Self::on_run_query))
            .on_action(cx.listener(Self::on_run_query_unlimited))
            .on_action(cx.listener(Self::on_cancel_query))
            .on_action(cx.listener(Self::on_toggle_tree))
            .on_action(cx.listener(Self::on_toggle_history))
            .on_action(cx.listener(Self::on_open_palette))
            .on_action(cx.listener(Self::on_open_autocomplete))
            .on_action(cx.listener(Self::on_save_script))
            .on_action(cx.listener(Self::on_format_sql))
            .on_action(cx.listener(Self::on_format_sql))
            .child(self.render_top_bar(cx))
            .child(body);

        // Mouse tracking lives on the ROOT, not on the splitter: once the
        // drag starts the pointer routinely leaves the 5 px handle, and a
        // handler bound to the handle would stop receiving moves the instant
        // it did. `on_mouse_up_out` is the other half — releasing outside the
        // window must also end the drag, or the panel keeps following the
        // mouse after the button is up. Both learned from `grid.rs`.
        if self.sidebar_resizing.is_some() {
            root = root
                .on_mouse_move(cx.listener(|view, e: &gpui::MouseMoveEvent, _w, cx| {
                    if let Some((start_x, start_w)) = view.sidebar_resizing {
                        view.sidebar_width =
                            clamp_sidebar_width(start_w + (f32::from(e.position.x) - start_x));
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|view, _e, _w, cx| view.end_sidebar_resize(cx)),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|view, _e, _w, cx| view.end_sidebar_resize(cx)),
                );
        }

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
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .bg(theme.bg_hover)
                .text_color(theme.text_muted)
                .child({
                    // G13 T6: "Vysvětlit" (estimated EXPLAIN) — always safe
                    // on any engine/connection (design §5), so the only
                    // gating here is "one run at a time" + "there's SQL to
                    // run", same as the RunQuery keybinding's own guard.
                    let enabled = self.cancel.is_none() && !self.sql.read(cx).text().trim().is_empty();
                    let color = if enabled { theme.text_primary } else { theme.border };
                    div()
                        .id("btn-explain")
                        .cursor_pointer()
                        .text_color(color)
                        .child("Vysvětlit")
                        .on_click(cx.listener(move |view, _, _window, cx| {
                            if enabled {
                                view.run_explain(false, cx);
                            }
                        }))
                })
                .child({
                    // G13 T6: "Analyzovat" (EXPLAIN ANALYZE) — hidden
                    // entirely for SQLite (design §1c/§4: no such mode at
                    // all, not merely disabled) via `plan::analyze_button_visible`.
                    let engine = self.active_engine();
                    let visible = engine.map(plan::analyze_button_visible).unwrap_or(true);
                    let enabled = visible && self.cancel.is_none() && !self.sql.read(cx).text().trim().is_empty();
                    if !visible {
                        div().into_any_element()
                    } else {
                        let color = if enabled { theme.text_primary } else { theme.border };
                        div()
                            .id("btn-analyze")
                            .cursor_pointer()
                            .text_color(color)
                            .child("Analyzovat")
                            .on_click(cx.listener(move |view, _, _window, cx| {
                                if enabled {
                                    view.run_explain(true, cx);
                                }
                            }))
                            .into_any_element()
                    }
                })
                .child({
                    // G12 T3: „SQL soubor…“/„SQL složku…“ — the script
                    // runner's picker entry points, same toolbar-row
                    // placement precedent as Vysvětlit/Analyzovat above
                    // (adaptation: the plan's grounding describes these as
                    // separate editor-toolbar buttons, but this status-bar
                    // row IS the app's existing "run-adjacent action
                    // buttons" toolbar — reusing it avoids growing a new
                    // toolbar row for two buttons).
                    let enabled = self.cancel.is_none() && self.modal.is_none();
                    let color = if enabled { theme.text_primary } else { theme.border };
                    div()
                        .id("btn-run-sql-file")
                        .cursor_pointer()
                        .text_color(color)
                        .child("SQL soubor…")
                        .on_click(cx.listener(move |view, _, _window, cx| {
                            if enabled {
                                view.start_script_pick(false, cx);
                            }
                        }))
                })
                .child({
                    let enabled = self.cancel.is_none() && self.modal.is_none();
                    let color = if enabled { theme.text_primary } else { theme.border };
                    div()
                        .id("btn-run-sql-folder")
                        .cursor_pointer()
                        .text_color(color)
                        .child("SQL složku…")
                        .on_click(cx.listener(move |view, _, _window, cx| {
                            if enabled {
                                view.start_script_pick(true, cx);
                            }
                        }))
                })
                .child(div().flex_1().child(self.status.clone())),
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
        // G6 T7: last, so it paints above every other overlay it could
        // plausibly coexist with (in practice only while typing in the SQL
        // editor with nothing else open — `render_autocomplete_popup`/
        // `refresh_autocomplete` both already close it whenever a modal is
        // up).
        if let Some(overlay) = self.render_autocomplete_popup(cx) {
            root = root.child(overlay);
        }
        root
    }

}

/// `{database}-{unix-seconds}.{ext}` suggested filename for the backup SAVE
/// dialog — same `SystemTime`-based scheme `grid.rs::export_timestamp` uses
/// for its own Downloads-fallback filenames (no new date-formatting
/// dependency added for this).
fn backup_timestamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn backup_file_ext(engine: dbc_state::Engine) -> &'static str {
    match engine {
        dbc_state::Engine::Postgres => "backup", // pg_dump -Fc (default format here)
        dbc_state::Engine::Mssql => "bak",
        dbc_state::Engine::Sqlite => "sqlite",
        dbc_state::Engine::Duckdb => "duckdb",
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The restore dispatch plan for `cfg`'s engine — shared by
/// `AppView::begin_restore_confirm`'s preview and `AppView::run_restore_now`'s
/// actual dispatch so the two can never disagree about which tool/format
/// was chosen (e.g. a dump-format sniff that somehow differed between
/// preview time and confirm time because the picked file changed on disk
/// in between — re-deriving fresh each time, rather than caching the first
/// result, means a changed file is picked up rather than silently ignored).
enum RestorePlan {
    /// Postgres — `tool_name` is `"pg_restore"` or `"psql"`
    /// (`backup::detect_dump_format`'s sniff), `args` already built
    /// (`backup::build_pg_restore_args`/`build_psql_args`) but NOT yet
    /// paired with a resolved program path — that's a separate,
    /// possibly-failing step (`runner::resolve_tool_path`) the dispatch
    /// side performs right before spawning.
    PgTool { tool_name: String, args: Vec<String> },
    Mssql,
    Sqlite,
    /// G16 T4: sniff-and-copy, mirroring Sqlite — the DUCK-magic check is
    /// runner-level (`runner::run_duckdb_restore`), same division of labor.
    Duckdb,
}

/// Reads AT MOST the first 16 bytes of `path` — exactly what
/// `backup::detect_dump_format`'s `PGDMP`-magic sniff needs, never the
/// whole file. Review BLOCKER fix: `plan_restore` used to `fs::read` the
/// ENTIRE dump (realistically gigabytes) into memory, synchronously on the
/// UI thread, and it runs TWICE per restore (`begin_restore_confirm`'s
/// preview + `run_restore_now`'s actual dispatch) — same bounded-read shape
/// `run_sqlite_restore_inner` (runner.rs) already uses for its own magic-
/// header check. A file shorter than 16 bytes just yields whatever `read`
/// actually filled (`n` bytes) — never a panic on a short buffer.
fn read_sniff_prefix(path: &str) -> Result<[u8; 16], String> {
    use std::io::Read;
    let mut header = [0u8; 16];
    let mut f = std::fs::File::open(path).map_err(|e| format!("nelze číst {path}: {e}"))?;
    f.read(&mut header).map_err(|e| format!("nelze číst {path}: {e}"))?;
    Ok(header)
}

fn plan_restore(cfg: &dbc_state::ConnectionConfig, source_path: &str) -> Result<RestorePlan, String> {
    match cfg.engine {
        dbc_state::Engine::Postgres => {
            let header = read_sniff_prefix(source_path)?;
            let target_host = cfg.host.clone();
            let target_port = cfg.port.unwrap_or(5432);
            match backup::detect_dump_format(&header) {
                backup::DumpFormat::Custom => {
                    let args = backup::build_pg_restore_args(
                        cfg,
                        &target_host,
                        target_port,
                        &backup::PgRestoreOptions::default(),
                        source_path,
                    )?;
                    Ok(RestorePlan::PgTool { tool_name: "pg_restore".to_string(), args })
                }
                backup::DumpFormat::Plain => {
                    let args = backup::build_psql_args(cfg, &target_host, target_port, source_path)?;
                    Ok(RestorePlan::PgTool { tool_name: "psql".to_string(), args })
                }
            }
        }
        dbc_state::Engine::Mssql => Ok(RestorePlan::Mssql),
        dbc_state::Engine::Sqlite => Ok(RestorePlan::Sqlite),
        // G16 T4: no filesystem touch here — the magic sniff is
        // runner-level, same division as sqlite.
        dbc_state::Engine::Duckdb => Ok(RestorePlan::Duckdb),
    }
}

#[cfg(test)]
mod plan_restore_tests {
    use super::*;

    fn pg_cfg() -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: "c1".into(),
            name: "demo".into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Postgres,
            host: "db.internal".into(),
            port: Some(5432),
            database: "shop".into(),
            user: "alice".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    /// Review BLOCKER fix: `read_sniff_prefix` must detect the `PGDMP`
    /// custom-format magic from a large file (simulated with a multi-KB
    /// garbage tail after the magic) while reading only its bounded 16-byte
    /// prefix — never the whole file.
    #[test]
    fn read_sniff_prefix_detects_pgdmp_magic_with_a_large_garbage_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.backup");
        let mut content = b"PGDMP\x01\x0e\x00rest".to_vec();
        content.extend(std::iter::repeat(0xABu8).take(4 * 1024 * 1024)); // 4 MiB tail
        std::fs::write(&path, &content).unwrap();

        let header = read_sniff_prefix(path.to_str().unwrap()).unwrap();
        assert_eq!(backup::detect_dump_format(&header), backup::DumpFormat::Custom);
    }

    #[test]
    fn read_sniff_prefix_never_panics_on_a_short_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.sql");
        std::fs::write(&path, b"-- x").unwrap();

        let header = read_sniff_prefix(path.to_str().unwrap()).unwrap();
        assert_eq!(backup::detect_dump_format(&header), backup::DumpFormat::Plain);
    }

    #[test]
    fn read_sniff_prefix_missing_file_is_an_error_not_a_panic() {
        assert!(read_sniff_prefix(r"D:\definitely\not\a\real\path.backup").is_err());
    }

    #[test]
    fn plan_restore_postgres_picks_pg_restore_for_custom_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.backup");
        std::fs::write(&path, b"PGDMP\x01\x0e\x00rest").unwrap();

        let plan = plan_restore(&pg_cfg(), path.to_str().unwrap()).unwrap();
        assert!(matches!(plan, RestorePlan::PgTool { tool_name, .. } if tool_name == "pg_restore"));
    }

    #[test]
    fn plan_restore_postgres_picks_psql_for_plain_sql() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.sql");
        std::fs::write(&path, b"-- plain sql dump\nCREATE TABLE t (id int);").unwrap();

        let plan = plan_restore(&pg_cfg(), path.to_str().unwrap()).unwrap();
        assert!(matches!(plan, RestorePlan::PgTool { tool_name, .. } if tool_name == "psql"));
    }

    #[test]
    fn plan_restore_mssql_sqlite_and_duckdb_never_touch_the_filesystem() {
        // No file created at these paths at all — Mssql/Sqlite/Duckdb
        // variants must not attempt to read the source file (that's
        // runner-level, the magic-header sniffs for SQLite/DuckDB; MSSQL
        // has no client-side sniff at all).
        let mut mssql = pg_cfg();
        mssql.engine = dbc_state::Engine::Mssql;
        assert!(matches!(plan_restore(&mssql, r"D:\nope.bak"), Ok(RestorePlan::Mssql)));

        let mut sqlite = pg_cfg();
        sqlite.engine = dbc_state::Engine::Sqlite;
        assert!(matches!(plan_restore(&sqlite, r"D:\nope.sqlite"), Ok(RestorePlan::Sqlite)));

        // G16 T4 (was the T3 interim refusal — mechanics landed).
        let mut duckdb = pg_cfg();
        duckdb.engine = dbc_state::Engine::Duckdb;
        assert!(matches!(plan_restore(&duckdb, r"D:\nope.duckdb"), Ok(RestorePlan::Duckdb)));
    }
}

fn main() {
    // The log goes in the PROFILE directory, not the workspace: it
    // describes what this machine did, and a workspace folder is meant to
    // be shared. Same reasoning as `history.sqlite`, which also stays put.
    //
    // First statement in `main` on purpose — the panic hook is worth having
    // installed before anything that can panic runs, since a GPUI app that
    // panics leaves no window and no message behind.
    dbc_state::applog::init(&dbc_state::workspace::profile_dir());
    dbc_state::applog::install_panic_hook();
    dbc_state::applog::log(dbc_state::applog::Event::Startup {
        version: env!("CARGO_PKG_VERSION"),
    });

    // CLI arg is now optional: back-compat direct-connect path (phase 0-2)
    // when present, otherwise the app starts with no active connection and
    // the user picks one from the top-bar switcher (Task 7).
    let conn_url = std::env::args().nth(1);
    // Design §W0.1: workspace mode is a PATH-RESOLUTION change at exactly
    // two call sites; this is one of them (`dbc-mcp::parse_args` is the
    // other — workspace T6). Everything downstream still takes a `&Path`.
    let startup = startup_context(dbc_state::workspace::resolve());
    let config_path = startup.paths.config.clone();
    let vault_path = startup.paths.vault.clone();
    let workspace_root = startup.workspace_root.clone();
    let blocked = startup.blocked.clone();
    // §W4's enforcement, as a value rather than three scattered
    // `blocked.is_some()` conditions (T4 review MINOR-3) — pinned by
    // `workspace_startup_tests::a_broken_start_opens_no_context_store`.
    let loads = startup.loads();
    let blocked_start = blocked.is_some();
    // Design §W4: a broken pointer loads NOTHING. Not the workspace's
    // files (they are unusable — that is what "broken" means), and above
    // all not the profile's (that would be the silent fallback this design
    // bans). The modal opened after the window is the only way forward.
    //
    // A parse error (as opposed to a missing file, which `AppConfig::load`
    // treats as an empty default) means an existing config.toml is
    // corrupt — surfaced in the status bar below rather than silently
    // discarded (final-review must-fix #2). `finish_save` refuses to
    // overwrite the file until it's been moved aside.
    let (config, config_load_error) = if !loads.config {
        (AppConfig::default(), None)
    } else {
        match AppConfig::load(&config_path) {
            Ok(cfg) => (cfg, None),
            Err(e) => (AppConfig::default(), Some(e.to_string())),
        }
    };
    // G3 Task 3: opened once at startup; a failure (e.g. an unwritable
    // config dir) is surfaced in the status bar below but never blocks the
    // rest of the app — `record_history`/the panel's search both treat
    // `history: None` as "no history available" rather than panicking.
    // §W5: history is machine-local in BOTH modes — `workspace_paths`
    // already resolves it to the profile path, so this line is mode-blind
    // (and a blocked start still gets its history, which carries no
    // context-specific connection state).
    let (history, history_open_error) = if !loads.history {
        (None, None)
    } else {
        match HistoryDb::open(&startup.paths.history) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e.to_string())),
        }
    };
    // G4 Task 6: opened once at startup; a failure (e.g. a corrupt
    // views.toml) is surfaced in the status bar below but never blocks the
    // rest of the app — the feature is just off (`view_prefs: None`), same
    // "degrade gracefully" precedent `history_open_error` already follows.
    let (view_prefs, view_prefs_open_error) = if !loads.view_prefs {
        (None, None)
    } else {
        match ViewPrefsStore::load(&startup.paths.views) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e.to_string())),
        }
    };
    // G6 Task 3: same "open at startup, None on failure, degrade
    // gracefully" posture as `view_prefs` — a load failure here only means
    // the values dialog won't prefill/remember values across runs, not
    // that the feature stops working, so (unlike `view_prefs`/`history`)
    // this isn't surfaced as its own startup status notice.
    let param_values =
        if !loads.param_values { None } else { ParamValuesStore::load(&startup.paths.params).ok() };

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", RunQuery, None),
            KeyBinding::new("ctrl-shift-enter", RunQueryUnlimited, None),
            KeyBinding::new("escape", CancelQuery, None),
            KeyBinding::new("ctrl-b", ToggleTree, None),
            KeyBinding::new("ctrl-h", ToggleHistory, None),
            KeyBinding::new("ctrl-k", OpenPalette, None),
            // G6 T7: force-trigger, same "global, context None" precedent
            // as `RunQuery`/`OpenPalette` above (design §2).
            KeyBinding::new("ctrl-space", OpenAutocomplete, None),
            // Part S §5.2/§5.4: global, context `None` — the same posture
            // as `RunQuery`/`OpenPalette`. Bound => save; unbound =>
            // save-as. The chord was free repo-wide before this line.
            KeyBinding::new("ctrl-s", SaveScript, None),
            // Ctrl+Shift+F: the shape every other editor uses for "format
            // document". Ctrl+F is left free for a future find.
            KeyBinding::new("ctrl-shift-f", FormatSql, None),
        ]);
        sql_input::bind_keys(cx);
        grid::bind_keys(cx);
        connections_ui::bind_keys(cx);
        schema_tree::bind_keys(cx);
        palette::bind_keys(cx);

        // G14 Task 1: theme global — installed before the first window opens so
        // every render() can read cx.theme(). `config.theme` is Copy; `config`
        // itself moves into the window closure below untouched.
        cx.set_global(theme::Theme::from_mode(config.theme));

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
                        // T4 review NIT-9: „ready" behind a modal saying
                        // the workspace was not found is a lie, and it is
                        // the line that stays on screen after the modal is
                        // dealt with. §W4's blocked start outranks every
                        // other startup notice — there is no config to be
                        // corrupt and no view prefs to have failed.
                        let status = if blocked_start {
                            connections_ui::WORKSPACE_MISSING_STATUS.to_string()
                        } else if let Some(detail) = &config_load_error {
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
                        // G11 T6 binding carry-forward (BackupHandle has no
                        // Drop/kill-on-drop): the app-quit hook is the
                        // teardown path for "window close" — cancel any
                        // still-Running backup/restore's underlying
                        // process/task BEFORE the app actually exits, since
                        // nothing else reaps an abandoned pg_dump/pg_restore
                        // child.
                        cx.on_app_quit(|view, _cx| {
                            view.cancel_active_backup_if_running();
                            async {}
                        })
                        .detach();
                        // Read before `config` is moved into the struct below.
                        let sidebar_width = sidebar_width_from(config.sidebar_width);
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
                            workspace_root,
                            workspace_pick_generation: 0,
                            workspace_pick_error: None,
                            workspace_choice_focus: [
                                cx.focus_handle(),
                                cx.focus_handle(),
                                cx.focus_handle(),
                            ],
                            workspace_panel_focus: cx.focus_handle(),
                            vault: None,
                            active_connection_id: None,
                            active_database: None,
                            switch_generation: 0,
                            dropdown_open: false,
                            modal: None,
                            grouped_cache,
                            tree,
                            tree_visible: true,
                            sidebar_width,
                            sidebar_resizing: None,
                            pending_menu_action: None,
                            sidebar_fetch_generation: 0,
                            compare_fetch_generation: 0,
                            history,
                            history_visible: true,
                            history_search,
                            history_cache: Vec::new(),
                            last_history_query: String::new(),
                            palette: None,
                            view_prefs,
                            param_values,
                            apply_dialog: None,
                            discard_confirm: None,
                            script_binding: None,
                            script_dirty_flag: false,
                            script_binding_generation: 0,
                            editor_discard_grant: None,
                            script_save_in_flight: false,
                            modal_focus_handle: cx.focus_handle(),
                            modal_needs_focus: false,
                            autocomplete: None,
                            last_ac_text: String::new(),
                            last_ac_cursor: 0,
                        }
                    })
                },
            )
            .unwrap();
        cx.activate(true);

        // Sidebar rework startup wiring: seed the multi-root sidebar's
        // connection roots + context (favourites/read_only/admin/scope/CLI
        // url), then — CLI-arg back-compat path (brief contract #6) — fire
        // the CLI slot's initial schema fetch, exactly like a switch does.
        let _ = window_handle.update(cx, |view, window, cx| {
            // Focus the editor on startup. Without this the app opens with
            // focus nowhere, so the first keystroke is swallowed and
            // Ctrl+A, the arrow keys and autocomplete all appear BROKEN
            // until the user happens to click into the editor — and none of
            // them say why, because `refresh_autocomplete`'s first act is to
            // drop the popup when the editor is not focused. A SQL client
            // that opens ready to type is also just the right default.
            //
            // Startup only: forcing this every frame would fight the grid,
            // the tree and every dialog for focus.
            let editor_focus = view.sql.focus_handle(cx);
            window.focus(&editor_focus, cx);
            let grouped = view.grouped_cache.clone();
            view.tree.update(cx, |t, cx| t.sync_connections(grouped, cx));
            // The tree starts on `TreeGrouping::Schema` and learns the saved
            // choice here — this is what makes the setting survive a
            // restart. A blocked start carries a default config, so this
            // pushes the default, which is the right answer when there is no
            // trustworthy config to read.
            let grouping = view.config.tree_grouping;
            view.tree.update(cx, |t, cx| t.set_grouping(grouping, cx));
            view.refresh_tree_context(cx);
            // Part S §1.2: scan on startup when a root is configured. It is
            // a no-op-with-reset when there is none — and a BLOCKED start
            // has none (`workspace_root` stays `None` and the blocked config
            // is `AppConfig::default()`), so nothing touches the filesystem
            // behind the §W4 modal.
            view.start_scripts_scan(cx);
            if view.conn_url.is_some() {
                view.start_schema_slot_fetch(CLI_CONN_IDENTITY.to_string(), String::new(), cx);
            }
            // G3 Task 3 review fix: populate `history_cache` once at
            // startup (history panel defaults to visible) instead of
            // leaving it empty until the first recorded run/search edit.
            view.refresh_history_cache(cx);
            // Design §W4: the blocking modal goes up LAST, so it occludes a
            // fully-constructed (and deliberately empty) app.
            if let Some((root, reason)) = blocked {
                view.open_workspace_missing_modal(root, reason, cx);
            }
        });
    });
}

#[cfg(test)]
mod preview_sql_tests {
    use super::*;

    #[test]
    fn quotes_schema_and_table_with_limit_1000() {
        assert_eq!(
            preview_sql(dbc_core::Dialect::Postgres, Some("public"), "orders"),
            "SELECT * FROM \"public\".\"orders\" LIMIT 1000"
        );
    }

    #[test]
    fn omits_schema_qualifier_when_none() {
        assert_eq!(
            preview_sql(dbc_core::Dialect::Postgres, None, "orders"),
            "SELECT * FROM \"orders\" LIMIT 1000"
        );
    }

    /// Brief contract #4: a table literally named `we"ird` must not break
    /// out of the query or inject anything — `quote_qualified_d` doubles the
    /// embedded quote.
    #[test]
    fn survives_a_table_name_with_an_embedded_quote() {
        assert_eq!(
            preview_sql(dbc_core::Dialect::Postgres, None, "we\"ird"),
            "SELECT * FROM \"we\"\"ird\" LIMIT 1000"
        );
        assert_eq!(
            preview_sql(dbc_core::Dialect::Postgres, Some("we\"ird"), "t"),
            "SELECT * FROM \"we\"\"ird\".\"t\" LIMIT 1000"
        );
    }

    // G15 T4 required tests.
    #[test]
    fn preview_sql_mssql_uses_top() {
        assert_eq!(
            preview_sql(dbc_core::Dialect::Mssql, Some("public"), "orders"),
            "SELECT TOP 1000 * FROM [public].[orders]"
        );
        assert_eq!(
            preview_sql(dbc_core::Dialect::Mssql, None, "we]ird"),
            "SELECT TOP 1000 * FROM [we]]ird]"
        );
    }

    #[test]
    fn preview_sql_pg_unchanged() {
        assert_eq!(
            preview_sql(dbc_core::Dialect::Postgres, Some("public"), "orders"),
            "SELECT * FROM \"public\".\"orders\" LIMIT 1000"
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

    // G15 T8 ON-flip: MSSQL sandbox editing — see detect_editable_pk's doc
    // comment for the live evidence. read_only still excludes it, same as
    // every other engine (separate test below).
    #[test]
    fn mssql_engine_with_mapped_pk_is_editable() {
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert_eq!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Mssql), Some(&t), &h),
            EditableDecision::Editable(vec![0])
        );
    }

    #[test]
    fn mssql_read_only_connection_is_not_editable_even_with_a_mapped_pk() {
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert_eq!(
            detect_editable_pk(Some((true, dbc_state::Engine::Mssql)), Some(&t), &h),
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

    /// G16 (design §4 REQUIRED matrix rows): DuckDB is sandbox-editable by
    /// construction — no engine arm exists in detect_editable_pk since
    /// G15's flip; `dbc_core::quote_ident` pg-style `"…"`-doubling is
    /// exactly DuckDB's identifier quoting, `sql_value_d` emission is
    /// engine-neutral, the driver populates `is_pk`. read_only still
    /// blocks, same as every engine.
    #[test]
    fn duckdb_engine_is_editable_with_a_mapped_pk_and_read_only_still_blocks() {
        let t = table(vec![col("id", true)]);
        let h = headers(&["id"]);
        assert!(matches!(
            detect_editable_pk(rw_engine(dbc_state::Engine::Duckdb), Some(&t), &h),
            EditableDecision::Editable(_)
        ));
        assert_eq!(
            detect_editable_pk(Some((true, dbc_state::Engine::Duckdb)), Some(&t), &h),
            EditableDecision::NotEditable
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

/// G12 T5: `dialect_for_engine`/`auto_limit_each` pure-decision tests, plus
/// CURATION item 3's mandated test (params resolve BEFORE splitting).
#[cfg(test)]
mod multi_statement_tests {
    use super::*;

    // G15 T8 ON-flip: Mssql now maps to Some(Dialect::Mssql) — see
    // dialect_for_engine's doc comment for the live evidence.
    #[test]
    fn dialect_for_engine_maps_every_engine_including_mssql_and_duckdb() {
        assert_eq!(dialect_for_engine(dbc_state::Engine::Postgres), Some(dbc_core::Dialect::Postgres));
        assert_eq!(dialect_for_engine(dbc_state::Engine::Sqlite), Some(dbc_core::Dialect::Sqlite));
        assert_eq!(dialect_for_engine(dbc_state::Engine::Mssql), Some(dbc_core::Dialect::Mssql));
        // G16: DuckDB splits under the pg dialect — G12 curation item 2
        // (`"…"` ident quoting, LIMIT n, $tag$ dollar quoting are DuckDB's
        // own rules).
        assert_eq!(dialect_for_engine(dbc_state::Engine::Duckdb), Some(dbc_core::Dialect::Postgres));
    }

    #[test]
    fn auto_limit_each_limits_only_bare_selects() {
        let stmts = vec![
            "SELECT * FROM a".to_string(),
            "UPDATE t SET x = 1".to_string(),
            "SELECT * FROM b LIMIT 5".to_string(),
        ];
        let (out, changed) = auto_limit_each(stmts, Some(100), false, dbc_core::Dialect::Postgres);
        assert!(changed);
        assert_eq!(out[0], "SELECT * FROM a LIMIT 100");
        assert_eq!(out[1], "UPDATE t SET x = 1");
        assert_eq!(out[2], "SELECT * FROM b LIMIT 5");
    }

    #[test]
    fn auto_limit_each_bypass_and_none_are_noops() {
        let stmts = vec!["SELECT 1".to_string()];
        assert_eq!(
            auto_limit_each(stmts.clone(), Some(100), true, dbc_core::Dialect::Postgres),
            (stmts.clone(), false)
        );
        assert_eq!(
            auto_limit_each(stmts.clone(), None, false, dbc_core::Dialect::Postgres),
            (stmts, false)
        );
    }

    #[test]
    fn auto_limit_each_mssql_uses_top() {
        let stmts = vec!["SELECT * FROM a".to_string(), "UPDATE t SET x = 1".to_string()];
        let (out, changed) = auto_limit_each(stmts, Some(100), false, dbc_core::Dialect::Mssql);
        assert!(changed);
        assert_eq!(out[0], "SELECT TOP 100 * FROM a");
        assert_eq!(out[1], "UPDATE t SET x = 1");
    }

    #[test]
    fn split_error_message_go_count_is_czech() {
        assert_eq!(
            split_error_message(dbc_core::SplitError::UnsupportedGoCount),
            "GO s počtem opakování není podporováno".to_string()
        );
    }

    #[test]
    fn sql_dialect_is_total() {
        assert_eq!(sql_dialect(dbc_state::Engine::Postgres), dbc_core::Dialect::Postgres);
        assert_eq!(sql_dialect(dbc_state::Engine::Sqlite), dbc_core::Dialect::Sqlite);
        assert_eq!(sql_dialect(dbc_state::Engine::Mssql), dbc_core::Dialect::Mssql);
        assert_eq!(sql_dialect(dbc_state::Engine::Duckdb), dbc_core::Dialect::Postgres);
    }

    /// Batch C review BLOCKER 2 regression: on a read-only MSSQL connection
    /// (where this client-side check is the ONLY read-only enforcement — no
    /// server-side backstop), a bracket-quoted reserved word must not
    /// false-reject a genuine read. Also proves pg behavior is unchanged —
    /// `arr[1]` (an array subscript, not MSSQL bracket-quoting) still reads
    /// fine and a real write is still rejected on both dialects.
    #[test]
    fn read_only_guard_rejects_is_dialect_aware() {
        // MSSQL: bracket-quoted reserved word reads fine on a read-only
        // connection — this is exactly the probe-proven false-reject the
        // plain pg-only `is_read_statement` used to produce.
        assert!(!read_only_guard_rejects(
            "SELECT [Delete] FROM AuditLog",
            true,
            dbc_core::Dialect::Mssql
        ));
        // MSSQL: a real write is still rejected on a read-only connection.
        assert!(read_only_guard_rejects("UPDATE t SET x = 1", true, dbc_core::Dialect::Mssql));
        // MSSQL: writable connection never refuses regardless of statement.
        assert!(!read_only_guard_rejects("UPDATE t SET x = 1", false, dbc_core::Dialect::Mssql));
        // pg regression: `[1]` is an array subscript, not bracket-quoting —
        // behavior must stay exactly as before this fix.
        assert!(!read_only_guard_rejects(
            "SELECT arr[1] FROM t",
            true,
            dbc_core::Dialect::Postgres
        ));
        assert!(read_only_guard_rejects("UPDATE t SET x = 1", true, dbc_core::Dialect::Postgres));
    }

    /// CURATION item 3's mandated test: two statements each carrying `:p` —
    /// params resolve BEFORE splitting, so a substituted literal containing
    /// `;` inside quotes is handled by the splitter's normal string rules.
    #[test]
    fn params_resolve_before_split_two_statements() {
        let names = vec!["p".to_string()];
        let out = build_param_sql(
            "SELECT :p; UPDATE t SET x = :p;",
            &names,
            &[("a;b".to_string(), false)],
        )
        .unwrap();
        assert_eq!(out, "SELECT 'a;b'; UPDATE t SET x = 'a;b';");
        let stmts = dbc_core::split_sql(&out, dbc_core::Dialect::Sqlite).unwrap();
        assert_eq!(stmts, vec!["SELECT 'a;b'".to_string(), "UPDATE t SET x = 'a;b'".to_string()]);
    }
}

/// G12 T3: pure-helper tests behind the script-runner UI's pre-scan/modal
/// logic — `count_statements_in_file`/`list_sql_files`/`script_options_valid`/
/// `script_history_sql`.
#[cfg(test)]
mod script_ui_tests {
    use super::*;

    #[test]
    fn list_sql_files_filters_and_orders_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.sql"), "select 1;").unwrap();
        std::fs::write(dir.path().join("A.SQL"), "select 1;").unwrap();
        std::fs::write(dir.path().join("c.txt"), "nope").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("d.sql"), "select 1;").unwrap(); // non-recursive: ignored
        let files = list_sql_files(dir.path()).unwrap();
        let names: Vec<_> =
            files.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["A.SQL".to_string(), "b.sql".to_string()]);
    }

    #[test]
    fn count_statements_streams_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.sql");
        std::fs::write(&p, "SELECT 1;\n-- c ; c\nSELECT ';';\nSELECT 3").unwrap();
        assert_eq!(count_statements_in_file(&p, dbc_core::Dialect::Sqlite), Ok(3));
    }

    #[test]
    fn count_statements_surfaces_unterminated_as_err() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bad.sql");
        std::fs::write(&p, "SELECT 'oops").unwrap();
        assert!(count_statements_in_file(&p, dbc_core::Dialect::Sqlite).is_err());
    }

    #[test]
    fn whole_run_plus_continue_is_invalid() {
        use crate::runner::{ErrorPolicy::*, TxScope::*};
        assert!(script_options_valid(WholeRun, Stop));
        assert!(!script_options_valid(WholeRun, Continue));
        assert!(script_options_valid(PerFile, Continue));
    }

    #[test]
    fn script_history_sql_single_and_multi_file_wording() {
        let one = vec![(PathBuf::from("C:/s/a.sql"), 5)];
        assert_eq!(script_history_sql(&one, 5, 0), format!("[skript] {} — 5 příkazů, 5 OK, 0 chyb", PathBuf::from("C:/s/a.sql").display()));
        let two = vec![(PathBuf::from("C:/s"), 5), (PathBuf::from("C:/s/b.sql"), 2)];
        let s = script_history_sql(&two, 6, 1);
        assert!(s.starts_with("[skript] "));
        assert!(s.contains("2 souborů"));
        assert!(s.contains("7 příkazů"));
        assert!(s.contains("6 OK"));
        assert!(s.contains("1 chyb"));
    }

    /// Review fix (MAJOR 1): the connection-identity guard behind
    /// `confirm_script_run` — proves the refuse path (identity captured at
    /// `start_script_pick` time != active identity at confirm time), same
    /// shape as `csv_ui_tests::csv_import_dispatch_allowed_refuses_on_identity_mismatch`.
    #[test]
    fn script_run_dispatch_allowed_refuses_on_identity_mismatch() {
        assert!(script_run_dispatch_allowed("conn-a", "conn-a"));
        assert!(!script_run_dispatch_allowed("conn-a", "conn-b"));
        assert!(script_run_dispatch_allowed(CLI_CONN_IDENTITY, CLI_CONN_IDENTITY));
        assert!(!script_run_dispatch_allowed(CLI_CONN_IDENTITY, "conn-a"));
        assert!(!script_run_dispatch_allowed("conn-a", CLI_CONN_IDENTITY));
    }
}

/// G12 T4: pure-helper tests behind the CSV import mapping modal —
/// `default_csv_mapping`/`csv_field_to_value`.
#[cfg(test)]
mod csv_ui_tests {
    use super::*;

    #[test]
    fn default_csv_mapping_matches_names_case_insensitively() {
        let headers = vec!["ID".to_string(), "Name".to_string(), "extra".to_string()];
        let cols = vec![
            csv_import::TargetColumn { name: "id".into(), numeric: true },
            csv_import::TargetColumn { name: "name".into(), numeric: false },
        ];
        let m = default_csv_mapping(&headers, &cols);
        assert_eq!(m.targets, vec![Some(0), Some(1), None]);
    }

    #[test]
    fn csv_field_to_value_empty_is_null() {
        assert_eq!(csv_field_to_value(""), None);
        assert_eq!(csv_field_to_value("0"), Some("0".to_string()));
        assert_eq!(csv_field_to_value(" "), Some(" ".to_string()));
    }

    /// Review fix (BLOCKER): the connection-identity guard behind
    /// `confirm_csv_import` — proves the refuse path (captured identity at
    /// `start_csv_import` time != active identity at confirm time) is a
    /// pure, directly-testable decision, mirroring the shape of
    /// `runner::csv_import_tests::run_csv_import_refuses_read_only_spec_without_touching_anything`:
    /// there, the SHARED read-only guard fires before any file/DB touch;
    /// here, `csv_import_dispatch_allowed` returning `false` is exactly
    /// what stops `confirm_csv_import` from ever building a `CsvImportJob`
    /// or calling `resolve_spec_for_explain`/`self.runner.run_csv_import`
    /// (see the call site in `confirm_csv_import`, main.rs).
    #[test]
    fn csv_import_dispatch_allowed_refuses_on_identity_mismatch() {
        assert!(csv_import_dispatch_allowed("conn-a", "conn-a"));
        assert!(!csv_import_dispatch_allowed("conn-a", "conn-b"));
        // CLI-arg back-compat path: same sentinel on both sides is a match,
        // a switch away from it (to a saved connection, or vice versa) is
        // caught same as any other identity change.
        assert!(csv_import_dispatch_allowed(CLI_CONN_IDENTITY, CLI_CONN_IDENTITY));
        assert!(!csv_import_dispatch_allowed(CLI_CONN_IDENTITY, "conn-a"));
        assert!(!csv_import_dispatch_allowed("conn-a", CLI_CONN_IDENTITY));
    }

    /// Review fix (MINOR 5): short text is untouched; long text is
    /// truncated to the exact char cap plus a trailing ellipsis.
    #[test]
    fn cap_sql_sample_leaves_short_text_alone_and_truncates_long_text() {
        let short = "INSERT INTO t (id) VALUES (1);".to_string();
        assert_eq!(cap_sql_sample(short.clone()), short);

        let long = "x".repeat(CSV_SAMPLE_SQL_DISPLAY_CAP + 500);
        let capped = cap_sql_sample(long);
        assert_eq!(capped.chars().count(), CSV_SAMPLE_SQL_DISPLAY_CAP + 1);
        assert!(capped.ends_with('…'));
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

// Sidebar rework T3: the `(connection, database)` identity widening —
// `conn_identity_for`, `spec_for_database`, and the pure core of
// `resolve_active`. (`conn_spec_key` was deleted in T5 with its last
// consumer, the single-root `trigger_schema_fetch`.)
#[cfg(test)]
mod identity_widening_tests {
    use super::*;

    #[test]
    fn conn_identity_for_composes_with_unit_separator() {
        assert_eq!(conn_identity_for("conn-a", "sales"), "conn-a\u{1F}sales");
    }

    /// THE safety win of the whole phase (design §2.3): the same connection
    /// on two databases is two different identities — every pending write
    /// guard (Apply, admin, script, CSV) captured against one refuses to
    /// dispatch against the other, via the unchanged `conn_identity_matches`.
    #[test]
    fn same_connection_different_database_never_matches() {
        assert!(!conn_identity_matches(
            &conn_identity_for("conn-a", "sales"),
            &conn_identity_for("conn-a", "inventory"),
        ));
        assert!(conn_identity_matches(
            &conn_identity_for("conn-a", "sales"),
            &conn_identity_for("conn-a", "sales"),
        ));
        // Bare pre-phase shape never equals the composite (defensive).
        assert!(!conn_identity_matches("conn-a", &conn_identity_for("conn-a", "sales")));
    }

    fn test_cfg(id: &str, db: &str) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: id.into(), name: "prod".into(), folder: vec![],
            engine: dbc_state::Engine::Postgres, host: "localhost".into(),
            port: Some(5432), database: db.into(), user: "u".into(),
            read_only: true, timeout_secs: Some(30), auto_limit: Some(500),
            ssh: None, favourite: false, mssql: None,
        }
    }

    /// SECURITY (design §3.1): the derived spec inherits EVERYTHING except
    /// `database` — same id (⇒ same vault secret, same prefs bucket root),
    /// same read_only (⇒ server-side enforcement still applies), same
    /// timeout/auto_limit/ssh. No new secret storage.
    #[test]
    fn spec_for_database_swaps_only_the_database() {
        let cfg = test_cfg("conn-a", "sales");
        let spec = spec_for_database(&cfg, "inventory", Some("s3cret".into()));
        let ConnectSpec::Config { cfg: derived, secret } = spec else { panic!("Config expected") };
        assert_eq!(derived.database, "inventory");
        assert_eq!(secret.as_deref(), Some("s3cret"));
        let mut expect = cfg.clone();
        expect.database = "inventory".into();
        assert_eq!(*derived, expect); // read_only/timeout/auto_limit/ssh/engine/id all inherited
    }

    #[test]
    fn resolve_active_from_swaps_db_and_inherits_flags() {
        let mut config = dbc_state::AppConfig::default();
        config.connections.push(test_cfg("conn-a", "sales"));
        // Default database:
        let a = resolve_active_from(&config, None, "conn-a", None).unwrap();
        assert_eq!(a.cfg.database, "sales");
        // Identity coherence (T7: the snapshot no longer carries a
        // pre-composed identity field — see the ActiveConn audit verdict —
        // but the snapshot's cfg must still compose to the expected one):
        assert_eq!(conn_identity_for(&a.cfg.id, &a.cfg.database), conn_identity_for("conn-a", "sales"));
        assert!(a.read_only);
        assert_eq!(a.timeout_secs, Some(30));
        assert_eq!(a.auto_limit, Some(500));
        // Non-default database:
        let a = resolve_active_from(&config, None, "conn-a", Some("inventory")).unwrap();
        assert_eq!(a.cfg.database, "inventory");
        assert_eq!(
            conn_identity_for(&a.cfg.id, &a.cfg.database),
            conn_identity_for("conn-a", "inventory")
        );
        assert!(a.read_only, "read_only inherits into every derived db (design §4.2)");
        // Deleted connection:
        assert!(resolve_active_from(&config, None, "gone", None).is_none());
    }

    /// Batch B review NIT 1: the "never rendered raw" doc claim must hold
    /// in BOTH branches of the display translation — a hostile database
    /// name containing `\u{1F}` renders visibly even when the connection
    /// still exists.
    #[test]
    fn conn_name_for_identity_never_renders_the_raw_separator() {
        let connections = vec![test_cfg("conn-a", "sales")];
        // Found connection, non-default db: name / db.
        assert_eq!(
            conn_name_for_identity_from(&connections, &conn_identity_for("conn-a", "inventory")),
            "prod / inventory"
        );
        // Found connection, default db: bare name.
        assert_eq!(
            conn_name_for_identity_from(&connections, &conn_identity_for("conn-a", "sales")),
            "prod"
        );
        // Found connection, HOSTILE db name with an embedded separator —
        // the found-connection branch must scrub it too (NIT 1's fix).
        let rendered =
            conn_name_for_identity_from(&connections, &conn_identity_for("conn-a", "x\u{1F}y"));
        assert!(!rendered.contains('\u{1F}'), "raw separator leaked: {rendered:?}");
        assert_eq!(rendered, "prod / x / y");
        // Deleted connection: already-scrubbed fallback, unchanged.
        let rendered =
            conn_name_for_identity_from(&connections, &conn_identity_for("gone", "db"));
        assert!(!rendered.contains('\u{1F}'));
        assert_eq!(rendered, "gone / db");
        // CLI sentinel untouched.
        assert_eq!(conn_name_for_identity_from(&connections, CLI_CONN_IDENTITY), "cli");
    }
}

// Sidebar rework T5: pure decision slices of `switch_to_database` (this
// crate has no GPUI harness — the async/entity halves are covered by the
// structural pins in the method itself).
#[cfg(test)]
mod switch_decision_tests {
    use super::*;

    #[test]
    fn db_choice_normalizes_default_to_none() {
        // The `.filter(|d| d != &cfg.database)` line in switch_to_database —
        // pinned so identity/store-key/label logic keeps ONE canonical
        // spelling for "the default database".
        let default = "sales".to_string();
        assert_eq!(Some("sales".to_string()).filter(|d| d != &default), None);
        assert_eq!(
            Some("inventory".to_string()).filter(|d| d != &default),
            Some("inventory".to_string())
        );
    }

    /// The queued action is one-shot open-preview only (design §2.2) —
    /// this pins the enum stays single-variant (a second queued kind needs
    /// its own design pass).
    #[test]
    fn pending_tree_action_is_open_preview_only() {
        let a = PendingTreeAction::OpenPreview { schema: None, table: "t".into() };
        match a {
            PendingTreeAction::OpenPreview { .. } => {}
        }
    }

    /// T5 review MAJOR 2: each `switch_to_database` dispatch OWNS its
    /// follow-up (parameter → spawn-closure capture; there is no shared
    /// `pending_after_switch` field any more), and the success arm runs
    /// only under `switch_generation == my_generation` — this models that
    /// guard exactly: the superseded dispatch's follow-up can never
    /// replay, because its owning closure returns before reaching it.
    /// (Cancel disarmament is by the same ownership: the vault/confirm
    /// detours carry the follow-up INSIDE their pending payload —
    /// `PendingAfterUnlock::SwitchDatabase` / `PendingDiscard::
    /// SwitchDatabase` — which cancel drops wholesale.)
    #[test]
    fn superseded_switch_dispatch_never_replays_its_follow_up() {
        let mut switch_generation = 0u64;
        // Dispatch 1 carries a follow-up for (c1, dbB):
        switch_generation += 1;
        let d1 = (
            switch_generation,
            Some(PendingTreeAction::OpenPreview { schema: None, table: "orders".into() }),
        );
        // Dispatch 2 (user switched elsewhere) supersedes before d1
        // resolves:
        switch_generation += 1;
        let d2 = (switch_generation, None::<PendingTreeAction>);
        // The success arm's guard, verbatim shape:
        let applies = |my_generation: u64| my_generation == switch_generation;
        assert!(!applies(d1.0), "superseded dispatch must not run — its follow-up dies with it");
        assert!(applies(d2.0));
        drop((d1, d2));
    }

    /// T5 review MINOR 3: a switch attempted under ANY open overlay is
    /// refused outright — in particular an open discard-confirm from
    /// another flow must never let the switch bypass the dirty-admin
    /// confirmation.
    #[test]
    fn switch_refused_under_any_open_overlay() {
        assert!(!switch_blocked_by_overlay(false, false, false));
        assert!(switch_blocked_by_overlay(true, false, false));
        assert!(switch_blocked_by_overlay(false, true, false));
        assert!(switch_blocked_by_overlay(false, false, true));
        assert!(switch_blocked_by_overlay(true, true, true));
    }
}

// Sidebar rework T7: the identity-widening AUDIT's guard-family tests
// (design §7) — pinned ON THE FINAL CODE that no guard got weaker: every
// family refuses a stale identity across a SAME-CONNECTION database
// switch, which the pre-phase bare-id identities all passed.
#[cfg(test)]
mod identity_audit_tests {
    use super::*;

    /// Design §7's headline fix, per guard family: a SAME-CONNECTION
    /// database switch invalidates every pending write captured against
    /// the previous database. Pre-phase identities (bare ids) passed all
    /// four of these — pinning that they now refuse.
    #[test]
    fn same_connection_db_switch_refuses_script_and_csv_dispatch() {
        let sales = conn_identity_for("conn-a", "sales");
        let inventory = conn_identity_for("conn-a", "inventory");
        assert!(!script_run_dispatch_allowed(&sales, &inventory));
        assert!(!csv_import_dispatch_allowed(&sales, &inventory));
        assert!(script_run_dispatch_allowed(&sales, &sales));
    }

    /// Apply flow (on_open_apply_dialog / on_confirm_apply backstop /
    /// render_apply_bar dim-out all route through conn_identity_matches).
    #[test]
    fn apply_guard_refuses_across_db_switch_and_reenables_on_return() {
        let sales = conn_identity_for("conn-a", "sales");
        let inventory = conn_identity_for("conn-a", "inventory");
        assert!(!conn_identity_matches(&sales, &inventory));
        // Switching BACK re-enables the dimmed tab — staged grid edits are
        // never dropped by a switch, only inert while away (resolved
        // deviation 11's grid half).
        assert!(conn_identity_matches(&sales, &conn_identity_for("conn-a", "sales")));
    }

    /// Admin singleton: a db switch now yields Replace (stale staged admin
    /// edits must never survive a context switch — design §5 row 4).
    #[test]
    fn admin_open_decision_replaces_across_db_switch() {
        let mut tabs = Tabs::new();
        tabs.open(ResultTab {
            id: 0,
            title: "Správa serveru".into(),
            pinned: false,
            preview_key: Some(admin_panel::ADMIN_PREVIEW_KEY.to_string()),
            conn_identity: conn_identity_for("conn-a", "sales"),
            content: TabContent::Text { text: String::new(), scroll_lines: 0 },
        });
        assert!(matches!(
            admin_open_decision(&tabs, &conn_identity_for("conn-a", "inventory")),
            AdminOpenDecision::Replace(_)
        ));
        assert!(matches!(
            admin_open_decision(&tabs, &conn_identity_for("conn-a", "sales")),
            AdminOpenDecision::Activate(_)
        ));
    }

    /// Monitor tab singleton key widens automatically → one monitor tab
    /// per (conn, db) — consistent with its DATA_SIZE tile being
    /// per-database (design §5 row 5).
    #[test]
    fn monitor_preview_key_scopes_per_database() {
        assert_ne!(
            format!("monitor:{}", conn_identity_for("conn-a", "sales")),
            format!("monitor:{}", conn_identity_for("conn-a", "inventory")),
        );
    }

    /// CLI sentinel unchanged and never equal to any composite identity.
    #[test]
    fn cli_sentinel_is_disjoint_from_composites() {
        assert!(!conn_identity_matches(CLI_CONN_IDENTITY, &conn_identity_for("conn-a", "sales")));
        assert!(!conn_identity_matches(&conn_identity_for("cli", "x"), CLI_CONN_IDENTITY));
    }
}

// G10 T4: `admin_open_decision` — the pure decision behind `open_admin_tab`'s
// singleton-per-connection dedup/replace.
#[cfg(test)]
mod admin_open_tests {
    use super::*;

    fn admin_tab(identity: &str) -> ResultTab {
        ResultTab {
            id: 0,
            title: "Správa serveru".into(),
            pinned: false,
            preview_key: Some(admin_panel::ADMIN_PREVIEW_KEY.to_string()),
            conn_identity: identity.to_string(),
            content: TabContent::Text { text: String::new(), scroll_lines: 0 },
        }
    }

    #[test]
    fn admin_tab_is_singleton_per_connection() {
        let mut tabs = Tabs::new();
        assert_eq!(admin_open_decision(&tabs, "conn-a"), AdminOpenDecision::OpenFresh);
        let id = tabs.open(admin_tab("conn-a"));
        assert_eq!(admin_open_decision(&tabs, "conn-a"), AdminOpenDecision::Activate(id));
        assert_eq!(admin_open_decision(&tabs, "conn-b"), AdminOpenDecision::Replace(id));
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

// G6 Task 3: `build_param_sql` — pure `:name` substitution + the
// CURATION-mandated post-substitution rescan (design §5), tested standalone
// without a GPUI window (same "pure helper + its own `#[cfg(test)] mod`"
// convention as `preview_sql_tests`/`decide_retrigger_action_tests` above).
#[cfg(test)]
mod query_params_tests {
    use super::*;

    #[test]
    fn substitutes_string_and_numeric_and_null() {
        let names = vec!["name".to_string(), "age".to_string(), "note".to_string()];
        let values = vec![
            ("Alice".to_string(), false),
            ("30".to_string(), false),
            (String::new(), true),
        ];
        let sql = "SELECT * FROM t WHERE name = :name AND age = :age AND note = :note";
        let out = build_param_sql(sql, &names, &values).unwrap();
        assert_eq!(
            out,
            "SELECT * FROM t WHERE name = 'Alice' AND age = 30 AND note = NULL"
        );
    }

    #[test]
    fn empty_text_without_null_flag_is_empty_string_literal() {
        let names = vec!["note".to_string()];
        let values = vec![(String::new(), false)];
        let out = build_param_sql("UPDATE t SET note = :note", &names, &values).unwrap();
        assert_eq!(out, "UPDATE t SET note = ''");
    }

    #[test]
    fn repeated_param_name_substitutes_every_occurrence() {
        let names = vec!["x".to_string()];
        let values = vec![("5".to_string(), false)];
        let out = build_param_sql("WHERE a = :x OR b = :x", &names, &values).unwrap();
        assert_eq!(out, "WHERE a = 5 OR b = 5");
    }

    // CURATION-mandated (design §5): a substituted value that happens to
    // look like a `:name` token must never be allowed to reach the driver
    // unescaped — the post-substitution rescan must catch it and refuse.
    #[test]
    fn post_substitution_rescan_rejects_a_surviving_bare_param() {
        // A pathological template where the "value" text itself contains
        // `:leak` and is substituted into a position `sql_value` does NOT
        // quote (a non-numeric value always gets single-quoted, so this
        // simulates the defense actually firing on a scanner/positional
        // mismatch rather than proving it's reachable via normal typed
        // input — the rescan is deliberately unconditional, design §5).
        let names = vec!["x".to_string()];
        // A value containing a literal, unquoted `:leak` sequence next to
        // the substituted SQL text (outside any string this function
        // produces) reproduces the "bare :name survives substitution"
        // condition the rescan exists to catch.
        let sql_template = "SELECT :x";
        let out = build_param_sql(sql_template, &names, &[("1 UNION SELECT :leak".to_string(), false)]);
        // sql_value quotes any non-numeric text, so the substituted value
        // becomes a single string literal — the leaked `:leak` sits INSIDE
        // that string literal's quotes, meaning find_params correctly does
        // NOT flag it (it's not bare). This asserts the safe case succeeds...
        assert!(out.is_ok());
        // ...whereas a template that still has an UNRESOLVED name at
        // substitution time (a name in the SQL not present in `names`,
        // which `build_param_sql` maps to a defensive NULL — an
        // implementation bug scenario) must still round-trip safely:
        let out2 = build_param_sql("SELECT :x, :y", &names, &[("1".to_string(), false)]);
        assert_eq!(out2, Ok("SELECT 1, NULL".to_string()));
    }

    #[test]
    fn post_substitution_rescan_rejects_when_substitute_params_itself_fails_closed() {
        // sql_template with an unterminated string — substitute_params
        // returns None, build_param_sql must surface that as Err, not
        // silently pass the unmodified (still-parametrized) template
        // through to the caller.
        let out = build_param_sql("SELECT ':x", &["x".to_string()], &[("1".to_string(), false)]);
        assert!(out.is_err());
    }

    #[test]
    fn unscannable_substitution_result_is_refused_not_passed_through() {
        // Final-review fix: `$tag:x$` scans as literal `$tag` + param `:x` +
        // `$`; substituting the numeric value `1` yields `$tag1$` — a valid
        // dollar-quote OPENER with no closer, so the rescan's `find_params`
        // returns None (unscannable). Fail closed: Err, never Ok.
        let out = build_param_sql("SELECT $tag:x$", &["x".to_string()], &[("1".to_string(), false)]);
        assert!(out.is_err(), "unscannable rescan result must be refused, got {out:?}");
    }
}

#[cfg(test)]
mod move_selection_tests {
    use super::*;

    #[test]
    fn moves_down_within_bounds() {
        assert_eq!(move_selection(2, 5, 1), 3);
    }

    #[test]
    fn moves_up_within_bounds() {
        assert_eq!(move_selection(2, 5, -1), 1);
    }

    #[test]
    fn clamps_at_the_top_no_wraparound() {
        assert_eq!(move_selection(0, 5, -1), 0);
    }

    #[test]
    fn clamps_at_the_bottom_no_wraparound() {
        assert_eq!(move_selection(4, 5, 1), 4);
    }

    #[test]
    fn empty_list_always_yields_zero() {
        assert_eq!(move_selection(0, 0, 1), 0);
        assert_eq!(move_selection(0, 0, -1), 0);
    }
}

#[cfg(test)]
mod completion_edit_tests {
    use super::*;

    #[test]
    fn replaces_partial_prefix_with_full_candidate() {
        let (range, new_text) = completion_edit("SELECT sel", 10, "SELECT");
        assert_eq!(range, 7..10);
        assert_eq!(new_text, "SELECT SELECT");
    }

    #[test]
    fn force_trigger_with_no_prefix_inserts_at_cursor() {
        let (range, new_text) = completion_edit("SELECT ", 7, "FROM");
        assert_eq!(range, 7..7);
        assert_eq!(new_text, "SELECT FROM");
    }

    #[test]
    fn qualified_completion_only_replaces_the_column_part() {
        let (range, new_text) = completion_edit("SELECT o.tot", 12, "total");
        assert_eq!(range, 9..12);
        assert_eq!(new_text, "SELECT o.total");
    }

    // --- Review round 3 fixes ---

    /// MAJOR 3: the previous byte-only prefix walk truncated a non-ASCII
    /// prefix (`čas` -> only `as`), which then corrupted the replace-range
    /// math — `č` was left in place in the untouched region AND duplicated
    /// by the inserted candidate (`SELECT ččasovka`). The walk is now
    /// char-based (`autocomplete::cursor_context`), so the whole `čas`
    /// prefix is replaced cleanly.
    #[test]
    fn non_ascii_prefix_is_replaced_in_full_not_truncated_or_duplicated() {
        let (range, new_text) = completion_edit("SELECT čas", 11, "časovka");
        assert_eq!(range, 7..11);
        assert_eq!(new_text, "SELECT časovka");
    }

    /// RE-VERIFY: `accept_completion` cannot clear the buffer, whatever it
    /// is passed. It has no length parameter any more — it reads the span
    /// from `completion_range`, so what it deletes is one identifier
    /// prefix ending at the cursor, and everything before that prefix is
    /// untouchable by it. That is why this mutator needs no `BufferReplace`
    /// permit: `replace_buffer` can destroy an arbitrary amount of unsaved
    /// work and this provably cannot.
    ///
    /// The worst case is the whole buffer BEING one identifier, and then
    /// replacing it with the candidate is the completion the user asked
    /// for, not a clobber.
    #[test]
    fn accepting_a_completion_can_only_ever_replace_an_identifier_prefix() {
        // The shape the bypass used: a caller wanting to wipe everything.
        // There is no parameter for it, so the most it reaches is `tot`.
        let text = "SELECT a, b, c FROM orders WHERE tot";
        let range = completion_range(text, text.len());
        assert_eq!(&text[range.clone()], "tot");
        assert_eq!(range.start, text.len() - 3, "everything before the prefix is untouchable");

        // No prefix at the cursor -> an EMPTY range: an accept there
        // inserts and deletes nothing.
        let after_space = "SELECT ";
        assert!(completion_range(after_space, after_space.len()).is_empty());

        // The whole buffer as one identifier is the only case where the
        // range spans everything, and that is a completion, not a clobber.
        assert_eq!(completion_range("sel", 3), 0..3);

        // A cursor inside a longer word still only reaches backwards.
        assert_eq!(completion_range("usXer", 2), 0..2);
    }

    /// NIT: cursor-mid-word behavior is intentional (see `completion_edit`'s
    /// doc comment) — only the prefix BEFORE the cursor is replaced; a
    /// suffix already typed past the cursor (`Xer` here) is left as-is,
    /// not merged against the inserted candidate.
    #[test]
    fn mid_word_cursor_only_replaces_the_prefix_before_the_cursor() {
        let (range, new_text) = completion_edit("usXer", 2, "users");
        assert_eq!(range, 0..2);
        assert_eq!(new_text, "usersXer");
    }
}

#[cfg(test)]
mod autocomplete_handles_action_tests {
    use super::*;

    #[test]
    fn consumes_only_while_the_popup_is_open() {
        assert!(autocomplete_handles_action(true));
    }

    #[test]
    fn propagates_when_the_popup_is_closed() {
        // Review round 3, BLOCKER: this is the exact case `on_ac_escape`
        // previously got wrong (returned early without propagating),
        // silently eating Escape and making the global CancelQuery binding
        // unreachable while the editor had focus.
        assert!(!autocomplete_handles_action(false));
    }
}

/// Workspace T4 (design §W4): the never-silent-fallback rail, unit-tested
/// as pure data. `Resolution` is plain data (dbc-state T2), so the whole
/// "a configured-but-unusable workspace never yields a profile path"
/// property is provable without GPUI, a filesystem, or a tempdir.
/// T7 review MAJOR-1, pinned mechanically rather than by hand: EVERY
/// `config.toml` writer in this crate must pass `guard_corrupt_config`
/// first. The hazard is not one function's oversight — it is that the
/// guard is a CONVENTION with no compiler behind it, and the one writer
/// that forgot it (`set_theme`) was reachable by reflex from two surfaces.
/// Same shape as `dbc-mcp`'s `no_write_path_regression`: scan our own
/// source, build the needle at runtime so the check cannot flag itself.
///
/// T10 CARRY-FORWARD 7, the same widening Task 8's re-verify applied to
/// `editor_clobber_audit`. Two holes, both closed by reusing that audit's
/// machinery instead of keeping a second, weaker copy of it here:
///
/// * **The file list was a hand-written pair.** `main.rs` and
///   `connections_ui.rs` happen to hold all six writers TODAY, and the
///   other 31 `.rs` files in the crate were invisible to this test.
///   `AppView.config` and `config_path` are private-but-crate-reachable
///   (`main.rs` is the crate root, so every module is a descendant), so an
///   `impl crate::AppView` block in any other module could have saved an
///   unparsable config with this test green. It now walks `src/` at test
///   time, so a NEW file is covered the moment it exists.
/// * **The owner detector was a prefix match on three spellings.**
///   `fn ` / `pub fn ` / `pub(crate) fn ` — so a write inside an
///   `async fn`, a `pub(super) fn` or a `const fn` was attributed to the
///   PREVIOUS function, and a guard call in THAT function's body
///   sanctioned it. `defined_fn_name` strips an arbitrary visibility and
///   any order of qualifiers, so attribution is correct for every fn
///   spelling Rust allows.
///
/// What deliberately did NOT change is the RULE. `editor_clobber_audit`
/// sanctions by owner NAME (a fixed set of functions may call the
/// dangerous thing); this audit asks that the guard be CALLED in the same
/// function, above the write, whatever that function is called — because
/// the guard is cheap, idempotent and correct to call unconditionally, so
/// a new writer should add the call rather than add its name to a list.
/// `#[must_use]` on `guard_corrupt_config` (T10 carry-forward 1) is the
/// compiler half of the same rail: this test proves the call is THERE, the
/// attribute proves its verdict is not thrown away.
#[cfg(test)]
mod sidebar_width_tests {
    use super::*;

    #[test]
    fn an_unset_width_is_the_built_in_default() {
        assert_eq!(sidebar_width_from(None), SIDEBAR_DEFAULT_W);
    }

    #[test]
    fn a_stored_width_round_trips_when_it_is_in_range() {
        assert_eq!(sidebar_width_from(Some(317)), 317.0);
    }

    /// The reason the clamp is on LOAD and not only on drag: a `config.toml`
    /// hand-edited to `5`, or written on a wide monitor and reopened on a
    /// narrow one, would otherwise leave a panel too thin to contain the
    /// 5 px splitter — with no way to drag it back.
    #[test]
    fn a_stored_width_outside_the_range_is_pulled_back_in() {
        assert_eq!(sidebar_width_from(Some(5)), SIDEBAR_MIN_W, "too thin to grab");
        assert_eq!(sidebar_width_from(Some(u16::MAX)), SIDEBAR_MAX_W, "wider than any screen");
        assert_eq!(sidebar_width_from(Some(0)), SIDEBAR_MIN_W, "0 could never be dragged back");
    }

    /// `f32::clamp` PANICS on NaN, and this runs inside a mouse-move
    /// handler — a panic there takes the window down mid-gesture.
    #[test]
    fn a_nan_width_does_not_panic_and_falls_back_to_the_default() {
        assert_eq!(clamp_sidebar_width(f32::NAN), SIDEBAR_DEFAULT_W);
    }

    #[test]
    fn infinities_clamp_rather_than_escape() {
        assert_eq!(clamp_sidebar_width(f32::INFINITY), SIDEBAR_MAX_W);
        assert_eq!(clamp_sidebar_width(f32::NEG_INFINITY), SIDEBAR_MIN_W);
    }

    /// The drag stores `start_w + dx`, so dragging left past the minimum
    /// produces a NEGATIVE candidate, not merely a small one.
    #[test]
    fn dragging_far_left_stops_at_the_minimum() {
        assert_eq!(clamp_sidebar_width(SIDEBAR_DEFAULT_W - 9999.0), SIDEBAR_MIN_W);
    }
}

#[cfg(test)]
mod config_save_guard_audit {
    use super::editor_clobber_audit::{code_lines, defined_fn_name, sources};

    /// Every writer's ENCLOSING fn must call the guard above the write.
    ///
    /// FINAL-REVIEW MAJOR-2: **the needle is receiver-independent now.**
    /// It used to be the literal `.config.save(`, which sees only one
    /// spelling of one receiver — and the reviewer walked around it in two
    /// lines on a live production path:
    ///
    /// ```ignore
    /// let cfg = &self.config;
    /// let _ = cfg.save(&self.config_path);   // invisible to `.config.save(`
    /// ```
    ///
    /// Zero warnings, every audit green. `AppConfig::save(&self.config, …)`
    /// would have done the same job.
    ///
    /// The fix keys on the ARGUMENT instead of the receiver: a config
    /// write is a `.save(`/`::save(` call whose line names `config_path`.
    /// Rebinding the receiver, spelling the call UFCS-style or wrapping it
    /// in a macro all leave that argument in place, because it is the
    /// thing being written to. (`dbc-state`'s own `config.save(&p, …)`
    /// tests use a different variable and are correctly not matched.)
    ///
    /// This is the belt; the braces are `dbc_state::ConfigSaveGuard`,
    /// which `AppConfig::save` demands by type and which only
    /// `AppConfig::verify_savable` can mint.
    #[test]
    fn every_config_toml_write_passes_the_corrupt_config_guard() {
        // Built at runtime — written as literals, these would match this
        // test's own source and report phantom call sites.
        let field = format!("{}_{}", "config", "path");
        let call = format!(".{}(", "save");
        let ufcs = format!("::{}(", "save");
        let guard = format!("guard_corrupt_{}", "config");
        let mut sites = 0usize;
        for (name, src) in sources() {
            // Comments (incl. `///`) and string literals count neither as
            // a call site NOR as the guard: this check was vacuous in its
            // first draft because `set_theme`'s explanatory comment
            // MENTIONS the guard, and a prose mention satisfied it.
            let lines = code_lines(&src);
            for (i, line) in lines.iter().enumerate() {
                if !line.contains(&field) || !(line.contains(&call) || line.contains(&ufcs)) {
                    continue;
                }
                sites += 1;
                let start =
                    lines[..i].iter().rposition(|l| defined_fn_name(l).is_some()).unwrap_or(0);
                assert!(
                    lines[start..i].iter().any(|l| l.contains(&guard)),
                    "unguarded config.toml write at {name}:{} (in `{}`) — a config that \
                     failed to parse would be overwritten with defaults, destroying every \
                     connection (T7 review MAJOR-1)",
                    i + 1,
                    lines
                        .get(start)
                        .and_then(|l| defined_fn_name(l))
                        .unwrap_or_else(|| "<file scope>".to_string())
                );
            }
        }
        // Pinned so a NEW writer forces a deliberate look at this test
        // rather than silently inheriting whatever its neighbours do.
        //
        // 6 → 7 on 2026-08-28: `end_sidebar_resize` persists the splitter
        // width. Re-audited rather than bumped — the write is inside the
        // `if let Some(guard) = self.guard_corrupt_config(cx)` arm (this
        // test's own loop proves that, since it reports position, not just
        // count), it runs at drag END only, and an unchanged width returns
        // before reaching it so a bare click on the splitter writes nothing.
        //
        // 7 → 8 on 2026-08-28: `toggle_tree_grouping` persists the schema
        // tree's shape. Same re-audit: guarded arm, and it can only run from
        // a deliberate click on the header icon — there is no continuous
        // gesture behind it, so it needs no equality check of its own (the
        // value provably changed, it is a two-state flip).
        assert_eq!(sites, 8, "config.toml writer count changed — re-audit, do not just bump");
    }

    /// The widening is only worth anything if it actually reaches past the
    /// two files the old hand list named (T10 carry-forward 7). Same
    /// non-vacuity posture as `editor_clobber_audit`'s own rail.
    #[test]
    fn the_audit_reads_the_whole_crate_not_a_pair_of_files() {
        let files: Vec<String> = sources().into_iter().map(|(n, _)| n).collect();
        assert!(files.len() > 20, "the tree walk collapsed to {} files: {files:?}", files.len());
        for expected in [
            "crates/dbc-ui/src/main.rs",
            "crates/dbc-ui/src/connections_ui.rs",
            "crates/dbc-ui/src/scripts.rs",
            "crates/dbc-ui/src/schema_tree.rs",
            // Final-review MAJOR-2: and past dbc-ui altogether.
            "crates/dbc-state/src/config.rs",
        ] {
            assert!(files.iter().any(|f| f == expected), "{expected} missing from {files:?}");
        }
    }
}

#[cfg(test)]
mod workspace_startup_tests {
    use super::*;
    use dbc_state::workspace::{profile_paths, workspace_paths, Resolution};

    #[test]
    fn no_pointer_starts_in_profile_mode_with_todays_paths() {
        let ctx = startup_context(Resolution::Profile(profile_paths()));
        assert_eq!(ctx.paths, profile_paths());
        assert_eq!(ctx.workspace_root, None);
        assert!(ctx.blocked.is_none());
    }

    #[test]
    fn a_valid_pointer_starts_in_workspace_mode_over_the_folder() {
        let root = PathBuf::from("D:\\ws");
        let ctx = startup_context(Resolution::Workspace {
            root: root.clone(),
            paths: workspace_paths(&root),
        });
        assert_eq!(ctx.paths.config, root.join("config.toml"));
        assert_eq!(ctx.paths.vault, root.join("vault.bin"));
        // §W5: history stays machine-local even in workspace mode.
        assert_eq!(ctx.paths.history, profile_paths().history);
        assert_eq!(ctx.workspace_root, Some(root));
        assert!(ctx.blocked.is_none());
    }

    #[test]
    fn a_broken_pointer_blocks_and_never_yields_a_single_profile_path() {
        // THE never-silent-fallback rail (design §W4).
        let root = PathBuf::from("D:\\ws-gone");
        let ctx = startup_context(Resolution::Broken {
            root: Some(root.clone()),
            reason: "složka neexistuje".to_string(),
        });
        assert_eq!(ctx.blocked, Some((Some(root.clone()), "složka neexistuje".to_string())));
        assert_eq!(ctx.workspace_root, None, "a broken workspace is NOT an active workspace");
        let p = profile_paths();
        for got in [&ctx.paths.config, &ctx.paths.vault, &ctx.paths.views, &ctx.paths.params] {
            assert_ne!(got, &p.config);
            assert_ne!(got, &p.vault);
            assert_ne!(got, &p.views);
            assert_ne!(got, &p.params);
        }
        assert!(ctx.paths.config.starts_with(&root), "blocked paths stay inside the broken root");
    }

    #[test]
    fn a_broken_pointer_with_no_readable_root_still_never_targets_the_profile() {
        let ctx = startup_context(Resolution::Broken {
            root: None,
            reason: "ukazatel na pracovní prostor je poškozený: expected a table".to_string(),
        });
        assert!(ctx.blocked.is_some());
        let p = profile_paths();
        assert_ne!(ctx.paths.config, p.config);
        assert_ne!(ctx.paths.vault, p.vault);
        // The sentinel folder does not exist and is never created, so any
        // stray save fails LOUDLY instead of overwriting the profile the
        // user has not chosen (never-destructive rail).
        assert!(!ctx.paths.config.exists());
    }

    #[test]
    fn blocked_paths_never_collide_with_the_profile_dir_itself() {
        let p = blocked_paths(None);
        assert_ne!(p.config.parent(), Some(dbc_state::workspace::profile_dir().as_path()));
    }

    /// T4 review MAJOR-2. `blocked_paths(Some(root))` used to return
    /// `workspace_paths(root)` for WHATEVER the pointer said — so a
    /// hand-written `workspace.toml` naming the profile dir yielded the
    /// profile's real `config.toml`/`vault.bin`/`views.toml`/`params.toml`,
    /// directly contradicting the fn's own doc comment. Not reachable
    /// today (no writer runs while the blocking modal is up), but the
    /// failure mode is an empty default config written over the user's
    /// real connections, so the promise is now structural.
    #[test]
    fn a_pointer_aimed_at_the_profile_dir_falls_through_to_the_sentinel() {
        let prof = dbc_state::workspace::profile_dir();
        let sentinel = blocked_paths(None);
        let real = profile_paths();

        let got = blocked_paths(Some(&prof));
        assert_eq!(got, sentinel, "the profile dir must resolve to the sentinel");
        assert_ne!(got.config, real.config);
        assert_ne!(got.vault, real.vault);
        assert_ne!(got.views, real.views);
        assert_ne!(got.params, real.params);

        // A non-canonical spelling of the same directory must not slip
        // past the exact-match arm. `canonicalize` needs the path to
        // exist, so this half only runs where the profile dir is real.
        if prof.is_dir() {
            let odd = prof.join(".");
            assert_eq!(blocked_paths(Some(&odd)), sentinel, "{}", odd.display());
        }

        // A genuine workspace root is still used as-is.
        let real_ws = prof.join("nekde-jinde-workspace");
        assert_eq!(blocked_paths(Some(&real_ws)).config, real_ws.join("config.toml"));
    }

    /// FINAL-REVIEW MINOR-2, the residual the test above did not cover.
    /// The pointer's `path` is arbitrary TOML text — `read_pointer` hands
    /// back whatever string it finds, and a hand-edited pointer never goes
    /// through `write_pointer`'s absolute-path rail. A `path = ""` walked
    /// past every guard (`"" != profile`; `Path::new("").canonicalize()`
    /// errors on Windows, so `is_same_dir` said `false`) and `base` became
    /// `""` — whereupon `workspace_paths` returned the bare RELATIVE names
    /// `config.toml`, `vault.bin`, …, which the OS resolves against the
    /// process CWD. Launch the app from `%APPDATA%\dbc` and those are the
    /// profile's real files: precisely the promise `blocked_paths` makes.
    #[test]
    fn a_root_that_cannot_be_named_absolutely_falls_through_to_the_sentinel() {
        let sentinel = blocked_paths(None);
        // The empty pointer.
        assert_eq!(blocked_paths(Some(Path::new(""))), sentinel);
        // Any relative spelling at all — including `..`, which
        // canonicalizes just fine and is therefore the SUBTLE half: it
        // still resolves against the process CWD, which this app does not
        // set. All of these used to produce CWD-relative store paths.
        for rel in ["config", "..", "./nekde", "a/b"] {
            let got = blocked_paths(Some(Path::new(rel)));
            assert_eq!(got, sentinel, "relative root {rel:?} must not survive");
        }
        // The property that actually matters, stated directly: whatever a
        // blocked start's paths are, they are ABSOLUTE, so nothing about
        // them depends on the process working directory.
        for root in [None, Some(Path::new("")), Some(Path::new("a/b"))] {
            let p = blocked_paths(root);
            for path in [&p.config, &p.vault, &p.views, &p.params] {
                assert!(path.is_absolute(), "{} is CWD-relative", path.display());
            }
        }
    }

    /// The rule [`blocked_paths`] rests on, in isolation and against a
    /// SUPPLIED profile dir, so the three „we do not know where this
    /// meant" cases are pinned without depending on the machine's real
    /// `%APPDATA%`.
    #[test]
    fn the_blocked_base_accepts_only_an_absolute_non_profile_folder() {
        let prof = PathBuf::from(r"C:\Users\x\AppData\Roaming\dbc");
        assert_eq!(blocked_base(None, &prof), None, "no root at all");
        assert_eq!(blocked_base(Some(Path::new("")), &prof), None, "the empty pointer");
        assert_eq!(blocked_base(Some(Path::new("nekde")), &prof), None, "relative, unresolvable");
        assert_eq!(blocked_base(Some(Path::new("..")), &prof), None, "relative, but resolvable");
        assert_eq!(blocked_base(Some(&prof), &prof), None, "the profile dir itself");
        let ws = PathBuf::from(r"D:\ws");
        assert_eq!(blocked_base(Some(&ws), &prof), Some(ws.clone()), "a real root is kept");
    }

    /// T4 review MINOR-3: the rail's ENFORCEMENT, not just its paths. The
    /// five tests above all decide where stores would open; this one pins
    /// that on a blocked start they are not opened at all — the property a
    /// "simplification" of `fn main()`'s conditions would silently undo.
    #[test]
    fn a_broken_start_opens_no_context_store() {
        let blocked = startup_context(Resolution::Broken {
            root: Some(PathBuf::from("D:\\ws-gone")),
            reason: "složka neexistuje".to_string(),
        })
        .loads();
        assert!(!blocked.config, "config.toml is THE silent-fallback vector");
        assert!(!blocked.view_prefs);
        assert!(!blocked.param_values);
        // §W5: history is machine-local in both modes and carries no
        // context — it is the one store a blocked start may still open.
        assert!(blocked.history);

        for ok in [
            startup_context(Resolution::Profile(profile_paths())).loads(),
            startup_context(Resolution::Workspace {
                root: PathBuf::from("D:\\ws"),
                paths: workspace_paths(Path::new("D:\\ws")),
            })
            .loads(),
        ] {
            assert!(ok.config && ok.view_prefs && ok.param_values && ok.history);
        }
    }

    // ---------- The scripts-root seam (workspace T7, design §W8) ----------

    #[test]
    fn workspace_mode_roots_the_scripts_tree_in_the_folder() {
        let root = PathBuf::from("D:\\ws");
        assert_eq!(
            scripts_root_for(Some(&root), Some("C:\\jinde")),
            Some(root.join("scripts")),
        );
    }

    #[test]
    fn scripts_dir_is_inert_in_workspace_mode() {
        // §W8: one root per mode, no precedence question — a hand-edited
        // `scripts_dir` in a workspace config.toml is ignored, and this is
        // the test that says so out loud.
        let root = PathBuf::from("D:\\ws");
        assert_eq!(scripts_root_for(Some(&root), None), Some(root.join("scripts")));
        assert_eq!(
            scripts_root_for(Some(&root), Some("C:\\jinde")),
            scripts_root_for(Some(&root), None),
        );
    }

    #[test]
    fn profile_mode_uses_the_configured_scripts_dir_or_nothing() {
        assert_eq!(scripts_root_for(None, Some("C:\\skripty")), Some(PathBuf::from("C:\\skripty")));
        assert_eq!(scripts_root_for(None, None), None);
    }

    #[test]
    fn the_scripts_subdir_name_comes_from_dbc_state_not_a_local_literal() {
        assert_eq!(dbc_state::workspace::SCRIPTS_SUBDIR, "scripts");
        let root = PathBuf::from("D:\\ws");
        assert_eq!(
            scripts_root_for(Some(&root), None).unwrap(),
            root.join(dbc_state::workspace::SCRIPTS_SUBDIR),
        );
    }
}

/// T5 review MAJOR-1: `start_workspace_pick`'s continuation guard. The
/// three scenarios below are all reachable on the pre-fix code, because
/// `classify()` runs in `cx.background_spawn` (it yields the UI thread)
/// while `ModalState::Settings` is Esc-closable — so `open_workspace_confirm`
/// could raw-assign over ANY modal the user reached meanwhile.
#[cfg(test)]
mod workspace_pick_guard_tests {
    use super::*;

    #[test]
    fn a_current_pick_over_the_settings_modal_opens_the_confirm() {
        assert_eq!(workspace_pick_verdict(true, 7, 7), WorkspacePickVerdict::Open);
    }

    /// SCENARIO (A) — the context changing after the user cancelled.
    /// Pick folder A on a slow share → Esc closes Settings → re-open, pick
    /// local folder B → confirm → `running = true`, B's init dispatched →
    /// `classify(A)` finally returns. The generation has NOT bumped yet
    /// (B's `apply_context` has not run), so it is the MODAL check that has
    /// to catch this: without it, A's continuation overwrote the running
    /// confirm with `running: false`, unlatching Esc/„Zrušit" while B's
    /// init kept going toward an unconditional `apply_context(B)`.
    #[test]
    fn a_pick_landing_on_a_running_confirm_is_refused_not_committed() {
        // Settings is not open — a `WorkspaceConfirm { running: true }` is.
        assert_eq!(workspace_pick_verdict(false, 7, 7), WorkspacePickVerdict::OtherDialog);
    }

    /// SCENARIO (B) — a running `BackupRestore`. Same verdict, and the
    /// point is what does NOT happen: no raw assign, so the one teardown
    /// funnel that cancels a live `pg_restore` child (`close_modal` →
    /// `cancel_active_backup_if_running`) is never bypassed.
    ///
    /// SCENARIO (C) — a half-typed `ConnectionDialog`. Same verdict; the
    /// typed host/user/password survive. Both are the same predicate input
    /// as (A): "the modal is not Settings".
    #[test]
    fn a_pick_landing_on_any_other_dialog_is_refused_with_a_status() {
        for current in [0u64, 7, u64::MAX] {
            assert_eq!(
                workspace_pick_verdict(false, current, current),
                WorkspacePickVerdict::OtherDialog
            );
        }
    }

    /// A superseded pick is refused SILENTLY — not with a status line.
    /// The generation is checked first precisely so a pick that is both
    /// stale AND in the wrong modal says nothing: the user has already
    /// reached a newer explicit decision, and „výběr složky zahozen…"
    /// landing over „pracovní prostor: D:\\ws" would be a stale write over
    /// it. This is `recovery_pick_may_commit`'s posture.
    #[test]
    fn a_superseded_pick_is_silent_whatever_modal_is_open() {
        assert_eq!(workspace_pick_verdict(true, 7, 8), WorkspacePickVerdict::Superseded);
        assert_eq!(workspace_pick_verdict(false, 7, 8), WorkspacePickVerdict::Superseded);
    }

    /// Pins that the two refusals are DISTINCT states, not one bool. If a
    /// future edit collapses them, either a superseded pick starts writing
    /// status over a fresh context, or a wrong-modal pick goes silent and
    /// the user never learns why nothing happened.
    #[test]
    fn the_two_refusals_are_not_interchangeable() {
        assert_ne!(WorkspacePickVerdict::Superseded, WorkspacePickVerdict::OtherDialog);
        assert_ne!(WorkspacePickVerdict::Open, WorkspacePickVerdict::OtherDialog);
        assert_ne!(WorkspacePickVerdict::Open, WorkspacePickVerdict::Superseded);
    }
}

/// T4 review MAJOR-1: the „Najít složku…" continuation's commit guard.
/// The race it closes: broken pointer → modal → „Najít složku…" → the
/// picker and `classify()` (a `read_dir`, seconds on a network share) run
/// while the window stays interactive → the user gives up and clicks
/// „Použít lokální profil" → the stale task lands and swaps them into
/// workspace mode anyway, overriding an explicit choice.
#[cfg(test)]
mod recovery_pick_guard_tests {
    use super::*;

    #[test]
    fn a_current_pick_over_the_open_modal_commits() {
        assert!(recovery_pick_may_commit(true, 7, 7));
    }

    #[test]
    fn a_closed_modal_refuses_the_pick() {
        // „Použít lokální profil" closed it: the user has ALREADY chosen.
        assert!(!recovery_pick_may_commit(false, 7, 7));
    }

    #[test]
    fn a_superseded_generation_refuses_the_pick() {
        // A context swap happened under the task. Belt to the modal
        // check's braces: a modal reopened in the meantime must not make a
        // stale pick look current again.
        assert!(!recovery_pick_may_commit(true, 7, 8));
        assert!(!recovery_pick_may_commit(false, 7, 8));
    }

    #[test]
    fn both_conditions_are_required_not_either() {
        // Pins the AND. An OR here would reopen the exact hole: the modal
        // is closed but the generation happens to match, or vice versa.
        for (open, dispatched, current) in
            [(true, 1u64, 2u64), (false, 1, 1), (false, 1, 2)]
        {
            assert!(!recovery_pick_may_commit(open, dispatched, current));
        }
    }

    /// FINAL-REVIEW MINOR-1, the STRUCTURE of the fix rather than its
    /// copy (which `connections_ui` pins): the picker continuation must
    /// stage the folder and stop, and the pointer write must live behind
    /// the user's confirmation.
    ///
    /// This is also, incidentally, a strengthening of T4 review MAJOR-1's
    /// own rule — „nothing is persisted until the UI-thread guard passes"
    /// becomes „nothing is persisted in this function at all", and the one
    /// write that remains has no await anywhere near it, so it needs no
    /// re-verification.
    #[test]
    fn the_recovery_pick_stages_a_folder_and_only_the_confirm_writes_the_pointer() {
        let src = include_str!("main.rs");
        // The body ends at the next item OR its doc comment — stopping
        // only at `\n    fn ` would swallow the NEXT function's `///`
        // block, and these assertions are about code, not about prose that
        // happens to name it. (Found the honest way: the first draft of
        // this test failed on `confirm_workspace_recovery`'s own doc
        // comment mentioning `write_pointer`.)
        let slice = |marker: &str| -> String {
            let b = src.split(marker).nth(1).unwrap_or_else(|| panic!("{marker} exists"));
            let end = ["\n    fn ", "\n    /// ", "\n    pub"]
                .iter()
                .filter_map(|m| b.find(m))
                .min()
                .unwrap_or(b.len());
            b[..end].to_string()
        };

        let pick = slice("fn pick_workspace_for_recovery(");
        // Non-vacuity: the slice is the real continuation.
        assert!(pick.contains("prompt_for_paths"), "the sliced body is not the real one");
        assert!(pick.contains("recovery_pick_may_commit"), "the sliced body is not the real one");
        assert!(
            !pick.contains("write_pointer"),
            "the pick must not persist anything — adopting a workspace is the user's call, \
             and they have not been shown the vault line or the git warning yet"
        );
        assert!(
            !pick.contains("apply_context"),
            "the pick must not swap the context either — that is the confirm's job"
        );
        assert!(pick.contains("pending"), "the pick must stage the folder for the confirm");

        let confirm = slice("fn confirm_workspace_recovery(");
        assert!(confirm.contains("write_pointer"), "the confirm is what commits");
        assert!(confirm.contains("apply_context"), "…and what swaps the context");
        // The reason no post-await re-check is needed here: there is no
        // await. If one is ever added, this fails and the author has to
        // decide what to re-verify — the phase's own repeated lesson.
        assert!(
            !confirm.contains(".await") && !confirm.contains("cx.spawn"),
            "the confirm is synchronous by design; an await here needs a re-check"
        );

        // „Zpět" returns to the choices; it must never close the one modal
        // the design says cannot be closed, or the app is left with no
        // context and no dialog.
        let back = slice("fn cancel_workspace_recovery(");
        assert!(back.contains("pending"), "the sliced body is not the real one");
        assert!(
            !back.contains("close_modal"),
            "cancelling the adopt must return to the three choices, not dismiss the blocking modal"
        );
    }
}

#[cfg(test)]
mod script_binding_tests {
    use super::*;

    #[test]
    fn dirty_is_an_exact_compare_with_a_length_short_circuit() {
        assert!(!script_text_is_dirty("SELECT 1", "SELECT 1"));
        assert!(script_text_is_dirty("SELECT 1", "SELECT 2"));
        assert!(script_text_is_dirty("SELECT 1", "SELECT 1 "), "trailing space counts");
        // Whitespace-only differences are REAL differences: the file is the
        // truth and „ •" must not lie about it.
        assert!(script_text_is_dirty("a\r\nb", "a\nb"), "line endings count");
    }

    #[test]
    fn the_caption_relativizes_against_the_current_root_and_falls_back_to_the_name() {
        let root = PathBuf::from(r"D:\ws\scripts");
        assert_eq!(
            script_caption_rel(&root.join("prod").join("trzby.sql"), Some(&root)),
            "prod/trzby.sql"
        );
        // Outside the root (save-as onto the desktop, or the root changed
        // under a binding that holds an ABSOLUTE path) => bare file name.
        assert_eq!(
            script_caption_rel(Path::new(r"C:\jinde\ad-hoc.sql"), Some(&root)),
            "ad-hoc.sql"
        );
        assert_eq!(script_caption_rel(Path::new(r"C:\jinde\ad-hoc.sql"), None), "ad-hoc.sql");
    }

    #[test]
    fn the_caption_uses_the_tab_title_dirty_convention_exactly() {
        assert_eq!(script_caption("prod/trzby.sql", false), "Skript: prod/trzby.sql");
        assert_eq!(script_caption("prod/trzby.sql", true), "Skript: prod/trzby.sql •");
    }

    /// DEVIATION from the plan's `script_open_refusal` helper, recorded:
    /// `crate::scripts::read_script` ALREADY owns the stat + cap + UTF-8
    /// decision (and the symlink refusal the plan's inline snippet dropped),
    /// and the plan's own Interfaces list names `read_script` as a Task 8
    /// consumer. A second size probe in `main.rs` would be exactly the
    /// duplicate-rail defect the Global Constraints ban, so the refusal is
    /// pinned where it is produced.
    #[test]
    fn the_open_cap_refusal_names_the_limit_and_the_way_out() {
        let td = tempfile::tempdir().unwrap();
        let big = td.path().join("velky.sql");
        std::fs::write(&big, vec![b' '; (crate::scripts::SCRIPT_OPEN_CAP + 1) as usize]).unwrap();
        assert_eq!(
            crate::scripts::read_script(&big).unwrap_err(),
            "soubor je příliš velký pro editor (limit 1 MiB) — spusťte jej jako skript"
        );
        let ok = td.path().join("maly.sql");
        std::fs::write(&ok, vec![b' '; crate::scripts::SCRIPT_OPEN_CAP as usize]).unwrap();
        assert!(crate::scripts::read_script(&ok).is_ok());
    }

    #[test]
    fn save_as_appends_sql_when_missing_and_never_twice() {
        // Fact 0.6: GPUI file dialogs have no extension filter at the
        // pinned rev, so the `.sql` rule is client-side, here.
        assert_eq!(with_sql_extension(Path::new(r"C:\a\dotaz")), PathBuf::from(r"C:\a\dotaz.sql"));
        assert_eq!(
            with_sql_extension(Path::new(r"C:\a\dotaz.sql")),
            PathBuf::from(r"C:\a\dotaz.sql")
        );
        assert_eq!(
            with_sql_extension(Path::new(r"C:\a\dotaz.SQL")),
            PathBuf::from(r"C:\a\dotaz.SQL")
        );
        assert_eq!(
            with_sql_extension(Path::new(r"C:\a\dotaz.txt")),
            PathBuf::from(r"C:\a\dotaz.txt.sql")
        );
    }

    /// Part S §5.5's copy, and the fallback that keeps the pre-existing
    /// staged-rows prompt byte-identical for every non-script action.
    #[test]
    fn the_discard_prompt_names_the_script_and_leaves_the_row_prompt_alone() {
        assert_eq!(
            discard_confirm_question(Some("prod/trzby.sql"), 1),
            "Neuložené změny skriptu prod/trzby.sql budou zahozeny."
        );
        assert_eq!(discard_confirm_question(None, 3), "Neuložené změny (3) — zahodit?");
    }

    /// The stale-continuation rule (`script_binding_generation`). The
    /// phase has produced four MAJORs of the „a background step resumed
    /// after the user had moved on" class; a save-as that lands after the
    /// user opened a different script must not write to — or bind to —
    /// the new target, and a plain re-save must not be mistaken for one.
    #[test]
    fn only_a_different_target_counts_as_the_user_moving_on() {
        let a = PathBuf::from(r"D:\ws\scripts\a.sql");
        let b = PathBuf::from(r"D:\ws\scripts\b.sql");
        assert!(!script_binding_target_changed(Some(&a), Some(&a)), "a re-save is not a move");
        assert!(!script_binding_target_changed(None, None));
        assert!(script_binding_target_changed(Some(&a), Some(&b)));
        assert!(script_binding_target_changed(Some(&a), None), "unbinding is a move");
        assert!(script_binding_target_changed(None, Some(&a)), "save-as binds where nothing was");
    }

    /// T8 review BLOCKER-1. The guard runs at dispatch; `read_script` then
    /// yields the UI thread. The generation rail alone is structurally
    /// blind to typing — `set_script_binding` bumps only on a PATH change
    /// — so an open that lands over fresh keystrokes destroyed them
    /// permanently (`SqlInput` has no undo) and silently (`bind_script`
    /// clears the status). Both halves are load-bearing.
    #[test]
    fn an_open_may_only_land_while_the_root_the_binding_and_the_buffer_all_stand_still() {
        let root = PathBuf::from(r"D:\ws\scripts");
        let other = PathBuf::from(r"D:\jiny-ws\scripts");
        let ok = |root_now: Option<&Path>, g: u64, t: &str| {
            script_open_abort_reason(root_now, &root, g, 4, t, "SELECT 1")
        };
        assert_eq!(ok(Some(&root), 4, "SELECT 1"), None);

        // BLOCKER-1's half: same binding, typed buffer.
        assert_eq!(
            ok(Some(&root), 4, "SELECT 1 -- rozepsáno"),
            Some("otevření skriptu zrušeno — mezitím jste psali do editoru")
        );
        // Even one character from an EMPTY start — the unbound ad-hoc text
        // the guard deliberately does not protect from USER actions is
        // still protected from a background read landing on it.
        assert_eq!(
            script_open_abort_reason(Some(&root), &root, 0, 0, "s", ""),
            Some("otevření skriptu zrušeno — mezitím jste psali do editoru")
        );
        // The half that already existed: the binding moved.
        assert_eq!(
            ok(Some(&root), 5, "SELECT 1"),
            Some("otevření skriptu zrušeno — editor se mezitím změnil")
        );

        // NEW MAJOR's half: the ROOT moved under an open that resolved its
        // rel against the old one. Both other checks pass here — nothing
        // was bound, nobody typed — which is exactly why the generation
        // alone could not see it.
        let swapped = "otevření skriptu zrušeno — složka skriptů se mezitím změnila";
        assert_eq!(ok(Some(&other), 4, "SELECT 1"), Some(swapped));
        // …including „Odebrat" in profile mode, which leaves no root at all.
        assert_eq!(ok(None, 4, "SELECT 1"), Some(swapped));
        // The root is reported FIRST: it is the coarsest change, and
        // blaming the editor for a workspace swap would be a lie.
        assert_eq!(ok(Some(&other), 5, "typed"), Some(swapped));

        // FINAL-REVIEW NIT-3: the root leg folds like every other path
        // comparison in this crate. A re-pick that spells the SAME folder
        // with different casing is not a swap, and aborting the open there
        // would be a refusal the user cannot act on.
        let same_folder_other_casing = PathBuf::from(r"D:\WS\Scripts");
        assert_eq!(ok(Some(&same_folder_other_casing), 4, "SELECT 1"), None);
        // …and the fold does not make two DIFFERENT folders equal.
        assert_eq!(ok(Some(&PathBuf::from(r"D:\ws\scripts2")), 4, "SELECT 1"), Some(swapped));
    }

    /// T8 re-verify NEW MAJOR, the half no pure function can express: the
    /// swap must NOT rely on `set_script_binding`'s path-changed heuristic,
    /// because the case that matters is the one where the heuristic is
    /// silent — and an unbound editor is never dirty, so the gate lets that
    /// exact state through. Source-pinned (T9's `run_script_from_library`
    /// precedent), non-vacuously: the load-bearing markers are asserted
    /// present before the requirement is asserted at all.
    #[test]
    fn a_context_swap_supersedes_in_flight_opens_even_with_nothing_bound() {
        // The trap, restated as a fact this test depends on.
        assert!(!script_binding_target_changed(None, None), "the heuristic is silent here");

        let src = include_str!("main.rs");
        let body = src.split("fn apply_context(").nth(1).expect("apply_context exists");
        let body = &body[..body.find("\n    fn ").unwrap_or(body.len())];
        for marker in ["clear_active_connection", "reset_scripts", "set_script_binding(None)"] {
            assert!(body.contains(marker), "the sliced body is not the real apply_context");
        }
        assert!(
            body.contains("supersede_script_continuations()"),
            "a context swap must invalidate in-flight script continuations OUTRIGHT — \
             `set_script_binding(None)` bumps nothing when nothing was bound, so an \
             `open_script` dispatched against the OLD root would land and bind it"
        );
    }

    /// T8 review MAJOR-2's refusal text: not an „error:" — nothing failed.
    #[test]
    fn the_second_concurrent_save_is_refused_in_plain_words() {
        assert_eq!(SCRIPT_SAVE_IN_FLIGHT, "ukládání skriptu už probíhá");
        assert!(!SCRIPT_SAVE_IN_FLIGHT.starts_with("error:"));
    }

    /// T9 review MAJOR-1. Ctrl+S must be refused whenever a dialog is on
    /// screen — a modal, the Apply dialog, or a discard prompt. The
    /// scripts case is the sharp one (a save landing after a delete
    /// recreates an irreversibly deleted file), but the rule is the same
    /// one `run_query_with` already applies to Ctrl+Enter, and for the
    /// same reason: occlusion stops clicks, not keystrokes.
    #[test]
    fn ctrl_s_is_refused_whenever_any_dialog_owns_the_screen() {
        // Final-review MAJOR-2: the pure RULE stays a bool and stays
        // unit-pinned here; `save_guard::with_save_permission` is what turns
        // it into the `SaveAllowed` witness `save_script` demands — and it
        // reads the three facts off the live `AppView`, so nobody can mint
        // one by passing three convenient `false`s.
        assert!(save_guard::script_save_allowed(false, false, false));
        assert!(!save_guard::script_save_allowed(true, false, false), "a modal blocks it");
        assert!(!save_guard::script_save_allowed(false, true, false), "the Apply dialog blocks it");
        assert!(!save_guard::script_save_allowed(false, false, true), "a discard prompt blocks it");
        assert!(!save_guard::script_save_allowed(true, true, true));
        // Refused out loud, and not as an „error:" — nothing failed.
        assert_eq!(SCRIPT_SAVE_BLOCKED, "nejprve zavřete otevřený dialog");
        assert!(!SCRIPT_SAVE_BLOCKED.starts_with("error:"));
        assert!(!SCRIPT_SAVE_BLOCKED.is_empty(), "a silent refusal is the banned shape");
    }

    // ---------- T9: the binding stays coherent with the filesystem ----------

    /// T9 review MINOR-1: the binding comparison folds case the same way
    /// the rest of the phase does. The failure it closes is concrete — a
    /// root configured with different casing than the disk made the `✕` on
    /// the bound file leave the binding in place, so the caption kept
    /// naming a deleted file and the next Ctrl+S recreated it.
    #[test]
    fn the_binding_comparison_is_unicode_case_insensitive_like_every_other_probe() {
        let disk = Path::new(r"D:\ws\scripts\trzby.sql");
        let configured = Path::new(r"D:\ws\Scripts\Trzby.sql");
        assert!(same_path_ci(disk, configured), "ASCII casing must fold");
        assert!(script_binding_affected(disk, configured, false));
        // Non-ASCII is the pair `eq_ignore_ascii_case` would miss, and
        // Czech script names make it routine rather than exotic.
        assert!(same_path_ci(
            Path::new(r"D:\ws\scripts\Řezy.sql"),
            Path::new(r"D:\ws\scripts\řezy.sql")
        ));
        // T10 carry-forward 6: THE pair that separated the two folds this
        // crate used to carry. `to_lowercase` applies Unicode's
        // final-sigma context rule, so these two fold APART under it while
        // NTFS resolves them to ONE directory — a rename or delete of that
        // folder would then leave the binding standing on a dead path.
        // `fsutil::fold_name` (`to_uppercase`, measured against `$UpCase`)
        // folds them together, and this is what stops a revert.
        //
        // It has to be a FOLDER component, and that is worth knowing: the
        // final-sigma rule fires only word-FINALLY, so `ΟΔΟΣ.sql` lowers
        // to `οδοσ.sql` (the `.sql` follows the Σ) and a file name cannot
        // exhibit the divergence at all. Every `.sql` leaf in this tree is
        // therefore safe under either fold, and the whole difference lives
        // in the directory components — which is exactly where
        // `script_binding_affected`'s folder arm operates.
        assert_ne!(
            "ΟΔΟΣ".to_lowercase(),
            "οδοσ".to_lowercase(),
            "if this ever stops holding, the rationale on `fsutil::fold_name` needs re-deriving"
        );
        assert_eq!("ΟΔΟΣ".to_uppercase(), "οδοσ".to_uppercase(), "…and this is the fold we use");
        assert!(same_path_ci(
            Path::new(r"D:\ws\scripts\ΟΔΟΣ\a.sql"),
            Path::new(r"D:\ws\scripts\οδοσ\a.sql")
        ));
        assert!(script_binding_affected(
            Path::new(r"D:\ws\scripts\ΟΔΟΣ\a.sql"),
            Path::new(r"D:\ws\scripts\οδοσ"),
            true
        ));
        // Folding is not the same as being blind: different names stay
        // different, and a component boundary is never crossed.
        assert!(!same_path_ci(disk, Path::new(r"D:\ws\scripts\jine.sql")));
        assert!(!path_starts_with_ci(
            Path::new(r"D:\ws\scriptsX\a.sql"),
            Path::new(r"D:\ws\scripts")
        ));
        assert!(path_starts_with_ci(
            Path::new(r"D:\ws\Scripts\prod\a.sql"),
            Path::new(r"D:\ws\scripts")
        ));
        // …and a re-save through a differently-cased root is NOT the user
        // moving on, so it must not bump the generation.
        assert!(!script_binding_target_changed(Some(disk), Some(configured)));
    }

    /// The suffix of a folder rename keeps its REAL on-disk casing even
    /// when the prefix that matched did not — `strip_prefix` could not do
    /// this, which is why the split is by component count.
    #[test]
    fn a_case_mismatched_prefix_still_rebases_and_preserves_the_suffix() {
        assert_eq!(
            script_binding_retarget(
                Path::new(r"D:\ws\scripts\prod\Trzby.SQL"),
                Path::new(r"D:\ws\Scripts\PROD"),
                Path::new(r"D:\ws\Scripts\produkce"),
                true
            ),
            Some(PathBuf::from(r"D:\ws\Scripts\produkce\Trzby.SQL"))
        );
    }

    /// And the caption relativizes under the same rule, so the mismatch
    /// that used to hide this whole bug class is now visible.
    #[test]
    fn the_caption_relativizes_across_a_casing_mismatch() {
        assert_eq!(
            script_caption_rel(
                Path::new(r"D:\ws\scripts\prod\trzby.sql"),
                Some(Path::new(r"D:\ws\Scripts"))
            ),
            "prod/trzby.sql"
        );
    }

    /// Part S §4's binding fixup, as a pure decision. The three cases the
    /// Task 9 brief demanded be pinned: rename the bound file, delete the
    /// bound file, and — the one the plan text did not cover — rename or
    /// delete a FOLDER that CONTAINS the bound file. `rename_entry` renames
    /// folders too, so „only the exact path matters" would have left the
    /// binding pointing at a path that no longer exists.
    #[test]
    fn a_folder_rename_moves_the_binding_with_it_not_just_an_exact_hit() {
        let root = PathBuf::from(r"D:\ws\scripts");
        let bound = root.join("prod").join("trzby.sql");

        // The bound FILE itself is renamed.
        assert_eq!(
            script_binding_retarget(
                &bound,
                &root.join("prod").join("trzby.sql"),
                &root.join("prod").join("trzby-2025.sql"),
                false
            ),
            Some(root.join("prod").join("trzby-2025.sql"))
        );
        // The folder ABOVE it is renamed — the suffix is rebased.
        assert_eq!(
            script_binding_retarget(&bound, &root.join("prod"), &root.join("produkce"), true),
            Some(root.join("produkce").join("trzby.sql"))
        );
        // A folder rename must NOT rebase when `is_dir` is false: a FILE
        // whose name happens to be a path prefix of the binding is not an
        // ancestor of it.
        assert_eq!(script_binding_retarget(&bound, &root.join("prod"), &root.join("p2"), false), None);
        // An unrelated entry leaves the binding alone.
        assert_eq!(
            script_binding_retarget(&bound, &root.join("dev"), &root.join("dev2"), true),
            None
        );
    }

    #[test]
    fn the_delete_fixup_covers_the_bound_file_and_its_ancestors() {
        let root = PathBuf::from(r"D:\ws\scripts");
        let bound = root.join("prod").join("trzby.sql");
        assert!(script_binding_affected(&bound, &bound, false));
        assert!(script_binding_affected(&bound, &root.join("prod"), true));
        assert!(!script_binding_affected(&bound, &root.join("prod"), false));
        assert!(!script_binding_affected(&bound, &root.join("dev"), true));
        // The root itself is an ancestor of everything — but `delete_entry`
        // refuses an empty rel (`resolve_entry_rel`), so the root can never
        // BE a target; this only records that the predicate is honest.
        assert!(script_binding_affected(&bound, &root, true));
    }

    /// FINAL-REVIEW NIT-1. `save_script_as` resumes after a file picker
    /// that is not app-modal on every platform, so it owes the SAME
    /// three-part re-check `script_open_abort_reason` performs for an open
    /// — and it was missing the buffer leg, which is precisely the leg the
    /// generation counter is structurally blind to (`set_script_binding`
    /// bumps on a PATH change; typing changes no path).
    ///
    /// Source-pinned, the `run_script_from_library` precedent: the three
    /// legs are `if` statements in a GPUI continuation that no headless
    /// test can drive. Non-vacuous — the slice is proved to be the real
    /// function before anything is required of it.
    #[test]
    fn the_save_as_continuation_re_asks_all_three_things_it_captured() {
        let src = include_str!("main.rs");
        let body = src.split("fn save_script_as(").nth(1).expect("save_script_as exists");
        let body = &body[..body.find("\n    fn ").unwrap_or(body.len())];
        // RE-VERIFY MINOR-B: assert on CODE, never on the raw body. Leg 2
        // below looked for `script_save_allowed` in the RAW text, where it
        // occurs only in a COMMENT — the code calls the mint, not the pure
        // rule — so the leg was satisfied by prose about itself. That is
        // the exact failure `config_save_guard_audit`'s own doc warns
        // about, reproduced two hundred lines from the warning.
        let code = editor_clobber_audit::code_lines(body).join("\n");
        // The slice really is the picker continuation.
        assert!(code.contains("prompt_for_new_path"), "the sliced body is not the real one");
        assert!(code.contains("with_sql_extension"), "the sliced body is not the real one");
        // Leg 1: the binding must not have moved (T9 re-verify FAIL-1's
        // generation check).
        assert!(
            code.contains("script_binding_generation != dispatched"),
            "the binding leg is gone — a save-as landing after the editor was bound elsewhere \
             would write the old text and re-bind on top of it"
        );
        // Leg 2: the dialog predicate, re-asked continuation-side. The
        // needle is the MINT, because that is what the code calls — and
        // the OLD needle is asserted ABSENT from the code, which is both
        // the real invariant (this path must go through the permission
        // scope, never the bare rule) and the standing proof that the
        // previous version of this leg was satisfied by a comment.
        assert!(
            !code.contains("script_save_allowed"),
            "this path must ask the permission SCOPE, not the pure rule"
        );
        assert!(
            code.contains("with_save_permission"),
            "the guard leg is gone — T9 re-verify FAIL-1's delete/save-as race is back"
        );
        // Leg 3: the captured buffer, the one this finding added.
        assert!(
            code.contains(".text() != text"),
            "the BUFFER leg is gone — keystrokes during a non-app-modal picker would be \
             written to disk as text nobody can see, with `saved_text` bound to them"
        );
    }

    /// RE-VERIFY MINOR-A — MAJOR-1's resurrection in the MIRRORED
    /// ordering, which the first fix left open.
    ///
    /// MAJOR-1 was „the open lands, then the delete lands". The mirror is
    /// „the delete lands, then the open lands", and with the editor
    /// UNBOUND at the landing the re-asked `binding_targets` is false, so
    /// `set_script_binding` is never called and the generation is never
    /// bumped. An `open_script` dispatched before the delete then passes
    /// all three legs of `script_open_abort_reason` and binds a file that
    /// no longer exists; the next Ctrl+S recreates it. (Windows opens the
    /// read with `FILE_SHARE_DELETE`, so it completes across the delete
    /// and raises no error to notice.)
    ///
    /// Source-pinned because the whole point is that the call is
    /// UNCONDITIONAL — a behavioural test can only show that some path
    /// bumps, not that every path does. The indentation check is the
    /// assertion: at the function body's own level (8 spaces) it cannot be
    /// inside an `if`, which is exactly how it was missing before.
    #[test]
    fn a_landed_delete_supersedes_in_flight_opens_even_with_nothing_bound() {
        let src = include_str!("main.rs");
        let body = src.split("fn finish_script_delete(").nth(1).expect("it exists");
        let body = &body[..body.find("
    fn ").unwrap_or(body.len())];
        // Non-vacuity: the slice is the real landing.
        assert!(body.contains("owns_script_delete_modal"), "the sliced body is not the real one");
        assert!(body.contains("binding_targets"), "the sliced body is not the real one");

        let code = editor_clobber_audit::code_lines(body);
        let bump: Vec<&String> =
            code.iter().filter(|l| l.contains("supersede_script_continuations")).collect();
        assert_eq!(bump.len(), 1, "expected exactly one bump, found {}", bump.len());
        // UNCONDITIONAL, expressed as BRACE DEPTH rather than as
        // indentation. Re-verify's NIT is right that the old `assert_eq!`
        // on the literal line was positional: it broke on a CRLF checkout
        // (FAIL-9) and would break again on a `tab_spaces` change. Depth
        // is the property actually meant — depth 1 is the function body,
        // and anything deeper is inside an `if`/`match`/closure, which is
        // exactly how MINOR-A's data loss survived the first fix.
        let mut depth = 0i32;
        let mut depth_at_bump = None;
        for line in &code {
            if line.contains("supersede_script_continuations") {
                depth_at_bump = Some(depth);
            }
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        }
        assert_eq!(
            depth_at_bump,
            Some(1),
            "the bump must sit at the function body's own brace depth — anything deeper is              inside a conditional, which is how re-verify MINOR-A's data loss survived the              first fix"
        );
        // …and it must come BEFORE the binding is dropped, so a bump is
        // never skipped by an early return added later.
        let bump_at = code.iter().position(|l| l.contains("supersede_script_continuations"));
        let drop_at = code.iter().position(|l| l.contains("set_script_binding"));
        assert!(bump_at < drop_at, "supersede first, then adjust the binding");
    }

    /// FINAL-REVIEW MAJOR-1, the direction that loses data.
    ///
    /// The delete of `trzby.sql` was confirmed while the editor was
    /// UNBOUND, so a `was_bound` captured at dispatch said `false`. An
    /// in-flight `open_script` of the very same file then landed first and
    /// bound it (nothing in `script_open_abort_reason` stops that: an
    /// unbound editor never bumped the generation, the root did not move
    /// and the buffer was not typed into). The state that decides whether
    /// the binding must be dropped is therefore the state at the LANDING,
    /// and `binding_targets_entry` has no parameter that could carry the
    /// dispatch-time answer — which is the point of it being a free fn.
    ///
    /// If this ever answers `false`, the caption keeps naming a file the
    /// user irreversibly deleted and the next Ctrl+S recreates it.
    #[test]
    fn a_delete_landing_after_an_open_bound_its_own_target_still_clears_the_binding() {
        let root = PathBuf::from(r"D:\ws\scripts");
        let doomed = root.join("trzby.sql");
        // At DISPATCH the editor was unbound — the stale answer.
        assert!(!binding_targets_entry(None, Some(&root), "trzby.sql", false));
        // At the LANDING the racing open has bound the doomed file.
        assert!(binding_targets_entry(Some(&doomed), Some(&root), "trzby.sql", false));
        // Same story one level up: the open bound a file inside the folder
        // whose delete was confirmed while nothing was bound.
        let inside = root.join("prod").join("trzby.sql");
        assert!(binding_targets_entry(Some(&inside), Some(&root), "prod", true));
    }

    /// …and the symmetric direction, which is milder but equally wrong:
    /// bound to `a.sql`, an in-flight open of `b.sql` lands during the
    /// confirmed delete of `a.sql`. A `was_bound == true` captured at
    /// dispatch would drop the brand-new `b.sql` binding, silently turning
    /// the next Ctrl+S into a save-as over a file that still exists.
    #[test]
    fn a_delete_landing_after_the_binding_moved_elsewhere_keeps_the_new_binding() {
        let root = PathBuf::from(r"D:\ws\scripts");
        let a = root.join("a.sql");
        let b = root.join("b.sql");
        // At DISPATCH: bound to the doomed file — the stale answer.
        assert!(binding_targets_entry(Some(&a), Some(&root), "a.sql", false));
        // At the LANDING: the editor has moved on to `b.sql`, which the
        // delete does not touch.
        assert!(!binding_targets_entry(Some(&b), Some(&root), "a.sql", false));
    }

    /// The two „no answer is possible" arms, so neither degrades to a
    /// destructive `true`: no binding at all, and no scripts root (the
    /// workspace was swapped out from under the delete).
    #[test]
    fn the_binding_question_is_false_without_a_binding_or_a_root() {
        let root = PathBuf::from(r"D:\ws\scripts");
        let bound = root.join("a.sql");
        assert!(!binding_targets_entry(None, Some(&root), "a.sql", false));
        assert!(!binding_targets_entry(Some(&bound), None, "a.sql", false));
        // And the library ROOT is never a mutation target, so an empty rel
        // is `false` rather than „everything is affected"
        // (`resolve_entry_rel`'s rail, asked here so the free fn inherits
        // it as visibly as the method did).
        assert!(!binding_targets_entry(Some(&bound), Some(&root), "", true));
    }

    /// Part S §1.3, recorded as a TEST because it is the kind of
    /// "helpful" behaviour a future edit adds by accident: ▶ runs what
    /// is on DISK. The pre-scan reads the file; nothing writes it.
    /// CARGO_MANIFEST_DIR, not `file!()`: cargo runs tests with the
    /// PACKAGE dir as CWD while `file!()` is workspace-relative.
    #[test]
    fn running_a_library_script_never_auto_saves_first() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
        let run_fn = src
            .split("fn run_script_from_library")
            .nth(1)
            .expect("run_script_from_library exists");
        let body = &run_fn[..run_fn.find("\n    fn ").unwrap_or(run_fn.len())];
        // Non-vacuity: a slice that stopped early would pass the bans by
        // containing nothing at all. These two are the load-bearing halves
        // of the fn — the from-disk pre-scan and the SHARED continuation.
        assert!(body.contains("count_statements_in_file"), "the sliced body is not the real one");
        assert!(body.contains("open_script_run_modal"), "the sliced body is not the real one");
        for banned in ["save_script", "write_script", "replace_buffer", "bind_script"] {
            assert!(!body.contains(banned), "▶ must not {banned}: it runs the DISK content");
        }
    }
}

/// Workspace T8: the „never destructive" rail for the EDITOR, pinned the
/// same way `config_save_guard_audit` pins the `config.toml` writers —
/// as a source audit, because a GPUI click listener cannot be driven
/// headlessly and the two sites this task fixed were both listeners.
///
/// Part S §5.5 says there is ONE dirty guard. That is only true while
/// `editor_load_guarded` is the ONLY way the editor's text gets replaced:
/// the two sites this audit was written for (`history_panel.rs`'s row
/// click and the palette's `HistoryEntry` arm) had clobbered a bound
/// script's unsaved changes with no guard at all since long before this
/// phase. A third such site added later would silently reopen the hole,
/// and nothing else in the test suite would notice.
///
/// T8 REVIEW MAJOR-3, rewritten. The first version keyed on
/// `sql.update(cx, |s,` + `set_text` ON THE SAME LINE, and six shapes
/// walked straight past it — a rebound local, a method chain, a UFCS call,
/// a rustfmt-wrapped closure body, a differently-named closure parameter,
/// and a direct `perform_script_action` call that skipped the guard
/// entirely. Two of those are ordinary formatting and naming, not
/// adversarial. This version keys on IDENTIFIERS that are unique
/// crate-wide instead of on an expression shape, so how the editor entity
/// is reached and how rustfmt breaks the lines are both irrelevant:
///
/// * `SqlInput::set_text` was RENAMED to `replace_buffer` (see its doc
///   comment) precisely so that the one identifier every buffer
///   replacement must mention cannot be confused with `TextField`'s or
///   `TextModel`'s same-named methods.
/// * `perform_script_action` and `bind_script` are counted too, so a
///   call of the performer from an unsanctioned owner is REPORTED. (Not
///   „cannot be bypassed" — that is a text check, and re-verify rounds 2
///   and 3 walked past text checks six times between them.)
///
/// **What actually stops a buffer clobber is now a TYPE**, not this
/// module: `SqlInput::replace_buffer` demands an
/// `editor_guard::BufferReplace<'brand>`, and `accept_completion` — the
/// only other `pub` mutator — was narrowed so it can delete at most one
/// identifier prefix. These audits remain as the belt: they report a NEW
/// mutator or a mention that escapes as a value, neither of which a type
/// can notice.
///
/// The structural alternative the review preferred — the editor entity
/// behind a private accessor whose mutator is unreachable outside the
/// guard — was NOT taken, and re-verify judged that decline WRONG for the
/// scope shape (which needs no accessor and no module move; see
/// `editor_guard`). What follows is the original Task 8 reasoning, kept
/// because it is still the correct objection to the accessor shape: Rust's finest privacy granularity is the module,
/// so it would mean moving `AppView.sql` and both guard functions into a
/// separate module, splitting `impl AppView` across files and dragging the
/// autocomplete plumbing (`accept_completion`, `set_autocomplete_active`,
/// `kick_highlight`, the ten `read(cx)` sites) with it. That is a `main.rs`
/// restructure, not a Task 8 fix.
#[cfg(test)]
mod editor_clobber_audit {
    use std::path::PathBuf;

    /// EVERY `.rs` file in the WORKSPACE, read at TEST TIME by walking
    /// the repository root — deliberately never a hand-written list, and
    /// (re-verify FAIL-4) never a list of directories either.
    ///
    /// T8 re-verify MAJOR-3 / G1: the previous version enumerated 8 of the
    /// crate's 33 files, and the other 25 were invisible to it. Privacy
    /// does NOT cover them: `main.rs` is the CRATE ROOT, so every module is
    /// a descendant, and Rust grants a descendant access to a private
    /// ancestor item. `AppView.sql`, `open_script` and
    /// `perform_script_action` are private-but-reachable crate-wide, and
    /// `bind_script` / `editor_load_guarded` are `pub(crate)` outright — so
    /// an `impl crate::AppView` block in ANY module could replace the
    /// editor's buffer with all three tests green. That is not theoretical:
    /// Task 9 added the scripts-tree mutation handlers, and a „Nový
    /// skript" handler that loads the created file into the editor is
    /// exactly the code that would want to.
    ///
    /// Reading the directory also means a NEW file is covered the moment it
    /// exists, with nobody having to remember this list.
    /// FINAL-REVIEW MAJOR-2 (structural gap 1): it now walks the WHOLE
    /// WORKSPACE, not `dbc-ui/src`. `crates/dbc-ui/tests/`, a future
    /// `build.rs` (which runs at BUILD time with full filesystem access)
    /// and every other crate were invisible — notably `dbc-state`'s four
    /// `fsutil::write_atomic` callers, which write real bytes into the
    /// user's folder and were audited by nothing at all. Names are now
    /// `<crate>/<src|tests>/<path>` so a report says WHICH crate.
    pub(super) fn sources() -> Vec<(String, String)> {
        let root = workspace_root();
        let mut out: Vec<(String, String)> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                if is_pruned(&path) {
                    continue;
                }
                let rd = std::fs::read_dir(&path)
                    .unwrap_or_else(|e| panic!("audit cannot read {}: {e}", path.display()));
                for ent in rd {
                    stack.push(ent.expect("readable directory entry").path());
                }
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("audit cannot read {rel}: {e}"));
            out.push((rel, text));
        }
        out.sort();
        out
    }

    /// Is this directory outside the audits' remit?
    ///
    /// RE-VERIFY FAIL-6. This used to be `n.starts_with("target")`, a
    /// PREFIX match — so a plain `mod targets;` in
    /// `crates/dbc-ui/src/targets/` was invisible to every audit, and so
    /// were `target_picker/` and `targeting/`. No trick was needed: the
    /// re-verifier put verbatim `replace_buffer` and `write_script` calls
    /// there, called them from the live `Unbind` arm, and got 0 warnings
    /// and 964 passing. `targets` is a name somebody could add innocently,
    /// which makes it the worst of the three bypasses that round.
    ///
    /// So nothing is pruned by NAME SHAPE any more:
    ///
    /// * VCS and tooling metadata by EXACT name — these are not Rust
    ///   source trees and never contain a module of this workspace;
    /// * a cargo build directory by CONTENT — cargo writes `CACHEDIR.TAG`
    ///   into every target dir, so this recognises build output wherever
    ///   it is and whatever it is called, and recognises nothing else.
    ///
    /// A directory a developer names is therefore always scanned.
    pub(super) fn is_pruned(dir: &std::path::Path) -> bool {
        let name = dir.file_name().map(|n| n.to_string_lossy().to_string());
        if name.as_deref().is_some_and(|n| matches!(n, ".git" | ".claude" | "node_modules")) {
            return true;
        }
        // RE-VERIFY FAIL-13: the marker prunes a directory only when there
        // is no Rust source in it.
        //
        // Keying on `CACHEDIR.TAG` alone FAILED OPEN. The tag is an
        // unsigned, trivially-created file, so dropping one into
        // `crates/dbc-ui/src/helpers/` next to a `mod.rs` deleted that
        // directory from every audit - FAIL-6 from the other side, and
        // strictly worse, because it needs no plausible name at all.
        //
        // A cargo target directory contains build artefacts, never crate
        // sources, so „has the tag AND holds no `.rs` file" recognises
        // build output without ever hiding code. If someone puts the tag
        // beside a `.rs` file, the directory is scanned and the audits
        // speak up - which is the fail-CLOSED direction.
        if !dir.join("CACHEDIR.TAG").is_file() {
            return false;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        !rd.filter_map(|e| e.ok())
            .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rs"))
    }

    /// The workspace root — `CARGO_MANIFEST_DIR` is `<root>/crates/dbc-ui`.
    pub(super) fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("<root>/crates/dbc-ui")
            .to_path_buf()
    }

    /// The workspace members, read from the root `Cargo.toml` at TEST
    /// TIME rather than hard-coded.
    ///
    /// RE-VERIFY FAIL-4: the coverage test used to carry a literal list of
    /// ten crate names, so it could not notice a new member — and could
    /// not notice that the walk itself was list-shaped either. Deriving
    /// both from the manifest means a member added tomorrow is audited
    /// tomorrow, with nobody remembering anything.
    pub(super) fn workspace_members() -> Vec<String> {
        let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml"))
            .expect("workspace Cargo.toml");
        let list = manifest
            .split("members")
            .nth(1)
            .and_then(|t| t.split('[').nth(1))
            .and_then(|t| t.split(']').next())
            .expect("[workspace] members list");
        let out: Vec<String> = list
            .split(',')
            .filter_map(|m| {
                let m = m.trim().trim_matches('"').trim();
                (!m.is_empty()).then(|| m.to_string())
            })
            .collect();
        assert!(out.len() >= 10, "member list parse looks wrong: {out:?}");
        out
    }

    /// The source's lines with every COMMENT and every STRING LITERAL
    /// blanked out, so a needle can only match real code. One line in, one
    /// line out — indices still name the file's real line numbers.
    ///
    /// FINAL-REVIEW MAJOR-2 (structural gap 2). The old `code_of`
    /// truncated at the first `//` ANYWHERE on the line, including inside
    /// a string literal, where it silently swallowed the rest of a real
    /// statement; and block comments were not stripped at all, so
    /// `/* … */` was an invisibility cloak over any call an audit was
    /// looking for. Both are now handled by a scanner rather than a
    /// `find`:
    ///
    /// * `/* … */`, NESTED (Rust allows it) and spanning lines;
    /// * `"…"` with `\` escapes, and `r"…"` / `r#"…"#` raw strings, which
    ///   have no escapes at all and are how every Windows path literal in
    ///   this crate is written — INCLUDING the byte and C prefixes
    ///   (`b"…"`, `c"…"`, `br#"…"#`, `cr#"…"#`), which re-verify FAIL-3
    ///   caught this scanner mis-parsing: a `br#"…"#` fell through to the
    ///   ordinary-`"` branch, took the first quote in its payload as the
    ///   terminator, and with an odd quote count desynced into
    ///   „inside a string" and blanked the rest of the FILE;
    /// * `'x'` / `'\n'` char literals, told apart from LIFETIMES
    ///   (`'static`, `'a`) by requiring the closing quote — which matters
    ///   because `'/'` and `'"'` both appear in this crate's real code.
    ///
    /// Blanking rather than deleting keeps a prose mention from being an
    /// alibi too (`config_save_guard_audit`'s lesson: its first draft was
    /// vacuous because an explanatory comment satisfied it).
    pub(super) fn code_lines(src: &str) -> Vec<String> {
        // RE-VERIFY FAIL-9: the carriage return is DROPPED, so a line
        // here is a LOGICAL line. This scanner split on the newline and
        // kept the CR, so on a CRLF
        // checkout every returned line ended in an invisible carriage
        // return — and `a_landed_delete_supersedes_in_flight_opens_even_
        // with_nothing_bound` compares a line with `assert_eq!`. That pin
        // was RED on a fresh `git checkout` of this branch (this machine
        // has `core.autocrlf = true` globally) while passing in the
        // worktree where the file had been written by an editor. Every
        // consumer here wants logical lines; none wants the terminator.
        let chars: Vec<char> = src.chars().filter(|c| *c != '\r').collect();
        let mut out: Vec<String> = Vec::new();
        let mut line = String::new();
        let mut block_depth = 0usize;
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            if c == '\n' {
                out.push(std::mem::take(&mut line));
                i += 1;
                continue;
            }
            if block_depth > 0 {
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    block_depth -= 1;
                    line.push_str("  ");
                    i += 2;
                    continue;
                }
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    block_depth += 1;
                    line.push_str("  ");
                    i += 2;
                    continue;
                }
                line.push(' ');
                i += 1;
                continue;
            }
            if c == '/' && chars.get(i + 1) == Some(&'*') {
                block_depth = 1;
                line.push_str("  ");
                i += 2;
                continue;
            }
            if c == '/' && chars.get(i + 1) == Some(&'/') {
                while i < chars.len() && chars[i] != '\n' {
                    line.push(' ');
                    i += 1;
                }
                continue;
            }
            // A STRING LITERAL, prefix and all.
            //
            // RE-VERIFY FAIL-3. This used to be two branches — one for a
            // bare `r`, one for a bare `"` — and the raw branch demanded
            // that the character before the `r` not be identifier-ish. A
            // BYTE raw string spells a `b` there, so `br#"…"#` failed the
            // raw test and fell into the ordinary-`"` branch, which then
            // took the FIRST `"` inside the payload as its terminator. With
            // an odd number of quotes in the payload the scanner desynced
            // into "inside a string" and blanked every following character,
            // ACROSS LINES — hiding real calls from every audit (the
            // re-verifier hid a `write_script(` this way, zero warnings,
            // 961 green) and, mid-file, blanking legitimate ones so counts
            // silently dropped. Live `br#"…"#` literals already exist at
            // `dbc-driver-postgres/src/types.rs:201,208,227`; they survived
            // only because their quote counts happen to be even.
            //
            // So the prefix is parsed properly: `b`, `c`, `r`, `br`, `cr`
            // (and none). Anything with an `r` is RAW — no escapes, `#`
            // hashes delimit; anything else is an ordinary escaped string.
            // The not-identifier-ish test now applies to the START of the
            // prefix, which is the only place it was ever meaningful.
            let prefix = ["br", "cr", "b", "c", "r", ""]
                .into_iter()
                .find(|p| p.chars().enumerate().all(|(k, pc)| chars.get(i + k) == Some(&pc)));
            if let Some(prefix) = prefix {
                let plen = prefix.len();
                let raw = prefix.contains('r');
                let after_prefix = i + plen;
                let mut j = after_prefix;
                let mut hashes = 0usize;
                if raw {
                    while chars.get(j) == Some(&'#') {
                        hashes += 1;
                        j += 1;
                    }
                }
                let opens = chars.get(j) == Some(&'"');
                // A prefix is only a prefix if it starts a token.
                let token_start =
                    !line.chars().next_back().is_some_and(|p| p.is_alphanumeric() || p == '_');
                if opens && (plen == 0 || token_start) {
                    for _ in 0..=(plen + hashes) {
                        line.push(' ');
                    }
                    j += 1;
                    while j < chars.len() {
                        if raw {
                            if chars[j] == '"'
                                && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#'))
                            {
                                for _ in 0..=hashes {
                                    line.push(' ');
                                }
                                j += hashes + 1;
                                break;
                            }
                        } else {
                            if chars[j] == '\\' {
                                line.push(' ');
                                if chars.get(j + 1) == Some(&'\n') {
                                    out.push(std::mem::take(&mut line));
                                } else if j + 1 < chars.len() {
                                    line.push(' ');
                                }
                                j += 2;
                                continue;
                            }
                            if chars[j] == '"' {
                                line.push(' ');
                                j += 1;
                                break;
                            }
                        }
                        if chars[j] == '\n' {
                            out.push(std::mem::take(&mut line));
                        } else {
                            line.push(' ');
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
            if c == '\'' {
                // A char literal closes; a lifetime does not.
                let close = if chars.get(i + 1) == Some(&'\\') { i + 3 } else { i + 2 };
                if chars.get(close) == Some(&'\'') {
                    for _ in i..=close {
                        line.push(' ');
                    }
                    i = close + 1;
                    continue;
                }
            }
            line.push(c);
            i += 1;
        }
        out.push(line);
        out
    }

    /// Single-line convenience over [`code_lines`]. A line handed here in
    /// isolation cannot know it is inside a block comment — `code_lines`
    /// is what knows that, and every audit goes through it.
    pub(super) fn code_of(l: &str) -> String {
        code_lines(l).into_iter().next().unwrap_or_default()
    }

    /// The NAME of the function a line defines, or `None` if it defines
    /// none.
    ///
    /// T8 re-verify MAJOR-3 / G2, both halves. Sanctioning used to be
    /// `owner.contains(name)`, so a helper called `bind_script_and_focus`
    /// or `perform_script_action_inner` — ordinary refactor names, not
    /// adversarial ones — was silently sanctioned; the caller now compares
    /// the extracted name EXACTLY. And the old detector recognised only
    /// `fn` / `pub fn` / `pub(crate) fn` / `pub(super) fn`, so a call under
    /// an `async fn`, `unsafe fn`, `const fn` or `pub(in path) fn` was
    /// attributed to the PREVIOUS function — possibly a sanctioned one.
    /// This strips an arbitrary visibility and any order of qualifiers.
    pub(super) fn defined_fn_name(line: &str) -> Option<String> {
        let stripped = code_of(line);
        let mut t = stripped.trim_start();
        if let Some(rest) = t.strip_prefix("pub") {
            let rest = rest.trim_start();
            t = match rest.strip_prefix('(') {
                // `pub(crate)`, `pub(super)`, `pub(in a::b)`
                Some(inner) => inner[inner.find(')')? + 1..].trim_start(),
                None => rest,
            };
        }
        loop {
            let before = t.len();
            for q in ["default ", "const ", "async ", "unsafe ", "extern "] {
                if let Some(rest) = t.strip_prefix(q) {
                    t = rest.trim_start();
                }
            }
            // `extern "C"`'s ABI string.
            if let Some(rest) = t.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    t = rest[end + 1..].trim_start();
                }
            }
            if t.len() == before {
                break;
            }
        }
        let rest = t.strip_prefix("fn ")?.trim_start();
        let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_')?;
        (end > 0).then(|| rest[..end].to_string())
    }

    /// The function ENCLOSING each line, by index — `None` where a line is
    /// not inside any function body.
    ///
    /// FINAL-REVIEW MAJOR-2 (structural gap 3). The old `owner_fn` was
    /// „the nearest `fn` definition ABOVE this line", with no brace
    /// balancing at all, so a call at file scope (a `static` initializer,
    /// a `lazy_static!`, a macro invocation at module level) after a
    /// sanctioned function had CLOSED was attributed to that closed
    /// function and silently sanctioned. This tracks brace depth over the
    /// comment- and string-stripped text, so a function owns exactly its
    /// own body and nothing after it.
    ///
    /// Closures do not disturb it: they are extra `{}` inside the body,
    /// and the innermost still-open `fn` remains the owner — which is the
    /// answer these audits want (`cx.spawn(async move |…| { … })` is the
    /// enclosing function's own code).
    pub(super) fn owners(code: &[String]) -> Vec<Option<String>> {
        let mut depth = 0usize;
        // (depth at which this fn's body opened, name)
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut pending: Option<String> = None;
        let mut out: Vec<Option<String>> = Vec::with_capacity(code.len());
        for line in code {
            let defines = defined_fn_name(line);
            let scan = |depth: &mut usize,
                            stack: &mut Vec<(usize, String)>,
                            pending: &mut Option<String>| {
                for ch in line.chars() {
                    match ch {
                        '{' => {
                            *depth += 1;
                            if let Some(n) = pending.take() {
                                stack.push((*depth, n));
                            }
                        }
                        '}' => {
                            if stack.last().is_some_and(|(d, _)| *d == *depth) {
                                stack.pop();
                            }
                            *depth = depth.saturating_sub(1);
                        }
                        _ => {}
                    }
                }
            };
            match defines {
                // A definition line belongs to the function it opens, so
                // its braces are consumed BEFORE the owner is recorded.
                Some(name) => {
                    pending = Some(name);
                    scan(&mut depth, &mut stack, &mut pending);
                    out.push(stack.last().map(|(_, n)| n.clone()));
                }
                // Every other line belongs to whatever was open when it
                // STARTED — a lone `}` closing a function is still that
                // function's line.
                None => {
                    out.push(stack.last().map(|(_, n)| n.clone()));
                    scan(&mut depth, &mut stack, &mut pending);
                }
            }
        }
        out
    }

    /// Every CALL of `needle` must sit inside a function named EXACTLY one
    /// of `sanctioned`, and there must be exactly `expected` of them. The
    /// definition itself is not a call site.
    pub(super) fn audit(needle: &str, sanctioned: &[&str], expected: usize, why: &str) {
        audit_excluding(needle, &[], sanctioned, expected, why);
    }

    /// [`audit`] for a FIELD rather than a function.
    ///
    /// A field is never „called", so re-verify FAIL-8's call-shape rule
    /// does not apply — and must not, or every read of the field is a
    /// finding. Everything else is identical: whole-word mentions, exact
    /// owner names, pinned count.
    pub(super) fn audit_field(needle: &str, sanctioned: &[&str], expected: usize, why: &str) {
        audit_inner(needle, &[], sanctioned, expected, why, false);
    }

    /// [`audit`], plus tokens that must NOT count as a mention.
    ///
    /// FINAL-REVIEW MAJOR-2 named the false positive instead of dodging it
    /// with punctuation: the needle used to be `.save_script`, WITH a
    /// leading dot, purely to keep `on_save_script(` out of the count, and
    /// the dot is exactly what the UFCS bypass (`AppView::save_script(..)`,
    /// which spells a colon there) walked around.
    ///
    /// **RE-VERIFY FAIL-1 finished the job: this no longer looks for a
    /// CALL at all. It looks for the NAME.** Matching `needle + "("` still
    /// assumes a call syntax, and the re-verifier simply stopped using one:
    ///
    /// ```ignore
    /// use crate::scripts::write_script as persist_bytes;   // no `(`
    /// let _ = persist_bytes(&doomed, "-- truncated by the run");  // no name
    ///
    /// let clobber = crate::sql_input::SqlInput::replace_buffer;
    /// self.sql.update(cx, |s, cx| clobber(s, "", cx));
    /// ```
    ///
    /// Zero warnings, 961 green, `script_write_audit` and
    /// `editor_clobber_audit` both passing over a truncating write and an
    /// unguarded buffer clobber.
    ///
    /// The rebuttal is that **the alias has to be introduced somewhere,
    /// and introducing it NAMES the thing.** A `use … as`, a fn-pointer
    /// binding, a re-export, a qualified path, a trait-dispatched call —
    /// every one of them writes the identifier down. So a mention of the
    /// identifier as a WHOLE WORD, anywhere in code, is now a site, and it
    /// must sit inside a sanctioned function exactly as a call did. There
    /// is no call syntax left to vary.
    ///
    /// The word test is what keeps this honest: without it,
    /// `on_save_script` would match `save_script`, and the whole reason
    /// the dot existed would come back. Boundaries are Rust identifier
    /// characters, so `save_script_as` and `bind_script_and_focus` do not
    /// match either — the same exact-name rule T8 re-verify MAJOR-3/G2
    /// established for owners.
    ///
    /// Excluded tokens are blanked in place, so a line holding BOTH an
    /// excluded mention and a real one still reports the real one.
    pub(super) fn audit_excluding(
        needle: &str,
        exclude: &[&str],
        sanctioned: &[&str],
        expected: usize,
        why: &str,
    ) {
        audit_inner(needle, exclude, sanctioned, expected, why, true);
    }

    /// The shared body. `require_call` is re-verify FAIL-8's rule, which
    /// applies to functions and not to fields.
    fn audit_inner(
        needle: &str,
        exclude: &[&str],
        sanctioned: &[&str],
        expected: usize,
        why: &str,
        require_call: bool,
    ) {
        let mut sites = 0usize;
        for (name, src) in sources() {
            let code = code_lines(&src);
            let who = owners(&code);
            for (i, line) in code.iter().enumerate() {
                let mut line = line.clone();
                for ex in exclude {
                    while let Some(at) = line.find(ex) {
                        line.replace_range(at..at + ex.len(), &" ".repeat(ex.len()));
                    }
                }
                if !mentions_word(&line, needle)
                    || defined_fn_name(&line).as_deref() == Some(needle)
                    || plain_import(&line, needle)
                {
                    continue;
                }
                let owner = who[i].as_deref();
                // A struct FIELD DECLARATION is not a write. It sits at
                // file scope inside the `struct` body, so it is told apart
                // by having no owning function and by being `name:` at the
                // start of its line — a shape no assignment has.
                if !require_call
                    && owner.is_none()
                    && line.trim_start().starts_with(&format!("{needle}:"))
                {
                    continue;
                }
                sites += 1;
                // RE-VERIFY FAIL-8: a mention must be a CALL. The name rule
                // bounds where the identifier appears; it does not bound
                // where the CAPABILITY goes. The re-verifier rewrote
                // `save_script`'s single existing mention - inside a
                // SANCTIONED owner, leaving the count at 5 - as
                // `let w = crate::scripts::write_script;`, stashed `w` in a
                // thread-local and called it from `Unbind`. 0 warnings, 964
                // green. Binding a function ITEM is the escape, and it is
                // visible right here: a call is followed by `(`, a binding
                // by `;` or `,` or `)`.
                assert!(
                    !require_call || is_call_mention(&line, needle),
                    "`{needle}` is MENTIONED but not CALLED at {name}:{} (in `{}`) - binding it                      as a value (`let f = ...;`, a rename, an argument) hands the capability to                      code this audit cannot see, which is exactly how it was defeated. Call it,                      or import it plainly",
                    i + 1,
                    owner.unwrap_or("<file scope>")
                );
                let owner = who[i].as_deref();
                assert!(
                    owner.is_some_and(|o| sanctioned.contains(&o)),
                    "unsanctioned mention of `{needle}` at {name}:{} (in `{}`) — {why}",
                    i + 1,
                    owner.unwrap_or("<file scope>")
                );
            }
        }
        assert_eq!(
            sites, expected,
            "`{needle}` mention count changed — re-audit deliberately, do not just bump"
        );
    }

    /// Is this line a `use` item that imports `needle` UNDER ITS OWN
    /// NAME? Those are not sites; a RENAME is.
    ///
    /// RE-VERIFY FAIL-1's whole shape is the rename: `use … as
    /// persist_bytes;` detaches the identifier from the call, so the call
    /// no longer spells it. But a plain `use crate::scripts::write_script;`
    /// detaches nothing — every call through it still says `write_script`,
    /// and the audit still sees those. Re-exports (`pub use …;`) are plain
    /// for the same reason: they carry the name forward, and whoever
    /// eventually renames it is the line that gets flagged.
    ///
    /// So the rule is narrow and mechanical: inside a `use` item, an
    /// occurrence followed by `as` is a rename and stays a site;
    /// everything else in a `use` item is import bookkeeping.
    pub(super) fn plain_import(line: &str, needle: &str) -> bool {
        let t = line.trim_start();
        let t = t.strip_prefix("pub").map_or(t, |r| {
            // `pub(crate)`, `pub(super)`, `pub(in a::b)`
            let r = r.trim_start();
            r.strip_prefix('(').map_or(r, |i| i.find(')').map_or(r, |e| i[e + 1..].trim_start()))
        });
        if !t.starts_with("use ") {
            return false;
        }
        // RE-VERIFY FAIL-14: the line must be NOTHING BUT the `use` item.
        //
        // This used to decide from the PREFIX alone, and `audit_inner` then
        // skipped the line entirely - not counted, not owner-checked, not
        // call-checked. Rust is happy to put a `use` item and further
        // statements on one physical line inside a function body:
        //
        //     use crate::scripts::write_script; let _ = write_script(&p, t);
        //
        // in the live `Unbind` arm: 0 warnings, 966 green, mention count
        // unchanged. The needle did not even matter, because the skip was
        // decided by the prefix - so ANY audited identifier could ride
        // along, including the one-line form that forges
        // `editor_discard_grant` behind a `use std::mem as _x;`.
        //
        // A `use` item ends at its first `;`. Anything after that `;` is a
        // statement, and a statement is exactly what these audits exist to
        // look at.
        let Some((item, rest)) = t.split_once(';') else {
            // No terminator on this line: a multi-line `use` group. The
            // continuation lines are not `use`-prefixed, so they are
            // examined normally; this first line carries no statement.
            return true;
        };
        if !rest.trim().is_empty() {
            return false;
        }
        // `needle` immediately followed by `as` is a rename, not an import.
        let mut from = 0usize;
        while let Some(rel) = item[from..].find(needle) {
            let at = from + rel;
            let end = at + needle.len();
            if item[end..].trim_start().starts_with("as ") {
                return false;
            }
            from = end;
        }
        true
    }

    /// Is every whole-word occurrence of `needle` on this line
    /// immediately (modulo spaces) followed by `(`?
    ///
    /// RE-VERIFY FAIL-8. A call SPENDS the capability here, where the audit
    /// can see the owner; a binding MOVES it somewhere the audit cannot.
    /// `let w = crate::scripts::write_script;` is the whole bypass, and it
    /// differs from the legitimate line by exactly this character.
    ///
    /// An identifier that ENDS the line counts as not-a-call. That is
    /// deliberate and slightly conservative: a call whose `(` sits on the
    /// next line is not something rustfmt produces, while a binding
    /// continued on the next line is easy to write. A false positive here
    /// is a named line in a failing assertion, which is cheap; a false
    /// negative is a leaked writer.
    pub(super) fn is_call_mention(line: &str, needle: &str) -> bool {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        let mut from = 0usize;
        let mut saw = false;
        while let Some(rel) = line[from..].find(needle) {
            let at = from + rel;
            let end = at + needle.len();
            let before_ok = at == 0 || !line[..at].chars().next_back().is_some_and(is_ident);
            let rest = &line[end..];
            let after_ident = rest.chars().next().is_some_and(is_ident);
            if before_ok && !after_ident {
                saw = true;
                if !rest.trim_start().starts_with('(') {
                    return false;
                }
            }
            from = end;
        }
        saw
    }

    /// Does `line` mention `needle` as a WHOLE Rust identifier?
    ///
    /// The boundary test is what stops `save_script` matching inside
    /// `on_save_script` now that the trailing `(` is gone — see
    /// [`audit_excluding`]. Rust identifier characters are alphanumerics
    /// and `_`; everything else (`.`, `:`, `(`, `,`, a space, end of line)
    /// is a boundary, which is precisely why every alias spelling the
    /// re-verifier used still counts.
    pub(super) fn mentions_word(line: &str, needle: &str) -> bool {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        let bytes = line.as_bytes();
        let mut from = 0usize;
        while let Some(rel) = line[from..].find(needle) {
            let at = from + rel;
            let before_ok = at == 0 || !line[..at].chars().next_back().is_some_and(is_ident);
            let end = at + needle.len();
            let after_ok = end >= bytes.len() || !line[end..].chars().next().is_some_and(is_ident);
            if before_ok && after_ok {
                return true;
            }
            from = at + needle.len();
        }
        false
    }

    /// The audit's own non-vacuity rail: if `sources()` ever came back
    /// short — a moved `src`, a wrong `CARGO_MANIFEST_DIR`, a read that
    /// quietly failed — the three tests below would pass by scanning
    /// nothing. `main.rs`'s neighbours are named explicitly because they
    /// are precisely the files the hand-written list used to omit.
    #[test]
    fn the_audit_actually_reads_the_whole_crate() {
        let files = sources();
        assert!(
            files.len() >= 60,
            "expected the workspace's ~79 sources, got {} — the audit would be vacuous",
            files.len()
        );
        for expected in [
            "crates/dbc-ui/src/main.rs",
            "crates/dbc-ui/src/plan.rs",
            "crates/dbc-ui/src/scripts.rs",
            "crates/dbc-ui/src/sql_input.rs",
            "crates/dbc-ui/src/runner.rs",
            // Final-review MAJOR-2: the walk reaches past dbc-ui — these
            // two are the reason it had to. `dbc-state` holds four
            // `write_atomic` callers that write into the user's folder and
            // were audited by nothing; `dbc-mcp` reaches the same vault
            // and config this app does.
            "crates/dbc-state/src/workspace.rs",
            "crates/dbc-mcp/src/main.rs",
            // RE-VERIFY FAIL-4: and past `src`/`tests`. This one is a
            // BENCH, invisible to the previous walk, which enumerated
            // `<crate>/{src,tests,build.rs}` and therefore also missed
            // `examples/`, generated trees, and anything a `#[path]`
            // attribute pulls in from outside the crate directory.
            "crates/dbc-buffer/benches/push_1m.rs",
        ] {
            assert!(files.iter().any(|(n, _)| n == expected), "{expected} not scanned");
        }
        // Every workspace member is represented — and the list is READ
        // FROM THE MANIFEST, not written here, so a crate added tomorrow
        // is covered tomorrow. The hard-coded version of this loop could
        // not notice a new member, which is half of why FAIL-4 worked.
        for member in workspace_members() {
            assert!(
                files.iter().any(|(n, _)| n.starts_with(&format!("{member}/"))),
                "workspace member {member} not scanned"
            );
        }
        assert!(
            files.iter().any(|(_, s)| s.contains("fn editor_load_guarded")),
            "the scanned text is not this crate's source"
        );
    }

    /// FINAL-REVIEW MAJOR-2, structural gap 2 — the three ways the old
    /// one-line `code_of` could be walked past, each pinned.
    ///
    /// Probe sources are ASSEMBLED at runtime, never written as literals:
    /// this module's own file is one of the files `sources()` scans, so a
    /// literal `write_atomic(` here would be counted as a real, unguarded
    /// call site.
    #[test]
    fn comments_and_string_literals_cannot_hide_or_invent_a_call() {
        let call = format!("{}_{}(x)", "write", "atomic");

        // 1. A BLOCK comment used to hide nothing at all, because block
        //    comments were not stripped — so this call was VISIBLE and
        //    would have been reported. Now it is code_lines' job.
        let hidden = format!("    /* {call} */");
        assert_eq!(code_lines(&hidden)[0].trim(), "", "a block comment is not code");

        // …including a NESTED one spanning lines, which Rust allows.
        let multi = format!("a();\n/* one /* two */ still-comment\n{call}\n*/\nb();");
        let out = code_lines(&multi);
        assert_eq!(out.len(), 5, "one line in, one line out — line numbers must survive");
        assert!(out[0].contains("a()"));
        assert!(!out[2].contains(&call), "a nested block comment must still be a comment");
        assert!(out[4].contains("b()"));

        // 2. A `//` INSIDE A STRING used to truncate the line, so the real
        //    code after it disappeared. That is a hiding place, not a
        //    false positive: put the URL first and the call vanished.
        let after = format!("    let u = \"http://x\"; self.{call};");
        assert!(
            code_lines(&after)[0].contains(&call),
            "a `//` inside a string must not swallow the rest of the line"
        );
        // …and the string's own contents must not be readable AS code.
        let inside = format!("    let s = \"{call}\";");
        assert!(
            !code_lines(&inside)[0].contains(&call),
            "text inside a string literal is not a call site"
        );
        // Raw strings too — how every Windows path literal here is written.
        let raw = format!("    let s = r\"{call}\"; done();");
        assert!(!code_lines(&raw)[0].contains(&call));
        assert!(code_lines(&raw)[0].contains("done()"));
        let hashed = format!("    let s = r#\"{call} \"quoted\" \"#; done();");
        assert!(!code_lines(&hashed)[0].contains(&call));
        assert!(code_lines(&hashed)[0].contains("done()"));

        // 3. RE-VERIFY FAIL-3: a BYTE raw string with an ODD number of
        //    quotes in its payload. The old scanner rejected the `b` as a
        //    raw prefix, fell into the ordinary-`"` branch, terminated on
        //    the payload's first quote and then desynced into
        //    "inside a string" — blanking every subsequent character,
        //    across lines, for the rest of the FILE. That HID real calls
        //    from every audit and silently lowered legitimate counts.
        //    Live `br#"…"#` literals already exist in
        //    `dbc-driver-postgres/src/types.rs`.
        let odd = format!("    let _p: &[u8] = br#\"a\"b\"#; self.{call};");
        let got = code_lines(&odd);
        assert!(got[0].contains(&call), "a byte raw string must not swallow the line");
        assert!(!got[0].contains("a\"b"), "its payload is not code either");
        // The desync was the dangerous half: prove nothing leaks past the
        // literal's own line.
        let after = format!("let _p = br#\"a\"b\"#;
self.{call};
more();");
        let got = code_lines(&after);
        assert!(got[1].contains(&call), "the NEXT line must survive a byte raw string");
        assert!(got[2].contains("more()"));
        // The other prefixes, since the fix generalised over all of them.
        for pre in ["b", "c", "br", "cr", "r", ""] {
            let hashed = format!("    let _s = {pre}#\"x\"#; done();");
            let hashed = if pre.contains('r') { hashed } else { format!("    let _s = {pre}\"x\"; done();") };
            assert!(code_lines(&hashed)[0].contains("done()"), "prefix {pre:?} desynced");
        }

        // 4. Char literals must not be mistaken for string openers — `'/'`
        //    and `'"'` are both real code in this crate — while LIFETIMES
        //    must survive untouched.
        let ch = format!("    rel.split('/').for_each(|_| {{}}); self.{call};");
        assert!(code_lines(&ch)[0].contains(&call), "`'/'` must not open a string");
        let quote_ch = format!("    let q = '\"'; self.{call};");
        assert!(code_lines(&quote_ch)[0].contains(&call), "`'\"'` must not open a string");
        assert!(code_lines("fn f<'a>(x: &'a str) -> &'a str { x }")[0].contains("'a"));
    }

    /// THE SOUND VERSION, and the end of a loop that ran four times.
    ///
    /// The property wanted is „no file outside the audited tree was
    /// compiled". Four rounds tried to get it by BANNING THE SPELLINGS
    /// that pull one in, and lost every time — first `#[path = "…"]`
    /// inside the crate, then outside it, then no-space / split-line /
    /// brace-delimited forms, and finally at the TOKEN level:
    /// `#[r#path = "…"]` (a raw identifier resolving to the same built-in
    /// attribute), a `macro_rules!` taking `$a:meta` and expanding to
    /// `#[$a] mod evil;`, and `use std::include as inc; inc!{"…"}`
    /// (`include!` is an ordinary `std` macro and can be renamed).
    ///
    /// Whitespace flattening made SPACING irrelevant and was described as
    /// making „spelling irrelevant by construction". That was true of
    /// whitespace and did not reach the conclusion it was used for. No
    /// string predicate over Rust source closes this class, because the
    /// attacker picks the tokens.
    ///
    /// So the question is no longer asked of the SOURCE at all. It is
    /// asked of the COMPILER. Cargo writes a dep-info file next to every
    /// binary listing every source it actually read — `#[path]` targets,
    /// `include!` splices, `include_str!` data, whatever the spelling, and
    /// whatever macro produced it. Comparing that set against the walked
    /// set answers the real question exactly, and nothing an attacker
    /// writes changes what rustc had to open.
    ///
    /// Fail-CLOSED throughout: an unreadable dep-info, an unparsable
    /// entry, or a suspiciously short list all fail, because every one of
    /// those would otherwise pass by checking nothing.
    ///
    /// One honest limit: the dep-info sits in a target dir SHARED between
    /// worktrees of this repo, so in principle it could describe a build
    /// of a sibling worktree. Its paths are workspace-relative, so it is
    /// resolved against THIS root, and the crate's own `main.rs` is
    /// asserted present — which is the best freshness check available from
    /// inside the test and the reason not to build two worktrees at once.
    #[test]
    fn every_source_the_compiler_read_is_inside_the_audited_tree() {
        let exe = std::env::current_exe().expect("test binary path");
        let dep = exe.with_extension("d");
        let text = std::fs::read_to_string(&dep)
            .unwrap_or_else(|e| panic!("dep-info {} unreadable: {e}", dep.display()));

        let root = workspace_root();
        let root_c = root.canonicalize().expect("workspace root canonicalizes");
        let walked: std::collections::HashSet<PathBuf> = sources()
            .into_iter()
            .filter_map(|(rel, _)| root.join(&rel).canonicalize().ok())
            .collect();

        let (mut outside, mut unwalked, mut unresolved) =
            (Vec::new(), Vec::new(), Vec::new());
        let mut seen = 0usize;
        let mut saw_main = false;
        for line in text.lines() {
            // `<target>.d: <dep> <dep> …`; the per-dep empty rules that
            // follow have no `: ` and are skipped.
            let Some((_, deps)) = line.split_once(".d: ") else { continue };
            for raw in deps.split(' ').filter(|t| !t.is_empty()) {
                let p = PathBuf::from(raw.replace('\\', "/"));
                let abs = if p.is_absolute() { p } else { root.join(p) };
                let Ok(abs) = abs.canonicalize() else {
                    unresolved.push(raw.to_string());
                    continue;
                };
                seen += 1;
                if !abs.starts_with(&root_c) {
                    outside.push(abs.display().to_string());
                    continue;
                }
                if abs.ends_with("main.rs") {
                    saw_main = true;
                }
                if abs.extension().and_then(|e| e.to_str()) == Some("rs")
                    && !walked.contains(&abs)
                {
                    unwalked.push(abs.display().to_string());
                }
            }
        }

        assert!(seen >= 30, "dep-info listed only {seen} sources — this check would be vacuous");
        assert!(saw_main, "dep-info does not mention this crate's main.rs — wrong or stale file");
        assert!(unresolved.is_empty(), "dep-info entries could not be resolved: {unresolved:?}");
        assert!(
            outside.is_empty(),
            "the compiler read source from OUTSIDE the workspace, so no audit in this module              saw it: {outside:?}"
        );
        assert!(
            unwalked.is_empty(),
            "the compiler read Rust source the audit walk does not visit — it is inside the              tree but pruned, so every audit is blind to it: {unwalked:?}"
        );
    }

    /// A file's CODE with every whitespace character removed, plus, for
    /// each retained character, the index of the line it came from.
    ///
    /// RE-VERIFY FAIL-7's lesson in one function: any check that reads a
    /// line at a time, or that assumes a particular spacing, is a check
    /// about FORMATTING. Flattening first makes `#[path = "x"]`,
    /// `#[path="x"]` and an attribute split across three lines the same
    /// string, so there is nothing left for a formatter to vary.
    pub(super) fn flatten_code(src: &str) -> (String, Vec<usize>) {
        let mut flat = String::new();
        let mut where_from: Vec<usize> = Vec::new();
        for (i, line) in code_lines(src).iter().enumerate() {
            for ch in line.chars().filter(|c| !c.is_whitespace()) {
                flat.push(ch);
                where_from.push(i);
            }
        }
        (flat, where_from)
    }


    /// RE-VERIFY FAIL-6 / FAIL-7 / FAIL-8 / FAIL-9, the four predicates
    /// the last round walked past, each pinned at the shape that beat it.
    #[test]
    fn the_scanner_predicates_survive_the_spellings_that_beat_them() {
        // FAIL-6: pruning by NAME SHAPE. `starts_with("target")` hid a
        // plain `mod targets;` from every audit — a name someone could
        // add innocently, which is what made it the worst of the three.
        let td = tempfile::tempdir().unwrap();
        for plausible in ["targets", "target_picker", "targeting", "targetsomething"] {
            let d = td.path().join(plausible);
            std::fs::create_dir(&d).unwrap();
            assert!(!is_pruned(&d), "{plausible} must be scanned");
        }
        // Build output is recognised by cargo's OWN marker, not by a name
        // we guessed — so it is pruned whatever it is called…
        let odd = td.path().join("not-called-target");
        std::fs::create_dir(&odd).unwrap();
        std::fs::write(odd.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172").unwrap();
        assert!(is_pruned(&odd));
        // …but RE-VERIFY FAIL-13: the marker must never hide SOURCE. This
        // assertion used to be the opposite one, and pinned the hole as
        // desired behaviour: the tag is unsigned and trivially created, so
        // dropping one beside a `mod.rs` deleted that directory from every
        // audit. A cargo target dir holds artefacts, never crate sources.
        let sneaky = td.path().join("helpers");
        std::fs::create_dir(&sneaky).unwrap();
        std::fs::write(sneaky.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172").unwrap();
        std::fs::write(sneaky.join("mod.rs"), b"// real code").unwrap();
        assert!(!is_pruned(&sneaky), "a marker must not hide Rust source");
        // …and metadata by exact name.
        for meta in [".git", ".claude", "node_modules"] {
            let d = td.path().join(meta);
            std::fs::create_dir(&d).unwrap();
            assert!(is_pruned(&d));
        }

        // FAIL-7: the escape-hatch ban was a spelling test. Flattening the
        // code removes every formatting degree of freedom the three
        // bypasses used.
        let attr = "#[path=\"x.rs\"]".to_string();
        let (flat, _) = flatten_code(&(attr + " mod evil;"));
        assert!(flat.contains("#[path="), "no-space spelling");
        let split = "#[".to_string() + &chr_nl() + "    path = \"x.rs\"" + &chr_nl() + "]";
        let (flat, _) = flatten_code(&split);
        assert!(flat.contains("#[path="), "attribute split across lines");
        let braced = "include!{\"x.rs\"}".to_string();
        let (flat, _) = flatten_code(&braced);
        assert!(flat.contains("include!{"), "brace delimiter");
        let spaced = "include! (\"x.rs\")".to_string();
        let (flat, _) = flatten_code(&spaced);
        assert!(flat.contains("include!("), "space before the delimiter");
        // …and the DATA macros stay legal, which this crate depends on.
        let (flat, _) = flatten_code("include_str!(\"main.rs\")");
        assert!(!flat.contains("include!("), "include_str! is not token splicing");

        // FAIL-8: a mention must SPEND the capability, not move it.
        let w = format!("{}_{}", "write", "script");
        assert!(is_call_mention(&format!("let _ = crate::scripts::{w}(&p, &t);"), &w));
        assert!(is_call_mention(&format!("    {w} (&p, &t)"), &w), "a space before `(` is a call");
        assert!(!is_call_mention(&format!("let f = crate::scripts::{w};"), &w), "fn-item binding");
        assert!(!is_call_mention(&format!("takes_fn(crate::scripts::{w}, x)"), &w), "as an argument");
        assert!(!is_call_mention(&format!("let f = crate::scripts::{w}"), &w), "end of line");
        // A near-miss name is not a mention at all, so it is vacuously
        // fine — the exact-name rule still does that job.
        assert!(!is_call_mention(&format!("self.on_{w}(cx);"), &w));

        // FAIL-9: logical lines. A CRLF checkout must produce byte-identical
        // lines to an LF one, or every exact-match assertion is a coin flip
        // depending on how the file arrived on disk.
        let lf = "fn f() {".to_string() + &chr_nl() + "    a();" + &chr_nl() + "}";
        let crlf = lf.replace(&chr_nl(), &(chr_cr() + &chr_nl()));
        assert_eq!(code_lines(&lf), code_lines(&crlf), "CRLF must not change a logical line");
        assert_eq!(code_lines(&crlf)[1], "    a();");
    }

    fn chr_nl() -> String {
        String::from_utf8(vec![10]).unwrap()
    }
    fn chr_cr() -> String {
        String::from_utf8(vec![13]).unwrap()
    }

    /// RE-VERIFY FAIL-1: the matcher looks for the NAME, not for a call,
    /// and a whole-word one — so every way of detaching an identifier from
    /// its call site is a site, while the near-miss names the exact-match
    /// rule exists to protect stay out.
    #[test]
    fn an_alias_or_a_fn_pointer_cannot_detach_a_name_from_its_audit() {
        let guarded = format!("{}_{}", "write", "script");

        // The two shapes the re-verifier used. Neither is a call of the
        // guarded name; both NAME it, which is the point.
        let aliased = format!("    use crate::scripts::{guarded} as persist_bytes;");
        assert!(mentions_word(&aliased, &guarded));
        assert!(!plain_import(&aliased, &guarded), "a RENAME is a site, not bookkeeping");
        let ptr = format!("    let clobber = crate::sql_input::SqlInput::{guarded};");
        assert!(mentions_word(&ptr, &guarded), "a fn-pointer binding names it too");

        // A plain import (and a re-export) carries the name forward, so
        // every call through it still spells it — bookkeeping, not a site.
        let plain = format!("    use crate::scripts::{guarded};");
        assert!(plain_import(&plain, &guarded));
        let grouped = format!("use dbc_state::fsutil::{{join_component, {guarded}}};");
        assert!(plain_import(&grouped, &guarded));
        let reexport = format!("    pub(crate) use crate::scripts::{guarded};");
        assert!(plain_import(&reexport, &guarded));
        // …but a rename hidden inside a group is still a rename.
        let sneaky = format!("use crate::scripts::{{a, {guarded} as p}};");
        assert!(!plain_import(&sneaky, &guarded));

        // Whole-word, or `on_save_script` would match `save_script` and
        // the leading dot that FINAL-REVIEW MAJOR-2 removed would have to
        // come back. Same exact-name rule as T8 re-verify MAJOR-3/G2.
        let save = format!("{}_{}", "save", "script");
        assert!(!mentions_word(&format!("self.on_{save}(cx);"), &save));
        assert!(!mentions_word(&format!("self.{save}_as(cx);"), &save));
        assert!(mentions_word(&format!("self.{save}(p, t, false, a, cx);"), &save));
        assert!(mentions_word(&format!("AppView::{save}(self, p, t, false, a, cx)"), &save));
    }

    /// FINAL-REVIEW MAJOR-2, structural gap 3: attribution is now BRACE
    /// BALANCED. The old owner detector was „the nearest `fn` above",
    /// full stop, so anything at file scope after a sanctioned function
    /// closed inherited that function's sanction.
    #[test]
    fn a_call_after_a_function_closes_is_not_attributed_to_it() {
        let guarded = format!("{}_{}", "bind", "script");
        let src = format!(
            "fn {guarded}() {{\n    inner();\n}}\n\nstatic X: u8 = danger();\n\nfn other() {{\n    more();\n}}\n"
        );
        let code = code_lines(&src);
        let who = owners(&code);
        assert_eq!(who[1].as_deref(), Some(guarded.as_str()), "the body belongs to its fn");
        assert_eq!(who[2].as_deref(), Some(guarded.as_str()), "so does its closing brace");
        assert_eq!(who[4], None, "FILE SCOPE — the old detector said `{guarded}` here");
        assert_eq!(who[7].as_deref(), Some("other"));

        // A closure inside a body does not steal ownership: the innermost
        // still-open `fn` is the answer these audits want.
        let nested = "fn outer() {\n    spawn(move || {\n        danger();\n    });\n}\n";
        let who = owners(&code_lines(nested));
        assert_eq!(who[2].as_deref(), Some("outer"));
    }

    /// The parser everything above rests on. Probe lines are ASSEMBLED at
    /// runtime, never written as literals: this module's own source is one
    /// of the files `sources()` scans, so a literal `bind_script(` here
    /// would be counted as a real, unguarded call site — verified, it fails
    /// exactly that way. (Which is itself a nice proof the scan is live.)
    #[test]
    fn the_owner_parser_handles_every_fn_spelling_in_use() {
        let guarded = "bind_script";
        let vis = format!("    pub(crate) fn {guarded}(&mut self) {{");
        assert_eq!(defined_fn_name(&vis).as_deref(), Some(guarded));
        let asy = format!("async fn open_{}(rel: String) {{", "script");
        assert_eq!(defined_fn_name(&asy).as_deref(), Some("open_script"));
        // Qualifiers in combination, and a restricted-path visibility —
        // the four spellings the old detector attributed to the PREVIOUS
        // function instead.
        assert_eq!(defined_fn_name("    pub(in crate::a) const unsafe fn x() {}").as_deref(), Some("x"));
        assert_eq!(defined_fn_name("    pub async unsafe fn y() {}").as_deref(), Some("y"));
        assert_eq!(defined_fn_name("    unsafe extern \"C\" fn z() {}").as_deref(), Some("z"));
        // Not definitions.
        assert_eq!(defined_fn_name("    /// fn not_a_definition(&self)"), None);
        let call = format!("        self.{guarded}(p, t, cx);");
        assert_eq!(defined_fn_name(&call), None);
        assert_eq!(defined_fn_name("        cx.spawn(async move |this, cx| {"), None);
        // A near-miss name must NOT be read as the sanctioned one — the
        // exact-match half of G2.
        let near = format!("    fn {guarded}_and_focus(&mut self) {{");
        assert_ne!(defined_fn_name(&near).as_deref(), Some(guarded));
        assert_eq!(defined_fn_name(&near).as_deref(), Some("bind_script_and_focus"));
    }

    /// Shape-independent: whatever expression reaches the editor entity,
    /// the replacement itself must name `replace_buffer`.
    #[test]
    fn only_the_guarded_sites_may_replace_the_sql_editors_buffer() {
        audit(
            "replace_buffer",
            &[
                "bind_script",
                "perform_script_action",
                // 2 → 3 on 2026-08-29 for „Formátovat". Sanctioned after
                // re-reading what this audit protects: content from
                // ELSEWHERE must not replace unsaved work.
                // `rewrite_buffer_in_place` takes no text — it takes
                // `&str -> String` and feeds it the live buffer — so there
                // is no parameter through which foreign content could
                // arrive, and nothing for the user's work to be lost to.
                // That is why it may skip the dirty check the other mint
                // exists to enforce, and why sanctioning it is not a
                // widening of this rail.
                "rewrite_buffer_in_place",
            ],
            3,
            "route it through `AppView::editor_load_guarded` (Part S §5.5) or a bound \
             script's unsaved changes are destroyed silently, with no undo",
        );
    }

    /// The sanction above is only sound while `rewrite_buffer_in_place`
    /// cannot be handed text from outside. If someone ever gives it a
    /// `&str` parameter, it becomes an unguarded clobber wearing a
    /// sanctioned name — and the audit above would wave it straight
    /// through, because the owner is on the list.
    #[test]
    fn the_in_place_rewrite_takes_a_transform_and_never_a_string() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("own source");
        let lines = code_lines(&src);
        let start = lines
            .iter()
            .position(|l| l.contains("fn rewrite_buffer_in_place"))
            .expect("the function must exist for its sanction to mean anything");
        let sig: String = lines[start..start + 6].join(" ");
        assert!(
            sig.contains("impl FnOnce(&str) -> String"),
            "the transform parameter is what makes the sanction sound: {sig}"
        );
        assert!(
            !sig.contains("text: &str") && !sig.contains("text: String"),
            "a text parameter would make this an unguarded clobber: {sig}"
        );
    }

    /// RE-VERIFY: the editor permit is a real rail only while the DISCARD
    /// GRANT is honest, so the grant's writers are audited by name.
    ///
    /// `with_editor_replaceable` refuses a dirty editor unless the user
    /// has just answered „Zahodit". That answer cannot be re-derived from
    /// live state, so it is recorded in `AppView::editor_discard_grant` —
    /// and anything that could SET that field is a way to fake the user's
    /// answer and clobber unsaved text. There are exactly three legal
    /// mentions: where the user answers, where it is spent, and the field
    /// declaration itself; plus the struct literal that initialises it.
    ///
    /// This is a name audit, with all the limits the as-built note spells
    /// out — but the thing it guards is a single `Option<u64>` written in
    /// one place, which is about the smallest surface a name audit can be
    /// asked to cover.
    #[test]
    fn the_discard_grant_is_written_only_where_the_user_answered() {
        audit_field(
            "editor_discard_grant",
            &[
                // The user's answer, recorded.
                "on_discard_confirm_yes",
                // …and spent, once.
                "with_editor_replaceable",
                // `AppView`'s one construction site, which must name every
                // field. `None` is the only value it may start at, and a
                // grant minted before the window exists would be spent by
                // the first load — so this one is sanctioned by NAME and
                // pinned by the count, not waved through by shape.
                "main",
            ],
            4,
            "setting this fakes the user's answer to the discard prompt, which is the one              thing standing between a background load and a bound script's unsaved changes",
        );
    }

    /// …and the performer must not be reachable around the guard. The two
    /// legal callers are the guard's own clean-editor fall-through and
    /// „Zahodit" (`on_discard_confirm_yes`), which is the user answering
    /// the guard's question.
    #[test]
    fn the_parked_action_is_only_performed_by_the_guard_or_its_confirm() {
        audit(
            "perform_script_action",
            &["editor_load_guarded", "on_discard_confirm_yes"],
            2,
            "the guard exists to stand in front of this — call \
             `AppView::editor_load_guarded` instead",
        );
    }

    /// `bind_script` replaces the buffer too, so it is guard-territory
    /// even though it takes text rather than a rel: only `open_script`
    /// (itself reachable only through `perform_script_action`) may call it.
    #[test]
    fn the_binder_is_only_reached_from_the_guarded_open_path() {
        audit(
            "bind_script",
            &["open_script"],
            1,
            "binding replaces the editor's buffer — go through \
             `AppView::editor_load_guarded(PendingScriptAction::Open { .. })`",
        );
    }
}

/// T9 REVIEW MAJOR-2: the same audit shape, aimed at the FILESYSTEM.
///
/// `running_a_library_script_never_auto_saves_first` bans four identifiers
/// inside `run_script_from_library`'s own body — and the review defeated
/// it with a real `cargo test` by putting a `crate::scripts` write call at
/// the top of `open_script_run_modal` instead: the library's run
/// truncating its target immediately before running it, using a banned
/// identifier verbatim, green. No alias or macro was needed, because
/// `open_script_run_modal` is BY DESIGN the shared continuation both run
/// paths funnel through — and that audit's own non-vacuity assertion
/// certifies an unaudited region is on the path.
///
/// The fix is the mechanism that has now survived attack twice rather than
/// a longer ban list: audit the WRITER workspace-wide, by identifier, with
/// an exact-name owner check — `editor_clobber_audit`'s scanner, reused
/// wholesale (it walks every `.rs` under the workspace root, so a new
/// file is covered the moment it exists). A ban list only ever covers the
/// regions someone remembered to list; this covers the tree.
///
/// **RE-VERIFY FAIL-1: what this actually promises, stated honestly.**
/// The sentence here used to read „NO BYTE reaches THE SCRIPTS LIBRARY
/// from this crate except through `AppView::save_script` … or
/// `scripts::create_script`", and that was FALSE as written. These are
/// TEXT audits. They had no type behind them, and the re-verifier walked
/// past them with an alias:
///
/// ```ignore
/// use crate::scripts::write_script as persist_bytes;
/// let _ = persist_bytes(&doomed, "-- truncated by the run");
/// ```
///
/// Zero warnings, 961 green, a truncating write into the library.
///
/// **What these audits check — the whole of it, with no verb stronger
/// than „check".** Every MENTION of the audited identifiers, in the
/// source files the walk visits, sits inside a sanctioned function; the
/// mention is a call rather than a binding; and the count is pinned. That
/// is a property of SOURCE TEXT. It is not a property of the program.
///
/// **It is NOT a guarantee that nothing else writes into the library.**
/// The demonstration is one line and needs no trick at all:
///
/// ```ignore
/// std::fs::write(root.join("trzby.sql"), "-- truncated");
/// ```
///
/// That truncates a library file, mentions no audited identifier, and
/// passes every test here. So does a closure wrapper minted at a
/// sanctioned site and called from somewhere else (see §T/§W of the
/// as-built note). Neither is closed, and this comment previously implied
/// both were — three rounds running, an over-claim here is what let the
/// next round through, so the claim is now bounded to what runs.
///
/// The compiler-enforced rails, named precisely so nobody mistakes their
/// scope, are THREE: the Ctrl+S permission
/// (`save_guard::with_save_permission` in front of `AppView::save_script`),
/// the editor buffer (`editor_guard::with_editor_replaceable` in front of
/// `SqlInput::replace_buffer`) and the config write
/// (`dbc_state::ConfigSaveGuard`, mintable only by a real parse of the
/// very file about to be overwritten). `write_script`, `write_atomic` and
/// `bind_script` have NO type rail — the as-built note gives the reason,
/// which for the two writers is that their call happens inside a
/// `'static` background future where a branded permit cannot go. They are
/// held up by these text audits alone, and the paragraph above says what
/// that is worth.
///
/// The one thing outside the walk is now checked soundly rather than
/// banned by spelling: `every_source_the_compiler_read_is_inside_the_audited_tree`
/// reads cargo's dep-info, so „the audit did not see this file" is
/// answered by the compiler.
///
/// The library's RUN is a separate, narrower claim and it still holds:
/// `run_script_from_library`'s own body bans the four write identifiers,
/// and the counts here pin their call sites. That is „no audited writer
/// is on the run path", not „the run cannot write".
///
/// **T9 re-verify NIT-A: "the scripts library", NOT "a user-chosen
/// folder".** The older, wider sentence was false and it is worth knowing
/// exactly how, so nobody re-widens it. Two other writers in this crate do
/// reach folders the user chose:
///
/// * `grid.rs`'s CSV/JSON export (`File::create` + rename)
/// * `er_diagram_view.rs`'s SVG export (`fs::write`)
///
/// Neither can silently reach the library: both are user-directed through
/// `prompt_for_new_path` opened with an EMPTY start directory, so the user
/// types the destination every time and no automatic path leads there. The
/// invariant this audit actually protects — nothing writes into the
/// scripts library behind the user's back — holds; the sentence claiming
/// coverage of every user folder did not.
///
/// **Recorded, not fixed (pre-existing, out of this phase's scope):**
/// `grid.rs`'s exporter independently derives `<path>.tmp`, byte-identical
/// to `fsutil::tmp_path_for`. That is a second writer over the same tmp
/// convention whose single-writer contract `write_atomic`'s doc states as
/// if it covered every writer over a user folder. It is only reachable by
/// exporting a grid onto the exact path of an in-flight script save, which
/// takes deliberate effort, and the blanket `*.tmp` in the shipped
/// `.gitignore` covers it either way. See the as-built §C table.
///
/// The test call sites are sanctioned BY NAME rather than skipped: the
/// exact-count assertion is what makes a new one a deliberate decision,
/// and a `#[cfg(test)]`-region filter would be one more thing to defeat.
#[cfg(test)]
mod script_write_audit {
    use super::editor_clobber_audit::{audit, audit_excluding};

    /// The ONE funnel from this crate into
    /// `dbc_state::fsutil::write_atomic`, whose tmp path is a pure
    /// function of the target (T8's single-writer contract). A second
    /// caller would be a second writer over a path this crate believes
    /// only one function can touch.
    ///
    /// FINAL-REVIEW MAJOR-2, structural gap 1: this used to scan
    /// `dbc-ui/src` alone and could therefore assert „exactly one caller"
    /// while `dbc-state` held FOUR more — `write_pointer`, `copy_one`,
    /// `init_contents` and `write_marker`, every one of them writing real
    /// bytes into the user's chosen folder, audited by nothing. The count
    /// is honest now, and the sanction list is the inventory: a NEW
    /// atomic write anywhere in the workspace fails here.
    ///
    /// The five `dbc-state` production owners are legitimate and named
    /// individually rather than waved through by crate: each writes ONE
    /// well-known file (the pointer, one copied store file, the
    /// `.gitignore`, the marker) and none of them can be aimed at a
    /// script, which is the single-writer contract this test protects.
    #[test]
    fn the_shared_atomic_writer_has_exactly_one_funnel() {
        audit(
            "write_atomic",
            &[
                // dbc-ui: THE funnel, and the only one.
                "write_script",
                // dbc-state: the workspace lane's own writers (§W3.2).
                "write_pointer",
                "copy_one",
                "init_contents",
                "write_marker",
                // dbc-state: the rail's own tests.
                "write_atomic_refuses_a_missing_parent_while_the_store_savers_create_one",
                "write_atomic_leaves_no_tmp_file_and_writes_bytes",
                "write_atomic_overwrites_in_place",
                "write_atomic_failure_leaves_no_tmp_behind",
            ],
            10,
            "`crate::scripts::write_script` is the ONE funnel from dbc-ui into the shared \
             atomic writer, and dbc-state's own callers each own exactly one well-known \
             file - a new caller forks T8's single-writer-per-path contract",
        );
    }

    /// ...and that funnel itself has exactly two production callers.
    #[test]
    fn nothing_but_the_guarded_save_and_create_may_write_a_script() {
        audit(
            "write_script",
            &[
                // Production: the guarded, serialized Ctrl+S / save-as.
                "save_script",
                // Production: creation, which probes `conflicting_name`
                // first because the writer REPLACES by design.
                "create_script",
                // Tests of the writer itself (scripts.rs).
                "write_and_read_script_roundtrip_and_caps",
                "write_script_replaces_an_existing_target",
            ],
            5,
            "writing into the user's scripts folder is `AppView::save_script`'s job (guarded              by `script_save_allowed` + `script_save_in_flight`) or `scripts::create_script`'s              - the library's run serves DISK content and must never write, from any function              on its path",
        );
    }

    /// T9 RE-VERIFY FAIL-1, the missing link in the chain. The audit above
    /// sanctions the OWNER `save_script` unconditionally, so the chain
    /// stopped there: `save_script`'s own callers were audited by nothing,
    /// and the re-verify added a plausible future handler calling
    /// `self.save_script(..)` directly — straight past `script_save_allowed`
    /// — and got the whole suite green, all three audits included. The only
    /// signal was a dead-code warning, which disappears the moment the
    /// handler is wired to a listener.
    ///
    /// Two legal callers, and only two: `on_save_script` (the ONE entry
    /// point for Ctrl+S, the caption strip's „Uložit" and the palette
    /// action — all three verified to route through it) and
    /// `save_script_as`, which re-asks the predicate itself because its
    /// check sits after an await.
    ///
    /// **FINAL-REVIEW MAJOR-2: the needle no longer carries a receiver.**
    /// It used to be `.save_script`, WITH THE DOT, and the paragraph that
    /// stood here argued for the dot at length: `audit` matches
    /// `needle + "("` as a plain substring, a bare `save_script` also
    /// matches `on_save_script(`, and the dot excluded that for free while
    /// keeping the definition line out of the count.
    ///
    /// The argument was sound and the conclusion was wrong, because the
    /// dot is a claim about CALL SYNTAX. UFCS spells a colon:
    /// `AppView::save_script(self, path, text, false, cx)` contains
    /// `::save_script(`, not `.save_script(`. The reviewer put exactly
    /// that on a live production path — inside `perform_script_action`'s
    /// `Unbind` arm — and got zero warnings and 11/11 audits green.
    ///
    /// So the false positive is now NAMED instead of dodged
    /// (`audit_excluding`), and the needle matches any receiver, any path
    /// qualification, and no receiver at all. The definitive rail is no
    /// longer here in any case — `save_script` demands a
    /// `save_guard::SaveAllowed` witness the crate root cannot construct
    /// — but this stays as the belt to those braces, and it is what would
    /// catch a future caller that mints the witness legitimately and still
    /// writes from somewhere it should not.
    #[test]
    fn the_writer_itself_is_reachable_only_through_the_guarded_entry_points() {
        audit_excluding(
            "save_script",
            // The ENTRY POINT is not the writer. Named, not punctuated
            // around.
            &["on_save_script"],
            &["on_save_script", "save_script_as"],
            2,
            "`AppView::save_script` is the WRITER - reaching it around `on_save_script` \
             skips `script_save_allowed` entirely, and MAJOR-1's scenario (a Ctrl+S racing \
             a confirmed delete, recreating the file the user just irreversibly removed) \
             is back. A new save path asks the predicate first, then calls this",
        );
    }

    /// MAJOR-1's guard, pinned structurally as well as behaviourally: the
    /// predicate exists, and it is asked at every point where a save can
    /// still be stopped. A future "quick" save path that skips it fails
    /// here rather than silently racing a delete.
    ///
    /// T9 re-verify FAIL-1 made this TWO production sites, not one.
    /// `on_save_script` asks it synchronously, before anything is
    /// dispatched; `save_script_as` asks it AGAIN in its post-await
    /// continuation, because the file picker is not app-modal on every
    /// platform and the entry-point check is, by then, a statement about
    /// the past. (The test name used to say „exactly once" and was the
    /// clearest statement of the bug.)
    ///
    /// FINAL-REVIEW MAJOR-2 split the predicate in two.
    /// `script_save_allowed` is the pure RULE, still unit-pinned, and it
    /// now has exactly one production caller: `save_guard::with_save_permission`,
    /// which is the only MINT of the `SaveAllowed` witness and which reads
    /// the three facts off the live `AppView` instead of accepting three
    /// booleans a caller could choose. `with_save_permission` is audited
    /// separately below, at the two entry points.
    #[test]
    fn the_ctrl_s_dialog_guard_is_asked_at_every_stoppable_point() {
        audit(
            "script_save_allowed",
            // The predicate's own unit test is a real call site; naming it
            // here is what makes the exact count meaningful.
            &["with_save_permission", "ctrl_s_is_refused_whenever_any_dialog_owns_the_screen"],
            6,
            "the pure Ctrl+S rule belongs to `save_guard::with_save_permission`, which reads the \
             live view - a caller that asks it with its own three booleans has bought \
             nothing, because it can pass whichever three suit it",
        );
    }

    /// …and the MINT is asked at every point where a save can still be
    /// stopped (final-review MAJOR-2). This is the test that used to be
    /// the one above: two production sites, and only two.
    ///
    /// `on_save_script` asks synchronously, before anything is dispatched;
    /// `save_script_as` asks AGAIN in its post-await continuation, because
    /// the file picker is not app-modal on every platform and the
    /// entry-point answer is, by then, a statement about the past. The
    /// witness being neither `Copy` nor `Clone` is what makes carrying the
    /// first answer across the picker impossible rather than merely
    /// discouraged.
    #[test]
    fn the_save_permission_is_opened_at_every_stoppable_point_and_nowhere_else() {
        audit(
            "with_save_permission",
            &["on_save_script", "save_script_as"],
            2,
            "every Ctrl+S entry point must mint the witness HERE - `.occlude()` blocks \
             clicks, not keys - and a path that awaits between the mint and the write must \
             mint a fresh one",
        );
    }
}
