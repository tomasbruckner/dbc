# G15 MSSQL Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Recommend **sonnet** implementers for every task, a **sonnet** adversarial code review per task before it's considered done, and a **default-model** final review once all tasks land (same staffing convention as the G9/G13 plans). T8 additionally requires a REAL machine with Docker Desktop + ODBC Driver 18 — its keystone matrix result gates every feature-ON flip; do not dispatch T8's flip steps to a worker that cannot run docker.

**Goal:** Make the existing-but-unreachable MSSQL support real. `dbc-driver-mssql` (odbc-api 29.0.0) exists and is unit-tested but unwired: `connect::open_config`'s `Engine::Mssql` arm hard-errors, and a backlog of MSSQL-shaped code from G9–G13 (monitor SQL, admin catalogs/builders, backup/restore T-SQL, Showplan parser, bracket quoting) has never touched a live server. G15 wires the driver in (connection-string only, eager `probe()`, SQL auth only, SSH refused), dialectizes every app-side SQL composer through a single `dbc_core::Dialect` authority (brackets, `N''` literals, GO-batch splitting, `TOP n`), fixes G12's bare-`BEGIN` bug via fused `SET XACT_ABORT ON; BEGIN TRANSACTION` helpers, delivers G13 T7 (Showplan via driver session preludes), wires the monitor for real, and runs the entire never-live backlog against a dockerized SQL Server before ANY feature gate flips.

**Architecture:** Three moves in dependency order (design §0): (1) **Wire** — `open_config`'s `Engine::Mssql` arm builds `MssqlConfig` from saved config + vault secret and returns an `MssqlConnection` after an eager `probe()`; the driver already implements the full `Connection` trait (blocking-internally via `spawn_blocking`), so runner/UI transport needs zero changes. (2) **Dialectize** — `dbc_core::Dialect` (today `{Postgres, Sqlite}` in `split.rs`) is promoted to THE app-wide dialect authority: it gains `Mssql`, and every SQL composer (sandbox Apply, CSV import, preview/fk-join/diff SELECTs, auto-limit, transaction control, script splitting, plans) takes it as a parameter. `Engine` stays a config/state concept; the one Engine→Dialect mapping lives in `main.rs`. (3) **Verify** — the accumulated never-live MSSQL SQL plus the new transactional semantics (§3c XACT_ABORT matrix — the phase KEYSTONE) run against a dockerized SQL Server via `testcontainers-modules`' `mssql_server` module before any ON-flip merges. The driver crate gains exactly TWO public items (`probe()`, `query_with_session()`); the `Connection` trait in `dbc-core` is untouched; dbc-mcp stays gated (curation item 5).

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no new GPUI primitive: the connection dialog's conditional-row idiom, `checkbox`/`field_row` helpers, and `cx.spawn` are all already demonstrated in `connections_ui.rs`/`main.rs`), `odbc-api = "29"` (already pinned in `dbc-driver-mssql`, resolves 29.0.0; `Cursor::more_results(self) -> Result<Option<Self>, Error>` confirmed against the vendored source at `odbc-api-29.0.0/src/cursor.rs:102/382` during G13), `testcontainers-modules = { version = "0.13", features = ["mssql_server"] }` — **VERIFIED during plan drafting** against the vendored registry source (`testcontainers-modules-0.13.0/src/mssql_server/mod.rs`): the module exists at the workspace's existing 0.13 pin (design risk item "confirm the module exists" resolves as *no crate bump needed*), `MssqlServer::default()` uses image `mcr.microsoft.com/mssql/server:2022-CU14-ubuntu-22.04` with `DEFAULT_SA_PASSWORD = "yourStrong(!)Password"`, and **`.with_accept_eula()` is a REQUIRED explicit call** (the `Default` impl sets `MSSQL_SA_PASSWORD` + `MSSQL_PID=Developer` but NOT `ACCEPT_EULA` — the container will not start without it; the design's "the module handles ACCEPT_EULA" is true only through that explicit call).

**Spec:** `docs/superpowers/specs/drafts/g15-mssql-wiring-design.md` — binding, including its CURATION block (5 items) and §8's pre-documented autocommit fork. Every API claim below is grounded against the actual code of this worktree (branch `feature/g15-mssql-wiring`, off main v0.15.0): `crates/dbc-driver-mssql/src/{lib.rs,config.rs}`, `crates/dbc-core/src/{split.rs,ddl.rs,guards.rs,connection.rs,lib.rs}`, `crates/dbc-ui/src/{connect.rs,connections_ui.rs,runner.rs,main.rs,sandbox.rs,csv_import.rs,fk_join.rs,admin_sql.rs,admin_panel.rs,monitor.rs,monitor_sql.rs,plan.rs,backup.rs}`, `crates/dbc-state/src/config.rs`. Re-locate every line reference below **by symbol, not line number** if any other branch merges first.

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched. New pub items get doc comments; nothing lands behind `#[allow(dead_code)]` except where a task explicitly says so with a removal owner named.
- GPUI pin `907ed09c9f4476caf250e6ce4bbffb23b4622f3b` — no GPUI upgrade, no new primitives.
- **Write invariant (§3-novela, binding project-wide):** every write reaches `Connection::execute` only through (a) a confirm modal showing the exact SQL, or (b) a sanctioned runner-owned method with explicit transaction discipline, and (c) the SHARED read-only guard at the runner choke point (`runner::guard_not_read_only`/`spec_is_read_only`). **G15 adds NO new sanctioned member** — it makes existing members dialect-correct (T5) and adds one engine branch to an existing member (`run_mssql_plan` is the MSSQL face of the already-sanctioned analyze-write sequence; `execute()`'s sanctioned-caller doc list in `dbc-core/src/connection.rs` gains exactly that one line, T7).
- **Security — passwords (PGPASSWORD-analog rules for ODBC):** vault-only at rest (AES-GCM via `Vault::set_secret`, unchanged); in memory the password lives ONLY in the ODBC connection string built at open time (`MssqlConfig::to_connection_string` → `MssqlConnection.conn_str`), for the connection's lifetime; **never persisted, never logged, never formatted into any error** (`odbc_err` renders driver diagnostic records only; `open_config` never formats `secret` into a string). `escape_odbc_value`'s brace-wrapping makes hostile passwords round-trip. **No DSN, permanently** — a User/System DSN is on-disk state that invites storing the password outside the vault. **Redaction is REQUIRED with a negative test** (T8: a failed `probe()` against a wrong password produces an error that does NOT contain the password text). `ConnectionFormData`'s hand-written redacting `Debug` is extended for every new field (T3).
- **No silent encryption downgrade:** `encrypt` defaults ON; `TrustServerCertificate` is NEVER auto-enabled — the dev-server/self-signed case (including EVERY docker test instance) requires the explicit dialog checkbox. A cert-validation failure (SQLSTATE 08001, "certificate chain" text) is surfaced verbatim; no auto-retry-with-trust, ever (that would be a silent MITM downgrade).
- **Read-only honesty (driver integration note 5):** there is NO server-side read-only mode for MSSQL (`ApplicationIntent=ReadOnly` only routes AG secondaries). Client-side `is_read_statement` + the shared runner guard are the ONLY enforcement for MSSQL, unlike pg (`default_transaction_read_only=on`) and sqlite (`SQLITE_OPEN_READ_ONLY`). Every place that documents a "server-side backstop" gets an MSSQL exception note (T3 arm doc, T4 `run_query_with` Guard 1 comment, T7 plan.rs defense-in-depth paragraph, T3 dialog hint `"u MSSQL vynuceno pouze na straně klienta"`). REQUIRED tests: a read-only MSSQL config refuses a write via the shared guard without connecting (T5), and the backup-on-read-only exemption / restore hard-block matrix stays green (existing tests, retargeted in T8).
- **Deep-recursion rules:** no new recursive tree code anywhere. `StatementSplitter` stays a flat char-at-a-time state machine (the new bracket/GO modes are plain `Mode` variants, no nesting stack beyond the existing depth counters); `parse_mssql_xml` keeps its iterative frame-stack + `MAX_XML_DEPTH` cap from G13 — fixture corrections in T8 must not restructure it recursively.
- **Single-writer serialized files:** `runner.rs`, `main.rs`, and `connections_ui.rs` are single-writer across tasks. Consequently (RESOLVED against the design's §7 hint, which predates the ground-truth finding that `run_monitor_refresh`, `compose_diff_select`, and `auto_limit_each`'s call paths live where they do): **T5 → T6 → T7 → T8 serialize on `runner.rs`**; **T4 → T7 → T8 serialize on `main.rs`**; `connections_ui.rs` is touched only by T3. Additionally, **T3 runs SOLO before T4/T5** — its `ConnectionConfig` field addition compile-breaks every test struct literal in `runner.rs`/`main.rs`, and T3's own mechanical `mssql: None` sweep is what fixes them (a second RESOLVED deviation from the design's "then in parallel" hint). T4 ∥ T5 remain disjoint and parallel.
- **Docker test conventions:** in `dbc-ui`, docker-gated tests are plain `#[test]` + `QueryRunner::handle().block_on(...)` — NEVER `#[tokio::test]` (nested-runtime panic, documented at `runner.rs`'s `monitor_pg_tests` module doc; copy that module doc's explanation verbatim into new modules). In `dbc-driver-mssql` there is no `QueryRunner`; its integration tests keep the crate's existing `#[tokio::test]` + `#[ignore]` convention. All docker tests are `#[ignore]`-gated, run explicitly (`%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored mssql_docker_tests::` / `... -p dbc-driver-mssql -- --ignored`). **Honest SKIP, never silent green:** when the HOST has no ODBC Driver 17/18 installed (the one prerequisite docker cannot provide), tests `eprintln!("SKIP …: install msodbcsql18 …")` and return early — detection is the IM002-probe pattern (T8), chosen over `odbc_api::Environment::drivers()` because dbc-ui doesn't depend on odbc-api and a second `Environment` would violate the one-per-process rule the driver's module doc records.
- **The XACT_ABORT matrix (T2 §3c, run in T8) is the phase keystone.** ALL feature ON-flips — `detect_editable_pk`'s Mssql exclusion, `dialect_for_engine`'s `Mssql → Some(Dialect::Mssql)`, `monitor_available`, the gate-message grep sweep — land ONLY in T8, in the same commit tier as the integration sweep, gated on the matrix passing on at least one real machine (same standing rule as G10's live-pg sweep; CI-without-ODBC's green-with-skips is the honest tier, not proof). If matrix case 4 (autocommit interference) fails, execute the pre-documented fork in **Appendix F1**; if the new case 0 (row-count reporting for tx-control batches) fails, execute **Appendix F2**. Neither fork changes any T3–T7 app-layer code — that is why they are appendices, not branches of the task graph.
- **Versioning:** the final task bumps the workspace version (`Cargo.toml` `[workspace.package] version`, currently `0.15.0`) to the **next minor per merge order, determined at merge time** — check `main`'s version when T8 lands and take the next unclaimed minor (e.g. `0.16.0` if G15 merges next). Do NOT hardcode a number now; the design's own "v0.15.0" is stale (main is already there).
- Czech user-facing strings exactly as the design specifies them (they are quoted verbatim in the tasks below); English keywords in parentheses where the keyword is what appears in Microsoft docs/errors.

### Task dependency graph

| Task | Name | Depends on | Files (crate-relative) | Batch |
|---|---|---|---|---|
| T1 | T-CORE: Dialect::Mssql everywhere in dbc-core | — | `dbc-core/src/{split.rs,ddl.rs,guards.rs,tx.rs(new),lib.rs}` | A (parallel) |
| T2 | T-DRV: `probe()`, `query_with_session()`, §3c matrix | — | `dbc-driver-mssql/src/lib.rs`, `tests/mssql_tx_matrix.rs(new)` | A (parallel) |
| T3 | T-CONN: open_config arm, MssqlOptions, dialog | T2 | `dbc-ui/src/{connect.rs,connections_ui.rs}`, `dbc-ui/Cargo.toml`, `dbc-state/src/config.rs` (+ mechanical `mssql: None` sweep in runner.rs/main.rs test literals) | B (SOLO) |
| T4 | T-SQLGEN: composers dialectized | T1, T3 | `dbc-ui/src/{sandbox.rs,csv_import.rs,fk_join.rs,admin_sql.rs,backup.rs,main.rs}` | C (parallel) |
| T5 | T-TX: tx helpers through runner.rs | T1, T3 | `dbc-ui/src/runner.rs` | C (parallel) |
| T6 | T-MON: real monitor wiring | T1, T5 | `dbc-ui/src/{runner.rs,monitor.rs,monitor_sql.rs}` | serialized after T5 |
| T7 | T-PLAN: G13 T7 delivered | T2, T4, T5, T6 | `dbc-ui/src/{plan.rs,runner.rs,main.rs,connect.rs}`, `dbc-core/src/connection.rs` (doc) | serialized after T6 |
| T8 | T-LIVE: docker tier, live backlog 1–8, ALL flips, version | all | Cargo.tomls, `dbc-driver-mssql/tests/`, `dbc-ui/src/{runner.rs,plan.rs,main.rs,monitor.rs}`, fixtures | last, docker-gated |

Suggested batches: **{T1, T2}** in parallel (no file overlap) → **{T3}** solo (its `ConnectionConfig.mssql` field addition compile-breaks test struct literals across the workspace; T3's sweep fixes them, so nothing else may hold those files) → **{T4, T5}** in parallel (disjoint files) → **{T6}** → **{T7}** → **{T8}**. Docker-gated work: T2 *authors* the matrix (runnable only from T8's infra); T8 runs everything live.

---

### Task 1 (T1): `dbc-core` — `Dialect::Mssql`, GO-batch splitting, bracket quoting, `TOP` auto-limit, tx helpers

**Files:**
- Modify: `crates/dbc-core/src/split.rs`, `crates/dbc-core/src/ddl.rs`, `crates/dbc-core/src/guards.rs`, `crates/dbc-core/src/lib.rs`
- Create: `crates/dbc-core/src/tx.rs`

**Interfaces (produced; consumed by T3–T8):**

```rust
// split.rs — extended, same types:
pub enum Dialect { Postgres, Sqlite, Mssql }               // + Mssql
pub enum UnterminatedKind { StringLiteral, QuotedIdent, BlockComment, DollarQuote, TriggerBody, BracketIdent } // + BracketIdent
pub enum SplitError { InvalidUtf8, UnterminatedAtEof(UnterminatedKind), UnsupportedGoCount } // + UnsupportedGoCount

// ddl.rs — new dialect-aware siblings; existing fns become thin pg wrappers:
pub fn quote_ident_d(dialect: Dialect, name: &str) -> String;
pub fn quote_qualified_d(dialect: Dialect, schema: Option<&str>, name: &str) -> String;
pub fn synthesize_create_table_d(dialect: Dialect, t: &TableInfo) -> String;

// guards.rs — new dialect-aware sibling; existing fn becomes a pg wrapper:
pub fn apply_auto_limit_d(sql: &str, limit: u64, dialect: Dialect) -> (String, bool);

// tx.rs — NEW module (design §3a; fixes G12's bare-BEGIN bug at the source):
pub fn tx_begin_sql(dialect: Dialect) -> &'static str;   // pg/sqlite: "BEGIN"; Mssql: "SET XACT_ABORT ON; BEGIN TRANSACTION"
pub fn tx_commit_sql(dialect: Dialect) -> &'static str;  // "COMMIT" for every dialect (param kept for symmetry/call-site uniformity)
pub fn tx_rollback_sql(dialect: Dialect) -> &'static str; // "ROLLBACK" for every dialect
```

**Grounding — split.rs mechanics (design §2c, curation item 3: `Dialect::Mssql` lives INSIDE `StatementSplitter`, split trigger is GO-lines, NEVER `;`).** Current code facts: the only dialect branches today are `'$' if self.dialect == Dialect::Postgres` in `feed_top_level` and `if self.dialect == Dialect::Sqlite` in `finalize_word`; `TriggerLead::initial` matches exhaustively on `Dialect` (adding `Mssql` won't compile until it's handled); the stale module comment at `split.rs:18-21` ("GO … belongs in a separate line-based pre-pass") is superseded and must be rewritten.

Changes, precisely:

1. `Dialect` gains `Mssql`. `TriggerLead::initial`: `Dialect::Sqlite => AwaitingCreate`, `Dialect::Postgres | Dialect::Mssql => NotATrigger` (no trigger-body tracking, no dollar quotes for Mssql — both stay dialect-gated exactly as today).
2. **`;` never splits for Mssql.** In `feed_top_level`, the emit branch becomes:

```rust
        if c == ';' && self.trigger_depth == 0 && self.dialect != Dialect::Mssql {
            self.emit_statement(out);
            return;
        }
```

For Mssql a `;` falls through to the `_ =>` catch-all (ordinary content). Rationale (record in the module doc): T-SQL DDL bodies (`CREATE PROCEDURE … AS BEGIN … END`) have no dollar-quoting — their interior `;`s are top-level to a lexer, so `;`-splitting would shred any procedure/trigger script. "Statement" simply means "batch" for Mssql.

3. **Bracket-ident mode** (mirrors `InDoubleIdent`/`DoubleIdentMaybeEnd`). New `Mode::InBracketIdent` and `Mode::BracketIdentMaybeEnd`; in `feed_top_level`, before the word arm:

```rust
            '[' if self.dialect == Dialect::Mssql => {
                self.stmt_buf.push(c);
                self.has_content = true;
                self.line_only_ws = false;
                self.mode = Mode::InBracketIdent;
            }
```

```rust
    fn handle_in_bracket_ident(&mut self, c: char) {
        self.stmt_buf.push(c);
        if c == ']' {
            self.mode = Mode::BracketIdentMaybeEnd;
        }
    }

    fn handle_bracket_ident_maybe_end(&mut self, c: char, out: &mut Vec<String>) {
        if c == ']' {
            // `]]` is an escaped `]` — still inside the ident.
            self.stmt_buf.push(c);
            self.mode = Mode::InBracketIdent;
        } else {
            self.mode = Mode::Normal;
            self.feed_top_level(c, out);
        }
    }
```

`finish()`: `Mode::InBracketIdent => Err(SplitError::UnterminatedAtEof(UnterminatedKind::BracketIdent))` (fail-closed EOF, same posture as `QuotedIdent`); `BracketIdentMaybeEnd` at EOF means the ident just closed — NOT an error (mirror of `DoubleIdentMaybeEnd`'s absence from the error list).

4. **GO-line detection** — a finalized bare word `GO` (case-insensitive) that is the FIRST non-whitespace content since the last newline, followed on its line only by whitespace or a `--` comment, emits the accumulated batch. `GO` anywhere else (mid-line, inside strings/comments/brackets) is ordinary text that reaches the server verbatim ("Incorrect syntax near 'GO'" — the honest outcome). `GO <n>` is refused fail-closed: `SplitError::UnsupportedGoCount`.

   New `StatementSplitter` fields (document each):
   - `word_start: usize` — byte offset into `stmt_buf` where the current `InWord` run began (recorded in `feed_top_level`'s word arm BEFORE pushing the first char: `self.word_start = self.stmt_buf.len();`).
   - `line_only_ws: bool` — "no non-comment, non-whitespace content since the last newline". Init `true`. Rules: (a) set `true` whenever a `'\n'` is consumed at top level (whitespace arm of `feed_top_level`) OR inside `InLineComment`/`InBlockComment(_)`/`BlockCommentMaybeOpen`/`BlockCommentMaybeClose` handlers (a newline inside a comment still starts a fresh line; a newline inside a STRING or BRACKET ident is data and does NOT touch the flag); (b) set `false` for every other non-whitespace char consumed at top level — including `'`, `"`, `[`, `$`, `-`, `/`, the word arm, and the catch-all (a `-`/`/` that turns out to open a comment conservatively disqualifies a GO later on the SAME line — GO then reaches the server verbatim, the honest posture; GO on the NEXT line still works because the comment's terminating newline resets the flag).
   - `word_at_line_start: bool` — captured from `line_only_ws` in the word arm, before it is cleared.
   - `go_word_start: usize` — `word_start` snapshot taken when a GO line is recognized.

   New modes `Mode::GoPending`, `Mode::GoPendingDash`, `Mode::GoPendingComment`. In ALL THREE, every incoming char is still pushed to `stmt_buf` (so an aborted GO-line loses nothing); `confirm_go` truncates the buffered `GO…` tail away:

```rust
    /// The GO line is confirmed a batch separator: drop the buffered
    /// `GO[ …ws/comment]` tail, emit the batch, start a fresh line.
    fn confirm_go(&mut self, out: &mut Vec<String>) {
        self.stmt_buf.truncate(self.go_word_start);
        self.emit_statement(out);
        self.line_only_ws = true;
    }
```

   `finalize_word` (currently sqlite-only) becomes:

```rust
    fn finalize_word(&mut self) {
        match self.dialect {
            Dialect::Sqlite => {
                let w = self.word_buf.to_uppercase();
                self.apply_trigger_word(&w);
            }
            Dialect::Mssql => {
                if self.word_at_line_start && self.word_buf.eq_ignore_ascii_case("go") {
                    self.go_word_start = self.word_start;
                    self.word_buf.clear();
                    self.mode = Mode::GoPending;
                    return;
                }
            }
            Dialect::Postgres => {}
        }
        self.word_buf.clear();
        self.mode = Mode::Normal;
    }
```

   The GO handlers (the ONLY fallible handlers — the `push` dispatch loop's arms for `InWord`, `GoPending`, `GoPendingDash` become `?`-propagating; every other arm stays infallible):

```rust
    fn handle_go_pending(&mut self, c: char, out: &mut Vec<String>) -> Result<(), SplitError> {
        if c == '\n' {
            self.stmt_buf.push(c); // truncated away by confirm_go
            self.confirm_go(out);
            return Ok(());
        }
        if c.is_ascii_digit() {
            // `GO <n>` repeat count: refused, fail-closed (design §2c iv).
            return Err(SplitError::UnsupportedGoCount);
        }
        self.stmt_buf.push(c);
        if c.is_whitespace() {
            return Ok(()); // still a candidate separator line
        }
        if c == '-' {
            self.mode = Mode::GoPendingDash;
            return Ok(());
        }
        // Something else follows GO on its line (`GO,`, `GO SELECT` …):
        // not a separator — everything is already in stmt_buf, resume as
        // ordinary text. The server errors on it verbatim.
        self.has_content = true;
        self.mode = Mode::Normal;
        Ok(())
    }

    fn handle_go_pending_dash(&mut self, c: char, out: &mut Vec<String>) -> Result<(), SplitError> {
        if c == '-' {
            self.stmt_buf.push(c);
            self.mode = Mode::GoPendingComment;
            return Ok(());
        }
        // Lone `-` after GO: not a comment, not a separator line.
        self.has_content = true;
        self.mode = Mode::Normal;
        self.feed_top_level(c, out);
        Ok(())
    }

    fn handle_go_pending_comment(&mut self, c: char, out: &mut Vec<String>) {
        self.stmt_buf.push(c);
        if c == '\n' {
            self.confirm_go(out);
        }
    }
```

   `handle_word_char`'s post-`finalize_word` dispatch must route by the NEW mode: `match self.mode { Mode::GoPending => self.handle_go_pending(c, out)?, _ => self.feed_top_level(c, out) }` (and its signature becomes `Result`-returning).

   `finish()` additions, before the existing unterminated checks: `Mode::GoPending | Mode::GoPendingComment` ⇒ EOF counts as the implicit EOL (same convention as line comments): `self.stmt_buf.truncate(self.go_word_start); self.mode = Mode::Normal;` then fall through (the pre-GO batch, if non-empty, is returned by the normal tail logic). `Mode::GoPendingDash` ⇒ the dangling `-` is real content: `self.has_content = true; self.mode = Mode::Normal;` and fall through.

5. Rewrite the stale `split.rs:18-21` comment: the GO pre-pass sentence is superseded in mechanism, upheld in substance — `Dialect::Mssql` splits on GO-lines inside this state machine, never on `;` (cite design curation item 3).

**Grounding — ddl.rs** (current `quote_ident` at `ddl.rs:42-44` hardcodes `"…"`; `synthesize_create_table(t: &TableInfo)` calls `quote_ident` at its column/PK/constraint sites and `quote_qualified` for the table name):

```rust
/// Dialect-aware identifier quoting (G15 §2a — THE one bracket
/// implementation; `admin_sql::quote_ident_for` delegates here).
/// Mssql: brackets, `]` doubled — valid in EVERY T-SQL session regardless
/// of QUOTED_IDENTIFIER (the unconditional choice for the write path,
/// per the driver's integration note 1). Others: ANSI double quotes.
pub fn quote_ident_d(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::Mssql => format!("[{}]", name.replace(']', "]]")),
        Dialect::Postgres | Dialect::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

pub fn quote_qualified_d(dialect: Dialect, schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", quote_ident_d(dialect, s), quote_ident_d(dialect, name)),
        None => quote_ident_d(dialect, name),
    }
}

/// Thin pg-convention wrappers — callers that are pg/sqlite-only by
/// construction keep compiling unchanged (design §2a).
pub fn quote_ident(name: &str) -> String {
    quote_ident_d(Dialect::Postgres, name)
}
pub fn quote_qualified(schema: Option<&str>, name: &str) -> String {
    quote_qualified_d(Dialect::Postgres, schema, name)
}
```

`synthesize_create_table_d(dialect, t)`: same body as today's `synthesize_create_table` with every `quote_ident`/`quote_qualified` call switched to the `_d` sibling threading `dialect`; `synthesize_create_table(t)` becomes `synthesize_create_table_d(Dialect::Postgres, t)`. (The MSSQL driver reports `ddl: None` for tables, so schema-tree DDL and G7 text-diff fall back to synthesis — it must bracket-quote for MSSQL; call sites are T4's job.) `use crate::split::Dialect;` at the top of `ddl.rs`.

**Grounding — guards.rs `apply_auto_limit_d`** (design §2d). Current `apply_auto_limit` (guards.rs:309-354) tokenizes, requires first word `SELECT`, refuses on `LIMIT|OFFSET|FETCH|INTO` tokens, appends `" LIMIT {n}"` before a trailing `;`. The Mssql form inserts `TOP {n}` immediately after the leading `SELECT` (after `DISTINCT`/`ALL` when next) instead — `tokenize` is lossy (no byte spans), so the insertion point comes from a small dedicated scanner:

```rust
/// Byte offset just past the leading `SELECT [ALL|DISTINCT]` head of `sql`,
/// skipping leading whitespace and comments. `None` if the head isn't
/// found (caller then returns the SQL unchanged — under-apply, never
/// over-apply, same posture as the flat token scan).
fn select_head_insert_offset(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    // skip whitespace and comments, iteratively
    loop {
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if sql[i..].starts_with("--") {
            i += sql[i..].find('\n').map(|p| p + 1).unwrap_or(sql.len() - i);
            continue;
        }
        if sql[i..].starts_with("/*") {
            let mut depth = 1u32;
            let mut j = i + 2;
            while j < bytes.len() && depth > 0 {
                if sql[j..].starts_with("/*") { depth += 1; j += 2; }
                else if sql[j..].starts_with("*/") { depth -= 1; j += 2; }
                else { j += 1; }
            }
            if depth > 0 { return None; } // unterminated — tokenize() already refused anyway
            i = j;
            continue;
        }
        break;
    }
    let word_end = |start: usize| -> usize {
        sql[start..]
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|p| start + p)
            .unwrap_or(sql.len())
    };
    let end = word_end(i);
    if !sql[i..end].eq_ignore_ascii_case("SELECT") {
        return None;
    }
    // optionally consume one ALL/DISTINCT
    let mut k = end;
    while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
        k += 1;
    }
    let k_end = word_end(k);
    if sql[k..k_end].eq_ignore_ascii_case("DISTINCT") || sql[k..k_end].eq_ignore_ascii_case("ALL") {
        return Some(k_end);
    }
    Some(end)
}

pub fn apply_auto_limit_d(sql: &str, limit: u64, dialect: Dialect) -> (String, bool) {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => apply_auto_limit_pg(sql, limit), // today's body, renamed
        Dialect::Mssql => {
            let items = match tokenize(sql) {
                Some(items) => items,
                None => return (sql.to_string(), false),
            };
            if first_word(&items) != Some("SELECT") {
                return (sql.to_string(), false);
            }
            // T-SQL blocking tokens (design §2d list): flat scan, depth-
            // unaware — can only under-apply, never over-apply.
            let has_limiting_clause = items.iter().any(|i| {
                matches!(i, Item::Word(w) if matches!(w.as_str(), "TOP" | "OFFSET" | "FETCH" | "INTO"))
            });
            if has_limiting_clause {
                return (sql.to_string(), false);
            }
            match select_head_insert_offset(sql) {
                Some(pos) => (format!("{} TOP {}{}", &sql[..pos], limit, &sql[pos..]), true),
                None => (sql.to_string(), false),
            }
        }
    }
}

pub fn apply_auto_limit(sql: &str, limit: u64) -> (String, bool) {
    apply_auto_limit_d(sql, limit, Dialect::Postgres)
}
```

(Rename today's body to a private `apply_auto_limit_pg`; the public `apply_auto_limit` keeps byte-identical pg/sqlite behavior — existing tests must pass untouched. `use crate::split::Dialect;` in guards.rs.)

**Grounding — tx.rs** (design §3a/§3b, full file):

```rust
//! G15 §3a: dialect-correct transaction-control text for every sanctioned
//! write sequence (fixes G12's bare-`BEGIN` bug — `BEGIN` alone is invalid
//! T-SQL). Postgres/Sqlite strings are byte-identical to the literals the
//! runner used before G15: zero behavior change for those engines.

use crate::split::Dialect;

/// Mssql: `SET XACT_ABORT ON` is FUSED to `BEGIN TRANSACTION` in one batch
/// (it has no only-statement restriction) so no sequence anywhere can open
/// an MSSQL transaction without it. §3b: under `XACT_ABORT OFF`, T-SQL's
/// per-error-class batch-vs-statement abort behavior makes "stop at first
/// error, roll back everything" untestable; `ON` collapses it to the
/// pg-like contract (any runtime error dooms and rolls back the whole
/// transaction). The subsequent explicit ROLLBACK then failing with "no
/// corresponding BEGIN TRANSACTION" is swallowed by the sequences'
/// existing `let _ =` discard posture. Verified empirically by the §3c
/// matrix (dbc-driver-mssql/tests/mssql_tx_matrix.rs) before any
/// feature-ON flip merges.
pub fn tx_begin_sql(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => "BEGIN",
        Dialect::Mssql => "SET XACT_ABORT ON; BEGIN TRANSACTION",
    }
}

/// `COMMIT` is valid T-SQL as-is; the dialect parameter is kept so every
/// call site reads uniformly and a future divergence has a seam.
pub fn tx_commit_sql(_dialect: Dialect) -> &'static str {
    "COMMIT"
}

pub fn tx_rollback_sql(_dialect: Dialect) -> &'static str {
    "ROLLBACK"
}
```

`lib.rs`: add `mod tx;` and extend the re-exports: `pub use ddl::{quote_ident, quote_ident_d, quote_qualified, quote_qualified_d, synthesize_create_table, synthesize_create_table_d};`, `pub use guards::{apply_auto_limit, apply_auto_limit_d, is_read_statement};`, `pub use tx::{tx_begin_sql, tx_commit_sql, tx_rollback_sql};`.

**Steps:**

- [ ] **Step 1: Write the failing tests.** In `split.rs`'s `mod tests` (new Mssql section; use the existing `split_bytes_one_at_a_time` helper):
  - `mssql_go_splits_batches`: `split_sql("SELECT 1\nGO\nSELECT 2\n", Dialect::Mssql)` → `["SELECT 1", "SELECT 2"]`.
  - `mssql_semicolon_is_not_a_separator`: `"SELECT 1; SELECT 2"` → ONE batch `"SELECT 1; SELECT 2"`.
  - `mssql_go_case_insensitive_with_leading_and_trailing_whitespace`: `"SELECT 1\n  go  \nSELECT 2"` → 2 batches.
  - `mssql_go_with_trailing_line_comment_splits`: `"SELECT 1\nGO -- next\nSELECT 2"` → 2 batches.
  - `mssql_go_with_repeat_count_is_refused`: `split_sql("SELECT 1\nGO 5\n", Dialect::Mssql)` → `Err(SplitError::UnsupportedGoCount)`.
  - `mssql_go_mid_line_is_ordinary_text`: `"SELECT 1 GO"` → one batch `"SELECT 1 GO"`.
  - `mssql_go_followed_by_other_text_is_ordinary`: `"GO, SELECT 1"` → one batch containing the text verbatim.
  - `mssql_go_inside_string_comment_and_bracket_is_not_a_separator`: `"SELECT '\nGO\n', [a\nGO\nb] /*\nGO\n*/ FROM t"` → one batch (interior GO-looking lines inert in all three modes).
  - `mssql_bracket_ident_hides_semicolon_and_escaped_bracket`: `"SELECT [a;b], [we]]ird] FROM t"` → one batch, text intact.
  - `mssql_unterminated_bracket_at_eof`: `"SELECT [oops"` → `Err(UnterminatedAtEof(BracketIdent))`.
  - `mssql_create_procedure_with_interior_semicolons_is_one_batch`: a `CREATE PROCEDURE p AS BEGIN … ; … ; END` body followed by `\nGO\n` → exactly one batch containing both semicolons.
  - `mssql_final_batch_without_trailing_go` (finish path) and `mssql_go_at_eof_without_newline`: `"SELECT 1\nGO"` → `push` yields `[]`… `finish` sequence overall yields exactly `["SELECT 1"]`.
  - `mssql_go_after_block_comment_line_still_splits`: `"SELECT 1\n/* deploy\nnote */\nGO\nSELECT 2"` → 2 batches (newline inside a block comment resets the line flag).
  - `mssql_dollar_dollar_is_ordinary_text_and_no_trigger_tracking`: `$$` inert; `"CREATE TRIGGER t ON x AFTER INSERT AS BEGIN SELECT 1 END\nGO"` splits only on GO.
  - `round_trip_one_push_vs_byte_by_byte_mssql`: byte-wise == one-shot over a corpus containing all of the above.
  - In `ddl.rs` tests: `quote_ident_d_mssql_brackets_and_doubles_closing` (`we]ird` → `[we]]ird]`, `plain` → `[plain]`, pg unchanged), `quote_qualified_d_mssql`, `synthesize_create_table_d_mssql_bracket_quotes_everything` (a `TableInfo` with a `we]ird` column; assert `[we]]ird]` and bracket-quoted table/PK/constraint names), plus an assertion that `synthesize_create_table` (no `_d`) output is byte-identical to before.
  - In `guards.rs` tests (mirror the existing pg auto-limit rows one-for-one): `auto_top_inserts_after_select` (`"select * from big"` → `"select TOP 1000 * from big"`, changed), `auto_top_after_distinct` (`"SELECT DISTINCT x FROM t"` → `"SELECT DISTINCT TOP 1000 x FROM t"`), `auto_top_leaves_top_offset_fetch_into_alone` (already-`TOP`, `OFFSET … FETCH`, `SELECT … INTO` all unchanged), `auto_top_with_trailing_semicolon` (`"select * from t;"` → `"select TOP 1000 * from t;"`), `auto_top_after_leading_comment` (`"/* hint */ SELECT x FROM t"` inserts after `SELECT`), `auto_top_string_literal_top_is_not_a_blocker` (`"select 'top secret' from t"` DOES get `TOP` — token scan ignores strings), and `apply_auto_limit_wrapper_is_byte_identical_pg` (delegation check).
  - In `tx.rs` tests: `tx_helpers_pg_sqlite_are_the_historic_literals` (`"BEGIN"`/`"COMMIT"`/`"ROLLBACK"` byte-equal for both), `tx_begin_mssql_is_fused_xact_abort`.
- [ ] **Step 2: Run to see them fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core` (expect compile errors first: the new `Dialect::Mssql` variant makes `TriggerLead::initial` and `dialect_for_engine`'s consumers non-exhaustive ONLY inside dbc-core; `main.rs`'s match already has an `Mssql` arm returning `None`, so dbc-ui keeps compiling).
- [ ] **Step 3: Implement** everything in the Interfaces/Grounding blocks above, in order: enum variants → bracket mode → line/word tracking fields → GO modes + `confirm_go` + `finish()` additions → ddl `_d` functions → `apply_auto_limit_d` + `select_head_insert_offset` → `tx.rs` → `lib.rs` re-exports → rewrite the `split.rs:18-21` comment.
- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core` (all new + ALL existing tests, zero warnings). Then `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-mcp` to prove no downstream breakage (both call `apply_auto_limit`/`quote_ident`/`split_sql` through the unchanged wrappers).
- [ ] **Step 5: Commit** — `feat(core): Dialect::Mssql — GO batches, brackets, TOP auto-limit, tx helpers (G15 T1)`.

---

### Task 2 (T2): `dbc-driver-mssql` — `probe()`, `query_with_session()`, and the §3c XACT_ABORT matrix (authored)

**Files:**
- Modify: `crates/dbc-driver-mssql/src/lib.rs`
- Create: `crates/dbc-driver-mssql/tests/mssql_tx_matrix.rs`

The driver crate is NOT frozen but gains **exactly two public items** (design curation item 4) — `probe()` and `query_with_session()`. Nothing else in the crate changes (unless a T8 fork fires — Appendix F1/F2 are the only pre-approved additional changes). The `Connection` trait in `dbc-core` is untouched.

**Interfaces (produced; consumed by T3's `open_config` arm and T7's `run_mssql_plan`):**

```rust
impl MssqlConnection {
    /// G15 §1a eager handshake: opens ONE ODBC connection with the stored
    /// connection string and drops it. `MssqlConnection::new` is lazy
    /// (connects per operation); `open_config`'s contract — relied on by
    /// `test_connect` and the status bar's connect-vs-query error split —
    /// is "bad host/credentials fail HERE". Blocking (no async, no
    /// `block_on`): callers must be on a blocking-legal thread
    /// (`open_config` only ever runs inside `spawn_blocking`).
    pub fn probe(&self) -> Result<(), QueryError> {
        connect(&self.conn_str).map(|_| ())
    }

    /// G15 §2e (G13 T7): one fresh connection, session preludes, main
    /// batch, best-effort postludes — the Showplan delivery mechanism.
    /// `SET SHOWPLAN_XML` must be the ONLY statement in its batch and is
    /// session-scoped, which is why `query()` (fresh connection per call)
    /// cannot deliver it (design curation item 1).
    pub async fn query_with_session(
        &self,
        prelude: &[String],
        sql: &str,
        postlude: &[String],
        cancel: CancelToken,
    ) -> Result<QueryStream, QueryError>;
}
```

**Grounding — `query_with_session` mechanics** (mirrors `query()`'s `spawn_blocking` + channel shape at `lib.rs:203-318`, reusing `connect`, `wide::build`, `wide::cell_text`, `odbc_err`, `cancelled_err`):

1. `spawn_blocking` closure owns a clone of `conn_str`, owned copies of `prelude`/`sql`/`postlude`, the `cancel` token, a `oneshot` for the chosen schema and an `mpsc(CHANNEL_CAPACITY)` for batches (same contract as `query()`: after an `Err` batch the sender stops and drops).
2. Cancel check → `connect(&conn_str)` → cancel check.
3. Each prelude string runs as its OWN batch: `match conn.execute(p, (), None)` — `Ok(Some(cursor))` is dropped (unexpected but drained-by-drop), `Ok(None)` is the normal case, `Err(e)` short-circuits to the error path (which STILL runs postludes — see 6).
4. `conn.execute(&sql, (), None)`: `Ok(None)` ⇒ error `"statement produced no result set"`; `Err` ⇒ error path. `Ok(Some(cursor))` ⇒ walk ALL result sets:
   - Per result set: read `column_names()`; materialize the rows as `Vec<Vec<Option<String>>>` via `wide::build(&mut cursor, BATCH_ROWS, QUERY_MAX_STR_LEN)` + `cursor.bind_buffer(&mut buffers)` + a fetch loop with a cancel check per fetch (mirror `query()`'s loop, but collecting instead of streaming — plan payloads are single-cell XML, materialization is bounded and tiny).
   - Advance: `let (cursor, _buffers) = block_cursor.unbind().map_err(odbc_err)?;` then `cursor.more_results().map_err(odbc_err)?` — `Some(next)` continues the walk, `None` ends it. (odbc-api 29.0.0: `Cursor::more_results(self) -> Result<Option<Self>, Error>` consumes self — confirmed during G13 against the vendored source; `BlockCursor::unbind` returns the cursor + buffer. Implementer: verify both against the vendored `odbc-api-29.0.0/src/cursor.rs` before writing the loop, same posture as G13's Spec note.)
   - Selection rule (design §2e): the result set whose SINGLE column is named `Microsoft SQL Server 2005 XML Showplan` wins (first such match); otherwise fall back to the LAST result set walked (fail-open on the name — the needs-verification flag from G13 §1b resolves against live captures in T8; the fallback bounds the damage to "wrong text handed to a parser that fails closed").
5. Ship the chosen result set: send its schema (all `DataType::Utf8`, nullable — same as `query()`) over the oneshot, then its rows as `RecordBatch`es over the mpsc (chunk at `BATCH_ROWS`), then drop the sender.
6. **Postludes run best-effort, ALWAYS** — on the success path after the walk, AND on every error path after `connect` succeeded: for each postlude, `let _ = conn.execute(p, (), None);` (dropping any unexpected cursor) — the `let _ =` discard posture. The connection then drops with the closure, which is itself the backstop: an ODBC disconnect rolls back any still-open transaction, and session settings can never leak (connection-ownership rationale from G13 §1b).
7. The async fn awaits the schema oneshot exactly like `query()` (`.map_err(|_| QueryError::msg("driver task died"))??`) and returns `QueryStream { columns, batches: rx }`.

Module-doc updates in the same task: the Cancellation section gains one sentence (`query_with_session` checks the token before connect, before the main batch, and per fetch — batch granularity, same as `query`); integration note 3's "should be checked alongside the XACT_ABORT behavior" sentence gains a pointer to `tests/mssql_tx_matrix.rs`.

**Grounding — the §3c matrix** (`tests/mssql_tx_matrix.rs`, new). Authored NOW, runnable from T8 (until then it follows the crate's existing convention: `#[tokio::test]` + `#[ignore]`, connection from `DBC_MSSQL_TEST_CONN` via the same `conn_str()`/`connect()` helpers as `mssql_integration.rs`; T8 rewires both files onto the shared testcontainers helper). Drives `execute()` exactly as the app does: persistent `exec_conn`, statements as separate calls. Session-state assertions use the execute-compatible probe (no result set, legal on `exec_conn`):

```rust
use dbc_core::{CancelToken, Connection};
use dbc_driver_mssql::MssqlConnection;

fn conn_str() -> Option<String> {
    std::env::var("DBC_MSSQL_TEST_CONN").ok()
}

fn connect() -> MssqlConnection {
    MssqlConnection::from_connection_string(conn_str().expect("DBC_MSSQL_TEST_CONN not set"))
}

async fn exec(conn: &mut MssqlConnection, sql: &str) -> Result<u64, dbc_core::QueryError> {
    conn.execute(sql, CancelToken::new()).await
}

/// Errors exactly when the assertion fails; produces no result set, so it
/// is legal on the persistent exec_conn the app actually uses (§3c).
fn trancount_probe(n: u32) -> String {
    format!("IF @@TRANCOUNT <> {n} THROW 50000, 'trancount mismatch', 1")
}

/// The exact text every sanctioned sequence sends from T5 on.
const TX_BEGIN: &str = "SET XACT_ABORT ON; BEGIN TRANSACTION";
```

(Table names process-scoped via `std::process::id()`, same as `mssql_integration.rs`. Data-visibility assertions use a SECOND connection's `query()` — a fresh connection by construction.)

- **Case 0 (row-count characterization — added by this plan, gates Appendix F2):** `tx_control_batches_report_a_row_count` — `exec(&mut c, TX_BEGIN)` then `exec(&mut c, "COMMIT")` must both be `Ok(_)`. Today's `run_execute` maps a driver-reported `SQL_NO_ROW_COUNT` (`row_count() == None`) to an ERROR (`types.rs::map_row_count`) — never verified live for SET/BEGIN/COMMIT batches. If this case fails with "row count not available", every sequence in T5 is broken on MSSQL: execute Appendix F2 before anything else.
- **Case 1:** `xact_abort_pk_violation_aborts_and_rolls_back_whole_tx` — create table with PK; `TX_BEGIN` → INSERT ok → duplicate-PK INSERT errors → `exec(trancount_probe(0))` succeeds (tx gone) → second connection's `query()` sees ZERO rows (the first INSERT's row is GONE).
- **Case 2:** `conversion_and_arithmetic_errors_behave_like_constraint_errors` — same shape with an `INSERT` selecting `CAST('x' AS int)` and a divide-by-zero — the classes that diverge under `XACT_ABORT OFF` must all behave identically under ON (trancount 0, no rows).
- **Case 3:** `best_effort_rollback_after_abort_errors_but_session_stays_usable` — after case 1's abort, `exec("ROLLBACK")` returns `Err` (no corresponding BEGIN TRANSACTION) AND a following plain INSERT on the SAME connection succeeds (the `let _ =` discard is safe, not masking a poisoned session).
- **Case 4 (KEYSTONE — gates Appendix F1):** `autocommit_does_not_commit_between_execute_calls_inside_open_tx` — `TX_BEGIN` → `exec(trancount_probe(1))` ok → INSERT → the SECOND connection must NOT see the row: issue `SET LOCK_TIMEOUT 1000` on it first, then `SELECT COUNT(*)`; accept either a 0 count or a lock-timeout error, both prove non-visibility (do NOT read with READUNCOMMITTED — that would see the uncommitted row and prove nothing; document this in the test) → `exec("COMMIT")` → the second connection now counts 1. Proves ODBC's `SQL_ATTR_AUTOCOMMIT = ON` does not commit between `execute()` calls once a literal `BEGIN TRANSACTION` is open (driver note 3's second open question).
- **Case 5:** `xact_abort_persists_across_tx_begins_on_same_exec_conn` — after a full `TX_BEGIN`…`COMMIT` cycle, a SECOND `TX_BEGIN` on the same connection followed by a PK violation still aborts to trancount 0 (harmless redundancy either way; characterizes the session).
- **Case 6** (CSV all-or-nothing) is app-level and lives in T8's `mssql_docker_tests` (backlog item 6) — noted here so the numbering matches the design.

**Steps:**

- [ ] **Step 1: Author the red state** — write `tests/mssql_tx_matrix.rs` in full (cases 0–5 above; it references `probe`/nothing new besides `execute`, so it compiles only against the finished crate) and add a compile-visible use of both new methods (the matrix file plus a `#[cfg(test)]` smoke assertion in `lib.rs` that `probe` is callable from a non-async fn). `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql` must fail to compile until Step 2.
- [ ] **Step 2: Implement `probe()` and `query_with_session()`** per the grounding, plus the module-doc updates.
- [ ] **Step 3: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql` (unit tests + compile of the ignored integration/matrix files; zero warnings). The matrix itself runs in T8.
- [ ] **Step 4: Commit** — `feat(driver-mssql): probe() eager handshake + query_with_session() session preludes; XACT_ABORT matrix authored (G15 T2)`.

---

### Task 3 (T3): Connection wiring — `open_config` arm, `MssqlOptions`, dialog rows, Test path

**Files:**
- Modify: `crates/dbc-state/src/config.rs`, `crates/dbc-ui/src/connect.rs`, `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/Cargo.toml`

**Interfaces:**
- Consumes: T2's `MssqlConnection::probe()`, existing `MssqlConfig` builder (`new(host, port: u16, database, user, password)`, `.encrypt(bool)`, `.trust_server_certificate(bool)`, `.connect_timeout_sec(u32)`, `.driver(impl Into<String>)` — exact per `dbc-driver-mssql/src/config.rs:42-85`).
- Produces (consumed by T7, T8):

```rust
// dbc-state/src/config.rs
fn default_true() -> bool { true }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MssqlOptions {
    #[serde(default = "default_true")]
    pub encrypt: bool,
    #[serde(default)]
    pub trust_server_certificate: bool,
    #[serde(default)]
    pub driver: Option<String>,
}

impl Default for MssqlOptions {
    fn default() -> Self {
        Self { encrypt: true, trust_server_certificate: false, driver: None }
    }
}

// ConnectionConfig gains, after `ssh`:
//     #[serde(default)]
//     pub mssql: Option<MssqlOptions>,

// dbc-ui/src/connect.rs
pub(crate) fn mssql_connection_from_config(cfg: &ConnectionConfig, secret: Option<String>) -> Result<MssqlConnection, QueryError>;
pub(crate) fn mssql_im002_hint(e: QueryError) -> QueryError;
```

**Grounding — `dbc-state`:** `None` ⇒ all defaults (encrypt on, trust off, Driver 18) — old TOML files load unchanged; toml-rs omits `None` struct fields on save (same as the existing `port`/`ssh` fields), so non-MSSQL configs never serialize the table. No password field anywhere in TOML — unchanged invariant (`no_password_field_serialized` must stay green; `MssqlOptions` carries no secret). Every `ConnectionConfig` struct literal in tests (e.g. runner.rs's `run_write_transaction_refuses_read_only_connection_without_connecting`) gains `mssql: None` — grep `favourite: false` across `runner.rs`/`main.rs`/`connections_ui.rs`/`dbc-mcp` tests to find them all.

**Grounding — `connect.rs`** (the arm replaces the permanent error at `connect.rs:97-101`; imports gain `use dbc_driver_mssql::{MssqlConfig, MssqlConnection};`):

```rust
/// Shared MSSQL builder — used by `open_config`'s arm AND (T7)
/// `runner::run_mssql_plan`. Refusals first, before touching the
/// vault-provided secret's destination string. NO probe here — callers
/// decide (open_config probes eagerly; run_mssql_plan lets
/// query_with_session's own connect fail naturally).
pub(crate) fn mssql_connection_from_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<MssqlConnection, QueryError> {
    // §1d: Encrypt=yes + a 127.0.0.1 tunnel endpoint makes the server
    // cert's hostname never match, so a tunneled MSSQL connection only
    // works with TrustServerCertificate=yes — an untested encryption
    // downgrade path. Fail honest; same message pattern as the
    // backup-over-tunnel gates in main.rs.
    if cfg.ssh.is_some() {
        return Err(QueryError::msg(
            "SSH tunel pro MSSQL zatím není podporován — použij přímé připojení",
        ));
    }
    // §0 non-goal: SQL auth only in v1 (no Trusted_Connection).
    if cfg.user.trim().is_empty() {
        return Err(QueryError::msg(
            "MSSQL: zadejte uživatele — ověření přes Windows účet zatím není podporováno",
        ));
    }
    let opts = cfg.mssql.clone().unwrap_or_default();
    let mut mssql_cfg = MssqlConfig::new(
        cfg.host.clone(),
        cfg.port.unwrap_or(1433),
        cfg.database.clone(),
        cfg.user.clone(),
        secret.unwrap_or_default(),
    )
    .encrypt(opts.encrypt)
    .trust_server_certificate(opts.trust_server_certificate)
    // Same 15s fallback bound the pg arm uses, rendered as ODBC
    // `Connection Timeout` so an unreachable host fails inside the same
    // envelope instead of hanging for the OS TCP timeout.
    .connect_timeout_sec(
        cfg.timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS).min(u32::MAX as u64) as u32,
    );
    if let Some(driver) = opts.driver.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        mssql_cfg = mssql_cfg.driver(driver.to_string());
    }
    Ok(MssqlConnection::new(&mssql_cfg))
}

/// §1c: SQLSTATE IM002 ("data source name not found and no default
/// driver specified") is the exact failure an uninstalled msodbcsql18
/// produces. Best-effort sugar, never load-bearing — the original
/// diagnostic is appended, and a non-IM002 error passes through
/// untouched. Detection checks the structured code first (odbc_err puts
/// the bare SQLSTATE there) and falls back to a substring match.
pub(crate) fn mssql_im002_hint(e: QueryError) -> QueryError {
    let is_im002 = e.code.as_deref() == Some("IM002") || e.message.contains("IM002");
    if !is_im002 {
        return e;
    }
    QueryError {
        code: e.code.clone(),
        message: format!(
            "ODBC Driver 18 for SQL Server není nainstalován — nainstalujte balíček \
             msodbcsql18 (nebo v nastavení připojení zadejte název nainstalovaného \
             driveru): {}",
            e.message
        ),
        position: e.position,
    }
}
```

The arm itself:

```rust
        Engine::Mssql => {
            // READ-ONLY POSTURE (driver integration note 5, G15 §1a):
            // there is NO server-side read-only mode to set —
            // ApplicationIntent=ReadOnly only routes AG secondaries; on a
            // standalone instance it accepts writes. Client-side
            // `is_read_statement` + the SHARED runner guard are the ONLY
            // enforcement for MSSQL, unlike pg
            // (default_transaction_read_only=on) and sqlite
            // (SQLITE_OPEN_READ_ONLY). Nothing server-side is set here.
            //
            // SECURITY: the password lives only in the in-memory ODBC
            // connection string (escape_odbc_value brace-wraps hostile
            // values); it is never persisted, never logged, never
            // formatted into an error (probe surfaces driver diagnostic
            // records only — REQUIRED negative test in T8's
            // mssql_docker_tests). No DSN, ever.
            let conn = mssql_connection_from_config(cfg, secret)?;
            // Eager handshake: bad host/credentials fail HERE (probe is
            // plain blocking code; this arm already runs on a
            // blocking-legal thread — no block_on needed).
            conn.probe().map_err(mssql_im002_hint)?;
            Ok(OpenConnection { conn: Box::new(conn), _tunnel: None })
        }
```

Also extend `open_config`'s SECURITY doc comment with one MSSQL sentence mirroring the arm comment. A cert-validation failure (SQLSTATE 08001 with "certificate chain" text) is deliberately NOT wrapped — verbatim, no auto-retry-with-trust, ever.

**Grounding — `connections_ui.rs`:**

1. `ConnectionDialogUi` gains `pub mssql_encrypt: bool`, `pub mssql_trust_cert: bool`, `pub mssql_driver: Entity<TextField>`. The dialog-open path (the destructure-and-populate block around connections_ui.rs:1433-1471) initializes them from `c.mssql.clone().unwrap_or_default()` when editing, from `MssqlOptions::default()` when creating.
2. Rendering — after the existing `if ui.ssh_enabled { … }` block in `render_connection_dialog_panel` (the repo's ONLY conditional-row idiom; the design's "same pattern the SQLite path uses" does not exist in code — RESOLVED: copy the SSH block's `if` + `panel = panel.child(…)` shape):

```rust
    if ui.engine == Engine::Mssql {
        panel = panel
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .child(
                        checkbox("chk-mssql-encrypt", "Šifrovat připojení (Encrypt)", ui.mssql_encrypt)
                            .on_click(cx.listener(|v, _, _, cx| v.toggle_mssql_encrypt(cx))),
                    )
                    .child(
                        checkbox(
                            "chk-mssql-trust",
                            "Důvěřovat certifikátu serveru (TrustServerCertificate)",
                            ui.mssql_trust_cert,
                        )
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_mssql_trust(cx))),
                    ),
            )
            .child(field_row("ODBC driver (volitelné)", ui.mssql_driver.clone(), *cx.theme()))
            // Read-only honesty, UI half (§1a): rendered only for MSSQL.
            .child(
                div()
                    .text_color(cx.theme().text_muted)
                    .child("Pouze pro čtení: u MSSQL vynuceno pouze na straně klienta"),
            );
    }
```

   Toggle handlers `toggle_mssql_encrypt`/`toggle_mssql_trust` copy `toggle_read_only`'s exact shape (connections_ui.rs:1644-1649). Czech labels with the English keyword in parentheses — the keyword is what appears in Microsoft docs/errors.
3. `ConnectionFormData` gains `mssql: Option<MssqlOptions>`; `to_form_data` sets it only for the Mssql engine:

```rust
            mssql: if self.engine == Engine::Mssql {
                let driver = self.mssql_driver.read(cx).text();
                Some(MssqlOptions {
                    encrypt: self.mssql_encrypt,
                    trust_server_certificate: self.mssql_trust_cert,
                    driver: if driver.trim().is_empty() { None } else { Some(driver) },
                })
            } else {
                None
            },
```

   The hand-written redacting `Debug` (connections_ui.rs:875-899) prints the new field as-is (non-secret); `to_connection_config` maps `mssql: self.mssql.clone()`.
4. `test_connect_spec` (connections_ui.rs:2173-2192): DELETE the `if cfg.engine == Engine::Mssql { return Err(…) }` short-circuit and rewrite the doc comment — Test now goes through the runner to `open_config` → `probe()` like every engine (the "permanent behaviour per the brief" sentence is obsolete). Keep the `Result<ConnectSpec, String>` signature so both call sites (`on_test_clicked`, `switch_to_connection`) compile unchanged.
5. `crates/dbc-ui/Cargo.toml`: add `dbc-driver-mssql = { path = "../dbc-driver-mssql" }` to `[dependencies]` (absent today — step zero of this task).

**Steps:**

- [ ] **Step 1: Write the failing tests.**
  - `dbc-state` (`config.rs` `mod tests`, copying the `tool_paths_defaults_to_none_when_absent_from_old_config` hand-written-TOML shape): `old_config_without_mssql_options_loads` (asserts `mssql == None`), `mssql_options_roundtrip_save_load` (`Some(MssqlOptions { encrypt: false, trust_server_certificate: true, driver: Some("ODBC Driver 17 for SQL Server".into()) })`), `mssql_options_partial_table_applies_serde_defaults` (a TOML `[connections.mssql]` table with only `trust_server_certificate = true` ⇒ `encrypt == true`, `driver == None`), `non_mssql_config_serializes_no_mssql_table` (raw saved TOML of a Postgres config does not contain `mssql`), and confirm `no_password_field_serialized` still passes.
  - `dbc-ui` connect tests (pure, no I/O, in a `#[cfg(test)]` module in `connect.rs`): `mssql_ssh_config_is_refused_before_any_io` (cfg with `ssh: Some(…)` ⇒ the exact Czech SSH message), `mssql_empty_user_is_refused_with_integrated_auth_message`, `im002_hint_wraps_only_im002` (a `QueryError { code: Some("IM002".into()), .. }` gains the msodbcsql18 hint with the original appended; an `08001` error passes through byte-identical).
  - `connections_ui` pure test: `form_data_maps_mssql_options_only_for_mssql_engine` (`ConnectionFormData::to_connection_config` — Postgres form data maps `mssql: None`; Mssql maps all three fields).
- [ ] **Step 2: Run to see them fail, then implement** (dbc-state → connect.rs → connections_ui.rs), and sweep the `ConnectionConfig` struct literals in tests for the new `mssql: None` field.
- [ ] **Step 3: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state -p dbc-ui -p dbc-mcp` (zero warnings). NOTE the branch-intermediate contract: after this task an MSSQL connection CAN open, but `dialect_for_engine` still returns `None` (single-statement editor path), `detect_editable_pk` still excludes Mssql, and `monitor_available` is still false — exactly the design's "nothing turns on before the live tier exists" posture; the single-statement editor path is made safe by T4 dialectizing BOTH auto-limit call paths.
- [ ] **Step 4: Manual smoke (no server needed):** launch the app; an MSSQL connection with SSH enabled → Test shows the SSH refusal; without a user → the integrated-auth refusal; with nothing listening → a connect error within ~15s, password nowhere in the status text.
- [ ] **Step 5: Commit** — `feat(ui,state): MSSQL connection wiring — open_config arm, probe, MssqlOptions, dialog rows (G15 T3)`.

---

### Task 4 (T4): SQL generation dialectized — sandbox, CSV import, preview, FK join, admin_sql delegation, backup builders, auto-limit call paths

**Files:**
- Modify: `crates/dbc-ui/src/sandbox.rs`, `crates/dbc-ui/src/csv_import.rs`, `crates/dbc-ui/src/fk_join.rs`, `crates/dbc-ui/src/admin_sql.rs`, `crates/dbc-ui/src/backup.rs`, `crates/dbc-ui/src/main.rs`

**Interfaces:**
- Consumes: T1's `quote_ident_d`/`quote_qualified_d`/`synthesize_create_table_d`/`apply_auto_limit_d`/`Dialect::Mssql`.
- Produces (consumed by T7, T8):

```rust
// main.rs — total Engine→Dialect mapping for SQL COMPOSITION. Distinct
// from `dialect_for_engine` (the SPLITTER gate), which stays
// `Mssql → None` until T8's flip: composers need the dialect even while
// the multi-statement path is still gated.
fn sql_dialect(engine: dbc_state::Engine) -> dbc_core::Dialect {
    match engine {
        dbc_state::Engine::Postgres => dbc_core::Dialect::Postgres,
        dbc_state::Engine::Sqlite => dbc_core::Dialect::Sqlite,
        dbc_state::Engine::Mssql => dbc_core::Dialect::Mssql,
    }
}

// main.rs — SplitError → user text (used by count_statements_in_file and
// run_query_with's split-error arm; runner.rs's script path duplicates the
// one Czech literal in T5 — two sites, deliberately self-contained tasks):
pub(crate) fn split_error_message(e: dbc_core::SplitError) -> String {
    match e {
        dbc_core::SplitError::UnsupportedGoCount => {
            "GO s počtem opakování není podporováno".to_string()
        }
        other => format!("{other:?}"),
    }
}

// sandbox.rs
pub fn sql_value_d(v: Option<&str>, numeric: bool, dialect: dbc_core::Dialect) -> String;
pub fn sql_value(v: Option<&str>, numeric: bool) -> String; // delegates, pg behavior
pub struct TableMeta<'a> { /* existing fields */ pub dialect: dbc_core::Dialect }

// csv_import.rs
pub fn generate_insert_batches(dialect: dbc_core::Dialect, schema: Option<&str>, table: &str, columns: &[TargetColumn], mapping: &ColumnMapping, rows: &[CsvRow]) -> Result<Vec<String>, String>;

// fk_join.rs
pub fn build_join_sql(dialect: dbc_core::Dialect, schema: Option<&str>, table: &str, joins: &[JoinSpec]) -> String;

// main.rs
fn preview_sql(dialect: dbc_core::Dialect, schema: Option<&str>, table: &str) -> String;
fn auto_limit_each(statements: Vec<String>, limit: Option<u64>, bypass: bool, dialect: dbc_core::Dialect) -> (Vec<String>, bool);
```

**Grounding — `sandbox.rs` (§2b):** Apply is the app's ONLY user-data write path — quoting here is CRITICAL (module doc). A bare `'…'` literal is `varchar` in T-SQL and transcodes through the database collation's code page — Czech diacritics staged in the grid would corrupt exactly the way `wide.rs` exists to prevent on the read side. `N''` is harmless for ASCII and correct for everything else:

```rust
pub fn sql_value_d(v: Option<&str>, numeric: bool, dialect: dbc_core::Dialect) -> String {
    match v {
        None => "NULL".to_string(),
        Some(s) => {
            if numeric {
                let trimmed = s.trim();
                // (unchanged finite/i128 passthrough — Task 2 review issue 1)
                let finite_float = trimmed.parse::<f64>().map(|f| f.is_finite()).unwrap_or(false);
                if !trimmed.is_empty() && (trimmed.parse::<i128>().is_ok() || finite_float) {
                    return trimmed.to_string();
                }
            }
            let quoted = s.replace('\'', "''");
            match dialect {
                // §2b: N'' — non-finite floats keep the existing
                // quote-and-let-the-server-decide posture; MSSQL rejects
                // N'NaN' for a float column server-side, error surfaces
                // verbatim (documented, not special-cased).
                dbc_core::Dialect::Mssql => format!("N'{quoted}'"),
                _ => format!("'{quoted}'"),
            }
        }
    }
}

pub fn sql_value(v: Option<&str>, numeric: bool) -> String {
    sql_value_d(v, numeric, dbc_core::Dialect::Postgres)
}
```

`TableMeta` gains `pub dialect: dbc_core::Dialect`; `generate_statements`, `pk_where_fragment`, `where_clause` switch every `quote_qualified`/`quote_ident`/`sql_value` call to the `_d` sibling threading `meta.dialect`. `main.rs::on_open_apply_dialog` (the one production `TableMeta` constructor) supplies `dialect: sql_dialect(engine)` — unreachable for Mssql until T8's `detect_editable_pk` flip, by design.

**Grounding — `csv_import.rs`:** `generate_insert_batches` gains the leading `dialect` param; `quote_qualified`/`quote_ident` → `_d`; `sql_value(v, …)` → `sql_value_d(v, …, dialect)`. The batch-size cap test (design §3c tail):

```rust
    #[test]
    fn csv_import_batch_size_is_under_the_tsql_values_row_cap() {
        // T-SQL: a VALUES clause may contain at most 1000 row
        // constructors — a future bump past that would silently break
        // MSSQL imports at runtime.
        assert!(CSV_IMPORT_BATCH_SIZE <= 1000);
    }
```

**Grounding — `main.rs` composers and BOTH auto-limit paths (§2d — an MSSQL connection must never reach the `LIMIT`-appending form on any branch-intermediate state):**

```rust
fn preview_sql(dialect: dbc_core::Dialect, schema: Option<&str>, table: &str) -> String {
    let target = dbc_core::quote_qualified_d(dialect, schema, table);
    match dialect {
        // `LIMIT 1000` is invalid T-SQL — TOP is the grammar-correct cap.
        dbc_core::Dialect::Mssql => format!("SELECT TOP 1000 * FROM {target}"),
        _ => format!("SELECT * FROM {target} LIMIT 1000"),
    }
}
```

- `auto_limit_each` gains `dialect: dbc_core::Dialect` and calls `apply_auto_limit_d(&s, n, dialect)`; its caller in `run_query_with`'s multi-statement branch passes the dialect it already resolved for splitting.
- `run_query_with` Guard 2 (the single-statement fallback) switches to `apply_auto_limit_d(&sql, n, sql_dialect(engine))`; the status suffix becomes dialect-vocabulary-correct: `" · auto-TOP {n}"` for Mssql, `" · auto-LIMIT {n}"` otherwise (its own string so the user sees the actual rewrite vocabulary — same rule at the multi-statement `limited` suffix site).
- `run_query_with` Guard 1's comment (main.rs:1434-1438, "Server-side enforcement lives in connect::open_config …") gains the honesty exception: "— EXCEPT MSSQL, which has no server-side read-only mode (driver integration note 5): for MSSQL this client-side check IS the only line."
- `count_statements_in_file` and `run_query_with`'s `split_sql` `Err(e)` arm format via `split_error_message(e)` instead of `{e:?}`.
- CSV dispatch call site(s) of `generate_insert_batches` thread `sql_dialect(engine)`.

**Grounding — `fk_join.rs`:** `build_join_sql(dialect, …)` — every `quote_ident`/`quote_qualified` → `_d`; the tail changes from one format string to:

```rust
    match dialect {
        dbc_core::Dialect::Mssql => {
            format!("SELECT TOP 1000 t.*{select_extra} FROM {base} t{from_extra}")
        }
        _ => format!("SELECT t.*{select_extra} FROM {base} t{from_extra} LIMIT 1000"),
    }
```

(`build_lookup_sql` is engine-neutral — no change.) Callers in `main.rs` thread `sql_dialect(engine)`.

**Grounding — `admin_sql.rs` delegation (§2a — one implementation, two names during transition):**

```rust
/// DEPRECATED-IN-COMMENT (G15 §2a): delegating wrapper over
/// `dbc_core::quote_ident_d` — dbc-core is the single bracket authority
/// now; remove this pair once all admin call sites take a Dialect
/// directly. Tests below stay as the contract.
pub fn quote_ident_for(engine: Engine, name: &str) -> String {
    dbc_core::quote_ident_d(crate::sql_dialect(engine), name)
}

pub fn quote_qualified_for(engine: Engine, schema: &str, object: &str) -> String {
    format!(
        "{}.{}",
        quote_ident_for(engine, schema),
        quote_ident_for(engine, object)
    )
}
```

(The existing `catalog_tests` — `we]ird` → `[we]]ird]` etc. — are kept UNCHANGED as the contract proving delegation preserved behavior.)

**Grounding — `backup.rs`:** `build_backup_sql`/`build_restore_sql` are MSSQL-only T-SQL builders that today double-quote the database name via `dbc_core::quote_ident` (their own comment names this as the follow-up). Switch both to `dbc_core::quote_ident_d(dbc_core::Dialect::Mssql, database)` (⇒ `BACKUP DATABASE [db] TO DISK = N'…' …`), delete the stale comment, update their unit tests to the bracket-quoted expectations.

**Grounding — `synthesize_create_table` call sites:** grep `synthesize_create_table` across `crates/` (schema-tree DDL fallback and the G7 text-diff are the known consumers); every call site where an engine is in scope switches to `synthesize_create_table_d(sql_dialect(engine), t)` (or threads an existing dialect). Call sites that are pg-only by construction may keep the wrapper — but record WHY at the call site if so.

**Steps:**

- [ ] **Step 1: Write the failing tests** (mirror the existing `we"ird` pg tests one-for-one, per design §2a's REQUIRED list):
  - sandbox: `sql_value_d_mssql_uses_nchar_literals` (`Some("Příliš žluťoučký")` → `N'Příliš žluťoučký'`; `'` doubling inside `N''`; numeric passthrough unchanged; `None` → `NULL`), `generate_statements_mssql_brackets_and_nchar` (a `TableMeta` with `dialect: Mssql`, a `we]ird` column and a staged Czech value → `UPDATE [s].[t] SET [we]]ird] = N'…' WHERE …`), plus an assertion the pg output is byte-identical to before.
  - csv_import: `generate_insert_batches_mssql_brackets_and_nchar` (bracket-quoted table/columns, `N''` values), `csv_import_batch_size_is_under_the_tsql_values_row_cap`.
  - fk_join: `build_join_sql_mssql_top_and_brackets` (mirror the existing pg golden-string test: `SELECT TOP 1000 t.*, j1.[name] AS [customers.name] FROM [public].[orders] t LEFT JOIN [public].[customers] j1 ON t.[customer_id] = j1.[id]`), `build_join_sql_pg_unchanged`.
  - main.rs: `preview_sql_mssql_uses_top` + `preview_sql_pg_unchanged`; `auto_limit_each_mssql_uses_top`; `split_error_message_go_count_is_czech`; `sql_dialect_is_total`.
  - backup.rs: updated golden strings (`BACKUP DATABASE [we]]ird] TO DISK = N'…'`).
  - admin_sql: existing `catalog_tests` untouched and green (the delegation contract).
- [ ] **Step 2: Run to see them fail, implement, run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (zero warnings; all existing quoting tests untouched and green).
- [ ] **Step 3: Commit** — `feat(ui): dialect-correct SQL generation — brackets, N'' literals, TOP previews, admin_sql delegation (G15 T4)`.

---

### Task 5 (T5): `runner.rs` — tx helpers through every sanctioned sequence (fixes the G12 bare-BEGIN bug)

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`

**Interfaces:**
- Consumes: T1's `tx_begin_sql`/`tx_commit_sql`/`tx_rollback_sql`, `quote_qualified_d`, `Dialect`.
- Produces (consumed by T6/T7/T8):

```rust
// runner.rs — spec → dialect for the tx sequences and compose_diff_select.
fn spec_dialect(spec: &ConnectSpec) -> dbc_core::Dialect {
    match spec {
        ConnectSpec::Config { cfg, .. } => match cfg.engine {
            dbc_state::Engine::Postgres => dbc_core::Dialect::Postgres,
            dbc_state::Engine::Sqlite => dbc_core::Dialect::Sqlite,
            dbc_state::Engine::Mssql => dbc_core::Dialect::Mssql,
        },
        // CLI-arg URLs have no MSSQL form (main.rs::engine_from_url:
        // postgres scheme or a sqlite file path only) — mirror
        // connect::open's exact scheme dispatch here (verify against it
        // when implementing).
        ConnectSpec::Url(url) => {
            if url.starts_with("postgres://") || url.starts_with("postgresql://") {
                dbc_core::Dialect::Postgres
            } else {
                dbc_core::Dialect::Sqlite
            }
        }
    }
}
```

**Grounding — literal inventory to replace (all in runner.rs; located by symbol, line numbers as of this branch):** every sanctioned sequence switches from the literal to the helper, threaded with the connection's dialect. The §3-novela sanctioned-caller LIST is unchanged in membership — each member just becomes dialect-correct:

| Sequence | Sites today | Change |
|---|---|---|
| `drive_write_sequence` (sandbox Apply + G10 admin) | `"BEGIN"`@1020, `"ROLLBACK"`@1021/1029/1035/1050, `"COMMIT"`@1049 | fn gains `dialect: dbc_core::Dialect` param; literals → `tx_begin_sql(dialect)`/`tx_rollback_sql(dialect)`/`tx_commit_sql(dialect)` |
| `drive_write_sequence_bounded` | `"ROLLBACK"`@1112 | gains + threads `dialect`; caller `run_write_transaction_inner` computes `spec_dialect(&spec)` BEFORE `open_spec` |
| `drive_analyze_write` (+ bounded) | `"BEGIN"`@1173, `"ROLLBACK"`@1174/1179/1199 | same treatment (T7's MSSQL plan path uses the helper STRINGS via query_with_session preludes instead — this keeps the pg path identical) |
| G12 script runner (`run_script` inner, `TxScope`) | WholeRun `"BEGIN"`@1587, PerFile `"BEGIN"`@1620, PerFile `"COMMIT"`@1693 + `"ROLLBACK"`@1694, failure-action `"ROLLBACK"`@1707/1713, WholeRun `"COMMIT"`@1739 + `"ROLLBACK"`@1740 | inner fn gains `dialect` (from `spec_dialect` at its top); all sites → helpers. T-SQL transactions legally span batches; statements that refuse explicit transactions (`BACKUP`, `ALTER DATABASE`, fulltext DDL) error verbatim — documented at the TxScope enum doc, not detected |
| `run_csv_import_inner` | `"BEGIN"`@1926 (the G12-noted bug — bare `BEGIN` is invalid T-SQL), `"ROLLBACK"`@1975/1982/2015/2041/2054, `"COMMIT"`@2053 | same treatment; this is the bug §3a exists to fix |

Additional changes in the same task:
- **`compose_diff_select` (G7)** gains a `dialect: dbc_core::Dialect` first param; `dbc_core::quote_qualified` → `quote_qualified_d(dialect, …)`; its stale doc sentence ("MSSQL bracket quoting via admin_sql::quote_ident_for is out of scope here since MSSQL is unwired") is rewritten. Callers (`fetch_diff_side` etc.) thread `spec_dialect(&spec)`. (RESOLVED vs the design's §7 hint, which put this in T-SQLGEN: it lives in runner.rs, so it moves to this task to honor single-writer.)
- **Fake-driver BEGIN matching:** the test harness matches the literal at `runner.rs:2653` (`if sql == "BEGIN"`) and `runner.rs:2847` (`"BEGIN" =>`) — extend both to also recognize `dbc_core::tx_begin_sql(dbc_core::Dialect::Mssql)` so fake-driver sequences stay assertable for all dialects (locate by symbol; these are the runner-seam fake-driver tests the phase-3 follow-ups doc wanted exercised before MSSQL work).
- **Script-path split-error text:** the `run_script` file-streaming maps `SplitError` into an event/status string — add the `UnsupportedGoCount => "GO s počtem opakování není podporováno"` arm there (grep `SplitError` in runner.rs; same literal as T4's `split_error_message`, duplicated deliberately so T4/T5 stay parallel-safe).
- **Doc sweep:** `drive_write_sequence`'s doc comment (the `let _ =` ROLLBACK discard paragraph) gains the §3b sentence: on MSSQL, after an XACT_ABORT abort the explicit ROLLBACK fails with "no corresponding BEGIN TRANSACTION" — exactly the case the discard tolerates (verified by matrix case 3).

**Steps:**

- [ ] **Step 1: Write the failing tests.**
  - `spec_dialect_maps_engines_and_url_schemes` (Config×3 engines; `postgres://`/`postgresql://` URLs → Postgres; a path → Sqlite).
  - `pg_sequences_still_send_the_literal_begin` — via the existing fake-driver capture at the `run_write_transaction` seam: the captured first statement for a pg spec is byte-equal `"BEGIN"` (zero behavior change for pg/sqlite is a hard requirement).
  - `mssql_write_sequence_opens_with_fused_xact_abort_begin` — same seam with an Mssql-engine spec + fake driver: first captured statement equals `"SET XACT_ABORT ON; BEGIN TRANSACTION"`.
  - **REQUIRED (read-only, §1a):** `run_write_transaction_refuses_read_only_mssql_without_connecting` — clone of the existing sqlite-shaped test at runner.rs:2527 with `engine: Engine::Mssql, read_only: true` (+ `mssql: None`): the shared guard fires before `open_spec`, no driver call, fast.
  - `csv_import_mssql_begin_is_dialect_correct` — fake-driver (or statement-capture) assertion that `run_csv_import_inner` against an Mssql spec issues the fused begin (the G12 bug's regression test).
- [ ] **Step 2: Run to see them fail, implement** (helpers threaded per the table, `spec_dialect`, compose_diff_select, fake-driver matches, split-error arm, doc sweep).
- [ ] **Step 3: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (every existing tx/script/csv/read-only test untouched and green — the pg/sqlite strings are byte-identical by T1's tx.rs contract; zero warnings).
- [ ] **Step 4: Commit** — `feat(ui): dialect-correct transaction control through every sanctioned sequence — fixes G12 bare-BEGIN on MSSQL (G15 T5)`.

---

### Task 6 (T6): Monitor — real MSSQL wiring (curation item 2: this is a task, not a flag flip)

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs` (serialized after T5), `crates/dbc-ui/src/monitor.rs`, `crates/dbc-ui/src/monitor_sql.rs`

`monitor.rs`'s refresh is hard-wired to `monitor_sql::pg`'s 8-statement shape (`run_monitor_refresh` lives in **runner.rs:2289-2320**, not monitor.rs — RESOLVED vs the design's §7 file hint; hence this task serializes after T5). MSSQL is 11 statements with a different tile mapping. `monitor_available` does NOT flip here — that is T8; after this task the Mssql arm is real code reachable the moment T8 flips the one gate.

**Interfaces (produced; pure, unit-tested in monitor.rs):**

```rust
// monitor.rs — merge rule (documented on each fn): a merged tile is Err
// ONLY when every constituent query failed; a failed/absent SECONDARY
// degrades to a NULL cell in the synthesized pg-shaped row, so the tile
// still renders its healthy half ("n/a" per-value, per the existing
// Option-cell posture). Row = the existing monitor::Row (cells as
// Option<String> text — verify the alias at implementation).
pub fn merge_mssql_connections(counts: Result<Vec<Row>, String>, max: Result<Vec<Row>, String>) -> Result<Vec<Row>, String>;
pub fn merge_mssql_locks(waiting: Result<Vec<Row>, String>, deadlocks: Result<Vec<Row>, String>) -> Result<Vec<Row>, String>;
pub fn split_mssql_size(size: Result<Vec<Row>, String>) -> (Result<Vec<Row>, String>, Result<Vec<Row>, String>);
pub fn merge_mssql_perf(cache: Result<Vec<Row>, String>, uptime: Result<Vec<Row>, String>, xact: Result<Vec<Row>, String>) -> Result<Vec<Row>, String>;
```

**Grounding — tile mapping (design §4 feature-matrix row, against the 11 `monitor_sql::mssql` constants and `RefreshResults`' 8 pg-shaped fields):**

- `merge_mssql_connections`: `CONNECTIONS` row `[active, idle]` + `CONNECTIONS_MAX` row `[value_in_use]` → one row `[active, idle, max]` matching `parse_connections`' pg contract. `value_in_use == "0"` means unlimited ⇒ max cell `None` (design: "value_in_use = 0 ⇒ max None"); `max` query Err ⇒ max cell `None`; `counts` Err with `max` Err ⇒ `Err(first message)`.
- `merge_mssql_locks`: `LOCKS_WAITING` `[count]` + `DEADLOCKS` `[cumulative]` → `[waiting, deadlocks]` (pg `LOCKS` contract). Same NULL-cell degradation.
- `split_mssql_size`: `SIZE` is ONE row, TWO cols `[data_bytes, log_bytes]` → `(data_size_rows, wal_size_rows)` each one row/one col, feeding the existing `SizeTile { data_bytes, wal_or_log_bytes }` cell reads unchanged.
- `merge_mssql_perf`: `CACHE_HIT` yields ratio + ratio-base counters — compute `pct = ratio / base * 100.0` client-side (base `0`/unparsable ⇒ `None` cell); + `UPTIME` `[secs]` + `XACT_TOTAL` `[cumulative]` → `[cache_hit_pct, uptime_secs, xact_total]` (pg `PERF` contract). `xact_total` stays CUMULATIVE — the existing `compute_rate` client-side delta in `monitor_view.rs` is reused untouched.
- `RUNNING`/`BLOCKING`/`TABLES` pass through `drain_rows` directly — column order already matches the pg parse contracts by construction (monitor_sql.rs module doc: "Column ORDER is monitor.rs's parse contract"); the REQUIRED live proof is T8 backlog item 4.

**Grounding — `run_monitor_refresh` Mssql arm (runner.rs; replaces the current `_ =>` all-error arm's "Unreachable today" comment):**

```rust
        dbc_state::Engine::Mssql => {
            use crate::monitor_sql::mssql as ms;
            let counts = drain_rows(conn, ms::CONNECTIONS, &cancel).await;
            let max = drain_rows(conn, ms::CONNECTIONS_MAX, &cancel).await;
            let waiting = drain_rows(conn, ms::LOCKS_WAITING, &cancel).await;
            let deadlocks = drain_rows(conn, ms::DEADLOCKS, &cancel).await;
            let size = drain_rows(conn, ms::SIZE, &cancel).await;
            let cache = drain_rows(conn, ms::CACHE_HIT, &cancel).await;
            let uptime = drain_rows(conn, ms::UPTIME, &cancel).await;
            let xact = drain_rows(conn, ms::XACT_TOTAL, &cancel).await;
            let (data_size, wal_size) = monitor::split_mssql_size(size);
            monitor::RefreshResults {
                connections: monitor::merge_mssql_connections(counts, max),
                locks: monitor::merge_mssql_locks(waiting, deadlocks),
                data_size,
                wal_size,
                perf: monitor::merge_mssql_perf(cache, uptime, xact),
                running: drain_rows(conn, ms::RUNNING, &cancel).await,
                blocking: drain_rows(conn, ms::BLOCKING, &cancel).await,
                tables: drain_rows(conn, ms::TABLES, &cancel).await,
            }
        }
        dbc_state::Engine::Sqlite => { /* the existing all-err arm, now Sqlite-only, message unchanged */ }
```

(Strictly sequential over the ONE dedicated monitor connection — session-sharing caveat, same as pg. 11 statements per refresh vs pg's 8; per-statement failure still degrades per-tile via the merge rule.)

Also in this task: remove `#[allow(dead_code)]` from `pub mod mssql` in monitor_sql.rs and rewrite its module doc (the "NOT runnable — no driver exists" paragraph is obsolete; the constants are now the live Mssql refresh set, gated only by `monitor_available` until T8). `kill_sql` needs NO change (`KILL {pid}` already routes through `execute` with the belt-and-braces read-only gate at `MonitorCmd::Kill` — sanctioned, G9 §0 rationale). `monitor_available`'s doc comment (monitor.rs:509-515) is corrected NOW to stop claiming "no other monitor-side change needed" (superseded by curation item 2) while its body stays Postgres-only until T8.

**Steps:**

- [ ] **Step 1: Write the failing tests** (pure, monitor.rs `mod tests`): `merge_connections_synthesizes_pg_shape` (2+1 cells → 3-cell row), `merge_connections_zero_max_means_unlimited_none`, `merge_connections_max_error_degrades_to_null_cell_not_tile_error`, `merge_connections_all_err_is_err`, `merge_locks_pairs_waiting_and_deadlocks`, `split_size_two_cols_to_two_tiles` (+ Err propagates to BOTH halves), `merge_perf_computes_cache_pct_and_zero_base_is_null`, `merge_perf_keeps_xact_cumulative` (cell passthrough — the delta stays in `compute_rate`).
- [ ] **Step 2: Run to see them fail, implement** (merge helpers → runner arm → doc/dead_code sweep).
- [ ] **Step 3: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` (existing `assemble_snapshot`/monitor tests untouched; zero warnings).
- [ ] **Step 4: Commit** — `feat(ui): real MSSQL monitor wiring — 11-statement refresh, tile merges (G15 T6)`.

---

### Task 7 (T7): Execution plans — G13 T7 delivered via `run_mssql_plan`

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs` (serialized after T6), `crates/dbc-ui/src/plan.rs`, `crates/dbc-ui/src/main.rs` (serialized after T4), `crates/dbc-ui/src/connect.rs` (reuses T3's helpers), `crates/dbc-core/src/connection.rs` (doc only)

**Interfaces:**
- Consumes: T2's `query_with_session`, T3's `mssql_connection_from_config`/`mssql_im002_hint`, T1's `tx_begin_sql`/`tx_rollback_sql`, existing `plan::parse_plan`/`analyze_gate`/`AnalyzeGate`.
- Produces:

```rust
// runner.rs — engine-specific runner method (same precedent as
// run_mssql_backup/run_mssql_restore: no Connection-trait change, no
// downcasting; the runner constructs the concrete MssqlConnection).
impl QueryRunner {
    pub fn run_mssql_plan(
        &self,
        spec: ConnectSpec,
        sql: String,
        analyze: bool,
        timeout_secs: Option<u64>,
    ) -> tokio::sync::oneshot::Receiver<Result<String, QueryError>>;
}

// runner.rs — pure, unit-testable prelude/postlude builder:
fn mssql_plan_session(analyze: bool) -> (Vec<String>, Vec<String>) {
    let d = dbc_core::Dialect::Mssql;
    if analyze {
        // SET STATISTICS XML has no only-statement restriction (run-time
        // setting); tx_begin is the FUSED XACT_ABORT form; the postlude
        // ROLLBACK runs ALWAYS (query_with_session contract) — the exact
        // drive_analyze_write discipline expressed as prelude/postlude.
        (
            vec!["SET STATISTICS XML ON".to_string(), dbc_core::tx_begin_sql(d).to_string()],
            vec![dbc_core::tx_rollback_sql(d).to_string()],
        )
    } else {
        // SET SHOWPLAN_XML ON must be alone in its batch; the server then
        // returns the plan XML INSTEAD of executing — inherently safe on
        // any connection including read-only (G13 §5 holds).
        (vec!["SET SHOWPLAN_XML ON".to_string()], Vec::new())
    }
}
```

**Grounding — `run_mssql_plan_inner`:**

```rust
async fn run_mssql_plan_inner(
    spec: ConnectSpec,
    sql: String,
    analyze: bool,
    timeout_secs: Option<u64>,
) -> Result<String, QueryError> {
    // Belt-and-braces (G13 parity): an ACTUAL plan of a WRITE refuses
    // read-only independently of the UI's analyze_gate. Reads — and every
    // estimated plan — stay allowed on read-only connections (§2e /
    // G13 §5 "Explain is always safe").
    if analyze && !dbc_core::is_read_statement(&sql) {
        guard_not_read_only(spec_is_read_only(&spec))?;
    }
    let ConnectSpec::Config { cfg, secret } = spec else {
        // No MSSQL URL form exists (engine_from_url) — defensive only.
        return Err(QueryError::msg("MSSQL plán vyžaduje uložené připojení"));
    };
    // Pure string building — no I/O, no spawn_blocking needed; the ssh /
    // integrated-auth refusals fire here exactly like open_config's arm.
    let conn = connect::mssql_connection_from_config(&cfg, secret)?;
    let (prelude, postlude) = mssql_plan_session(analyze);
    let run = async {
        let mut stream = conn
            .query_with_session(&prelude, &sql, &postlude, CancelToken::new())
            .await
            .map_err(connect::mssql_im002_hint)?;
        drain_stream_single_text_cell(&mut stream).await // new sibling of drain_single_text_cell, reading QueryStream directly
    };
    match timeout_secs {
        Some(t) => tokio::time::timeout(Duration::from_secs(t), run)
            .await
            .map_err(|_| QueryError::msg(format!("[timeout] analýza překročila {t}s")))?,
        None => run.await,
    }
    // On timeout the blocking session is orphaned; its connection drops
    // with the driver task — the ODBC-disconnect rollback backstop (§2e).
}
```

`run_mssql_plan` wraps the inner exactly like `run_mssql_backup` (oneshot + `self.runtime.spawn`). `drain_stream_single_text_cell`: first `Ok` batch, cell `[0][0]` as `String` (schema arrives via `stream.columns`); an `Err` batch or an empty stream → that error / `QueryError::msg("plán nevrátil žádná data")`.

**Grounding — `plan.rs` (curation item 1: the one-string `"SET SHOWPLAN_XML ON; {sql}"` form CANNOT work — only-statement-per-batch + session-scoped vs fresh-connection `query()`):**

- `explain_sql`'s Mssql arm: the broken string is deleted; the arm becomes a documented passthrough — `dbc_state::Engine::Mssql => sql.to_string()` with the comment "G15 §2e: MSSQL never routes through this builder — run_explain dispatches Engine::Mssql to run_mssql_plan (session preludes) before calling it; passthrough keeps the match total without a panic path."
- `explain_analyze_sql`'s Mssql arm: `Some("SET STATISTICS XML ON; …")` → `None`, comment "G15 §2e: delivered via run_mssql_plan's session preludes — a None here makes the generic wrap-and-run path structurally unable to emit the broken form (fail closed)." `analyze_button_visible(Mssql)` stays `true`.
- Update plan.rs's existing sql-builder tests for both arms; `parse_mssql_xml` and its fixtures are NOT touched here (their correction against real captures is T8 backlog item 8).
- The G13 defense-in-depth paragraph (plan.rs module doc / `analyze_gate` docs) gains the MSSQL honesty note: no server-side read-only backstop exists for MSSQL — the gate + the runner guard are the whole defense.

**Grounding — `main.rs` routing** (`run_explain` at main.rs:3170-3211, `on_confirm_analyze_write` at 3377-3424; gating is UNCHANGED — `analyze_gate`'s three cases dispatch by classification, not engine):

- `run_explain`, estimated path: before the generic dispatch, `if engine == dbc_state::Engine::Mssql { self.dispatch_mssql_plan(spec, sql, false, timeout_secs, cx); return; }`.
- `run_explain`, `AnalyzeGate::Run` arm: same two-line Mssql routing with `true`. `Blocked`/`NeedsConfirm` arms byte-unchanged (the confirm modal stays engine-agnostic).
- `on_confirm_analyze_write`: after the busy-guard/`running=true` block, `if engine == dbc_state::Engine::Mssql { … self.dispatch_mssql_plan(spec, sql, true, timeout_secs, cx); return; }` — the modal's running/error fields are driven the same way the pg path drives them.
- New `dispatch_mssql_plan(&mut self, spec, sql: String, is_analyze: bool, timeout_secs, cx)`: mirrors `on_confirm_analyze_write`'s existing rx plumbing — status `"vysvětluji plán…"`/`"analyzuji plán…"`, `let rx = self.runner.run_mssql_plan(spec, sql.clone(), is_analyze, timeout_secs);`, `cx.spawn` → on `Ok(Ok(raw))` parse via `plan::parse_plan(dbc_state::Engine::Mssql, is_analyze, &raw)` and open the `TabContent::Plan` tab with the same title/identity plumbing (`format!("Plán: {}", collapse_title(&sql))`, `current_conn_identity()`); analyze success status `"hotovo (změny vráceny zpět)"`; errors → status + (when the confirm modal is open) its `error` field, exactly like the pg analyze path.
- `dbc-core/src/connection.rs` `execute()` doc: the sanctioned-caller list gains ONE line — `run_mssql_plan` (G15 §2e: MSSQL face of the already-sanctioned analyze-write sequence; its session's tx control travels as query_with_session preludes/postludes over a dedicated connection, rollback ALWAYS).

**Steps:**

- [ ] **Step 1: Write the failing tests.**
  - runner.rs: `mssql_plan_session_estimated_is_lone_showplan_batch` (exact strings: `["SET SHOWPLAN_XML ON"]` / `[]`), `mssql_plan_session_actual_is_statistics_then_fused_begin_with_rollback_postlude` (`["SET STATISTICS XML ON", "SET XACT_ABORT ON; BEGIN TRANSACTION"]` / `["ROLLBACK"]`).
  - runner.rs: `run_mssql_plan_refuses_read_only_write_analyze_without_connecting` (read-only Mssql cfg + `"UPDATE t SET a=1"`, analyze=true ⇒ read-only error, no I/O) and `run_mssql_plan_read_analyze_passes_the_guard` (read-only cfg + `"SELECT 1"`, analyze=true, but with `ssh: Some(…)` so the SSH refusal proves the guard was passed and no connection was attempted — fast and deterministic).
  - plan.rs: updated builder tests (`explain_sql` Mssql passthrough; `explain_analyze_sql` Mssql `None`; `analyze_button_visible(Mssql)` still true).
- [ ] **Step 2: Run to see them fail, implement** (runner method + inner + drain sibling → plan.rs arms → main.rs routing → connection.rs doc line).
- [ ] **Step 3: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -p dbc-core` (zero warnings; all G13 pg/sqlite plan tests untouched).
- [ ] **Step 4: Commit** — `feat(ui,driver): MSSQL execution plans via session preludes — G13 T7 delivered (G15 T7)`.

---

### Task 8 (T8): T-LIVE — docker tier, the never-live backlog 1–8, the KEYSTONE matrix, ALL feature-ON flips, version bump

**Files:**
- Modify: `crates/dbc-driver-mssql/Cargo.toml`, `crates/dbc-ui/Cargo.toml`, `crates/dbc-driver-mssql/tests/{mssql_integration.rs,mssql_tx_matrix.rs}`, `crates/dbc-ui/src/{runner.rs,plan.rs,main.rs,monitor.rs}`, `crates/dbc-ui/tests/fixtures/mssql_showplan_*.xml`, workspace `Cargo.toml`
- Create: `crates/dbc-driver-mssql/tests/common/mod.rs`

This task runs on a machine with Docker Desktop (Linux containers/WSL2 — the same daemon the existing pg-gated tests require; the `mssql_server` image is Linux-only, ~1.5 GB, 30–60 s startup) AND, for the live tier, ODBC Driver 18 installed host-side. **Order within the task is mandatory: infra → live backlog with the §3c matrix FIRST → only after the matrix is green on this real machine do the ON-flips land** (if the matrix fails, STOP and execute Appendix F1/F2 before any flip).

**Step 8.1 — testcontainers infra.**

- [ ] `crates/dbc-driver-mssql/Cargo.toml` `[dev-dependencies]` += `testcontainers-modules = { version = "0.13", features = ["mssql_server"] }` (module existence at 0.13.0 VERIFIED during plan drafting — see Tech Stack; no crate bump needed, so the design's "bump if absent" contingency is dead).
- [ ] `crates/dbc-ui/Cargo.toml`: the existing dev-dep becomes `testcontainers-modules = { version = "0.13", features = ["postgres", "mssql_server"] }` (feature union, same crate+version — no version skew).
- [ ] Create `crates/dbc-driver-mssql/tests/common/mod.rs` (shared by both test files via `mod common;`):

```rust
//! Shared docker/env plumbing for the MSSQL live tier. `DBC_MSSQL_TEST_CONN`
//! stays the escape hatch (existing convention); testcontainers is the
//! default when unset. The container is started ONCE per test process and
//! deliberately leaked (`std::mem::forget`) — startup is 30–60 s and the
//! image ~1.5 GB, so per-test containers (the pg precedent) are not viable
//! here; testcontainers' reaper (ryuk) removes it after the process exits.

use tokio::sync::OnceCell;

static CONN: OnceCell<Option<String>> = OnceCell::const_new();

pub async fn conn_str_or_skip(test: &str) -> Option<String> {
    let s = CONN
        .get_or_init(|| async {
            if let Ok(s) = std::env::var("DBC_MSSQL_TEST_CONN") {
                return Some(s);
            }
            use testcontainers_modules::{
                mssql_server::MssqlServer, testcontainers::runners::AsyncRunner,
            };
            // ACCEPT_EULA is NOT set by Default — the explicit call is
            // required or the container exits immediately (verified
            // against the vendored 0.13.0 module source).
            let container = MssqlServer::default().with_accept_eula().start().await.ok()?;
            let host = container.get_host().await.ok()?;
            let port = container.get_host_port_ipv4(1433).await.ok()?;
            std::mem::forget(container);
            // TrustServerCertificate=yes: the container's self-signed dev
            // cert — the documented dialog path (§1c), never a default.
            Some(format!(
                "Driver={{ODBC Driver 18 for SQL Server}};Server={{tcp:{host},{port}}};\
                 Database=tempdb;Uid=sa;Pwd={{yourStrong(!)Password}};\
                 Encrypt=yes;TrustServerCertificate=yes;"
            ))
        })
        .await
        .clone();
    if s.is_none() {
        eprintln!("SKIP {test}: docker unavailable and DBC_MSSQL_TEST_CONN not set");
    }
    s
}

/// Honest SKIP for the one prerequisite docker cannot provide: no host
/// ODBC Driver 17/18. IM002-probe based — dbc-ui has no odbc-api dep, and
/// a second odbc Environment would violate the one-per-process rule, so
/// BOTH crates detect via the error the missing driver actually produces.
pub fn skip_if_no_odbc_driver(test: &str, e: &dbc_core::QueryError) -> bool {
    let missing = e.code.as_deref() == Some("IM002") || e.message.contains("IM002");
    if missing {
        eprintln!(
            "SKIP {test}: ODBC Driver 18 for SQL Server není nainstalován (IM002) — \
             install msodbcsql18 to run this test live"
        );
    }
    missing
}
```

- [ ] Rewire `mssql_integration.rs` and `mssql_tx_matrix.rs` onto `common::conn_str_or_skip` + a probe-first prologue per test: `let conn = MssqlConnection::from_connection_string(cs); if let Err(e) = conn.probe() { if common::skip_if_no_odbc_driver(name, &e) { return; } panic!("connect failed: {e}"); }` — no test ever panics on a missing environment fact, none is ever silently green.
- [ ] dbc-ui side: new `#[cfg(test)] mod mssql_docker_tests` in `runner.rs` (and a small sibling in `plan.rs` for backlog 8) — plain `#[test]` + `QueryRunner::handle().block_on`, module doc copied from `monitor_pg_tests`' nested-runtime writeup, run line `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui -- --ignored mssql_docker_tests::`. Shared helper inside the module: `fn mssql_spec(handle: &tokio::runtime::Handle) -> Option<(ConnectSpec, String)>` — starts/reuses the container via a `std::sync::OnceLock<Option<(String, u16)>>` (block_on the async start), builds `ConnectionConfig { engine: Mssql, host, port: Some(port), database: "master".into(), user: "sa".into(), mssql: Some(MssqlOptions { encrypt: true, trust_server_certificate: true, driver: None }), .. }` + `secret: Some("yourStrong(!)Password".into())`; first use probes via `open_spec` and converts an IM002 failure into the SKIP eprintln (the T3 hint text contains "msodbcsql18" — match on that or on `IM002`).

**Step 8.2 — the live backlog (design §5; run repeatedly while implementing, ALL must pass on this machine):**

- [ ] **(1)** `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql -- --ignored` — the entire never-run `mssql_integration.rs` suite ("the first thing to run against a live server"): UTF-16 diacritics round-trip, nulls, truncation marker, affected rows, exec_conn tx persistence, schema snapshot. Fix any driver bug it surfaces BEFORE proceeding (schema.rs's own caveat says its `sys.*` queries were never run).
- [ ] **(2) KEYSTONE:** the §3c matrix (`mssql_tx_matrix.rs`, cases 0–5). Case 0 red ⇒ Appendix F2. Case 4 red ⇒ Appendix F1. Record the green run's output in the task log/commit message — this evidence is the flip gate.
- [ ] **(3)** G10 admin (runner.rs `mssql_docker_tests`): `mssql_admin_catalogs_round_trip` — run `roles_catalog`/`privileges_catalog`/`sizes_catalog` MSSQL statement sets live; **REQUIRED**: `schema_sizes` against a freshly created EMPTY schema returns a `(name, 0)` row (the LEFT JOIN shape its comment admits was never verified); feed results through `parse_db_sizes`/`parse_schema_sizes`/`RoleRow`. `mssql_admin_builder_mutation_round_trip` — CREATE LOGIN/USER → GRANT/DENY/REVOKE → `sp_addrolemember` path → DROP, via the sanctioned admin write sequence.
- [ ] **(4)** G9 monitor: `mssql_monitor_refresh_populates_all_tiles` (all 11 constants through T6's real refresh + `assemble_snapshot`; asserts every tile `Some`), `mssql_monitor_blocking_chain_and_kill_round_trip` (a genuine two-session lock wait appears in the blocking tree; confirmed `KILL {spid}` through `monitor_loop` terminates it — mirror `monitor_pg_tests`' shape), and `mssql_monitor_low_privilege_login_degrades_tiles_not_refresh` (risk item: a login WITHOUT `VIEW SERVER STATE` — DMV-backed tiles go `None`/"n/a", the refresh itself survives; the `sa`-only suite would never catch a bad degradation path).
- [ ] **(5)** G11: `mssql_backup_restore_round_trip` — `run_mssql_backup` to a container-side path (`/var/opt/mssql/g15_test.bak` — the dialog's path is a SERVER-side path; in docker that means inside the container), mutate data, `run_mssql_restore`, assert the pre-backup state is back and the SINGLE_USER→RESTORE→MULTI_USER bracketing left the DB multi-user.
- [ ] **(6)** Sandbox Apply end-to-end (also §3c case 6): `mssql_sandbox_apply_bracket_and_nchar_round_trip` — a table with a `we]ird` column; staged UPDATE/INSERT/DELETE through `generate_statements` (TableMeta.dialect = Mssql) → `run_write_transaction` → re-read proves the Czech-diacritics `N''` value survived byte-exact; plus `mssql_csv_import_all_or_nothing` — a CSV whose LAST batch violates a constraint imports ZERO rows.
- [ ] **(7)** §2c/§2d live: `mssql_go_batched_script_with_procedure_per_file_tx` — a GO-batched script including a `CREATE PROCEDURE` body with interior semicolons through `run_script` with `TxScope::PerFile`; `mssql_auto_top_visible_in_result` — an editor-path query returns exactly TOP-n rows and the status suffix says `auto-TOP`.
- [ ] **(8)** Plans: capture REAL `SHOWPLAN_XML` and `STATISTICS XML` output (seq/index scan, join, missing-index case) via `run_mssql_plan`; REPLACE the hand-authored `crates/dbc-ui/tests/fixtures/mssql_showplan_*.xml` with (trimmed) real captures; correct `parse_mssql_xml` + its unit tests against them, closing every G13 needs-verification flag (`RunTimeCountersPerThread` aggregation, `<MissingIndexes>` shape, result-set column name — if the name differs from `Microsoft SQL Server 2005 XML Showplan`, fix the T2 selection constant, `loops: Some(1)` convention); end-to-end `mssql_plan_estimated_and_actual_round_trip` in plan.rs's docker module asserting actual-plan ROLLBACK left no trace.
- [ ] **REQUIRED redaction negative test (§1b):** `mssql_wrong_password_error_never_contains_the_password` — probe with password `"tajne$Heslo123"` against the live container: assert Err AND `!err.message.contains("tajne$Heslo123")` (and the conn string never printed).

**Step 8.3 — feature-ON flips (ONLY after 8.2's matrix evidence exists):**

- [ ] `main.rs::detect_editable_pk`: drop `|| engine == dbc_state::Engine::Mssql` (the driver doc's "MUST NOT be wired into Apply until ddl.rs is dialectized" precondition is discharged by T1/T4); replace test `mssql_engine_is_not_editable_even_with_a_mapped_pk` with `mssql_engine_is_editable_with_a_mapped_pk`.
- [ ] `main.rs::dialect_for_engine`: `Mssql => Some(dbc_core::Dialect::Mssql)` (total); flip its test to `dialect_for_engine_maps_all_three_engines`; rewrite the doc comment (the GO non-goal sentence is delivered). This turns ON: multi-statement editor (GO batches, first-result-set-only documented limitation), script runner, pre-scan counting for MSSQL.
- [ ] `monitor.rs::monitor_available`: `matches!(engine, Engine::Postgres | Engine::Mssql)`; doc rewritten; update its test.
- [ ] **Gate-message sweep:** grep `MSSQL driver zatím není k dispozici` across `crates/` — after this step it may remain ONLY in `crates/dbc-mcp/src/connect.rs` (curation item 5: dbc-mcp stays gated with its own message; the UI and MCP legitimately differ for one release — add that comment there). Retarget runner.rs: the doc comment on the backup inner fn (@~726) is rewritten; the four backup/restore tests asserting the old message (@4797-4865) become fast deterministic refusal tests — MSSQL cfg with `ssh: Some(…)` now fails with the SSH-tunnel message at `mssql_connection_from_config` (no server, no timeout), proving the arm is wired and the guard order (read-only exemption for backup, hard-block for restore) is unchanged.
- [ ] **Honesty-note sweep:** grep `server-side` in `crates/dbc-ui/src/` — confirm every backstop claim carries the MSSQL exception (T3 arm, T4 Guard 1, T7 plan docs; fix any straggler found).
- [ ] Full suite: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-driver-mssql -p dbc-ui -p dbc-mcp` (zero warnings) + re-run `-- --ignored mssql_docker_tests::` and the driver's `--ignored` tier one final time post-flip.
- [ ] Manual smoke on the live container: connect (Test button), browse schema tree + ER diagram, preview a table (TOP visible), run a GO script, stage+Apply an edit, open the monitor, Explain/Analyze, backup+restore, CSV import.

**Step 8.4 — version + merge.**

- [ ] Bump `[workspace.package] version` in the root `Cargo.toml` to the **next minor per merge order** — check `main`'s version at merge time and take the next unclaimed minor (main is at 0.15.0 as of this plan; do NOT hardcode ahead of the merge). `cargo build -p dbc-ui` to refresh `Cargo.lock`.
- [ ] **Commit** — `feat: G15 MSSQL wiring live tier — docker matrix green, feature flips, vX.Y.0 (G15 T8)` with the matrix evidence summarized in the body. Then follow superpowers:finishing-a-development-branch.

---

## Appendix F — pre-documented forks (design §8; execute ONLY on a red matrix, before any flip)

### F1 — Autocommit interference fork (matrix case 4 fails)

Symptom: with `SQL_ATTR_AUTOCOMMIT = ON`, the driver/ODBC commits between `execute()` calls even after a literal `BEGIN TRANSACTION` — case 4's second connection sees the row before `COMMIT`. The fix is the driver-contract change the driver's lib.rs note 3 pre-documents: transaction control moves from SQL text to the ODBC connection attribute, WITHOUT changing any T3–T7 app-layer code — the `tx_*_sql(Mssql)` strings become the driver's tx-protocol tokens:

- [ ] F1.1 `dbc-driver-mssql/src/lib.rs`: `execute()`'s blocking closure intercepts the EXACT dbc-core helper strings before `run_execute`: `sql == "SET XACT_ABORT ON; BEGIN TRANSACTION"` ⇒ `conn.set_autocommit(false)` (odbc-api `Connection::set_autocommit`) + `run_execute(&conn, "SET XACT_ABORT ON")` + return `Ok(0)`; `sql == "COMMIT"` ⇒ `conn.commit()` + `conn.set_autocommit(true)` + `Ok(0)`; `sql == "ROLLBACK"` ⇒ `conn.rollback()` + `conn.set_autocommit(true)` + `Ok(0)` (a rollback with no open tx must still return the Err the sequences' `let _ =` posture expects — match case 3's observed behavior; verify odbc-api's error here and mirror it). Interception applies ONLY to these three exact strings — arbitrary user SQL containing BEGIN/COMMIT text in a batch is untouched.
- [ ] F1.2 Update lib.rs module-doc note 3 (the "caller drives the SQL text" paragraph now documents the three-token exception and why), keep the two-public-items curation intact (interception is private).
- [ ] F1.3 Re-run the ENTIRE matrix (cases 0–5 must all pass under the new mechanism — case 5's XACT_ABORT persistence now set per-BEGIN) + backlog items 6–7 (Apply, CSV, script tx scopes) before proceeding to Step 8.3. The flips stay gated on THIS rerun.

### F2 — Row-count fork (matrix case 0 fails)

Symptom: SET/BEGIN/COMMIT/ROLLBACK batches report `SQL_NO_ROW_COUNT` (`row_count() == None`), which today's `types.rs::map_row_count` maps to an ERROR — every T5 sequence would fail at its first statement.

- [ ] F2.1 `dbc-driver-mssql/src/types.rs`: `map_row_count(None)` ⇒ `Ok(0)` (matching the pg/sqlite drivers' "0 affected" convention for non-DML), doc comment updated to name the live evidence; update its unit tests. DML row counts are unaffected, so `drive_write_sequence`'s affected-mismatch check keeps its meaning.
- [ ] F2.2 Re-run matrix case 0 + the affected-rows integration test (`execute_reports_affected_rows`) before proceeding.

---

## Self-review (performed at plan-authoring time)

- Every task grounds its edits in symbols verified to exist on this branch (four parallel code surveys, 2026-08-24); line numbers are hints, symbols are the contract.
- Resolved ambiguities vs the design are called out inline: `run_monitor_refresh`/`compose_diff_select`/`auto_limit_each` file locations (serialization consequences), T3 running SOLO (its `ConnectionConfig` field addition compile-breaks test struct literals workspace-wide), the nonexistent "SQLite conditional-row pattern" (SSH block idiom used instead), `sql_dialect` vs `dialect_for_engine` split (composers need the total mapping before the splitter gate flips), the IM002-probe SKIP (preserves the two-public-items curation AND the one-Environment rule), matrix case 0 (row-count risk found by grounding, gated by Appendix F2), `ACCEPT_EULA` requiring an explicit call.
- No task other than T8 flips a gate; T3's arm wiring is required by the live tier itself and is safe branch-intermediate because T4 dialectizes both auto-limit paths and all other features stay gated.
- pg/sqlite behavior is pinned byte-identical by explicit tests in T1 (wrappers) and T5 (literal `BEGIN` capture).
