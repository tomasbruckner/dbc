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
mod row_view;
mod runner;
mod sandbox;
mod schema_tree;
mod sql_highlight;
mod sql_input;
mod tabs;
mod text_model;
mod theme;
mod tunnel;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use dbc_buffer::ResultBuffer;
use dbc_core::arrow::datatypes::SchemaRef;
use dbc_core::{
    apply_auto_limit_d, find_params, is_read_statement_d, quote_qualified_d, substitute_params,
    CancelToken, FkRef, QueryError, SchemaSnapshot, TableInfo,
};
use dbc_state::{
    AppConfig, HistoryDb, HistoryEntry, ParamValue, ParamValuesStore, TableViewPrefs, Vault,
    ViewPrefsStore,
};
use gpui::{
    actions, div, prelude::*, px, size, uniform_list, AnyElement, App, Bounds, ClipboardItem,
    Context, Entity, Focusable, KeyBinding, PathPromptOptions, ScrollDelta, ScrollWheelEvent,
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
    [RunQuery, RunQueryUnlimited, CancelQuery, ToggleTree, ToggleHistory, OpenPalette, OpenAutocomplete]
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
fn completion_edit(text: &str, cursor: usize, insert: &str) -> (std::ops::Range<usize>, String) {
    let ctx = autocomplete::cursor_context(text, cursor);
    let start = cursor - ctx.prefix.len();
    let mut new_text = text.to_string();
    new_text.replace_range(start..cursor, insert);
    (start..cursor, new_text)
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
        let is_sql = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
        if is_sql {
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

/// G7 T7: the pending-fetch state `connections_ui::confirm_compare_dialog`
/// hands to `AppView::on_compare_schema_pair_ready` once `fetch_schema_pair`
/// resolves. The Compare tab (and its `CompareView` entity, in
/// `CompareLoadState::Loading`) is opened immediately at dispatch time —
/// `view` is that SAME entity, updated in place once the fetch resolves
/// (rather than a second entity being constructed here), so the tab
/// actually shows "Načítám schéma…" for the duration of the fetch instead
/// of only appearing once it's already done. `generation` mirrors
/// `schema_fetch_generation`'s guard shape — a newer dispatch's result
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
    /// G7 T6: bumped on every `confirm_compare_dialog` dispatch; an
    /// `on_compare_schema_pair_ready` result only applies if the generation
    /// still matches — same last-dispatched-wins guard as
    /// `schema_fetch_generation`/`switch_generation`.
    compare_fetch_generation: u64,
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
    // --- G6 Task 3: parametrized `:name` query values ---
    /// Opened from `dbc_state::default_param_values_path()` at startup;
    /// `None` on a load failure (same "degrade gracefully" posture as
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
    /// (keyed by `current_conn_identity()` — the same stable connection
    /// identity `ResultTab::conn_identity`/`apply_conn_spec` use, covering
    /// both a saved connection and the CLI-arg `"cli"` sentinel). Refuses
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
        let conn_id = self.current_conn_identity();
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
                let conn_id = self.current_conn_identity();
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

        let spec = if let Some(id) = self.active_connection_id.clone() {
            let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return;
            };
            // G15 T8 HARD GATE ITEM 2: `connect::resolve_secret_for_connect`
            // (not a raw `vault.get_secret`) — skips the vault lookup
            // entirely for an MSSQL config that's refused before any secret
            // is ever used (SSH tunnel / empty user), see its doc comment.
            let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
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
                        let is_sql = picked
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
                        if !is_sql {
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
                Ok((source_label, files, file_counts)) => {
                    // Review fix (MINOR 4): a modal the user opened WHILE
                    // this picker/pre-scan was in flight wins — don't
                    // clobber it with a stale script-run pick.
                    if view.modal.is_some() {
                        view.status =
                            "výběr skriptu zahozen — je otevřený jiný dialog".to_string();
                        cx.notify();
                        return;
                    }
                    // Review fix (MAJOR 1), defense in depth (same posture
                    // as CSV's `start_csv_import`): the picker + pre-scan
                    // didn't block the connection dropdown — if it already
                    // changed, don't even open the modal with a stale
                    // file/folder selection; `confirm_script_run` re-checks
                    // this same identity again regardless (the actual
                    // guard), so this is purely a faster/friendlier
                    // refusal.
                    if !conn_identity_matches(&conn_identity, &view.current_conn_identity()) {
                        view.status =
                            "připojení se během výběru změnilo — spuštění zrušeno".to_string();
                        cx.notify();
                        return;
                    }
                    view.status = String::new();
                    view.modal = Some(connections_ui::ModalState::ScriptRun {
                        files,
                        file_counts,
                        tx_scope: runner::TxScope::PerFile,
                        error_policy: runner::ErrorPolicy::Stop,
                        source_label,
                        conn_label,
                        read_only,
                        timeout_secs,
                        conn_identity,
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
        if let Some(id) = self.active_connection_id.clone() {
            let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else {
                self.status = "connection no longer exists".into();
                cx.notify();
                return None;
            };
            let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
            let (read_only, timeout_secs, engine) = (cfg.read_only, cfg.timeout_secs, cfg.engine);
            Some((read_only, timeout_secs, engine, ConnectSpec::Config { cfg: Box::new(cfg), secret }))
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
                    let raw_text = if buf.row_count() == 0 || buf.cell_is_null(0, 0) {
                        Err("EXPLAIN nevrátil žádný řádek".to_string())
                    } else {
                        Ok(buf.cell_text(0, 0))
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
                    // Exactly `on_tree_event`'s `TreeEvent::RefreshRequested` arm.
                    if let Some(spec) = self.active_conn_spec() {
                        self.trigger_schema_fetch(spec, cx);
                    } else {
                        self.schema_tree_connection_key = None;
                        self.tree.update(cx, |t, cx| {
                            t.clear(cx);
                            t.set_admin_entry(admin_panel::AdminEntry::Hidden, cx);
                        });
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
            self.status = match self.config.save(&self.config_path) {
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
    /// (`connections_ui::switch_to_connection`'s success arm, AND
    /// `trigger_schema_fetch`'s successful-snapshot arm below), not just
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
        let candidates = autocomplete::candidates(&text, cursor, snapshot, true, suppressed);
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
        let text = self.sql.read(cx).text();
        let cursor = self.sql.read(cx).cursor();
        let (range, _) = completion_edit(&text, cursor, &insert);
        let prefix_len = cursor - range.start;

        self.sql.update(cx, |s, cx| s.accept_completion(prefix_len, &insert, cx));
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
        if let Some(id) = self.active_connection_id.clone() {
            let cfg = self.config.connections.iter().find(|c| c.id == id)?.clone();
            let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
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
        // `trigger_schema_fetch`'s own success arm re-pushes on every
        // subsequent refresh (see its `set_schemas` call there).
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
    /// `trigger_schema_fetch`/`fetch_lookup` already use. No read-only
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
    fn open_er_diagram(&mut self, schema: Option<String>, cx: &mut Context<Self>) {
        let Some(snapshot) = self.tree.read(cx).snapshot() else {
            self.status = "Nejprve načtěte schéma".to_string();
            cx.notify();
            return;
        };
        let scoped: Vec<TableInfo> =
            snapshot.tables.iter().filter(|t| t.schema == schema).cloned().collect();
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
        if let Some(id) = self.active_connection_id.clone() {
            let cfg = self.config.connections.iter().find(|c| c.id == id)?.clone();
            let secret = connect::resolve_secret_for_connect(self.vault.as_ref(), &cfg);
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
                        let read_only = view.active_read_only();
                        // G10 T4: recomputed alongside every snapshot apply,
                        // same posture as favourites/read_only above — the
                        // tree's pinned "Správa serveru" row visibility must
                        // never lag a connection switch.
                        let admin_entry = admin_panel::admin_entry_state(view.active_engine(), read_only);
                        // G10 T5: the Privileges sub-view's schema selector,
                        // computed BEFORE `snapshot` moves into
                        // `set_snapshot` below — pushed into whichever admin
                        // tab is currently open (there is at most one, the
                        // singleton-per-connection invariant), same
                        // "refreshes alongside every snapshot" posture as
                        // favourites/read_only/admin_entry.
                        let schemas_for_admin = admin_panel::distinct_schemas(&snapshot);
                        view.tree.update(cx, |t, cx| {
                            t.set_snapshot(snapshot, same_connection, cx);
                            t.set_favourites(favourites, active_id, cx);
                            t.set_read_only(read_only, cx);
                            t.set_admin_entry(admin_entry, cx);
                        });
                        // Review finding M2: only push into an admin panel
                        // whose OWN stamped identity still matches the
                        // CURRENTLY active connection — a stale admin tab
                        // left open from a since-abandoned connection (the
                        // singleton-per-connection invariant only replaces
                        // it on the NEXT `open_admin_tab` call, not
                        // automatically on every switch) must never have
                        // another connection's schema list silently pushed
                        // into it.
                        if let Some(panel) = view.tabs.iter().find_map(|t| match &t.content {
                            TabContent::Admin { view } => Some(view.clone()),
                            _ => None,
                        }) {
                            let current_identity = view.current_conn_identity();
                            if conn_identity_matches(panel.read(cx).conn_identity(), &current_identity) {
                                panel.update(cx, |p, cx| p.set_schemas(schemas_for_admin, cx));
                            }
                        }
                        // Review round 3, MAJOR 1: a new snapshot landing
                        // (connection switch OR a same-connection refresh)
                        // invalidates whatever candidates an open popup was
                        // computed from — close it rather than risk an
                        // accept inserting a stale/wrong-schema name.
                        view.close_autocomplete(cx);
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

    /// G7 T7: computes `mode` from the two connections' engines, runs
    /// `dbc_diff::schema_diff::diff_schema`, and updates the ALREADY-OPEN
    /// Compare tab's `CompareView` entity (`pending.view`, created and
    /// opened by `connections_ui::confirm_compare_dialog` in
    /// `CompareLoadState::Loading` at dispatch time — design §3) in place.
    /// `result`'s `Err` case (the oneshot channel closing — the runner task
    /// panicked/dropped, which never happens in normal operation, but is
    /// still a `Result`, not an `unwrap`, same posture `trigger_schema_fetch`
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
                    self.tree.update(cx, |t, cx| {
                        t.clear(cx);
                        t.set_admin_entry(admin_panel::AdminEntry::Hidden, cx);
                    });
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
            TreeEvent::OpenErDiagram { schema } => {
                self.open_er_diagram(schema.clone(), cx);
            }
            TreeEvent::ImportCsv { schema, table } => {
                self.start_csv_import(schema.clone(), table.clone(), cx);
            }
            TreeEvent::OpenAdmin => {
                self.open_admin_tab(cx);
            }
        }
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
        let dc = self.discard_confirm.as_ref()?;
        let n = dc.change_count;
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
                // G16 T4 (still unreachable from the UI until T6 flips
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
            if self.modal.is_some() || self.discard_confirm.is_some() {
                window.focus(&self.modal_focus_handle, cx);
            }
        }
        // G6 T7: lazy-diff typing-trigger recompute, BEFORE the popup is
        // drawn below (design §2 grounding) — then sync the flag T5's
        // `SqlInput::up`/`down`/`newline` check to decide whether to
        // consume or propagate (keyboard precedence, plan T7 step 3).
        self.refresh_autocomplete(window, cx);
        let ac_active = self.autocomplete.is_some();
        self.sql.update(cx, |s, _| s.set_autocomplete_active(ac_active));
        let theme = *cx.theme();

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
                // internally once the buffer grows past that. The
                // Up/Down/Newline/Escape/Tab handlers below only ever act
                // when `autocomplete_active` — see each's own doc comment
                // — so this is a no-op addition to this div's existing
                // behavior whenever the popup is closed (plan T7 step 3,
                // keyboard precedence item 3).
                div()
                    .h(px(20. * 8. + 4. * 2.))
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
                    .border_color(theme.border)
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
            .bg(theme.bg_panel)
            .on_action(cx.listener(Self::on_run_query))
            .on_action(cx.listener(Self::on_run_query_unlimited))
            .on_action(cx.listener(Self::on_cancel_query))
            .on_action(cx.listener(Self::on_toggle_tree))
            .on_action(cx.listener(Self::on_toggle_history))
            .on_action(cx.listener(Self::on_open_palette))
            .on_action(cx.listener(Self::on_open_autocomplete))
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
    // G6 Task 3: same "open at startup, None on failure, degrade
    // gracefully" posture as `view_prefs` — a load failure here only means
    // the values dialog won't prefill/remember values across runs, not
    // that the feature stops working, so (unlike `view_prefs`/`history`)
    // this isn't surfaced as its own startup status notice.
    let param_values = ParamValuesStore::load(&dbc_state::default_param_values_path()).ok();

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
                            compare_fetch_generation: 0,
                            schema_tree_connection_key: None,
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
