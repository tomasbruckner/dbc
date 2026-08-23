# G6 Editor Pro — Design Pass

Date: 2026-08-23. Status: designed autonomously per the standing mandate (per
gui-target-design.md §4); decisions recorded here for later user review, same
posture as the G5 design pass it follows in style.

Scope (spec row G6): tree-sitter SQL highlighting; schema autocomplete;
parametrized queries (`:name` placeholders prompt a values dialog before run,
last values remembered). Inputs: `2026-08-23-g6-tree-sitter-highlighting.md`
(research blueprint, adopted below with its open questions resolved),
`crates/dbc-ui/src/sql_input.rs`, `crates/dbc-core/src/schema.rs`,
`crates/dbc-state/src/*`, `crates/dbc-ui/src/main.rs`'s `run_query_with`.

## 1. Highlighting

- **Approach:** adopt the research blueprint as-is — `tree-sitter` +
  `tree-sitter-sequel` bolted onto `TextElement`'s existing per-line
  `TextRun` construction, full-buffer reparse (no incremental `tree.edit`),
  background-spawned. No Zed `language`/`syntax_map` machinery. Lives in a
  new `crates/dbc-ui/src/sql_highlight.rs` — parsing itself has no GPUI
  dependency, but `Hsla` color resolution does, so it stays in `dbc-ui`
  rather than splitting the pure parse into `dbc-core`; keeping one module
  avoids a cross-crate seam for a single-language, single-theme feature.
- **Debounce/coalescing:** 60 ms trailing debounce, implemented as a
  monotonic `highlight_generation: u32` field on `SqlInput`, bumped by every
  mutating op (same call sites that already set `follow_cursor = true`).
  Each mutation spawns `cx.spawn(async move |this, cx| { cx.background_executor().timer(Duration::from_millis(60)).await; let text = ...; let spans = cx.background_spawn(...).await; this.update(...) })`;
  on write-back, the task compares its captured generation against the
  entity's current `highlight_generation` and silently drops its result if
  they differ (superseded by a newer edit) — exact reuse of the
  `run_generation` idiom already in `main.rs`'s `run_query_with`, rather than
  inventing a new cancellation mechanism. Rationale: per-keystroke background
  parse is "likely fine" per the research doc at these buffer sizes, but a
  60 ms debounce is free insurance against paste/autocomplete-accept bursts
  firing dozens of parses in a few ms, at a latency cost far below human
  keystroke-to-paint perception. **Needs verification:** exact
  `BackgroundExecutor` timer API on the pinned `907ed09` GPUI rev — confirm
  at T4/T5 implementation.
- **Stale-frame policy:** keep rendering the PREVIOUS highlight spans while a
  newer parse is in flight — never blank/flash to plain text. A completed
  task only overwrites `self.highlights` if its generation matches current;
  a slower-but-older task's result is simply discarded (never applied out of
  order). No synchronous `sync_parse_timeout` fallback (unlike Zed) — the
  editor is small-buffer interactive SQL, not a file editor opening
  megabyte files, so first-paint-unhighlighted for ~60ms+parse-time is
  acceptable and simpler than a sync-block escape hatch.
- **Capture → color palette:** flat `match` (not a `HighlightMap` table),
  fixed DARK-theme colors (CURATION FIX: the app today is Catppuccin
  Mocha-dark throughout — base text `0xcdd6f4` on dark panels; the original
  draft proposed light-theme values, unreadable here). Use the Mocha accent
  set the UI already draws from, hardcoded until G14's theme system:
  - `keyword` → `rgb(0xcba6f7)` (mauve)
  - `string` → `rgb(0xa6e3a1)` (green)
  - `number` → `rgb(0xfab387)` (peach)
  - `comment` → `rgb(0x6c7086)` (overlay gray; no italic — `TextRun`
    styling kept to color-only, matching the file's existing minimal style
    surface)
  - `function` / `function.builtin` → `rgb(0x89b4fa)` (blue)
  - `type` → `rgb(0x94e2d5)` (teal)
  - everything else (identifier, operator, punctuation, unrecognized capture
    names) → no override, falls through to the base `style.color`
    (`0xcdd6f4`), same as today's plain text.
  G14's Theme struct later absorbs these as its syntax-palette fields (its
  draft already reserves the hook).
- **Error-node degradation:** captures whose node (or an ancestor) is an
  `ERROR`/`MISSING` node are simply not colored (fall through to default) —
  the query only ever *adds* spans for successfully-captured nodes, so a
  parse error anywhere in the buffer degrades ONLY the affected sub-tree,
  never the whole buffer, never a panic/hard failure. If `parser.parse`
  itself returns `None` (cancellation only, per tree-sitter's contract —
  parse always returns a partial tree on syntax errors), keep the previous
  frame's spans rather than clearing them — same "never flash to
  unhighlighted" posture as the stale-frame policy above. T-SQL-only syntax
  (`TOP`, `OUTER APPLY`, `[col]`) against the generic grammar is expected to
  produce local `ERROR` nodes around those tokens specifically — acceptable,
  matches the research doc's own conclusion; **needs verification** that
  tree-sitter-sequel's error recovery is actually node-local and doesn't
  poison the rest of the statement (unverified, no build attempted yet).
- **Render integration:** generalize `build_runs` (sql_input.rs:173) from
  "0–1 marked sub-range" to "N colored sub-ranges, with the IME marked-range
  underline still overlaid on top" — i.e. color spans and the marked-range
  underline are independent dimensions merged into one `TextRun` list per
  line, not one replacing the other (a marked IME composition over a
  keyword must show both the keyword's color AND the underline).

## 2. Autocomplete

- **Trigger model:** two triggers, both gated by "cursor is not inside a
  string/comment span" — reusing §1's already-computed highlight capture
  ranges for that check (no second scanner needed, `string`/`comment`
  capture spans double as a suppression mask):
  1. Typing trigger: any alnum/`_` insert that leaves a non-empty partial
     identifier token under the cursor (or immediately after a `.`) opens/
     updates the popup with matching candidates; typing a space or most
     punctuation (other than `.`) closes it.
  2. `Ctrl+Space`: force-opens regardless of context, offering the full
     candidate set at the cursor's position (empty-prefix — everything,
     ranked).
  Esc, losing focus, or moving the cursor via mouse/arrow-without-popup-nav
  closes the popup.
- **v1 scope (what's completed):**
  - SQL keywords: a static list (`SELECT FROM WHERE JOIN ON GROUP BY ORDER
    BY LIMIT INSERT INTO VALUES UPDATE SET DELETE FROM AND OR NOT NULL IS IN
    LIKE BETWEEN AS DISTINCT HAVING UNION CASE WHEN THEN ELSE END ...`),
    always offered regardless of snapshot/connection state.
  - Table/view names from `SchemaSnapshot.tables`: schema-qualified when the
    snapshot spans multiple schemas, bare otherwise. Offered whenever the
    typed prefix matches, with **no clause-position awareness** — v1
    deliberately does not try to detect "cursor is after FROM/JOIN" via the
    tree-sitter tree; mapping grammar node types to "a table name is
    expected here" is a second, unverified query surface (research doc's
    own open question) and out of budget. Non-goal, explicit.
  - Column names: **only** after `alias.` or `table.` qualification. v1
    alias resolution is a lightweight token scan over the raw SQL text (not
    the tree-sitter tree — decouples this module from tree-sitter-sequel's
    unverified node shapes) matching `FROM <table> [AS] <alias>` and `JOIN
    <table> [AS] <alias>` patterns to build an alias→table map; `alias.` or
    bare `table.` then completes that table's columns from
    `SchemaSnapshot`. If the scan is ambiguous (subquery, CTE, `FROM (SELECT
    ...) x`, multiple tables aliased identically) it offers **nothing**
    rather than guessing — silent under-completion is safe, a wrong guess
    is not.
  - Bare (unqualified) column completion is a **non-goal** for v1 — it
    requires join-aware scope resolution across every table in the FROM
    clause, deferred.
- **Explicit non-goals (v1):** unqualified column completion; function
  signature/parameter hints; snippet expansion; fuzzy/typo-tolerant matching
  (prefix + substring only, see ranking below); per-engine dialect keyword
  sets (one generic list for all three engines); usage-frequency/ML ranking;
  completing routines/triggers/sequences (visible in the schema tree, not
  offered inline); completion inside string literals or comments (actively
  suppressed, see trigger model).
- **Popup UI:** `uniform_list` (same mechanism as `schema_tree.rs` /
  `history_panel.rs` / `grid.rs`), floating overlay anchored at the cursor's
  pixel position — computed via the same `ShapedLine::x_for_index`/line-row
  math `TextElement::prepaint` and `EntityInputHandler::bounds_for_range`
  already use for IME positioning, exposed from `SqlInput` as a new `pub fn
  cursor_screen_bounds(&self) -> Option<Bounds<Pixels>>`. Max 8 visible
  rows, scrollable; `Up`/`Down` navigate, `Enter`/`Tab` accept, `Esc`/click-
  away dismiss.
- **Keyboard precedence:** `Up`/`Down`/`Enter` are already bound in
  `SqlInput` to buffer cursor movement / newline. Rather than rewiring
  `SqlInput`'s action dispatch, add `pub fn autocomplete_active(&self) ->
  bool` (set by `AppView`) that `SqlInput::up`/`down`/`newline` check first
  and no-op on if true, letting a separate, AppView-owned higher-priority
  key handling do popup nav/accept instead — smallest possible change to a
  file whose header comment already documents several past
  precedence-subtlety review rounds (`follow_cursor`, cursor-line clamp);
  treat this as equally regression-sensitive.
- **Ranking:** case-insensitive prefix match first, then substring match;
  within a tier, exact-case-prefix beats case-insensitive-only; schema
  objects (tables/columns) rank above keywords when both match (more
  specific, usually what the user wants); ties broken alphabetically; capped
  at 20 shown (scroll for more, though `uniform_list` makes this moot).
- **Seam — how the snapshot reaches the editor:** `SqlInput` stays
  schema-agnostic (it's a reusable low-level text widget; no `SchemaSnapshot`
  import). `AppView` owns the seam: it already holds `tree: Entity<SchemaTree>`
  with a `snapshot() -> Option<&SchemaSnapshot>` accessor. On each window
  render, `AppView` reads `self.sql`'s text + a new `pub fn cursor(&self) ->
  usize` accessor (small, intentional addition to `SqlInput`'s public
  surface — its header comment's "frozen for this task" note was scoped to
  G1 Task 4, not binding here) and diffs against cached
  `last_ac_text`/`last_ac_cursor` fields, exactly the lazy-diff idiom
  `history_search`/`last_history_query` already established, before
  recomputing candidates — avoids adding an event-emission system to
  `SqlInput` just for this.
- **No/stale snapshot behavior:** no connection or snapshot not yet fetched
  → popup silently degrades to keywords-only, no error, never blocks typing.
  Stale snapshot (schema changed server-side since fetch) → best-effort,
  may suggest now-invalid names; identical staleness posture to
  `schema_tree.rs` itself today (no live re-validation there either) —
  consistent, not a new hole.

## 3. Parametrized queries

- **`:name` detection:** new `crates/dbc-core/src/params.rs`,
  `pub fn find_params(sql: &str) -> Option<Vec<String>>` (distinct names, in
  first-occurrence order; `None` = fail-closed, "cannot determine safety").
  A dedicated scanner, **not** a reuse/refactor of `guards::tokenize`
  (that function deliberately discards all punctuation except `;`/`=` for
  its own two safety-critical callers; retrofitting position-preserving
  colon-detection into it risks regressing `is_read_statement`/
  `apply_auto_limit`). Instead it duplicates the same string/quoted-ident/
  line-comment/nested-block-comment state tracking as `tokenize` (same
  escape rules: `''`, `""`, `--`, nesting `/* */`) — accepted duplication,
  consistent with this codebase's existing precedent of small purpose-built
  scanners over shared abstractions (e.g. `history_panel.rs`'s own
  `collapse_sql` documented as a deliberate copy of `tabs::collapse_title`'s
  logic). Outside those spans: `:` + `[A-Za-z_][A-Za-z0-9_]*` is a param;
  `::` (Postgres cast) and `:=` (assignment) are recognized as 2-char inert
  tokens and skipped without emitting a param, then scanning resumes
  normally past them. Unterminated string/comment → `None`, same fail-closed
  contract as `tokenize`.
- **Values dialog UX:** interception point is `run_query`/`on_run_query`
  (and the palette's `PaletteAction::RunQuery`), before `run_query_with` is
  called. `find_params` on the editor's current text; empty/`None` → proceed
  exactly as today, no behavior change. Non-empty → open a new
  `ModalState::QueryParams { names, inputs: Vec<Entity<TextField>>, null_flags: Vec<bool>, pending }`
  variant (same "compute a pending action, show a modal, resume on confirm"
  shape as `PendingAfterUnlock` in `connections_ui.rs`). One row per distinct
  name: a `TextField` (reused widget, same as history search / connection
  form) + a "NULL" checkbox beside it — same visual idiom as G5's cell-editor
  NULL button ("an empty string and NULL are distinct"), and a **live
  read-only preview line** of the fully-substituted SQL underneath the
  fields, updating as the user types — for the same transparency reason G5's
  Apply dialog shows the exact generated SQL before running ("SQL you can
  see is the SQL that runs"). Prefilled from the persisted store (below) when
  available. "Spustit" (Enter in the last field or click) substitutes, saves
  every value, closes the modal, and calls `run_query_with` with the
  substituted SQL (caller's original `bypass_auto_limit` unchanged). Esc
  cancels — no run, no persistence write.
- **Typing model:** raw text per field; NULL is an explicit checkbox, not
  inferred from an empty field — empty text submits as an empty-string
  literal unless the checkbox is ticked, mirroring G5's explicit-NULL
  precedent exactly rather than introducing a second, inconsistent
  empty-means-NULL convention.
- **Persistence shape:** new `crates/dbc-state/src/params.rs`,
  `ParamValuesStore` mirroring `ViewPrefsStore`'s shape almost exactly
  (`load`/`get`/`set`, atomic TOML write, same unit-separator `encode_key`
  scheme) at `dbc/params.toml` alongside `views.toml`. Key =
  **(connection_id, param name)** — not (connection, query text). Rationale:
  query text churns on every edit (whitespace, added columns) making it a
  poor stable key, while a param NAME is the stable semantic handle the user
  themselves assigns meaning to; this also matches DataGrip's own precedent
  of remembering parameter values per data source by name, not per exact
  query string. `connection_id` uses the saved connection's id, or the
  literal `"cli"` sentinel for the CLI-arg back-compat path — same fallback
  `active_connection_name_for_history` already uses for that path.
  `ParamValue { text: String, is_null: bool }`. Explicit non-goal: two
  different queries on the same connection that happen to reuse `:id` for
  different meanings will cross-pollinate their remembered value — accepted,
  same class of conservative-tradeoff already made elsewhere in this
  codebase (e.g. `is_read_statement`'s deliberate false positives).
- **Auto-LIMIT interaction:** substitution happens strictly BEFORE the
  existing auto-LIMIT/read-only guards — the dialog produces a fully literal
  SQL string, which then flows through `is_read_statement`/`apply_auto_limit`
  completely unchanged, exactly as a hand-typed query with literal values
  would. No changes to `guards.rs` needed or made; a substituted value never
  changes a statement's keyword shape, so the existing tokenizer-based guards
  remain correct over it.
- **History interaction:** history records the **substituted** SQL (the
  literal statement that actually ran), not the `:name` template. Rationale:
  the "no result data/credentials in history" rule concerns row DATA and
  secrets, not literal predicate values — history already stores literal
  `WHERE id = 5`-style values for every hand-typed query today, so this
  introduces no new category of exposure; storing the template instead would
  make history entries unreplayable (clicking one loads-and-runs a complete
  statement today, no re-prompt). One documented caveat, not a new hole: if
  a parameter value is itself sensitive, it lands in history exactly as a
  hand-typed literal would — same acceptable-risk class as today.
- **Substitution mechanism:** literal quoting, via the *existing*
  `sandbox::sql_value(value: Option<&str>, numeric: bool) -> String` from
  G5 (`dbc-ui/src/sandbox.rs`), called with `numeric = true` for opportunistic
  unquoting (strict i128/finite-f64 parse → bare token, e.g. typing `5` for
  `:id` doesn't become `'5'`) and NULL checkbox → `sql_value(None, _)` =
  `"NULL"`. Chosen over real driver-side bind parameters (`$1`/`?`) because
  `Connection::{query,execute}` in `dbc-core` accept only a raw `sql: &str`
  today — no parameter-binding capability exists in the trait for either
  driver. Adding one purely to serve this feature would mean extending the
  trait and both driver implementations, a materially bigger and riskier
  change than G6's budget, for a feature the phasing table already marks
  "least urgent." Literal substitution also keeps the same transparency
  principle G5 established (exact SQL is shown before running).

## 4. Task decomposition

| Task | Files/crates | Depends on | Parallel with |
|---|---|---|---|
| **T1** `:name` scanner (`find_params`) + tests (strings/idents/comments/`::`/`:=`/nesting/fail-closed) | `dbc-core/src/params.rs`, `lib.rs` export | — | T2, T4, T6 |
| **T2** `ParamValuesStore` + tests (roundtrip/missing-file/key-collision, copy `view_prefs.rs` test shapes) | `dbc-state/src/params.rs`, `lib.rs` export | — | T1, T4, T6 |
| **T3** Values dialog: `ModalState::QueryParams`, render, `run_query`/`on_run_query`/palette interception, prefill+save, substituted-SQL preview | `dbc-ui/src/main.rs`, `connections_ui.rs` | T1, T2 | — |
| **T4** `sql_highlight.rs`: tree-sitter-sequel wiring, `HIGHLIGHTS_SCM`, capture→color table, `highlight()` + tests (keyword/string/comment/error-node degrade) | `dbc-ui/Cargo.toml`, `dbc-ui/src/sql_highlight.rs` | — | T1, T2, T6 |
| **T5** Wire highlighting into `SqlInput`: `highlights`/`highlight_generation` fields, debounced background reparse, generalized `build_runs`, new `cursor()`/`cursor_screen_bounds()` accessors | `dbc-ui/src/sql_input.rs` | T4 | — |
| **T6** `autocomplete.rs`: candidate computation (keywords, tables/columns from `SchemaSnapshot`, text-scan alias resolution), ranking, tests | `dbc-ui/src/autocomplete.rs` | — | T1, T2, T4 |
| **T7** AppView autocomplete seam: lazy trigger diff, `autocomplete_state`, `uniform_list` overlay, `autocomplete_active()` keyboard-precedence gate | `dbc-ui/src/main.rs`, `sql_input.rs` | T5, T6 | — |

- **Parallelizable as one batch:** T1, T2, T4, T6 — disjoint files, no
  dependency edges among them.
- **Sequential tail:** T3 after {T1, T2}; T5 after T4; T7 after {T5, T6}.
  T3 and T7 both edit `main.rs` — no logical dependency between them, but
  flag as a merge-order/file-contention caveat (same author or rebase
  discipline), not a true blocking dependency.
- **Test strategy split:** T1/T2/T4(parse+color logic)/T6 are pure
  functions — unit-tested without GPUI/a window, matching this codebase's
  existing pure-logic/thin-glue split (`guards.rs`, `sandbox.rs`,
  `tabs::collapse_title`). T3/T5/T7 are thin `Render`/`Context` glue over
  those pure functions — tested indirectly through the pure functions plus
  a handful of focused entity-level tests where the codebase already does
  this (`schema_tree.rs` has snapshot-refresh entity tests; mirror that
  shape for `ModalState::QueryParams` transitions and for `SqlInput.highlights`
  population after a simulated edit).

## 5. Risks / needs-verification

- **tree-sitter-sequel API drift:** exact `Language` construction
  (`LanguageFn` vs `Language`) and its bundled `highlights.scm` capture
  names are unverified — no build was attempted in the research pass. First
  thing to spike at T4 start.
- **T-SQL-only syntax error recovery:** expected to degrade only the
  offending sub-span (§1), but tree-sitter-sequel's actual error-recovery
  locality for `TOP`/`OUTER APPLY`/`[col]` is unverified against this
  specific grammar.
- **GPUI background-executor timer API:** the 60 ms debounce needs a
  confirmed `BackgroundExecutor`/`cx.spawn` timer primitive on the pinned
  `907ed09` rev; not confirmed to exist under that exact name.
- **SQLite native `:name` collision:** SQLite's own bind-parameter syntax
  natively recognizes `:name`/`@name`/`$name`. If our client-side detector
  fails closed (unterminated construct) or a user somehow runs with an
  undetected/un-substituted `:name` still in the text, rusqlite may treat it
  as a valid, simply-unbound named parameter rather than erroring —
  potentially binding NULL silently instead of failing loudly. CURATION
  DECISION: the defensive post-substitution scan is a REQUIREMENT, not a
  consideration — after substitution, re-run `find_params` on the final SQL
  and refuse to dispatch (value-error surfaced in the dialog) if any bare
  `:name` survives, for EVERY engine (cheap, uniform, closes the silent-NULL
  hole without depending on rusqlite behavior verification). T3 must include
  a test for this.
- **Alias-resolution false positives:** the v1 text-scan alias resolver
  (§2) can misfire on subqueries/CTEs/self-joins with duplicate aliases;
  designed to degrade to "no column suggestions" rather than wrong ones,
  but this needs to be an explicit T6 test case, not just a design
  intention.
- **Triple text scanning on every debounced keystroke:** highlighting
  (T4/T5), and — while the popup is open — autocomplete candidate
  recomputation (T6/T7) both re-scan the buffer independently (param
  detection, T1, only runs once per Run click, not per keystroke — cheap).
  Acceptable at interactive SQL buffer sizes per the research doc's own
  conclusion; revisit only if a future phase (G12 script runner) starts
  loading large files into this same editor.
- **Keyboard-precedence regression risk:** `sql_input.rs`'s header comment
  documents multiple past review rounds specifically about action-handling
  subtleties (`follow_cursor`, cursor-line clamp). The `autocomplete_active()`
  gate added in T7 touches exactly that surface and should get the same
  level of scrutiny/tests as those prior rounds, not less.
