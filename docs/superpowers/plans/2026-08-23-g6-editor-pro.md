# G6 Editor Pro Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Tree-sitter SQL syntax highlighting, schema-driven autocomplete (keywords/tables/columns), and parametrized `:name` queries (values dialog before run, last values remembered) in the multiline SQL editor.

**Architecture:** Three semi-independent features that share `SqlInput`/`AppView` as their integration point. Each has a pure, GPUI-free core (`dbc-core/src/params.rs`'s scanner, `dbc-state/src/params.rs`'s store, `dbc-ui/src/sql_highlight.rs`'s tree-sitter query + color resolution, `dbc-ui/src/autocomplete.rs`'s candidate ranking) that is exhaustively unit-tested standalone; thin `Render`/`Context` glue in `sql_input.rs`/`main.rs`/`connections_ui.rs` wires those pure functions into the editor with the same debounce/generation and lazy-diff idioms this codebase already uses (`run_generation` in `main.rs`, `last_history_query` in `history_panel.rs`).

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`), `tree-sitter 0.25` + `tree-sitter-sequel 0.3.11` (new deps, `dbc-ui` only), TOML persistence via `dbc-state` (existing `toml`/`dirs` deps).

**Spec:** `docs/superpowers/specs/2026-08-22-gui-target-design.md` (G6 phasing row + §3 architecture constraints) and `docs/superpowers/specs/drafts/g6-editor-pro-design.md` (binding design for this phase — implement exactly what it specifies; the two CURATION blocks in it are non-negotiable, see Global Constraints). Research background: `docs/superpowers/research/2026-08-23-g6-tree-sitter-highlighting.md` (superseded in places by this plan's own verified spike findings in Task 4 — that doc's illustrative `tree_sitter::Language::new(...)` call and predicate-filtering concerns are corrected below).

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- Errors are values; no panics on DB or user-data paths. A parse error, an unresolvable alias, or a failed substitution surfaces as degraded behavior or a dialog error string — never a crash.
- `dbc-core` never sees GPUI (T1's `params.rs` is pure `std`). `dbc-ui` imports no concrete driver crate outside `connect.rs` (unaffected by this phase — no driver code changes).
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; the vendored checkout at `C:\Users\tomas\.cargo\git\checkouts\zed-a70e2ad075855582\907ed09\` is API ground truth for anything not directly verifiable by building this repo.
- UI strings are Czech (dialog labels, error messages, status notices) — English only in code/comments/tests.
- Tests green before every commit: `cargo test -p dbc-core -p dbc-state -p dbc-ui` (plus whichever other `-p` targets a given task touches) must pass with the task's new tests included. Baselines going in: `dbc-core` 13 passing, `dbc-state` 20 passing (both independently verified while writing this plan). `dbc-ui`'s count is presently in flux on this branch (mid-flight G5 Task 4 work, one pre-existing unrelated failing test in `runner::write_transaction_tests`); each G6 task must leave `dbc-ui` at least as green as it found it, plus its own new tests passing.
- **CURATION (design §1, binding):** the highlight color palette is the fixed DARK Catppuccin Mocha set given in Task 4 below — not the research doc's original light-theme draft.
- **CURATION (design §5, binding):** after `:name` substitution, T3 MUST re-run the scanner on the final SQL and refuse to dispatch (surfacing an error in the dialog) if any bare `:name` survives, for every engine — this closes a SQLite native-bind-parameter silent-NULL hole and is a hard requirement, not a nice-to-have.
- Version bump to `0.6.0` in `crates/dbc-ui/Cargo.toml` at merge (per the phasing table's `G6 → 0.6.0`; `dbc-ui` is currently `0.4.0` on this branch — confirm G5's own `0.5.0` bump has landed before bumping to `0.6.0`, don't skip a version).

### Task dependency graph (design §4)

| Task | Depends on | Notes |
|---|---|---|
| T1 `:name` scanner (dbc-core) | — | parallel batch |
| T2 `ParamValuesStore` (dbc-state) | — | parallel batch |
| T4 `sql_highlight.rs` (dbc-ui) | — | parallel batch |
| T6 `autocomplete.rs` (dbc-ui) | — | parallel batch |
| T3 Values dialog | T1, T2 | |
| T5 Wire highlighting into `SqlInput` | T4 | |
| T7 AppView autocomplete seam | T5, T6 | |

T1/T2/T4/T6 are disjoint files with no dependency edges — one parallel batch. T3 and T7 both edit `crates/dbc-ui/src/main.rs`; there is no logical dependency between them, but they will conflict textually if implemented concurrently by different workers — serialize their `main.rs` edits (same author, or rebase one onto the other) even though their task-graph dependencies don't force an order between them.

---

### Task 1 (T1): `:name` parameter scanner — `dbc-core`

**Files:**
- Create: `crates/dbc-core/src/params.rs`
- Modify: `crates/dbc-core/src/lib.rs` (add `mod params;` + `pub use params::{find_params, substitute_params};`)

**Interfaces:**
- Consumes: nothing (pure, `std`-only, no dependency on any other G6 task).
- Produces (consumed by T3):
  ```rust
  /// Distinct `:name` parameter names in `sql`, in first-occurrence order,
  /// scanning OUTSIDE single-quoted strings, double-quoted identifiers,
  /// `--` line comments, and nested `/* */` block comments. `::` (Postgres
  /// cast) and `:=` (assignment) are recognized as inert 2-char tokens and
  /// never emit a param. `None` = fail-closed ("cannot determine safety" —
  /// an unterminated string/quoted-ident/comment), same contract as
  /// `guards::tokenize`.
  pub fn find_params(sql: &str) -> Option<Vec<String>>;

  /// Same scanner as `find_params`, replacing every valid `:name`
  /// occurrence with `value(name)`'s return value (all other text copied
  /// verbatim); `None` on the same fail-closed condition. Shares one
  /// scanner implementation with `find_params` so T3's substitution can
  /// never target different positions than what `find_params` detected —
  /// two independently-written scanners over the same grammar would risk
  /// drifting apart at exactly the boundary cases (nested comments, `::`)
  /// that matter most for safety.
  pub fn substitute_params(sql: &str, value: &mut dyn FnMut(&str) -> String) -> Option<String>;
  ```

**Grounding:** `crates/dbc-core/src/guards.rs`'s `tokenize` (lines 86-191) is the scanner to mirror — same four state flags (`in_single_string`, `in_double_ident`, `in_line_comment`, `block_comment_depth: u32` for PostgreSQL-style *nesting*, not a bool) and the same escape rules: `''` inside a single-quoted string, `""` inside a double-quoted identifier, `--` runs to end of line, `/* */` nests via the depth counter. Per design §3, this is a **deliberate duplication**, not a refactor of `guards::tokenize` — that function already serves two safety-critical callers (`is_read_statement`, `apply_auto_limit`) and discards all punctuation except `;`/`=`; retrofitting position-preserving colon-detection into it risks regressing either. This mirrors the codebase's own precedent of small purpose-built scanners over shared abstractions (`history_panel.rs`'s `collapse_sql` is a documented deliberate copy of `tabs::collapse_title`'s logic).

Outside the four "in a construct" states: a `:` immediately followed by `[A-Za-z_][A-Za-z0-9_]*` is a parameter (name = the identifier, not including the leading `:`); a `:` immediately followed by another `:` is a 2-char inert `::` token (consume both, no param); a `:` immediately followed by `=` is a 2-char inert `:=` token (consume both, no param); any other bare `:` (e.g. followed by a digit, whitespace, or nothing) is not a parameter and is simply not special — carry on scanning. `substitute_params`'s replacement only touches recognized `:name` occurrences; `::`/`:=` and non-param colons pass through to the output unchanged, byte-for-byte, like every other character outside a construct.

- [ ] **Step 1: Write the failing tests** (`crates/dbc-core/src/params.rs`, `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_simple_params_in_order() {
        assert_eq!(
            find_params("SELECT * FROM t WHERE id = :id AND name = :name"),
            Some(vec!["id".to_string(), "name".to_string()])
        );
    }

    #[test]
    fn distinct_names_first_occurrence_order() {
        assert_eq!(
            find_params("WHERE a = :x OR b = :y OR c = :x"),
            Some(vec!["x".to_string(), "y".to_string()])
        );
    }

    #[test]
    fn no_params_is_some_empty_not_none() {
        assert_eq!(find_params("SELECT 1"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_single_quoted_string() {
        assert_eq!(find_params("SELECT ':id' FROM t"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_double_quoted_ident() {
        assert_eq!(find_params("SELECT \"a:b\" FROM t"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_line_comment() {
        assert_eq!(find_params("-- :id\nSELECT 1"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_nested_block_comment() {
        // Outer /* opens depth 1, inner /* depth 2, first */ drops to
        // depth 1 (still commented — PostgreSQL nesting semantics, same
        // as guards::tokenize), final */ drops to depth 0; only the
        // trailing `:c` is live.
        assert_eq!(
            find_params("/* :a /* :b */ still commented */ SELECT :c"),
            Some(vec!["c".to_string()])
        );
    }

    #[test]
    fn double_colon_cast_is_not_a_param() {
        assert_eq!(find_params("SELECT x::int FROM t"), Some(vec![]));
    }

    #[test]
    fn walrus_assignment_is_not_a_param() {
        assert_eq!(find_params("DO $$ BEGIN a := 1; END $$"), Some(vec![]));
    }

    #[test]
    fn colon_not_followed_by_identifier_start_is_not_a_param() {
        // A bare `:` followed by a digit, space, or end-of-string is never
        // a valid parameter name — just an ordinary character.
        assert_eq!(find_params("LIMIT :1"), Some(vec![]));
        assert_eq!(find_params("a : b"), Some(vec![]));
        assert_eq!(find_params("trailing:"), Some(vec![]));
    }

    #[test]
    fn names_are_case_sensitive_and_distinct() {
        assert_eq!(
            find_params(":Id = :id"),
            Some(vec!["Id".to_string(), "id".to_string()])
        );
    }

    #[test]
    fn unterminated_single_string_fails_closed() {
        assert_eq!(find_params("SELECT ':id"), None);
    }

    #[test]
    fn unterminated_double_ident_fails_closed() {
        assert_eq!(find_params("SELECT \"a"), None);
    }

    #[test]
    fn unterminated_block_comment_fails_closed() {
        assert_eq!(find_params("SELECT 1 /* :id"), None);
    }

    #[test]
    fn unterminated_nested_block_comment_fails_closed() {
        assert_eq!(find_params("/* /* :id */ SELECT 1"), None);
    }

    // --- substitute_params ---

    #[test]
    fn substitute_replaces_every_occurrence() {
        let out = substitute_params("WHERE a = :x OR b = :x", &mut |name| {
            assert_eq!(name, "x");
            "'lit'".to_string()
        });
        assert_eq!(out, Some("WHERE a = 'lit' OR b = 'lit'".to_string()));
    }

    #[test]
    fn substitute_leaves_double_colon_and_walrus_untouched() {
        let out = substitute_params("x::int := :v", &mut |name| {
            assert_eq!(name, "v");
            "1".to_string()
        });
        assert_eq!(out, Some("x::int := 1".to_string()));
    }

    #[test]
    fn substitute_skips_strings_and_comments_like_find_params() {
        let out = substitute_params("SELECT ':id', :id -- :id\n", &mut |name| {
            assert_eq!(name, "id");
            "5".to_string()
        });
        assert_eq!(out, Some("SELECT ':id', 5 -- :id\n".to_string()));
    }

    #[test]
    fn substitute_fails_closed_on_unterminated_construct() {
        let out = substitute_params("SELECT ':id", &mut |_| "5".to_string());
        assert_eq!(out, None);
    }
}
```

- [ ] **Step 2: Run to see the tests fail (module doesn't exist yet)**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core params::`
Expected: compile error (`params` module/functions don't exist).

- [ ] **Step 3: Implement `find_params` and `substitute_params`**, sharing one internal scanner (e.g. a private `enum ScanEvent { Literal(char), Param(String) }` iterator, or a closure-driven walk — implementer's choice, as long as both public functions provably run the same state machine over the same input).

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core`
Expected: all tests pass (13 pre-existing + this task's new ones), zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-core/src/params.rs crates/dbc-core/src/lib.rs
git commit -m "feat: :name parameter scanner (find_params/substitute_params)"
```

---

### Task 2 (T2): `ParamValuesStore` — `dbc-state`

**Files:**
- Create: `crates/dbc-state/src/params.rs`
- Modify: `crates/dbc-state/src/lib.rs` (add `mod params;` + `pub use params::{default_param_values_path, ParamValue, ParamValuesStore};`)

**Interfaces:**
- Consumes: nothing (pure persistence, no dependency on any other G6 task).
- Produces (consumed by T3):
  ```rust
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
  pub struct ParamValue {
      pub text: String,
      pub is_null: bool,
  }

  pub struct ParamValuesStore { /* path + HashMap<String, ParamValue> */ }

  impl ParamValuesStore {
      pub fn load(path: &Path) -> Result<ParamValuesStore, StateError>;
      pub fn get(&self, connection_id: &str, name: &str) -> Option<&ParamValue>;
      pub fn set(&mut self, connection_id: &str, name: &str, value: ParamValue) -> Result<(), StateError>;
  }

  /// `dbc/params.toml` alongside `dbc/views.toml`.
  pub fn default_param_values_path() -> PathBuf;
  ```

**Grounding:** `crates/dbc-state/src/view_prefs.rs` (full file) is the shape to mirror almost exactly: `load` (missing file → empty store; corrupt file → `Err` via `toml::from_str`'s `?`-propagated `StateError`), atomic `save` (write to `path.with_extension("toml.tmp")`, `sync_all()`, then `fs::rename` over the real path), and `encode_key` using the unit separator `\u{1F}` to avoid collisions with dots/other chars in connection ids or param names. The only structural difference: `view_prefs.rs`'s key is 3 parts (`connection_id, schema, table`); this store's key is 2 parts (`connection_id, name`) — per design §3, keyed by **(connection_id, param name)**, not by query text (query text churns on every edit; a param name is the stable semantic handle the user assigns).

```rust
fn encode_key(connection_id: &str, name: &str) -> String {
    format!("{}\u{1F}{}", connection_id, name)
}
```

- [ ] **Step 1: Write the failing tests** (mirror `view_prefs.rs`'s three tests exactly, 2-part key):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_set_save_load_get() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");

        let mut store = ParamValuesStore::load(&p).unwrap();
        let value = ParamValue { text: "42".to_string(), is_null: false };
        store.set("conn1", "id", value.clone()).unwrap();

        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn1", "id"), Some(&value));
    }

    #[test]
    fn missing_file_creates_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope.toml");
        let store = ParamValuesStore::load(&p).unwrap();
        assert_eq!(store.get("any", "name"), None);
    }

    #[test]
    fn key_collision_safety() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");
        let mut store = ParamValuesStore::load(&p).unwrap();

        // "conn" + "a:b" vs "conn:a" + "b" must not collide via naive
        // concatenation (the unit-separator encode_key prevents this,
        // same guard as view_prefs.rs's own key_collision_safety test).
        let v1 = ParamValue { text: "one".to_string(), is_null: false };
        let v2 = ParamValue { text: "two".to_string(), is_null: false };
        store.set("conn", "a:b", v1.clone()).unwrap();
        store.set("conn:a", "b", v2.clone()).unwrap();

        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn", "a:b"), Some(&v1));
        assert_eq!(loaded.get("conn:a", "b"), Some(&v2));
    }

    #[test]
    fn null_flag_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");
        let mut store = ParamValuesStore::load(&p).unwrap();
        let value = ParamValue { text: String::new(), is_null: true };
        store.set("conn1", "note", value.clone()).unwrap();

        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn1", "note"), Some(&value));
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state params::`
Expected: compile error (module doesn't exist).

- [ ] **Step 3: Implement** `ParamValue`, `ParamValuesStore::{load,get,set,save}`, `encode_key`, `default_param_values_path` (mirror `view_prefs.rs`'s `default_view_prefs_path` — `dirs::config_dir()...join("dbc").join("params.toml")`).

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state`
Expected: all tests pass (20 pre-existing + this task's new ones), zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-state/src/params.rs crates/dbc-state/src/lib.rs
git commit -m "feat: ParamValuesStore for remembered :name values"
```

---

### Task 3 (T3): Values dialog + run interception — `dbc-ui`

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (new `ModalState::QueryParams` variant; render helper)
- Modify: `crates/dbc-ui/src/main.rs` (interception in `run_query`; `AppView::param_values: Option<ParamValuesStore>` field + startup wiring mirroring `view_prefs`; `open_query_params_dialog`/`confirm_query_params`/`cancel_query_params` handlers; a new pure `build_param_sql` helper + its tests)

**Interfaces:**
- Consumes: `dbc_core::{find_params, substitute_params}` (T1), `dbc_state::{ParamValue, ParamValuesStore, default_param_values_path}` (T2), `sandbox::sql_value` (existing, G5 — `crates/dbc-ui/src/sandbox.rs:174`, signature `pub fn sql_value(v: Option<&str>, numeric: bool) -> String`), `connections_ui::TextField` (existing widget, `crates/dbc-ui/src/connections_ui.rs:223`).
- Produces: nothing consumed by later G6 tasks (T3 is a leaf in the dependency graph) — but its `run_query` interception point and `ModalState::QueryParams` variant must not be touched by T7's `main.rs` edits without a rebase (see the file-contention caveat in the dependency graph above).

**Grounding — where `ModalState` actually lives:** despite `main.rs` owning the `modal: Option<connections_ui::ModalState>` field (`main.rs:394`) and matching on it throughout its render, the `ModalState` enum itself is defined in `crates/dbc-ui/src/connections_ui.rs:892-905`, alongside `PendingAfterUnlock` (`connections_ui.rs:886-889`). Add the new variant there:

```rust
#[derive(Clone)]
pub enum ModalState {
    ConnectionDialog(ConnectionDialogUi),
    MasterPasswordPrompt { /* ...unchanged... */ },
    CreateMasterPassword { /* ...unchanged... */ },
    QueryParams {
        names: Vec<String>,
        inputs: Vec<Entity<TextField>>,
        null_flags: Vec<bool>,
        sql_template: String,
        bypass_auto_limit: bool,
        error: Option<String>,
    },
}
```

**Grounding — interception point:** `main.rs:499-505`'s `run_query` is the single call site behind all three existing triggers — `on_run_query` (`main.rs:473-475`), `on_run_query_unlimited` (`main.rs:480-487`), and the palette's `PaletteAction::RunQuery` (`main.rs:1212`) all funnel through it before ever reaching `run_query_with`. Intercepting inside `run_query` itself (rather than duplicating the check at each of those three call sites, as the design's looser phrasing "interception point is `run_query`/`on_run_query`" might suggest) covers all three with one change:

```rust
fn run_query(&mut self, bypass_auto_limit: bool, cx: &mut Context<Self>) {
    let sql = self.sql.read(cx).text();
    if sql.trim().is_empty() {
        return;
    }
    match dbc_core::find_params(&sql) {
        Some(names) if !names.is_empty() => {
            self.open_query_params_dialog(sql, names, bypass_auto_limit, cx);
        }
        // Some(empty) or None (fail-closed scan failure) — proceed exactly
        // as today, no behavior change (design §3 "Values dialog UX").
        _ => self.run_query_with(sql, None, bypass_auto_limit, cx),
    }
}
```

`open_query_params_dialog` builds one `Entity<TextField>` per distinct name, prefilled from `self.param_values` (keyed by `self.active_connection_id.clone().unwrap_or_else(|| "cli".to_string())` — the SAME `"cli"` sentinel `history_panel.rs:126-133`'s `active_connection_name_for_history` uses for the CLI-arg back-compat path, though here it's the connection **id**, not name, being stored — `self.active_connection_id` already holds the id directly, no lookup needed):

```rust
fn open_query_params_dialog(
    &mut self,
    sql: String,
    names: Vec<String>,
    bypass_auto_limit: bool,
    cx: &mut Context<Self>,
) {
    if self.modal.is_some() {
        return;
    }
    let conn_id = self.active_connection_id.clone().unwrap_or_else(|| "cli".to_string());
    let mut inputs = Vec::with_capacity(names.len());
    let mut null_flags = Vec::with_capacity(names.len());
    for name in &names {
        let stored = self.param_values.as_ref().and_then(|s| s.get(&conn_id, name));
        let prefill = stored.filter(|v| !v.is_null).map(|v| v.text.clone()).unwrap_or_default();
        null_flags.push(stored.map(|v| v.is_null).unwrap_or(false));
        inputs.push(cx.new(|cx| {
            let mut f = connections_ui::TextField::new(cx, "", false);
            f.set_text(&prefill, cx);
            f
        }));
    }
    self.modal = Some(connections_ui::ModalState::QueryParams {
        names,
        inputs,
        null_flags,
        sql_template: sql,
        bypass_auto_limit,
        error: None,
    });
    cx.notify();
}
```

**Pure substitution + mandatory rescan** — a new free function in `main.rs` (alongside `preview_sql`/`fk_info_from_table`, same "pure helper + its own `#[cfg(test)] mod` further down the file" convention this file already uses), so the CURATION-mandated rescan is unit-testable without a GPUI window:

```rust
/// Substitutes `sql_template`'s `:name` params (via `sandbox::sql_value`,
/// `numeric = true` for opportunistic unquoting per design §3) using
/// `values` (same order as `names`; `(text, is_null)` per entry), then
/// re-scans the result and refuses if any bare `:name` survives — the
/// CURATION-mandated defense (design §5) against SQLite's own native
/// `:name`/`@name`/`$name` bind-parameter syntax silently binding NULL to
/// an undetected/un-substituted parameter, for every engine.
fn build_param_sql(
    sql_template: &str,
    names: &[String],
    values: &[(String, bool)],
) -> Result<String, String> {
    let lookup: std::collections::HashMap<&str, &(String, bool)> =
        names.iter().map(String::as_str).zip(values.iter()).collect();
    let substituted = dbc_core::substitute_params(sql_template, &mut |name| match lookup.get(name) {
        Some((_, true)) => sandbox::sql_value(None, true),
        Some((text, false)) => sandbox::sql_value(Some(text.as_str()), true),
        None => sandbox::sql_value(None, true), // unreachable: every :name in sql_template is in `names`
    })
    .ok_or_else(|| "nepodařilo se sestavit SQL".to_string())?;

    match dbc_core::find_params(&substituted) {
        Some(remaining) if !remaining.is_empty() => {
            Err("po dosazení hodnot zůstal v SQL neplatný parametr — spuštění zrušeno".to_string())
        }
        _ => Ok(substituted),
    }
}
```

`confirm_query_params` (Enter in the last field or the "Spustit" click) reads every input's live text + its `null_flags` entry, calls `build_param_sql`; on `Ok`, persists every value to `self.param_values` (best-effort — an `Err` from `store.set` degrades silently, same posture as `view_prefs`'s own callers), closes the modal, and calls `self.run_query_with(final_sql, None, bypass_auto_limit, cx)`; on `Err`, sets the modal's `error` field (shown in the dialog) and does NOT close the modal, run anything, or persist — matching "Esc cancels — no run, no persistence write" (persistence only happens in the `Ok` branch, on Confirm). The live substituted-SQL preview line (design §3) recomputes `build_param_sql` read-only on every render pass from the inputs' current text (cheap at interactive SQL sizes, same posture as T4/T6's per-keystroke re-scans) and just displays its `Ok`/`Err` string — it does not gate typing.

**Startup wiring** (in `main`, alongside the existing `ViewPrefsStore::load` call — same "open at startup, `None` on failure, degrade gracefully, feature works without persistence" posture as `view_prefs`):

```rust
let param_values = ParamValuesStore::load(&dbc_state::default_param_values_path()).ok();
```

- [ ] **Step 1: Write the failing tests** for `build_param_sql` (pure, no GPUI — place in `main.rs`'s own `#[cfg(test)] mod query_params_tests`, matching the file's existing per-feature test-module convention):

```rust
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
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui query_params_tests::`
Expected: compile error (`build_param_sql` doesn't exist).

- [ ] **Step 3: Implement** `ModalState::QueryParams`, `build_param_sql`, `open_query_params_dialog`, `confirm_query_params`, `cancel_query_params` (closes modal, no persistence — reuse the existing generic "Esc closes any modal" handler if `main.rs` already has one; otherwise add a `QueryParams`-specific arm), the `run_query` interception, `AppView::param_values` field + startup load, and the dialog's render (one row per name: label + `TextField` + "NULL" checkbox — same visual idiom as G5's cell editor, `grid.rs:2006-2079`'s `Uložit`/`NULL`/`Zrušit` — plus the live preview line and an error line when `modal`'s `error` is `Some`). Czech labels: "Spustit", "Zrušit", "NULL".

- [ ] **Step 4: Run to green + zero warnings + a sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all tests pass, zero warnings. Manually launch the app against the SQLite fixture, type `SELECT * FROM t WHERE id = :id`, press Ctrl+Enter, confirm the dialog opens, fill a value, confirm it runs and the value is remembered next time.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/main.rs
git commit -m "feat: parametrized :name query values dialog"
```

---

### Task 4 (T4): `sql_highlight.rs` — tree-sitter-sequel wiring

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml` (add `tree-sitter = "0.25"`, `tree-sitter-sequel = "0.3"`)
- Create: `crates/dbc-ui/src/sql_highlight.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `mod sql_highlight;`)

**Interfaces:**
- Consumes: nothing (pure — parses raw `&str`; the only GPUI type it touches is `gpui::Hsla` for the resolved color, per design §1's "parsing itself has no GPUI dependency, but `Hsla` color resolution does").
- Produces (consumed by T5, T7):
  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub struct HighlightSpan {
      pub range: std::ops::Range<usize>,
      pub color: gpui::Hsla,
      /// True for `string`/`comment` captures — doubles as T6/T7's
      /// autocomplete-suppression mask (design §2's trigger model: "cursor
      /// is not inside a string/comment span", reusing this module's
      /// already-computed spans rather than a second scanner).
      pub suppresses_completion: bool,
  }

  /// Full-buffer parse + highlights query. Infallible in this codebase's
  /// usage: `tree_sitter::Parser::parse` only returns `None` on an
  /// explicit cancellation flag this code never sets, so every call
  /// returns real (possibly empty, possibly partially-degraded) spans —
  /// never panics, even on T-SQL-only syntax or an unterminated comment.
  pub fn highlight(text: &str) -> Vec<HighlightSpan>;
  ```

**Step 1 — the mandated API spike (design §5's #1 risk item, resolved here):** a throwaway example is not enough on its own to ground the plan, so this plan already ran the spike (building a standalone crate against `tree-sitter = "0.25"` + `tree-sitter-sequel = "0.3"`, both of which resolved to `0.25.10`/`0.3.11` from crates.io and compiled cleanly with no native-toolchain issues on this machine) and is reporting the verified findings so the implementer reproduces the same code, not the research doc's unverified sketch:

1. **Language construction — the research doc's sketch is wrong.** `tree_sitter_sequel::LANGUAGE` is a `tree_sitter_language::LanguageFn`, not a `tree_sitter::Language`; there is no `Language::new(LanguageFn)`. The crate's own doctest (`bindings/rust/lib.rs:1-16`) shows the correct conversion:
   ```rust
   let language: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
   parser.set_language(&language).expect("load grammar");
   ```
2. **`HIGHLIGHTS_QUERY`'s bundled numeric predicates are broken and must not be used as-is.** The upstream `queries/highlights.scm` (from the extracted `tree-sitter-sequel-0.3.11` package) captures numbers via `((literal) @number (#match? @number "^[-+]?%d+$"))` — `%d` is a **Lua pattern**, not a regex; tree-sitter's Rust binding evaluates `#match?` with the `regex` crate, where `%d` matches nothing useful. Empirically confirmed: parsing `SELECT 3.14, 42, 'text' FROM t` against the unmodified bundled query captures `3.14` and `42` **only** as `"string"` — the `"number"`/`"float"` captures never fire. Since design §1's palette gives `number` its own peach color, this plan vendors a **local, trimmed `HIGHLIGHTS_SCM` constant** in `sql_highlight.rs` (not `tree_sitter_sequel::HIGHLIGHTS_QUERY` verbatim) — exactly the fallback the research doc already anticipated ("a locally-vendored trimmed copy for control over capture names"), fixing only the two numeric regexes to real regex syntax and keeping every other pattern's node names verbatim from the upstream file (already confirmed to compile as-is):
   ```scm
   (literal) @string

   ((literal) @number
     (#match? @number "^[-+]?[0-9]+$"))

   ((literal) @number
     (#match? @number "^[-+]?[0-9]*\.[0-9]+$"))

   (comment) @comment
   (marginalia) @comment

   (invocation
     (object_reference
       name: (identifier) @function.call))

   (object_reference
     name: (identifier) @type)

   [
     (keyword_select) (keyword_from) (keyword_where) (keyword_join) (keyword_on)
     (keyword_left) (keyword_right) (keyword_outer) (keyword_inner) (keyword_full)
     (keyword_group) (keyword_order) (keyword_by) (keyword_having) (keyword_limit)
     (keyword_offset) (keyword_insert) (keyword_into) (keyword_values) (keyword_update)
     (keyword_set) (keyword_delete) (keyword_and) (keyword_or) (keyword_not)
     (keyword_null) (keyword_is) (keyword_in) (keyword_like) (keyword_between)
     (keyword_as) (keyword_distinct) (keyword_case) (keyword_when) (keyword_then)
     (keyword_else) (keyword_end) (keyword_union) (keyword_create) (keyword_table)
     (keyword_alter) (keyword_drop) (keyword_index) (keyword_primary) (keyword_key)
     (keyword_foreign) (keyword_references) (keyword_view) (keyword_with)
   ] @keyword

   [
     (keyword_int) (keyword_smallint) (keyword_bigint) (keyword_tinyint)
     (keyword_decimal) (keyword_numeric) (keyword_float) (keyword_double)
     (keyword_real) (keyword_char) (keyword_varchar) (keyword_nvarchar)
     (keyword_text) (keyword_string) (keyword_boolean) (keyword_date)
     (keyword_datetime) (keyword_timestamp) (keyword_uuid) (keyword_json)
     (keyword_jsonb)
   ] @type.builtin
   ```
3. **Real capture names emitted are NOT the design's assumed set.** The design's palette lists `keyword`/`string`/`number`/`comment`/`function`/`function.builtin`/`type`; the grammar (confirmed by the spike, both against the upstream file and the vendored trimmed copy above) actually emits `keyword`, `string`, `number`, `comment`, `function.call` (never bare `function` or `function.builtin`), `type`, `type.builtin`, plus several this design's palette doesn't style (`field`, `variable`, `operator`, `spell`, `punctuation.*`, `attribute`, etc. — all correctly fall through to the default color, no change needed there). `color_for_capture` must match on the REAL names:
   ```rust
   fn color_for_capture(name: &str) -> Option<(u8, gpui::Hsla)> {
       // (priority, color) — priority resolves same-range capture
       // collisions (see point 5 below); higher wins.
       match name {
           "keyword" => Some((1, gpui::rgb(0xcba6f7).into())),        // mauve
           "string" => Some((1, gpui::rgb(0xa6e3a1).into())),         // green
           "comment" => Some((1, gpui::rgb(0x6c7086).into())),        // overlay gray
           "type" | "type.builtin" => Some((1, gpui::rgb(0x94e2d5).into())), // teal
           "number" => Some((2, gpui::rgb(0xfab387).into())),         // peach — outranks "string"
           "function.call" => Some((2, gpui::rgb(0x89b4fa).into())),  // blue — outranks "type"
           _ => None,
       }
   }
   ```
4. **Predicates ARE auto-applied — no extra filtering code needed.** `tree_sitter::QueryMatches`' `StreamingIterator::advance` (confirmed in `binding_rust/lib.rs:3444-3460` of the vendored `tree-sitter-0.25.10` source) already calls `QueryMatch::satisfies_text_predicates` internally before yielding a match, so `#match?` correctly gates whether `@number` fires. Iteration is the research doc's original shape, gated by `use tree_sitter::StreamingIterator;`:
   ```rust
   let mut cursor = tree_sitter::QueryCursor::new();
   let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
   while let Some(m) = matches.next() {
       for cap in m.captures { /* ... */ }
   }
   ```
5. **A single node can legitimately satisfy two patterns at once** (e.g. a numeric literal gets both the unconditional `@string` pattern AND the predicate-gated `@number` pattern; `COUNT(...)` gets both `@function.call` and the generic `@type` object-reference pattern) — confirmed empirically (both captures appear, same byte range, in the spike's output). `highlight()` must resolve same-range collisions by the `(priority, color)` pairs above, not by iteration order (tree-sitter's same-node multi-pattern ordering is not a documented/stable contract to rely on):
   ```rust
   pub fn highlight(text: &str) -> Vec<HighlightSpan> {
       let mut parser = tree_sitter::Parser::new();
       let language: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
       parser.set_language(&language).expect("load grammar"); // grammar embedded at compile time, cannot fail at runtime
       let Some(tree) = parser.parse(text, None) else { return Vec::new() }; // cancellation only, never hit here
       let query = tree_sitter::Query::new(&language, HIGHLIGHTS_SCM).expect("vendored query must compile");
       let mut cursor = tree_sitter::QueryCursor::new();
       let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
       // (range, priority, color) — Vec, not HashMap: buffer sizes are
       // small (interactive SQL) and preserving a stable order simplifies
       // testing; O(n) linear scan per capture is fine at this scale.
       let mut spans: Vec<(std::ops::Range<usize>, u8, gpui::Hsla)> = Vec::new();
       while let Some(m) = matches.next() {
           for cap in m.captures {
               let name = query.capture_names()[cap.index as usize];
               let Some((priority, color)) = color_for_capture(name) else { continue };
               let range = cap.node.byte_range();
               if let Some(existing) = spans.iter_mut().find(|(r, _, _)| *r == range) {
                   if priority >= existing.1 {
                       *existing = (range, priority, color);
                   }
               } else {
                   spans.push((range, priority, color));
               }
           }
       }
       let string_or_comment: std::collections::HashSet<std::ops::Range<usize>> = {
           let mut cursor2 = tree_sitter::QueryCursor::new();
           let mut matches2 = cursor2.matches(&query, tree.root_node(), text.as_bytes());
           let mut set = std::collections::HashSet::new();
           while let Some(m) = matches2.next() {
               for cap in m.captures {
                   let name = query.capture_names()[cap.index as usize];
                   if name == "string" || name == "comment" {
                       set.insert(cap.node.byte_range());
                   }
               }
           }
           set
       };
       spans
           .into_iter()
           .map(|(range, _, color)| {
               let suppresses = string_or_comment.contains(&range);
               HighlightSpan { range, color, suppresses_completion: suppresses }
           })
           .collect()
   }
   ```
6. **Error-node degradation confirmed non-fatal but not perfectly local.** `SELECT TOP 10 * FROM users` (T-SQL-only syntax against the generic grammar) parses with `tree.root_node().has_error() == true`, and `FROM`/`users` lose their normal `keyword`/`type` captures too (not just `TOP` itself) — the ERROR node's recovery has more collateral than "only the offending token", but `SELECT` and `*` still resolve correctly and nothing panics. `SELECT 1 /* unterminated` similarly still highlights `SELECT`/`1` (the `/` `*` split into two bare `operator` captures — uncolored, falls through) with no panic. Write the T4 degradation tests against this ACTUAL behavior (assert `SELECT` is still colored + no panic), not an idealized "only TOP is uncolored" claim.

- [ ] **Step 2: Add the dependencies**

```bash
# In crates/dbc-ui/Cargo.toml [dependencies]:
#   tree-sitter = "0.25"
#   tree-sitter-sequel = "0.3"
```

Run: `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
Expected: builds clean (both crates already confirmed to compile in this plan's spike).

- [ ] **Step 3: Write the failing tests** (`crates/dbc-ui/src/sql_highlight.rs`, `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn color_at(spans: &[HighlightSpan], byte: usize) -> Option<gpui::Hsla> {
        spans.iter().find(|s| s.range.contains(&byte)).map(|s| s.color)
    }

    #[test]
    fn keyword_gets_keyword_color() {
        let spans = highlight("SELECT 1");
        let select_color = color_at(&spans, 0);
        assert!(select_color.is_some());
    }

    #[test]
    fn string_gets_string_color() {
        let spans = highlight("SELECT 'x' FROM t");
        assert!(color_at(&spans, 7).is_some()); // inside 'x'
    }

    #[test]
    fn numeric_literal_prefers_number_color_over_string_color() {
        let sql = "SELECT 42 FROM t";
        let spans = highlight(sql);
        let number_color = color_at(&spans, 7).unwrap(); // "42"
        let string_only = color_at(&highlight("SELECT 'x' FROM t"), 7).unwrap();
        assert_ne!(number_color, string_only);
    }

    #[test]
    fn function_call_prefers_function_color_over_type_color() {
        let spans = highlight("SELECT COUNT(x) FROM t");
        let count_color = color_at(&spans, 7).unwrap(); // "COUNT"
        let bare_table_color = color_at(&highlight("SELECT 1 FROM t"), 14).unwrap(); // "t"
        assert_ne!(count_color, bare_table_color);
    }

    #[test]
    fn line_comment_gets_comment_color_and_suppresses_completion() {
        let spans = highlight("SELECT 1 -- a note");
        let comment_span = spans.iter().find(|s| s.range.contains(&11)).unwrap();
        assert!(comment_span.suppresses_completion);
    }

    #[test]
    fn string_span_suppresses_completion_keyword_span_does_not() {
        let spans = highlight("SELECT 'x' FROM t");
        let string_span = spans.iter().find(|s| s.range.contains(&7)).unwrap();
        assert!(string_span.suppresses_completion);
        let keyword_span = spans.iter().find(|s| s.range.contains(&0)).unwrap();
        assert!(!keyword_span.suppresses_completion);
    }

    #[test]
    fn tsql_only_syntax_degrades_without_panicking_and_keeps_partial_highlighting() {
        // T-SQL's TOP against the generic grammar produces an ERROR node;
        // must not panic, and SELECT itself must still be colored.
        let spans = highlight("SELECT TOP 10 * FROM users");
        assert!(color_at(&spans, 0).is_some());
    }

    #[test]
    fn unterminated_block_comment_does_not_panic_and_keeps_partial_highlighting() {
        let spans = highlight("SELECT 1 /* unterminated");
        assert!(color_at(&spans, 0).is_some());
    }

    #[test]
    fn empty_text_returns_no_spans_without_panicking() {
        assert_eq!(highlight(""), Vec::new());
    }
}
```

- [ ] **Step 4: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui sql_highlight::`
Expected: compile error (module doesn't exist).

- [ ] **Step 5: Implement** `HIGHLIGHTS_SCM`, `color_for_capture`, `highlight` exactly per steps 1-6 above.

- [ ] **Step 6: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui sql_highlight::`
Expected: all pass, zero warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/Cargo.toml crates/dbc-ui/src/sql_highlight.rs crates/dbc-ui/src/main.rs
git commit -m "feat: tree-sitter SQL highlighting (sql_highlight.rs)"
```

---

### Task 5 (T5): Wire highlighting into `SqlInput`

**Files:**
- Modify: `crates/dbc-ui/src/sql_input.rs`

**Interfaces:**
- Consumes: `sql_highlight::{highlight, HighlightSpan}` (T4).
- Produces (consumed by T7):
  ```rust
  impl SqlInput {
      /// Current cursor byte offset — new in G6. The header comment's
      /// "frozen public surface" note (top of the file) scoped that freeze
      /// to `text_model.rs`'s API for G1 Task 4, not to `SqlInput` itself
      /// (design §2's "Seam" section confirms this reading).
      pub fn cursor(&self) -> usize;

      /// Screen-space pixel bounds of a single-point caret at the CURRENT
      /// cursor, from last frame's cached line layout. `None` before the
      /// first paint or if the cursor's line isn't in the cache (e.g.
      /// scrolled out of view) — callers degrade by not showing a popup
      /// that frame.
      pub fn cursor_screen_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>>;

      /// AppView-driven: true while T7's autocomplete popup is open. Makes
      /// `up`/`down`/`newline` no-op (propagating instead of consuming) so
      /// AppView's own higher-priority handler can do popup nav/accept.
      pub fn set_autocomplete_active(&mut self, active: bool);
  }
  ```

**Grounding — mutating call sites** (the exhaustive list, confirmed by reading the whole file — every one already sets `self.follow_cursor = true`, per design §1's "same call sites"): `newline` (`sql_input.rs:403-408`), `backspace` (410-415), `delete` (417-422), `paste` (469-476), `cut` (488-500), `set_text` (260-266), `EntityInputHandler::replace_text_in_range` (582-600, the IME/typed-character path — GPUI routes actual keystroke text entry through here, there is no separate `insert` action), `EntityInputHandler::replace_and_mark_text_in_range` (602-634, IME composition). Each of these eight sites gets one added call, e.g.:

```rust
fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
    self.buffer.backspace();
    self.marked_range = None;
    self.follow_cursor = true;
    self.kick_highlight(cx);
    cx.notify();
}
```

**Debounce** (design §1, exact `run_generation`-style idiom copied from `main.rs:591-592`/`973`, confirmed against the pinned GPUI rev: `BackgroundExecutor::timer(Duration) -> Task<()>` exists at `crates/gpui/src/executor.rs:187`; `background_spawn` exists on `Context`/`AsyncApp` at `crates/gpui/src/app/async_context.rs:127` and `crates/gpui/src/app/context.rs:857`; every `cx.spawn(...)` call in this codebase ends with `.detach()`, confirmed at `main.rs:1072/1775/2135/2198` — detaching is safe here specifically BECAUSE coalescing is done via the generation compare on write-back, not by cancelling the previous task):

```rust
fn kick_highlight(&mut self, cx: &mut Context<Self>) {
    self.highlight_generation += 1;
    let my_generation = self.highlight_generation;
    let text = self.buffer.text().to_string();
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(std::time::Duration::from_millis(60)).await;
        let spans = cx.background_spawn(async move { crate::sql_highlight::highlight(&text) }).await;
        this.update(cx, |this, cx| {
            if this.highlight_generation == my_generation {
                this.highlights = spans;
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}
```

New `SqlInput` fields (after `is_selecting: bool` at `sql_input.rs:233`): `highlights: Vec<sql_highlight::HighlightSpan>` (init `Vec::new()` in `new`), `highlight_generation: u32` (init `0`), `autocomplete_active: bool` (init `false`). `set_text` (line 260) also calls `self.kick_highlight(cx)` — it already takes `cx: &mut Context<Self>`, no signature change needed.

**`build_runs` generalization** (design §1 "Render integration" — `sql_input.rs:173-205` today handles 0-1 marked sub-ranges; must handle N colored sub-ranges AND the marked range as independent, mergeable dimensions):

```rust
/// Builds one `TextRun` per contiguous coloring segment of a line's
/// `display_len` bytes. `highlight_local` (line-local byte ranges with a
/// resolved color, already clipped/translated to this line's coordinate
/// space by the caller) supplies the base color per segment, falling back
/// to `run.color` where uncovered; `marked_local` (the IME marked range,
/// same clipping) ORs an underline on top of whichever segment it
/// overlaps — color and underline are independent, both apply together
/// where they coincide (design §1: "a marked IME composition over a
/// keyword must show both the keyword's color AND the underline").
fn build_runs(
    run: &TextRun,
    display_len: usize,
    highlight_local: &[(Range<usize>, gpui::Hsla)],
    marked_local: Option<Range<usize>>,
) -> Vec<TextRun> {
    let mut points: Vec<usize> = vec![0, display_len];
    for (r, _) in highlight_local {
        points.push(r.start.min(display_len));
        points.push(r.end.min(display_len));
    }
    if let Some(mr) = &marked_local {
        points.push(mr.start.min(display_len));
        points.push(mr.end.min(display_len));
    }
    points.sort_unstable();
    points.dedup();

    let mut runs = Vec::with_capacity(points.len().saturating_sub(1));
    for w in points.windows(2) {
        let (start, end) = (w[0], w[1]);
        if start >= end {
            continue;
        }
        let color = highlight_local
            .iter()
            .find(|(r, _)| r.start <= start && end <= r.end)
            .map(|(_, c)| *c)
            .unwrap_or(run.color);
        let underline = marked_local.as_ref().and_then(|mr| {
            (mr.start <= start && end <= mr.end).then(|| UnderlineStyle {
                color: Some(run.color),
                thickness: px(1.0),
                wavy: false,
            })
        });
        runs.push(TextRun { len: end - start, color, underline, ..run.clone() });
    }
    if runs.is_empty() {
        runs.push(TextRun { len: display_len, ..run.clone() });
    }
    runs
}
```

Call site (`TextElement::prepaint`, `sql_input.rs`'s per-line loop around lines 792-834): after computing `marked_local` (unchanged, lines 817-833), compute `highlight_local` the same way — filter `input.highlights` (read once before the loop, alongside `text`/`selection`/`cursor`/`marked_range` at the top of `prepaint`, lines 733-740) to spans overlapping `[line_start, line_end)`, clip and translate to line-local coordinates — then call the new `build_runs(&run, display_len, &highlight_local, marked_local)` in place of the old 2-arg call at line 834.

**`cursor_screen_bounds`** mirrors `EntityInputHandler::bounds_for_range`'s existing logic (`sql_input.rs:636-660`) and `offset_for_position`'s cached-line lookup (`309-335`), specialized to a single point at the live cursor rather than an arbitrary UTF-16 range:

```rust
pub fn cursor_screen_bounds(&self) -> Option<Bounds<Pixels>> {
    let bounds = self.last_bounds?;
    let line_height = self.last_line_height?;
    let cursor = self.buffer.cursor();
    let entry = self
        .last_lines
        .iter()
        .find(|e| cursor >= e.start && cursor <= e.start + e.shaped.len())?;
    let row = entry.index.checked_sub(self.scroll_offset_lines)?;
    let local = cursor.saturating_sub(entry.start).min(entry.shaped.len());
    let x = entry.shaped.x_for_index(local);
    let top = bounds.top() + line_height * row;
    Some(Bounds::new(point(bounds.left() + x, top), size(px(1.), line_height)))
}
```

**`up`/`down`/`newline` autocomplete gate** (T7 depends on this; implemented here since it's the same file/struct as the debounce work):

```rust
fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
    if self.autocomplete_active {
        cx.propagate();
        return;
    }
    self.buffer.move_up(false);
    self.follow_cursor = true;
    cx.notify();
}
// same guard added to `down` and `newline`.
```

- [ ] **Step 1: Write the failing tests** (new `#[cfg(test)] mod build_runs_tests` — `sql_input.rs` has no test module today, this is the first):

```rust
#[cfg(test)]
mod build_runs_tests {
    use super::*;
    use gpui::{hsla, rgb};

    fn plain_run() -> TextRun {
        TextRun { len: 0, font: gpui::Font::default(), color: hsla(0., 0., 1., 1.), background_color: None, underline: None, strikethrough: None }
    }

    #[test]
    fn no_highlights_no_marked_is_a_single_run() {
        let runs = build_runs(&plain_run(), 10, &[], None);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 10);
    }

    #[test]
    fn marked_range_only_matches_prior_three_run_shape() {
        let runs = build_runs(&plain_run(), 10, &[], Some(3..6));
        assert_eq!(runs.iter().map(|r| r.len).collect::<Vec<_>>(), vec![3, 3, 4]);
        assert!(runs[0].underline.is_none());
        assert!(runs[1].underline.is_some());
        assert!(runs[2].underline.is_none());
    }

    #[test]
    fn single_highlight_colors_its_span() {
        let color: gpui::Hsla = rgb(0xff0000).into();
        let runs = build_runs(&plain_run(), 10, &[(2..5, color)], None);
        let lens_and_colors: Vec<(usize, gpui::Hsla)> = runs.iter().map(|r| (r.len, r.color)).collect();
        assert_eq!(lens_and_colors, vec![(2, plain_run().color), (3, color), (5, plain_run().color)]);
    }

    #[test]
    fn highlight_and_marked_overlap_both_apply() {
        let color: gpui::Hsla = rgb(0x00ff00).into();
        let runs = build_runs(&plain_run(), 10, &[(2..8, color)], Some(4..6));
        let overlap = runs.iter().find(|r| r.color == color && r.underline.is_some());
        assert!(overlap.is_some(), "expected a run with BOTH the highlight color and the underline");
    }

    #[test]
    fn adjacent_different_colored_highlights_stay_separate_runs() {
        let c1: gpui::Hsla = rgb(0xff0000).into();
        let c2: gpui::Hsla = rgb(0x0000ff).into();
        let runs = build_runs(&plain_run(), 6, &[(0..3, c1), (3..6, c2)], None);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].color, c1);
        assert_eq!(runs[1].color, c2);
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui build_runs_tests::`
Expected: compile error (`build_runs`'s signature doesn't accept `highlight_local` yet).

- [ ] **Step 3: Implement** the field additions, `kick_highlight` + its 8 call sites, the generalized `build_runs` + its `prepaint` call site, `cursor`, `cursor_screen_bounds`, `set_autocomplete_active`, and the `up`/`down`/`newline` gates.

- [ ] **Step 4: Run to green + zero warnings + a sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass. Manually launch the app, type a query with keywords/strings/comments, confirm colors appear within ~60ms of the last keystroke and never flash to unhighlighted mid-edit.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/sql_input.rs
git commit -m "feat: wire tree-sitter highlighting into SqlInput"
```

---

### Task 6 (T6): `autocomplete.rs` — candidate computation

**Files:**
- Create: `crates/dbc-ui/src/autocomplete.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `mod autocomplete;`)

**Interfaces:**
- Consumes: `dbc_core::{SchemaSnapshot, TableInfo, ColumnInfo}` (`crates/dbc-core/src/schema.rs:1-46`, already exported from `dbc_core`'s `lib.rs`).
- Produces (consumed by T7):
  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub enum CandidateKind { Keyword, Table, Column }

  #[derive(Debug, Clone, PartialEq)]
  pub struct Candidate {
      pub text: String,   // inserted at the cursor
      pub label: String,  // shown in the popup (may carry a schema qualifier)
      pub kind: CandidateKind,
  }

  /// Identifier prefix ending exactly at `cursor` (walking backward over
  /// alnum/`_`), plus the qualifier token immediately before a `.` (if
  /// any) — used both to decide whether the typing trigger fires and to
  /// filter/rank candidates.
  pub struct CursorContext {
      pub prefix: String,
      pub qualifier: Option<String>,
  }
  pub fn cursor_context(text: &str, cursor: usize) -> CursorContext;

  /// Ranked candidates (design §2's ranking rules; capped at 20).
  /// `in_suppressed_span` is caller-supplied (T7 wires it from
  /// `SqlInput.highlights`' `suppresses_completion` flags, T4/T5) — this
  /// module never needs its own string/comment scan.
  pub fn candidates(
      text: &str,
      cursor: usize,
      snapshot: Option<&SchemaSnapshot>,
      force: bool, // true = Ctrl+Space, empty-prefix, full set
      in_suppressed_span: bool,
  ) -> Vec<Candidate>;

  /// `FROM <table> [AS] <alias>` / `JOIN <table> [AS] <alias>` text scan
  /// (NOT the tree-sitter tree — decouples this module from
  /// tree-sitter-sequel's node shapes, design §2). `None` = ambiguous
  /// (duplicate alias bound to two different tables, or an unresolvable
  /// `FROM (`/`JOIN (` subquery) — "offers nothing" rather than a guess.
  pub fn resolve_aliases(text: &str) -> Option<std::collections::HashMap<String, String>>;
  ```

**Grounding — cross-task interface note:** design §2 says the string/comment suppression check reuses "§1's already-computed highlight capture ranges... no second scanner needed." Since T6 is designed to have zero tree-sitter dependency (explicitly, to stay decoupled from `tree-sitter-sequel`'s node shapes) and is a *parallel* task with T4 (not dependent on it per the dependency graph), the reuse the design describes can only happen at the INTEGRATION point — T7, which depends on both T5 (holds `SqlInput.highlights`) and T6. This plan therefore has T6 take `in_suppressed_span: bool` as a plain parameter rather than computing it itself; T4's `HighlightSpan` carries the `suppresses_completion` flag T7 needs to compute that boolean. This is the resolution to a real gap in the design (which describes the reuse but not the exact type each side hands the other) — flagged for the controller in this plan's completion summary.

**v1 keyword list** (design §2 — "a static list", illustrative, extended slightly for coverage consistent with `guards.rs`'s own recognized vocabulary):

```rust
pub const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "ON", "LEFT", "RIGHT", "INNER", "OUTER", "FULL",
    "GROUP", "BY", "ORDER", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET",
    "DELETE", "AND", "OR", "NOT", "NULL", "IS", "IN", "LIKE", "BETWEEN", "AS", "DISTINCT",
    "HAVING", "UNION", "CASE", "WHEN", "THEN", "ELSE", "END", "EXISTS", "ALL", "ANY",
    "WITH", "EXPLAIN", "SHOW", "CREATE", "ALTER", "DROP", "TABLE", "INDEX", "VIEW",
    "PRIMARY", "KEY", "FOREIGN", "REFERENCES", "DEFAULT", "CHECK", "UNIQUE",
];
```

- [ ] **Step 1: Write the failing tests**:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{ColumnInfo, SchemaSnapshot, TableInfo};

    fn snapshot_two_schemas() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![
                TableInfo {
                    schema: Some("public".into()),
                    name: "users".into(),
                    columns: vec![
                        ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                        ColumnInfo { name: "email".into(), ..Default::default() },
                    ],
                    ..Default::default()
                },
                TableInfo {
                    schema: Some("audit".into()),
                    name: "log".into(),
                    columns: vec![ColumnInfo { name: "id".into(), ..Default::default() }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn snapshot_one_schema() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![TableInfo {
                schema: Some("public".into()),
                name: "orders".into(),
                columns: vec![
                    ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                    ColumnInfo { name: "total".into(), ..Default::default() },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn keywords_offered_regardless_of_snapshot() {
        let cs = candidates("sel", 3, None, false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Keyword && c.text == "SELECT"));
    }

    #[test]
    fn table_names_schema_qualified_when_snapshot_spans_multiple_schemas() {
        let cs = candidates("us", 2, Some(&snapshot_two_schemas()), false, false);
        let table = cs.iter().find(|c| c.kind == CandidateKind::Table && c.text.contains("users")).unwrap();
        assert!(table.label.contains("public"));
    }

    #[test]
    fn table_names_bare_when_snapshot_is_single_schema() {
        let cs = candidates("ord", 3, Some(&snapshot_one_schema()), false, false);
        let table = cs.iter().find(|c| c.kind == CandidateKind::Table).unwrap();
        assert_eq!(table.text, "orders");
    }

    #[test]
    fn suppressed_span_returns_no_candidates() {
        let cs = candidates("sel", 3, None, false, true);
        assert!(cs.is_empty());
    }

    #[test]
    fn force_ctrl_space_returns_full_set_with_empty_prefix() {
        let cs = candidates("", 0, None, true, false);
        assert!(cs.iter().any(|c| c.text == "SELECT"));
        assert!(cs.len() > 1);
    }

    #[test]
    fn column_completion_after_bare_table_dot() {
        let sql = "SELECT o.total FROM orders o WHERE orders.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "id"));
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "total"));
    }

    #[test]
    fn column_completion_after_alias_dot() {
        let sql = "SELECT * FROM orders o WHERE o.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "total"));
    }

    // Design §5's mandated risk-mitigation test: an ambiguous alias must
    // offer NOTHING, never a wrong guess.
    #[test]
    fn alias_ambiguity_offers_nothing() {
        let sql = "SELECT * FROM orders x JOIN users x ON x.id = x.id WHERE x.";
        assert_eq!(resolve_aliases(sql), None);
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_two_schemas()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn subquery_from_paren_is_ambiguous_offers_nothing() {
        let sql = "SELECT * FROM (SELECT 1) x WHERE x.";
        assert_eq!(resolve_aliases(sql), None);
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn unqualified_bare_column_completion_is_a_non_goal_returns_no_columns() {
        // design §2: bare column completion is explicitly out of scope for v1.
        let sql = "SELECT tot FROM orders";
        let cursor = 10; // inside "tot"
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn ranking_schema_objects_beat_keywords_on_equal_match() {
        // "orders" (table) vs no keyword literally named "orders" — use a
        // prefix that matches both a keyword and a table to prove
        // ordering: "o" alone is too broad; instead assert table entries
        // sort before keyword entries when both match the same prefix
        // tier by construction of the ranking, using "order" (keyword
        // ORDER exists) vs a same-prefixed table.
        let mut snap = snapshot_one_schema();
        snap.tables[0].name = "order".to_string();
        let cs = candidates("order", 5, Some(&snap), false, false);
        let table_ix = cs.iter().position(|c| c.kind == CandidateKind::Table).unwrap();
        let keyword_ix = cs.iter().position(|c| c.kind == CandidateKind::Keyword && c.text == "ORDER").unwrap();
        assert!(table_ix < keyword_ix);
    }

    #[test]
    fn cursor_context_extracts_prefix_and_qualifier_across_dot() {
        let ctx = cursor_context("SELECT o.tot", 12);
        assert_eq!(ctx.prefix, "tot");
        assert_eq!(ctx.qualifier, Some("o".to_string()));
    }

    #[test]
    fn cursor_context_no_qualifier_when_no_dot() {
        let ctx = cursor_context("SELECT sel", 10);
        assert_eq!(ctx.prefix, "sel");
        assert_eq!(ctx.qualifier, None);
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui autocomplete::`
Expected: compile error (module doesn't exist).

- [ ] **Step 3: Implement** `cursor_context`, `resolve_aliases`, `candidates`, `KEYWORDS`, and the ranking function (case-insensitive prefix tier, then substring tier; exact-case-prefix beats case-insensitive-only within a tier; schema objects rank above keywords when both match; ties alphabetical; cap 20 — design §2 "Ranking").

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui autocomplete::`
Expected: all pass, zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/autocomplete.rs crates/dbc-ui/src/main.rs
git commit -m "feat: schema autocomplete candidate computation"
```

---

### Task 7 (T7): AppView autocomplete seam

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` (autocomplete state, lazy trigger diff, `uniform_list` overlay, popup nav/accept action handlers)
- Modify: `crates/dbc-ui/src/sql_input.rs` (new `Escape` action scoped to the `"SqlInput"` key context)

**Interfaces:**
- Consumes: `sql_input::SqlInput::{cursor, cursor_screen_bounds, set_autocomplete_active, highlights}` (T5 — note `highlights` itself is private; T7 needs a small additional accessor, see below), `autocomplete::{Candidate, CandidateKind, candidates}` (T6).
- Produces: nothing consumed by a later task (leaf).

**Additional T5/T7 seam gap, resolved here:** T7 needs to know, at the CURRENT cursor position, whether it falls inside a `suppresses_completion` span — but `SqlInput.highlights` is a private field with no accessor exposed in T5's interface list. Add one more small `SqlInput` method as part of this task (touching `sql_input.rs`, already in this task's file list):
```rust
impl SqlInput {
    /// True if `cursor()` currently falls inside a string/comment
    /// highlight span (T4's `suppresses_completion`) — the autocomplete
    /// trigger's suppression check (design §2).
    pub fn cursor_in_suppressed_span(&self) -> bool {
        let c = self.buffer.cursor();
        self.highlights.iter().any(|h| h.suppresses_completion && h.range.contains(&c))
    }
}
```

**Grounding — the lazy-diff idiom** (design §2 "Seam" — "exactly the lazy-diff idiom `history_search`/`last_history_query` already established", `history_panel.rs:111-120`): `AppView` gains `last_ac_text: String`, `last_ac_cursor: usize`, `autocomplete: Option<AutocompleteState>`:
```rust
struct AutocompleteState {
    candidates: Vec<autocomplete::Candidate>,
    selected: usize,
}
```
On every render, before drawing the popup: read `self.sql.read(cx).text()` + `.cursor()`; if either differs from `last_ac_text`/`last_ac_cursor`, recompute (typing trigger: only reopens/updates if `autocomplete::cursor_context` yields a non-empty prefix or an active qualifier and the cursor isn't suppressed; otherwise closes the popup — space/most punctuation closes it because `cursor_context`'s prefix goes empty the moment the cursor sits after a non-identifier, non-`.` character).

**`Ctrl+Space` force-trigger** — new action, bound globally (context `None`, same precedent as `RunQuery`/`OpenPalette`):
```rust
actions!(dbc, [/* ...existing..., */ OpenAutocomplete]);
// KeyBinding::new("ctrl-space", OpenAutocomplete, None),
```
Handler opens the popup with `autocomplete::candidates(text, cursor, snapshot, /* force */ true, suppressed)` regardless of current prefix.

**Popup rendering** — `uniform_list` (same mechanism as `schema_tree.rs`/`history_panel.rs`/`grid.rs`), floating overlay anchored via `self.sql.read(cx).cursor_screen_bounds()`; max 8 visible rows, scrollable. `Up`/`Down` navigate `selected` (clamped), `Enter`/`Tab` accept (inserts the selected `Candidate.text` at the cursor, replacing `cursor_context`'s `prefix` range — implementer wires this through a new small `SqlInput` mutation, e.g. reusing `replace_text_in_range`-equivalent buffer surgery via `self.buffer.select_range(prefix_range); self.buffer.insert(text);` inside a new `pub fn accept_completion(&mut self, prefix_len: usize, text: &str, cx: &mut Context<Self>)` — bounded scope, same pattern as `newline`/`backspace`, also calls `kick_highlight`), `Esc`/click-away/losing focus/cursor-moved-by-mouse-or-unhandled-arrow closes it (sets `self.autocomplete = None` and calls `self.sql.update(cx, |s, _| s.set_autocomplete_active(false))`).

**Keyboard precedence wiring** (design §2 "Keyboard precedence" — the regression-sensitive part, per design §5's explicit callout that this deserves the same scrutiny as `sql_input.rs`'s prior `follow_cursor`/cursor-line-clamp review rounds):

1. Every render, sync the flag: `self.sql.update(cx, |s, _| s.set_autocomplete_active(self.autocomplete.is_some()))`.
2. T5 already made `SqlInput::up`/`down`/`newline` call `cx.propagate()` and return early when `autocomplete_active` — confirmed against the pinned GPUI rev's actual bubble-phase semantics (`crates/gpui/src/window.rs:5636-5657`): action dispatch during the bubble phase visits the FOCUSED element first (`dispatch_path.iter().rev()`, i.e. leaf-to-root) and sets `propagate_event = false` BEFORE invoking each node's listener, so a handler MUST call `cx.propagate()` (re-enabling it, `crates/gpui/src/app.rs:2271`) for an ancestor's own listener for the SAME action type to run next in the same bubble pass.
3. Bind `AppView`'s own listeners for the SAME action types (`sql_input::Up`, `sql_input::Down`, `sql_input::Newline` — these are `pub` structs generated by `sql_input.rs`'s `actions!(sql_input, [...])` macro, reachable as `sql_input::Up` etc.) on the wrapping `div` that directly contains `self.sql.clone()` — `main.rs:2667-2671`'s `div().h(px(20.*8.+4.*2.)).px_2().bg(rgb(0x181825)).child(self.sql.clone())` — via `.on_action(cx.listener(Self::on_ac_up))` / `on_ac_down` / `on_ac_confirm` added to that SAME div. This is the direct parent of `SqlInput`'s own focused/track_focus div in the render tree, so it's next in the reversed bubble path right after `SqlInput`'s own (now-propagating) handler.
4. `Escape` needs the SAME override treatment but there is currently NO `Escape` action anywhere in `sql_input.rs` — Escape falls straight through to the global `"escape" → CancelQuery` binding (`main.rs:2793`, context `None`). Add a new `sql_input::Escape` action + `KeyBinding::new("escape", Escape, Some("SqlInput"))` in `sql_input::bind_keys` (a context-scoped binding takes precedence over the global context-`None` one while `SqlInput`'s own `"SqlInput"` key context is active — same precedence mechanism already proven by `palette.rs`'s own `"escape"` binding under context `"Palette"` overriding the same global `CancelQuery`, documented at `main.rs:1085-1089`). `SqlInput::on_escape`: `if self.autocomplete_active { cx.propagate(); return; }` else do nothing itself (also propagate) — so Escape reaches `AppView`'s wrapper-div handler (closes the popup, consumes) when a popup is open, or falls through further to the global `CancelQuery` (cancels a running query) when it's not, preserving today's behavior exactly in the no-popup case.
5. `AppView`'s wrapper-div handlers (`on_ac_up`/`on_ac_down`/`on_ac_confirm`/`on_ac_escape`) only run at all when `SqlInput` propagated (i.e. `autocomplete_active` was true at dispatch time) — still guard defensively with `let Some(ac) = &mut self.autocomplete else { return };` before touching popup state, in case of a same-frame race between the flag sync and a stale dispatch.

- [ ] **Step 1: Write the failing tests** — the popup/keyboard-precedence plumbing is GPUI glue with no existing entity-test precedent in this codebase (confirmed: no `#[gpui::test]`/`TestAppContext` usage anywhere in `crates/dbc-ui/src`; `schema_tree.rs`'s "snapshot-refresh" tests the design's own Test Strategy section points to are plain `#[test]`s against its pure `flatten()` helper, not live GPUI entities). Per that same established split, extract the one genuinely testable-without-a-window piece of T7's own logic — the accept-completion range/text computation — as a pure helper and test it directly:
  ```rust
  /// Pure: given `text`, `cursor`, and the candidate's `text` to insert,
  /// returns the byte range to replace (the identifier prefix ending at
  /// `cursor`, or an empty range at `cursor` if there is none — e.g. a
  /// force-triggered accept with no partial prefix typed) and the final
  /// string. Extracted so T7's `accept_completion` wiring has a pure,
  /// directly-testable core instead of only being exercisable through a
  /// live `SqlInput`.
  fn completion_edit(text: &str, cursor: usize, insert: &str) -> (Range<usize>, String) {
      let ctx = autocomplete::cursor_context(text, cursor);
      let start = cursor - ctx.prefix.len();
      let mut new_text = text.to_string();
      new_text.replace_range(start..cursor, insert);
      (start..cursor, new_text)
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
  }
  ```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui completion_edit_tests::`
Expected: compile error (`completion_edit` doesn't exist).

- [ ] **Step 3: Implement** `AutocompleteState`, the lazy-diff trigger recompute, `OpenAutocomplete` action + binding, the popup `uniform_list` render (anchored at `cursor_screen_bounds()`), `accept_completion` on `SqlInput` (uses `completion_edit`'s range to drive `buffer.select_range` + `buffer.insert`, then `kick_highlight`), the new `sql_input::Escape` action + `"SqlInput"`-scoped binding, `SqlInput::on_escape`, and `AppView`'s `on_ac_up`/`on_ac_down`/`on_ac_confirm`/`on_ac_escape` bound on the wrapper div at `main.rs:2667-2671`, plus `self.sql.update(cx, |s, _| s.set_autocomplete_active(...))` synced every render.

- [ ] **Step 4: Run to green + zero warnings + a manual keyboard-precedence check**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass. Manually launch against the SQLite fixture: type a partial table name, confirm the popup opens and Up/Down navigate WITHOUT moving the cursor in the buffer, Enter accepts WITHOUT inserting a newline, Esc closes the popup WITHOUT cancelling a running query; then, with the popup closed, confirm Up/Down/Enter/Esc behave exactly as before this task (cursor movement, newline insert, query cancel).

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/main.rs crates/dbc-ui/src/sql_input.rs
git commit -m "feat: schema autocomplete popup wired into the SQL editor"
```

---

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §1 Highlighting: approach/debounce/stale-frame policy → T5; capture→color palette + error degradation → T4 (with the CURATION dark-Mocha palette applied verbatim); render integration (`build_runs` generalization) → T5.
- §2 Autocomplete: trigger model + suppression mask → T6 (pure logic) + T7 (wiring, including the T4→T7 `suppresses_completion` plumbing); v1 scope (keywords, tables, alias-qualified columns, explicit non-goals) → T6; popup UI + keyboard precedence → T7; ranking → T6; seam/no-snapshot degradation → T7.
- §3 Parametrized queries: `:name` detection → T1; values dialog UX, typing model, auto-LIMIT interaction (unchanged — substitution happens before `run_query_with`, which still runs the existing guards), history interaction (unchanged — `run_query_with` already records whatever SQL it's given) → T3; persistence shape → T2; substitution mechanism (`sql_value`, numeric=true) → T3.
- §5 Risks: tree-sitter-sequel API drift → resolved in T4 via the spike (Language construction, real capture names, broken upstream numeric predicates, vendored fix). T-SQL error recovery locality → empirically characterized in T4 (not perfectly local, but non-fatal; tests assert the actual observed behavior). GPUI background-executor timer API → confirmed in T5's grounding (executor.rs:187, async_context.rs:127). SQLite `:name` collision → the mandatory T3 post-substitution rescan, with a dedicated test. Alias-resolution false positives → explicit T6 test (`alias_ambiguity_offers_nothing`). Keyboard-precedence regression risk → T7's detailed bubble-phase grounding + manual precedence check.

**Placeholder scan:** every step above either shows real code (implementation snippets, full test modules) or a concrete cargo command; no "add tests"/"handle edge cases"/TBD-style steps remain. T3's dialog render and T7's popup render are described by contract (exact fields, exact Czech labels, exact anchor/behavior) rather than full GPUI render trees, matching this repo's own G5 plan's precedent (G5 Tasks 3-4 do the same for `render_cell_editor_overlay`/apply-bar render) — the underlying logic each render calls is fully specified and tested.

**Type consistency across tasks:** `dbc_core::find_params`/`substitute_params` (T1) signatures match T3's usage exactly. `dbc_state::{ParamValue, ParamValuesStore}` (T2) match T3's `param_values` field and `build_param_sql`'s `store.set` calls. `sql_highlight::HighlightSpan { range, color, suppresses_completion }` (T4) matches T5's `SqlInput.highlights: Vec<HighlightSpan>` and T7's `cursor_in_suppressed_span`. `autocomplete::{Candidate, CandidateKind, candidates, cursor_context, resolve_aliases}` (T6) match T7's `AutocompleteState.candidates` and `completion_edit`'s use of `cursor_context`. `SqlInput::{cursor, cursor_screen_bounds, set_autocomplete_active, cursor_in_suppressed_span}` (T5, +one method added in T7) match T7's every call site.

**Resolved design ambiguities (flagging for controller review, not vetoed unilaterally):**
1. **T4/T6/T7 suppression-mask handoff.** The design says T6 reuses T4's "already-computed highlight capture ranges" for the string/comment suppression check, but the task dependency table has T6 parallel with (not dependent on) T4. Resolved by having T4's `HighlightSpan` carry a `suppresses_completion: bool` and T6's `candidates` take a caller-supplied `in_suppressed_span: bool` instead of touching tree-sitter itself — the actual reuse happens in T7, which is the only task depending on both.
2. **Tree-sitter-sequel's bundled `highlights.scm` numeric predicates are broken** (Lua pattern syntax `%d`, not regex) — empirically confirmed via a real build+run spike in this plan, not just read from docs. T4 vendors a local trimmed, regex-corrected copy instead of using `tree_sitter_sequel::HIGHLIGHTS_QUERY` verbatim; this is within the research doc's own anticipated fallback, not a scope change, but is a concrete deviation from "use the bundled query" that the design's prose didn't spell out.
3. **Same-node multi-capture collisions** (a numeric literal matches both `@string` and `@number`; a function call matches both `@function.call` and the generic `@type` object-reference pattern) are resolved via an explicit priority table in T4 rather than relying on tree-sitter's match-iteration order (not a documented stable contract).
4. **T3's `ModalState` location:** the design refers to "ModalState enum" without noting it actually lives in `connections_ui.rs`, not `main.rs` (which only holds `Option<connections_ui::ModalState>`). Plan corrected this; no behavioral ambiguity, just a file-location correction.
5. **T7's `Escape` handling for the popup** is a genuinely new key-context interaction (a new `sql_input::Escape` action scoped to key context `"SqlInput"`) that the design didn't spell out at the same level of detail as Up/Down/Enter — resolved by mirroring the exact precedent `palette.rs`'s own `"Palette"`-scoped Escape binding already establishes for overriding the global `CancelQuery` binding.
6. **T6's ranking tie-break deviates from the design's literal "ties broken alphabetically"** in exactly one case: when the effective match prefix is empty (Ctrl+Space full-set mode, or a genuinely empty typed prefix), `rank_and_cap` preserves `KEYWORDS`' authored declaration order instead of sorting alphabetically. Rationale: `KEYWORDS` has ~57 entries and the cap is 20; under a literal empty-prefix tie, every keyword ties on every ranking rule (match tier, case tier, kind tier), so a strict alphabetical tie-break would cap the list at `ALL`..`FROM` alphabetically and drop `SELECT` — the single most common keyword — out of the popup entirely. The plan's own T6 test `force_ctrl_space_returns_full_set_with_empty_prefix` requires `SELECT` to survive the cap, which is incompatible with alphabetical-under-total-tie. Preserving declared order (which the `KEYWORDS` array already leads with `SELECT, FROM, WHERE, JOIN, ON, ...`) satisfies the test and is arguably the more useful default for a "browse everything" popup. Alphabetical tie-breaking is unaffected (and still meaningful) whenever the prefix is non-empty, since a real prefix comparison almost never produces a total tie across every keyword simultaneously. Flagged here per review round 1's process finding; not vetoed unilaterally.
