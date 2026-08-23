# G12 — Script Runner: Design Pass

Date: 2026-08-23
Status: designed autonomously per the G5-style standing mandate; decisions
recorded here for later user review.
Scope: spec row G12 (`docs/superpowers/specs/2026-08-22-gui-target-design.md`
line "G12 Script runner"). Run an external `.sql` file (streamed
statement-by-statement, never loaded whole into the editor) or a whole
folder (files in name order, per-file progress) against a chosen connection;
requires a statement splitter (also unlocks multi-statement SQL in the
editor); error policy (stop/continue); transaction scope; CSV import into a
table (column mapping, batched INSERTs, respects read-only flag).

Read before implementing: `crates/dbc-core/src/guards.rs` (FULL —
`is_read_statement`'s tokenizer/lexing discipline: `''`/`""` escaping, `--`
line comments, nested `/* */` block comments, fail-closed-on-EOF; the
splitter below shares this discipline but cannot reuse `tokenize` directly
since that function is private, whole-string, and non-resumable — see §1);
`crates/dbc-core/src/connection.rs` (the `execute()` write path + its
per-connection transaction invariants — pg aborts a transaction on the FIRST
error, sqlite doesn't, a tx driver must stop-and-rollback either way — the
execution model in §2 is built directly on this contract); `crates/dbc-ui/
src/runner.rs` (`QueryRunner`'s `mpsc`-streaming-event / `oneshot`-one-shot
conventions, `ConnectSpec`/`open_spec` dispatch — reused verbatim for the new
methods); `crates/dbc-ui/src/sandbox.rs` + the G5 design-pass block in the
target-UI spec (pure model + exhaustive tests + "confirm modal shows exact
SQL → one transaction" pattern — CSV import's SQL generation reuses
`sql_value` itself, not just the pattern); `crates/dbc-ui/src/grid.rs`'s
`start_export` (native save-dialog convention via `cx.prompt_for_new_path`
— the file/folder open-picker below is its `prompt_for_paths` sibling);
`crates/dbc-ui/src/tabs.rs` (`TabContent`/`ResultTab`/`TAB_CAP` — the
progress tab is a third `TabContent` variant); `crates/dbc-state/src/
history.rs` (`HistoryEntry`'s fixed field set — script/CSV runs reuse it via
a synthetic `sql` string, no schema migration).

> **CURATION (2026-08-23, binding):**
> 1. **§3-novela reconciliation (supersedes G10 §0's wording where they
>    conflict):** the app-wide write invariant is the PATTERN, not one
>    function: *every* write reaches `Connection::execute` only through (a) a
>    confirm modal showing the exact SQL that will run, (b) a runner-owned
>    method with explicit transaction discipline, and (c) the SHARED
>    read-only guard at the runner choke point. Sanctioned runner write
>    methods after G12: `run_write_transaction` (sandbox Apply + G10 admin),
>    `run_script` (write statements per §2's dispatch matrix), the CSV-import
>    batch runner, `connect_and_run_many` (editor multi-statement — its
>    per-statement read-only rejection counts as (c)). All four MUST call one
>    shared guard helper — no fresh read-only logic per method. Update
>    `execute()`'s doc comment once (per §7) to state the pattern + the
>    sanctioned-caller list.
> 2. **Stale driver claims superseded:** `dbc-driver-mssql` AND
>    `dbc-driver-duckdb` exist as of v0.5.0 (both unwired). `Dialect::Mssql`
>    + the GO line-pre-pass remain a follow-up (correct call). When DuckDB
>    wiring lands, map `Engine::Duckdb → Dialect::Postgres` — DuckDB supports
>    `$$` dollar-quoting and pg-style syntax; add one test for that mapping
>    at wiring time.
> 3. **G6 interaction — interception order in `run_query_with` is fixed as:**
>    param scan/substitution (G6, on the full editor text) → `split_sql` →
>    per-statement guards/auto-limit → dispatch. Rationale: `:name` params
>    must be resolved before splitting so a substituted literal containing
>    `;` inside quotes is handled by the splitter's normal string rules, and
>    the G6 mandatory post-substitution re-scan still runs on the full text
>    before any split. Add a test: two statements each carrying `:p`.
> 4. **REQUIRED tests (read-only discipline, mirrors G9/G10):** (a) script
>    containing a write statement over a `read_only` connection → that
>    statement rejected client-side before the driver, error-policy matrix
>    honored; (b) CSV import entry point hidden/disabled on read-only AND the
>    runtime guard refuses if reached anyway; (c) editor multi-statement
>    `SELECT 1; UPDATE …` on read-only runs the SELECT, stops at the UPDATE.
> 5. **T1 (`dbc-core/split.rs`) is approved to start immediately as an
>    orthogonal parallel track** — zero file overlap with G6's tail or any
>    other in-flight work.

## 1. Statement splitter (`dbc-core`, new module beside `guards.rs`)

- **Location:** `crates/dbc-core/src/split.rs`, `mod split;` added to
  `lib.rs`, exports `pub use split::{Dialect, SplitError, StatementSplitter,
  UnterminatedKind, split_sql};`. Lives in `dbc-core` (not `dbc-ui`) because
  it is pure SQL-text logic with zero GPUI/IO dependency, same rationale as
  `guards.rs` — and because the editor's multi-statement unlock (§4) needs it
  from the same crate `is_read_statement`/`apply_auto_limit` already live in.
- **Relationship to `guards.rs`:** NOT a reuse of `tokenize` — that function
  is private, operates on a whole `&str` in one pass, and has no notion of
  "pause mid-scan, resume on the next chunk". The splitter is a **parallel,
  independently-implemented** state machine that deliberately mirrors
  `tokenize`'s escaping/comment rules (single-quote `''`, double-quote `""`,
  `--` to EOL, `/* */` nesting via a depth counter) so behavior stays
  consistent between "is this read-only" and "where do statements split" —
  documented as a known duplication, not unified, because unifying them
  requires making `guards::tokenize` resumable too, which is out of scope
  here (flagged in §7). A side effect: the splitter's dollar-quote handling
  closes the exact gap `guards.rs`'s doc comment (lines 79–85) already
  names as a known limitation — but `guards.rs` itself is untouched by this
  design (binding constraint: this pass writes docs only); closing that gap
  for real is a follow-up noted in §7, not part of G12.
- **API — push-based incremental lexer over byte chunks:**
  ```rust
  pub enum Dialect { Postgres, Sqlite }

  pub enum UnterminatedKind {
      StringLiteral, QuotedIdent, BlockComment, DollarQuote, TriggerBody,
  }
  pub enum SplitError {
      InvalidUtf8,
      UnterminatedAtEof(UnterminatedKind), // only from `finish()`
  }

  pub struct StatementSplitter { /* opaque */ }
  impl StatementSplitter {
      pub fn new(dialect: Dialect) -> Self;
      /// Feed a chunk of bytes at ANY boundary (mid-token, mid-string,
      /// mid multi-byte UTF-8 char, mid `--`/`/*`/`$tag$` opener — all
      /// carried in internal state across calls). Returns statements that
      /// became complete as a result of this push, in order; empty Vec =
      /// no statement completed yet. `sql_preview`-safe: each returned
      /// String is the exact source text of one statement (interior
      /// comments/whitespace preserved; only leading/trailing whitespace
      /// trimmed), never re-serialized/re-tokenized.
      pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, SplitError>;
      /// Call once after EOF. `Ok(Some(text))` = a final statement with no
      /// trailing `;` (legal — a script's last statement need not end in
      /// one). `Ok(None)` = nothing pending (trailing `;`, or
      /// whitespace/comments only). `Err` = EOF occurred inside an open
      /// construct — fail closed, same posture as `guards::tokenize`
      /// returning `None`.
      pub fn finish(self) -> Result<Option<String>, SplitError>;
  }

  /// One-shot convenience over an in-memory string (editor unlock, §4;
  /// also the natural shape for unit tests) — internally just
  /// `push` + `finish`.
  pub fn split_sql(sql: &str, dialect: Dialect) -> Result<Vec<String>, SplitError>;
  ```
- **UTF-8 chunk-boundary safety:** `push` keeps an internal `Vec<u8>` carry
  buffer (≤3 bytes — max UTF-8 continuation-byte run) for a multi-byte
  sequence split across two chunks; `chunk` is decoded as `carry + chunk`
  via `str::from_utf8`, an incomplete trailing sequence re-enters the carry,
  a genuine invalid sequence returns `SplitError::InvalidUtf8` immediately
  (script files are required to be UTF-8 — non-UTF-8-encoded `.sql` files
  are an explicit v1 non-goal, surfaced as a file-level error before any
  statement runs, not a per-byte guess).
- **Top-level `;` splitting** follows `guards.rs`'s exact rules: `;` inside
  a single-quoted string, a double-quoted identifier, a `--` comment, or a
  (nestable) `/* */` block comment is NOT a split point. No paren-depth
  tracking is added (same as `guards.rs` — ordinary SQL never puts a
  semicolon inside plain parens; the two real exceptions are handled
  explicitly below, matching exactly why `guards.rs` needed a comment-depth
  counter and nothing else).
- **Postgres dialect — dollar-quoting (`Dialect::Postgres` only):** `$tag$`
  (tag = empty or `[A-Za-z_][A-Za-z0-9_]*`) opens a body that runs, literally
  (no nested lexing — not even for `'`/`"`/`--`/`/* */`), until the exact
  same `$tag$` recurs. Lexer detail: seeing a bare `$` starts a bounded
  lookahead buffer (cap 64 chars, generous beyond any real identifier) that
  is either (a) confirmed as a dollar-quote OPEN once a closing `$` is seen
  before any illegal tag character, entering "inside dollar-quote, tag=X"
  mode, or (b) abandoned the moment an illegal-for-a-tag character (or the
  64-char cap) is hit — the buffered text is simply appended to the
  statement's raw text as ordinary characters (a `$` is otherwise
  insignificant punctuation, same bucket `guards.rs` drops parens/commas
  into) and scanning resumes normally from there. This correctly leaves
  Postgres positional parameters (`$1`, `$2`) alone (`$1` never sees a
  closing `$`, so it's abandoned as ordinary text) while still recognizing
  `$$...$$` and `$body$...$body$`. EOF while inside a dollar-quote →
  `UnterminatedAtEof(DollarQuote)`.
- **SQLite dialect — trigger `BEGIN...END` bodies (`Dialect::Sqlite`
  only):** tracked ONLY while the current pending statement's leading
  keywords match `CREATE [TEMP|TEMPORARY] TRIGGER ... TRIGGER` (uppercased
  bare-word scan, same convention as `guards::first_word`/`WRITE_KEYWORDS`)
  — a bare top-level `BEGIN` word seen while in that state opens a
  trigger-body depth counter (starts at 1); a bare top-level `END` word
  decrements it; while depth > 0, top-level `;` is NOT a split point;
  depth reaching 0 lets the statement close normally at its own trailing
  `;`. Depth is a counter (not a bool) defensively mirroring `guards.rs`'s
  block-comment nesting fix, even though SQLite's own trigger grammar
  disallows nested `BEGIN...END` inside a trigger body — costs nothing,
  avoids a latent bug if that grammar assumption is ever wrong. Crucially,
  a STANDALONE `BEGIN;`/`BEGIN TRANSACTION;` ... `COMMIT;` sequence (not
  part of a `CREATE TRIGGER`) is unaffected — the tracking only activates
  when the CURRENT pending statement already started with `CREATE TRIGGER`,
  so ordinary transaction-control statements split exactly as today. EOF
  mid-trigger-body → `UnterminatedAtEof(TriggerBody)`.
- **MSSQL `GO` separator — explicit v1 non-goal.** No `Dialect::Mssql`
  variant exists yet (matches reality: `dbc-driver-mssql` doesn't exist —
  MSSQL is an orthogonal, unscheduled driver phase per the target-UI spec).
  `GO` is fundamentally a **client-tool line convention** (must be alone on
  its own line, case-insensitive, optionally followed by an integer repeat
  count — `GO 5`), not a token-nesting construct like dollar-quoting or
  trigger bodies; when the MSSQL driver phase lands, the right shape is a
  separate LINE-based pre-pass ahead of (or instead of) the char-level state
  machine here, not a bolt-on to this lexer. Flagged as required follow-up
  in §7, not a G12 blocker.
- **Empty-statement dropping:** matches `guards::split_statements` —
  consecutive `;`, or a trailing `;` followed only by whitespace/comments,
  never produces a phantom empty statement.
- **Exhaustive test list** (this is the most unit-testable piece of G12 —
  every case below is one `#[test]`):
  - *Basic splitting:* two simple statements with/without a trailing `;`;
    consecutive `;;;` collapse to nothing extra; input that is only
    whitespace/a comment → `finish()` returns `Ok(None)`, no error.
  - *Strings/idents/comments (shared with `guards.rs`'s discipline):* `;`
    inside a single-quoted string; `''`-escaped quote inside a string
    containing `;`; `;` inside a double-quoted identifier; `;` inside a
    `--` line comment (and the REAL `;` after the comment's newline still
    splits); `;` inside a `/* */` block comment; nested `/* /* */ */`
    matching `guards.rs`'s nesting semantics exactly.
  - *Chunk-boundary safety (the reason this is push-based at all):*
    splitting a keyword across two `push` calls; splitting the two chars of
    `--` across a boundary; splitting `/*` and `*/` across a boundary;
    splitting a `''` escape pair across a boundary (must not falsely close
    the string); splitting a multi-byte UTF-8 character (e.g. `café`)
    across a boundary — reconstructs correctly, no `InvalidUtf8`; feeding
    the identical input as one `push` vs. as many 1-byte `push` calls
    yields identical statement lists (round-trip property test over a
    fixed corpus).
  - *Postgres dollar-quoting:* simple `$$...;...$$`; tagged `$body$...$body$`
    containing internal `;`, `BEGIN`/`END`, even unbalanced quotes — none of
    it is lexed, only the literal closing tag ends it; a `$bar$` appearing
    mid-body does NOT close a `$foo$`-tagged body (exact-tag matching);
    two independent dollar-quoted bodies in one statement; positional
    parameters `$1`, `$2` are NOT mistaken for an unterminated dollar-quote;
    unterminated dollar-quote at EOF → `UnterminatedAtEof(DollarQuote)`.
  - *SQLite triggers:* single-statement `BEGIN...END` trigger body with one
    interior `;`; multiple interior statements; a `WHEN <cond> BEGIN...END`
    trigger (condition text doesn't confuse tracking); lowercase
    `create trigger ... begin ... end;` (case-insensitivity, matching
    `guards.rs`'s uppercasing convention); a standalone `BEGIN; ...;
    COMMIT;` (NOT inside a `CREATE TRIGGER`) still splits into 3 ordinary
    statements; unterminated trigger body at EOF →
    `UnterminatedAtEof(TriggerBody)`.
  - *Dialect isolation:* `Dialect::Sqlite` treats a literal `$$foo$$;` as
    ordinary text (no dollar-quote handling) and splits normally;
    `Dialect::Postgres` applies no `BEGIN`/`END` trigger-body tracking (pg's
    own `CREATE TRIGGER ... EXECUTE FUNCTION ...` form has no such body at
    the top level, so nothing to track).
  - *Invalid input:* unterminated string/quoted-ident/block-comment at EOF
    → the matching `UnterminatedKind`; a chunk containing invalid UTF-8 →
    `SplitError::InvalidUtf8`.

## 2. Execution model (`dbc-ui`, new `QueryRunner::run_script` — sibling of
### `run_write_transaction`, not a reuse of it)

- **Reuse vs. sibling — decision: sibling.** G5 Task 4's
  `run_write_transaction(spec, statements) -> oneshot::Receiver<Result<u64,
  QueryError>>` is a single oneshot result for a FIXED, always-stop,
  always-single-transaction sequence (the sandbox Apply flow never needs
  per-statement progress or a choice of error policy). `run_script` needs
  streaming per-statement/per-file progress, a configurable transaction
  scope, and a configurable error policy — different enough in shape that
  forcing reuse would either bloat `run_write_transaction`'s signature for
  its one real caller or make `run_script` fake a oneshot-of-Vec result and
  lose live progress. No low-level code is forced to merge either; if a
  future review finds the BEGIN/…/COMMIT/ROLLBACK-with-pg/sqlite-divergence
  logic duplicated between the two, that's a mechanical refactor at
  implementation time, not a design constraint here.
- **Transaction scope — three v1 options, exposed in the confirm modal:**
  `žádná transakce` (none — each statement autocommits individually),
  `transakce na soubor` (per-file — default; `BEGIN` before a file's first
  dispatched statement, `COMMIT` after its last, one file = one atomic
  unit), `jedna transakce na celý běh` (whole-run — a single `BEGIN`
  spanning every file/statement). **Whole-run is only selectable when error
  policy = stop** (grayed out otherwise): `connection.rs`'s documented
  invariant is that a transaction driver "must stop at the FIRST error and
  roll back" because Postgres aborts the whole transaction server-side on
  the first failing statement — a "continue past an error" policy is
  logically incompatible with "everything is one open transaction" on
  Postgres, and v1 disables the combination uniformly across engines
  (SQLite technically tolerates continuing inside an open tx per that same
  doc comment, but the UI doesn't special-case it — one rule, not
  per-engine exceptions, keeps the modal simple).
- **Error policy — `Stop` (default) | `Continue`, interacting with tx scope
  as a fixed matrix (no other combinations exist):**
  | tx scope | Stop | Continue |
  |---|---|---|
  | none | abort run at first error | log error, advance to next STATEMENT |
  | per-file | rollback current file, abort whole run | rollback current file, skip its remaining statements, advance to next FILE |
  | whole-run | rollback everything, abort run | *(unavailable — grayed out)* |
- **Dispatch policy per statement (read-only-flag interaction — decided,
  not "blocks entirely"):** running a read-only script against a
  `read_only` connection is legitimate and NOT blocked outright. Each
  statement is classified with the EXISTING `dbc_core::is_read_statement`
  before dispatch: if it passes, the statement runs via `Connection::
  query()` (the read path — rows are drained for a row COUNT only, same
  drain-and-count shape `fetch_lookup_inner` already uses, never rendered);
  if it fails AND the connection is `read_only`, the statement is REJECTED
  client-side with `QueryError::msg("connection is read-only")` — a
  statement-level failure, subject to the SAME error-policy matrix above
  (so `Continue` skips just that one write statement and keeps running the
  script's read statements); if it fails and the connection is NOT
  `read_only`, the statement runs via `Connection::execute()` (the write
  path) inside whatever tx scope is active. This reuses both existing
  `Connection` trait methods with no new trait surface, and matches the
  binding constraint's own hint ("use `is_read_statement` per statement").
- **Progress event channel — streaming `mpsc`, same convention as
  `QueryEvent`/`CHANNEL_CAPACITY` in `runner.rs`:**
  ```rust
  pub enum ScriptEvent {
      FileStarted { path: PathBuf, index: usize, total_files: usize },
      StatementStarted { stmt_index: usize, stmt_total_in_file: usize, sql_preview: String },
      StatementFinished { stmt_index: usize, affected: Option<u64>, elapsed: Duration },
      StatementFailed { stmt_index: usize, error: QueryError },
      FileFinished { path: PathBuf, statements_run: usize, statements_failed: usize, elapsed: Duration },
      RunFinished { files_run: usize, statements_run: usize, statements_failed: usize, elapsed: Duration, aborted: bool },
  }
  pub struct ScriptRunOptions {
      pub tx_scope: TxScope,          // None | PerFile | WholeRun
      pub error_policy: ErrorPolicy,  // Stop | Continue
      pub dialect: split::Dialect,
      pub statement_timeout_secs: Option<u64>, // from the connection's existing cfg.timeout_secs
  }
  pub fn run_script(
      &self, spec: ConnectSpec, files: Vec<PathBuf>, opts: ScriptRunOptions, cancel: CancelToken,
  ) -> tokio::sync::mpsc::Receiver<ScriptEvent>;
  ```
  `sql_preview` is the statement text truncated to a fixed cap (e.g. 200
  chars) for the log line — full text is never needed downstream since
  results aren't rendered, only counted.
- **Streaming file read — never whole-file-in-memory:** each `.sql` file is
  read via a `tokio::fs::File` + fixed-size chunk buffer (e.g. 64 KiB) fed
  straight into that file's own `StatementSplitter::push`; a statement
  completing triggers an immediate dispatch (no buffering of "all
  statements of a file" before running any of them) — this is what makes
  "streamed statement-by-statement" real rather than a description of a
  pre-split `Vec<String>`.
- **Cancellation:** one script run at a time, reusing the existing
  single-flight `self.cancel`/Esc convention `run_query_with` already
  establishes. `cancel` is checked before each statement dispatch (between
  statements/files) AND passed into the in-flight `query()`/`execute()`
  call for protocol-level cancel of whatever statement is currently
  running — same two-tier checking discipline `connect_and_run` already
  documents (checked at checkpoints, not mid-blocking-call).
  Cancellation while inside an open tx (per-file or whole-run scope)
  triggers the SAME rollback path as a statement error.
- **Timeout — per statement, not per run.** A whole-run timeout would be
  actively hostile (scripts can legitimately run for a long time); instead
  the connection's existing `cfg.timeout_secs` bounds EACH statement
  individually via `tokio::time::timeout`, mirroring `connect_and_run`'s
  own watchdog `tokio::select!` shape. A per-statement timeout is reported
  as an ordinary `StatementFailed` (`QueryError::msg("[timeout] statement
  exceeded {t}s")`), subject to the same error-policy matrix as any other
  failure.

## 3. UI

- **Entry points:** "Spustit SQL soubor…" / "Spustit SQL složku…" — new
  actions in the Ctrl+K palette (gated on an active, non-modal state, same
  guard `on_open_palette` already applies) and a small toolbar affordance
  next to Ctrl+Enter. File picker: GPUI's open-file dialog (the
  `prompt_for_paths` sibling of `grid.rs::start_export`'s
  `cx.prompt_for_new_path`), filtered to `*.sql`, single selection. Folder
  picker: same call with `directories: true`. No Downloads-style fallback
  on a failed/cancelled OPEN dialog (unlike export's save-fallback — there
  is nothing to write, so a failed/cancelled open just aborts with a status
  note, same as `start_export`'s own cancel branch).
- **Folder semantics:** non-recursive (subfolders ignored — explicit v1
  non-goal, easy to add as a checkbox later if requested); only entries
  matching `*.sql` (case-insensitive extension) are included; ordered by
  `file_name()` string comparison, NOT full path (matches the spec's "files
  in name order").
- **Pre-scan (statement count estimate):** before showing the confirm
  modal, each selected file is run once through its own
  `StatementSplitter` (streaming, same chunked reader as the real run) SOLE
  to produce an exact statement count — called an "estimate" in the UI
  copy only because a file that turns out to end mid-construct will fail at
  RUN time with the same `UnterminatedAtEof` the pre-scan already would
  have hit (so in practice it's exact, but the label sets expectations
  correctly for that edge). This is a second sequential read of each file
  (accepted cost — local `.sql` files, not multi-GB; flagged as a thing to
  revisit in §7 if it ever matters for huge scripts).
- **Confirm modal (write path — REQUIRED before every run, both read-only
  and writing connections, for UI-consistency with every other
  "review-before-write" flow in the app):** new `ModalState::ScriptRun`
  variant (`connections_ui.rs`'s enum grows a variant, same `.occlude()`
  overlay shape as the other three). Shows: the file list (path + pre-scan
  statement count each), total statement count, target connection name +
  read-only badge if applicable, and THREE controls — transaction scope
  radio (§2), error policy radio (§2, whole-run+continue combination
  disabled per the matrix), per-statement timeout display (read from
  `cfg.timeout_secs`, not editable here — it's a connection-level setting).
  "Spustit" / "Zrušit" buttons; not Esc-closable once running (same
  "unsaved/in-flight state blocks Esc" precedent as the master-password
  modal).
- **Progress surface — new result-tab kind, not a modal, once running**
  (decision: like G9's proposed dashboard tab, a special `TabContent`
  variant fits a long-lived streaming log far better than a modal that
  must stay pinned to the foreground): `TabContent::ScriptRun { state:
  Rc<RefCell<ScriptRunState>> }` added to `tabs.rs`'s enum alongside `Grid`
  and `Text`. Renders: a summary bar (files done/total, statements
  done/total, elapsed, a "Zrušit" button while running, replaced by
  "Hotovo"/"Selhalo"/"Zrušeno" once terminal), a per-file list (filename +
  status glyph ▶ running / ✓ done / ✗ failed / ⊘ skipped, with its own
  statement counts), and a scrolling monospace log below (one line per
  `StatementStarted`/`Finished`/`Failed` event, auto-scrolled, e.g. `✓ #12
  UPDATE … (3 řádky, 4 ms)` / `✗ #13 INSERT … — chyba: …`). The modal
  CLOSES once "Spustit" is confirmed — the tab is what stays open and is
  the run's only ongoing UI surface (matches the existing "every execution
  opens a result tab" convention almost exactly, just with different tab
  content). Title: `"Skript: {filename}"` or `"Skript: {foldername}/ ({n}
  souborů)"`. Subject to the same `TAB_CAP` eviction as every other tab
  (oldest-unpinned first) — pinnable to survive that if the user wants a
  long-running log kept around.
- **History entry — file path + stats, not content (binding constraint,
  literally satisfied by field reuse, no schema migration):** ONE
  `HistoryEntry` per RUN (not per file/statement). `sql` field holds a
  synthetic description, never file contents: `"[skript] {path} — {n}
  souborů, {m} příkazů, {ok} OK, {fail} chyb"` (single file: `"[skript]
  {path} — {m} příkazů, {ok} OK, {fail} chyb"`). `row_count` repurposed to
  hold total affected rows across the run; `duration_ms`/`connection`/
  `error` (run-level, if aborted) populate exactly as an ad-hoc query run
  would. History panel needs no new rendering — a `[skript]`-prefixed `sql`
  string already displays sensibly through the existing fulltext-search/
  click-to-load path (clicking it is a no-op for "load into editor" in
  practice since it's a description, not real SQL — acceptable, same as
  clicking to re-load any historical multi-statement text would just put
  text in the editor without re-running it).

## 4. Editor multi-statement unlock

- **Trigger:** `run_query_with`'s dispatch is extended to first run the
  active connection's `split::split_sql(sql, dialect)` (dialect from
  `Engine` — `Postgres`/`Sqlite`; unsupported engines, i.e. no driver yet,
  fall back to today's single-statement path unchanged) BEFORE the
  existing read-only/auto-limit guards. A `SplitError` (invalid UTF-8 is
  moot for in-memory editor text; `UnterminatedAtEof` from a genuinely
  unterminated string/comment/dollar-quote/trigger-body) surfaces exactly
  like today's "query failed" status line — fail closed, no statements run.
- **Guards apply per statement, not to the blob:** `is_read_statement`
  already validates a `;`-batch statement-by-statement internally (see
  `guards.rs`'s `multi_statement_batch_requires_every_statement_to_be_read`
  test) — unchanged, still called on the ORIGINAL full text for the
  read-only gate. `apply_auto_limit` changes from "once over the whole
  string" to "once per split statement" — only bare `SELECT` statements in
  the split list get a LIMIT appended; this is a pure behavior improvement
  (today a multi-statement blob never got auto-limited at all, since
  `apply_auto_limit` only fires when `first_word == SELECT` for the WHOLE
  string).
- **Sequencing — decision: STOP on first error, no continue option.**
  Unlike the script runner, the editor keeps today's simple "a run either
  finishes or fails" UX — error-policy choice is a script-runner-only
  concept; adding it to the editor would be new surface area for a feature
  (multi-statement paste/typing) that's meant to feel like "the same
  Ctrl+Enter, just smarter", not a mini script runner.
- **Execution — new sibling runner method, one connection, N statements,
  streamed per-statement events:**
  ```rust
  pub enum MultiQueryEvent {
      StatementStarted { index: usize, total: usize, columns: Option<SchemaRef> }, // None = non-row statement
      Batch(RecordBatch),   // belongs to the most recent StatementStarted with columns: Some
      StatementFinished { index: usize, affected: Option<u64>, elapsed: Duration },
      StatementFailed { index: usize, error: QueryError },
      RunFinished,
  }
  pub fn connect_and_run_many(
      &self, spec: ConnectSpec, statements: Vec<String>, cancel: CancelToken, timeout_secs: Option<u64>,
  ) -> tokio::sync::mpsc::Receiver<MultiQueryEvent>;
  ```
  Dispatch per statement: read-only-passing statements (or any statement on
  a non-read-only connection that AREN'T rejected) run via `query()`
  (columns present → rows stream as today); on a `read_only` connection a
  write statement is rejected the same way `run_query_with`'s existing
  guard 1 already does today, just now per-statement instead of for the
  whole blob (so `SELECT 1; UPDATE t SET x=1` on a read-only connection
  runs the `SELECT` and then stops with the existing "connection is
  read-only" error on the second statement — consistent with "stop on
  first error").
- **Result-tab policy — decision: one tab per statement that PRODUCES ROWS,
  opened in sequence; non-row statements don't open a tab.** This is the
  closest fit to the existing "every execution opens a result tab"
  convention without exploding `TAB_CAP` on a large pasted script (writes
  produce no rows to show anyway) and without silently discarding
  intermediate `SELECT`s the way a "last-result-only" policy would. Each
  opened tab's title uses `collapse_title` on THAT statement's text (not
  the whole blob), exactly like today's per-run title. Non-row statements'
  affected-row counts and any run-level failure surface as a single status
  bar line once `RunFinished`/the aborting `StatementFailed` arrives (e.g.
  `"3 příkazy, 2 s výsledky, 1 ovlivněn (5 řádků) — hotovo"` or `"…selhalo
  na příkazu #2: …"`). History records ONE entry for the whole multi-
  statement run (`sql` = the original full editor text, unchanged from
  today's single-statement recording shape).

## 5. CSV import

- **Location:** pure model in new `crates/dbc-ui/src/csv_import.rs`
  (mirrors `sandbox.rs`'s "pure model, GPUI-free, exhaustively tested" split
  — no dependency on `dbc-core`'s new `split.rs`, CSV parsing has none of
  SQL's dialect/nesting complexity). New dependency: the `csv` crate (RFC
  4180-compliant, quote-aware, and — critically — its `Reader::records()`
  iterator is ALREADY incremental/streaming over a `BufReader`, so no
  bespoke incremental state machine is needed here the way §1 needed one
  for SQL (the streaming requirement is satisfied by picking a streaming
  parser, not by hand-writing one).
- **Entry points:** "Import CSV…" on a table in the schema tree (target
  table pre-selected) and on an open PREVIEW tab's toolbar (target = that
  tab's table). Disabled/hidden entirely when the active connection is
  `read_only` — CSV import is a write path (INSERT-only), same "grayed out,
  not silently allowed" posture as every other write action gated on that
  flag.
- **File picker + header peek:** GPUI open-file dialog, `*.csv` filter,
  single file. After selection, a `spawn_blocking` peek reads ONLY the
  first line (header row) — not the whole file — to populate the mapping
  UI. **v1 requires a header row** (first CSV line = source column names);
  headerless CSV is an explicit non-goal (documented in the mapping modal's
  helper text).
- **Column mapping modal — new `ModalState::CsvImport` variant:** left
  column = CSV headers (from the peek); each gets a dropdown of target
  table columns (from `SchemaSnapshot`/`TableInfo.columns`, same source
  `detect_editable_pk` already reads) plus a `"(přeskočit)"` skip option.
  Target columns left unmapped by every CSV header take the table's default
  (or NULL if nullable with no default — no client-side NOT-NULL
  pre-validation; an unmappable NOT-NULL column with no default simply
  fails at INSERT time like any other constraint violation, surfaced
  through the normal error-policy path below — "errors are values", not a
  pre-flight schema audit). No PK requirement (unlike sandbox editing) —
  CSV import is pure `INSERT`, it never needs a `WHERE`.
- **Type coercion — text via existing `sql_value` quoting (per binding
  constraint), with its OWN numeric-column source:** reuses
  `sandbox::sql_value(v: Option<&str>, numeric: bool)` UNCHANGED for value
  emission. `numeric` here can't come from an Arrow result schema (no query
  has run) — instead a new pure helper `fn is_numeric_type_name(data_type:
  &str) -> bool` classifies each target column from its CATALOG
  `ColumnInfo.data_type` string (case-insensitive match against known
  numeric type-name substrings per engine: `int`, `serial`, `numeric`,
  `decimal`, `real`, `double`, `float` for Postgres; `INTEGER`, `REAL`,
  `NUMERIC` for SQLite's type-affinity names). Fail-closed like every other
  guard in this codebase: an unrecognized/unknown type name defaults to
  `false` (quoted-as-text) — always syntactically safe, worst case an
  unnecessary quote the server coerces away.
- **NULL handling for empty (per binding constraint):** relies on the `csv`
  crate's own quote-awareness, NOT a naive "empty string == NULL" rule: an
  UNQUOTED empty field (`a,,c`) maps to SQL NULL; a QUOTED empty field
  (`a,"",c`) maps to an empty STRING — the same NULL-vs-empty-string
  distinction the sandbox cell editor already makes explicit (§ G5 design
  pass), now expressed through CSV's own quoting convention instead of a UI
  toggle.
- **Batched multi-row INSERTs:** fixed `const CSV_IMPORT_BATCH_SIZE: usize
  = 500;` (same "fixed constant, not user-tunable in v1" posture as
  `TAB_CAP`/`LOOKUP_ROW_CAP` elsewhere in this codebase) rows per
  `INSERT INTO t (c1, c2, …) VALUES (…), (…), … ;` statement, one
  `Connection::execute()` call per batch. 500 is also the reason mid-batch
  cancellation isn't attempted (see below) — small enough that a batch's
  round-trip is never long enough to make waiting for it out feel broken.
- **Transaction scope — decision: always ONE transaction for the whole
  import (not configurable, unlike script runner):** simpler and safer for
  a single-table bulk load than script runner's per-file/continue nuance;
  matches the app's "confirm SQL → single transaction" precedent from G5's
  Apply flow. Any batch failure → ROLLBACK the entire import (zero rows
  committed), error shown, no partial data — CSV import never partially
  succeeds.
- **Confirm modal — sample INSERT + row count (per binding constraint):**
  before starting, the file is read once, fully, through the SAME streaming
  `csv::Reader` used for the real import, SOLELY to count rows (accepted
  extra sequential read, same "one extra pass is an accepted cost" call as
  §3's SQL statement-count pre-scan — never holds the file in memory, just
  counts). Modal shows: file path, target table, column mapping summary,
  exact row count, batch size, and the FIRST batch's fully-generated
  `INSERT` statement text (verbatim, monospace, scrollable) as the "sample"
  — not a synthetic single-row example, the REAL first statement that will
  run, same "show the exact generated SQL" posture as G5's Apply dialog.
  "Spustit import" / "Zrušit".
- **Progress + abort:** reuses the `ScriptEvent`-shaped streaming
  convention from §2, specialized to batches instead of statements/files
  (`BatchStarted { batch_index, rows_in_batch }`, `BatchFinished {
  batch_index, rows_committed_so_far }`, `Failed { error }`, `Finished {
  rows_imported, elapsed }`) rendered in the SAME new `TabContent::
  ScriptRun`-family tab kind as §3 (a progress bar this time IS honest and
  meaningful, unlike G11's backup progress — total row count is known
  upfront from the pre-count). Cancellation is checked BETWEEN batches only
  — a batch already dispatched to `execute()` runs to completion
  uninterrupted, the same accepted limitation `connection.rs`'s `execute()`
  doc comment already states ("no mid-statement interrupt needed for v1 —
  statements are tiny") — CSV batches are larger than a sandbox Apply
  statement but still bounded by the fixed 500-row cap, keeping worst-case
  cancel latency to one batch's round-trip.
- **Read-only respected:** gated at the entry point (menu item
  disabled/hidden), not just at execution time — consistent with how the
  rest of the app treats `read_only` as a UI-level gate first, a
  belt-and-braces runtime guard second.
- **History entry:** one `HistoryEntry` per import: `sql = "[CSV import]
  {file} → {table} ({n} řádků, dávka {batch_size})"`, `row_count = n`
  (rows actually committed — `0` on a rolled-back failure), `error` set on
  failure, same field reuse as §3 (no schema migration).

## 6. Task decomposition

- **T1 — Statement splitter (`dbc-core`, pure).** `split.rs`: `Dialect`,
  `StatementSplitter`, `SplitError`, `split_sql`, the full §1 test list.
  Zero dependency on anything else in this phase; blocks T2, T4, T5.
- **T6 — CSV pure model (`dbc-ui`, pure), independent of T1.** `csv_import.
  rs`: `is_numeric_type_name`, the header→column mapping data structure,
  batch-SQL generation over `sandbox::sql_value`/`quote_ident`, NULL-vs-
  empty via the `csv` crate's quote-awareness — fully unit tested against
  canned CSV byte strings, no filesystem/DB. Can start immediately in
  parallel with T1 (shares nothing but the already-existing `sandbox.rs`
  helpers).
- **T2 — `run_script` execution engine (`dbc-ui/src/runner.rs`).**
  `ScriptEvent`, `ScriptRunOptions`, `TxScope`/`ErrorPolicy`, the dispatch-
  policy matrix (§2), streaming file reads feeding per-file
  `StatementSplitter`s. Depends on T1 (splitter) + the already-existing
  `Connection::execute`/`query`. Independent of T4–T7.
- **T5 — Editor multi-statement unlock (`dbc-ui/src/runner.rs` +
  `main.rs`).** `connect_and_run_many`/`MultiQueryEvent`, per-statement
  auto-limit, per-statement-producing-rows tab-open loop (§4). Depends on
  T1 only; fully independent of T2/T3/T4/T6/T7 (a separate execution path
  sharing just the splitter) — safe to build in parallel with the script
  runner proper.
- **T3 — Script runner UI (`dbc-ui/src/main.rs`, `tabs.rs`,
  `connections_ui.rs`).** File/folder picker + pre-scan (§3), `ModalState::
  ScriptRun` confirm modal, `TabContent::ScriptRun` progress tab rendering,
  history recording (§3's field-reuse convention). Depends on T1 (pre-scan)
  + T2 (event shape). Independent of T5/T6/T7.
- **T7 — CSV import UI (`dbc-ui/src/main.rs`, `connections_ui.rs`).** File
  picker + header peek, `ModalState::CsvImport` mapping + confirm modal,
  row pre-count, batched-execute runner method (reuses §2's `ScriptEvent`-
  shaped channel per §5), progress rendering (reuses T3's `TabContent::
  ScriptRun` tab kind — small dependency on T3's tab-kind existing, but its
  OWN modal/mapping logic is independent). Depends on T6 (pure model) +
  loosely on T3 (shared tab kind, can stub against T2's event shape early
  and wire the shared tab kind in last).
- **T4 — final integration sweep.** Toolbar/palette entries for all three
  new entry points wired together; `execute()`'s doc comment amended (see
  §7 — it currently says "ONLY the sandbox Apply flow may call it", now
  false for the third time counting G11's design pass); zero-warnings +
  full test sweep; version bump per the target-UI spec's phase-completion
  convention. Depends on T2, T3, T5, T7 all landing.

**Parallelization:** T1 and T6 start immediately (no shared dependency).
Once T1 lands, T2 and T5 proceed in parallel (two independent execution
paths sharing only the splitter). T3 starts once T2's event shape is fixed;
T7 starts once T6 lands and can develop its modal/mapping independently of
T3, wiring into T3's shared tab kind only at the end. T4 is the integration
tail after T2, T3, T5, T7 are all individually done.

## 7. Risks / needs-verification

- **`guards.rs`'s dollar-quote gap stays open.** This design's splitter
  closes it for splitting purposes, but `is_read_statement`/
  `apply_auto_limit` themselves are untouched (out of scope for a
  docs-only design pass) — a `$$...$$` body containing a bare write
  keyword can still cause `is_read_statement` to fail-closed-reject a
  statement that's actually safe (documented existing behavior, not a new
  regression), and the reverse false-safe direction guards.rs's own doc
  comment already rules out. Unifying the two lexers (making
  `guards::tokenize` resumable and delegating to it from `split.rs`, or
  vice versa) is a clean follow-up, not designed further here.
- **`execute()`'s doc comment is now stale a second time over.** It reads
  "This is the app's write path — ONLY the sandbox Apply flow may call
  it." G11's design pass already flagged this as stale (MSSQL backup/
  restore); G12 adds a THIRD and FOURTH caller (script runner's write
  statements, CSV import's batched INSERTs). Whichever phase lands first
  should update the comment to name all sanctioned callers instead of
  re-declaring exclusivity each time; flagged here so it isn't mistaken for
  an oversight during G12 implementation.
- **NEEDS VERIFICATION: GPUI's open-file/open-folder dialog exact
  behavior** at the pinned rev (907ed09) — `start_export` only exercises
  the SAVE dialog (`prompt_for_new_path`); this design assumes a
  `prompt_for_paths`-shaped sibling exists with an equivalent
  `Receiver<Result<Result<Option<Vec<PathBuf>>, E>, Canceled>>` shape and a
  `directories: true` mode for folder selection on Windows — needs a spike
  at T3's start, before the picker UI is built against assumed signatures.
- **CSV/script pre-scan doubles file I/O** for both features (one pass to
  count, one to execute) — acceptable for realistic local file sizes;
  flagged to revisit (e.g. show "?" instead of an exact count, or fold
  counting into a progress-bar-less first pass of the real run) if it ever
  becomes a real complaint for very large inputs.
- **Fixed batch size (500) and fixed pre-scan/log caps are not
  user-tunable in v1** — consistent with this codebase's existing posture
  on similar constants (`TAB_CAP`, `LOOKUP_ROW_CAP`), but a very wide table
  (many columns) could make a 500-row multi-VALUES `INSERT` bump into a
  driver/server statement-size limit; no pre-flight check for that exists,
  it would surface as an ordinary batch failure (whole import rolls back)
  — flagged as a possible follow-up (adaptive or configurable batch size),
  not solved here.
- **Cancellation granularity matches `execute()`'s existing v1 limitation**
  (no mid-statement/mid-batch interrupt) — re-confirm this design's
  "acceptable because batches/statements are bounded and small" reasoning
  once real-world script/CSV sizes are exercised in testing; if statements
  or batches turn out to run long in practice, this is the first place to
  revisit.
- **Editor multi-statement unlock changes user-visible failure behavior**
  for any SQL that previously errored at the driver's own "multi-statement
  text rejected" layer (per `guards.rs`'s doc comment, both current drivers
  already fail closed on that today) — after T5, such text now actually
  RUNS, statement by statement. Low risk (strictly a capability unlock, not
  a behavior change for single-statement input) but worth a explicit
  regression pass over any existing saved/starred history entries that
  might contain multi-statement text during T5's testing.
