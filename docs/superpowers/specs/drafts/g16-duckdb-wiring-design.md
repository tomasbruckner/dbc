# G16 — DuckDB Wiring: Design Pass

Date: 2026-08-24
Status: designed autonomously per the G5-style standing mandate; decisions
recorded here for later user review.
Scope: make the existing, fully-implemented-but-unreachable
`dbc-driver-duckdb` crate a first-class engine — new `dbc_state::Engine::
Duckdb` variant, connection-manager support, `connect.rs`/`dbc-mcp`
dispatch, and per-feature enable/gate decisions across the whole G1–G14
surface. This phase discharges three standing curation debts recorded at
earlier phases: G12 curation item 2 (`Engine::Duckdb → Dialect::Postgres` +
test), G13 curation item 1 (DuckDB plans are a "follow-up at DuckDB wiring
time"), and the memory-file "known wiring blockers" list (per-path
shared-root registry, aborts-like-pg transaction divergence).

**ORDERING (binding): this phase is serialized AFTER the MSSQL wiring phase
(G15).** Both phases edit the SAME `match cfg.engine`/`match engine` arms in
`connect.rs`, `connections_ui.rs` (`next_engine`/`engine_label`/
`test_connect_spec`/`needs_secret`), `plan.rs`, `monitor.rs`,
`admin_panel.rs`, `main.rs` (backup dispatch, `dialect_for_engine`,
`editable_decision`) and `dbc-mcp/src/connect.rs` — running them in
parallel guarantees conflicts on nearly every file this phase touches.
Additional ordering constraints found while grounding (beyond the shared
files already named): (a) G15 will DELETE the two hard-coded "MSSQL driver
zatím není k dispozici" refusals (`connect.rs:98`, `connections_ui.rs::
test_connect_spec`) that G16's exhaustive-match sweep sits right next to;
(b) G15 presumably resolves whether `editable_decision`'s
`engine == Engine::Mssql → NotEditable` arm survives (MSSQL `quote_ident`
dialectization) — G16's own `editable_decision` arm (§4) must be written
against whatever shape G15 leaves behind, not today's; (c) version bumps
are one minor per phase (spec §Versioning): G15 → 0.15.0, G16 → 0.16.0 —
a parallel run would race the same `Cargo.toml` line; (d) `execute()`'s
sanctioned-caller doc comment (§3-novela ledger) gets amended by both
phases — serializing keeps it a simple append.

Read before implementing: `crates/dbc-driver-duckdb/src/lib.rs` (FULL — the
per-path shared-root registry (`RegistryEntry`, `get_or_create_root`,
`canonical_key`), the mixed-mode policy (`mixed_mode_error`), the
`translate_open_error` PID-scrubbing, the `mid_transaction_error_aborts_
like_postgres` proof, and the already-Czech driver error strings this UI
phase must NOT duplicate); `crates/dbc-ui/src/connect.rs` (`open_config`'s
Sqlite arm — the shape the Duckdb arm mirrors, incl. server-side read-only
enforcement precedent); `crates/dbc-ui/src/connections_ui.rs`
(`next_engine`/`engine_label`/`test_connect_spec`/`on_dropdown_item_click`'s
`needs_secret`); `crates/dbc-ui/src/main.rs` (`dialect_for_engine`,
`editable_decision`, `backup_file_ext`, `plan_restore`, the per-engine
backup dispatch in `open_backup_dialog`); `crates/dbc-ui/src/plan.rs`
(`explain_sql`/`explain_analyze_sql`/`analyze_button_visible`/`parse_plan`
dispatch); `crates/dbc-core/src/guards.rs` (`READ_LEADING_KEYWORDS`,
`apply_auto_limit` — §5 widens both); `crates/dbc-ui/src/backup.rs` +
`runner.rs`'s `run_sqlite_backup_inner`/`run_sqlite_restore_inner` (the
SQL-statement-backup + sniff-then-`fs::copy`-restore pair the DuckDB
mechanics mirror); `crates/dbc-mcp/src/connect.rs` (the always-read-only
Sqlite arm).

## 0. What already exists vs. what this phase adds

The driver is DONE and reviewed (v0.5.0, 30+ tests): streaming `query()`
with protocol-level interrupt, `execute()` with a persistent `exec_conn`
for BEGIN…COMMIT sequences, full `schema()` (tables/views/columns/PK/FK/
CHECK/UNIQUE constraints/indexes/sequences via the `duckdb_*()` table
functions), server-side read-only (`AccessMode::ReadOnly`), the per-path
shared-root registry that routes around DuckDB's exclusive per-process
file lock, and Czech-translated open/mixed-mode errors. **G16 writes zero
driver code.** Everything below is `dbc-state` + `dbc-ui` + `dbc-mcp`
dispatch, plus two small `dbc-core` guard widenings (§5) and the G13
plan-parser follow-up (§8).

One clarification against the original phase-0 spec's framing: the "Arrow
zero-copy" ambition ("DuckDB later reads it zero-copy", 2026-08-21 spec)
is about the *rejected* G7 embedded-analytics use of DuckDB, not this
driver — the driver deliberately renders every cell to UTF-8 text into
`StringBuilder`-built `RecordBatch`es like every other driver (uniform
grid contract; see `value_to_text`). No zero-copy claim attaches to G16;
G7's design (§ "DuckDB vs pure-Arrow") already ruled the embedded-analytics
route out and nothing here reopens it.

## 1. `Engine::Duckdb` (dbc-state)

- **The variant:** `pub enum Engine { Postgres, Mssql, Sqlite, Duckdb }`
  (`config.rs:23`). With the existing `#[serde(rename_all = "lowercase")]`
  it serializes as `engine = "duckdb"` in `config.toml`. No other
  `dbc-state` change: `ConnectionConfig` already carries everything DuckDB
  needs — `database` doubles as the file path (SQLite precedent),
  `read_only` exists, `timeout_secs`/`auto_limit` are engine-agnostic.
- **Serde back-compat — decided, and it's the cheap direction:** adding a
  variant is purely additive for *loading old configs* (no existing
  `engine = "postgres"|"mssql"|"sqlite"` value changes meaning; no field
  is added to `ConnectionConfig`, so no `#[serde(default)]` dance). The
  one-way cost: a config that CONTAINS `engine = "duckdb"` will fail to
  load in a pre-G16 binary (toml's unknown-variant error → `AppConfig::
  load` returns `Err`, which the app surfaces rather than silently
  defaulting — `corrupt_file_is_load_error_not_default` posture). Accepted:
  same forward-compat posture every additive config change so far has
  taken (favourites, theme, tool_paths), and strictly better than those —
  they at least changed the file shape unconditionally on save; this one
  only affects users who create a DuckDB connection.
- **REQUIRED tests (config.rs, mirroring `old_config_without_theme_loads`):
  ** (a) a pre-G16 TOML snippet (postgres connection, no duckdb anywhere)
  loads unchanged; (b) a `[[connections]]` block with `engine = "duckdb"`,
  `database = "D:\\data\\analytics.duckdb"`, `read_only = true`
  round-trips through save/load; (c) `engine = "duckdb"` string form
  asserted exactly (serde rename spelling pinned so a future enum rename
  can't silently break saved configs).

## 2. Connection manager UI (connections_ui.rs)

- **Picker:** `next_engine` cycle grows to `Postgres → Mssql → Sqlite →
  Duckdb → Postgres`; `engine_label(Engine::Duckdb) = "duckdb"` (same
  short-lowercase convention as `"pg"`/`"mssql"`/`"sqlite"` — appears in
  the top-bar label, dropdown, compare labels, history connection names).
- **Form — mirror the SQLite arm EXACTLY, which today means: no
  conditional field hiding.** Ground truth: the current dialog renders
  Host/Port/Databáze/Uživatel/Heslo unconditionally for ALL engines, and
  the SQLite convention is behavioral — `database` is the file path,
  host/port/user/password are simply ignored by `open_config`. Duckdb
  adopts the identical convention (its `open_config` arm reads only
  `cfg.database` + `cfg.read_only`, §3). One small addition, since two
  file-based engines now share the convention and it was previously
  tribal knowledge: a muted helper row under the Databáze field, rendered
  only when `ui.engine` is `Sqlite` or `Duckdb`:
  `"u SQLite/DuckDB: cesta k databázovému souboru (host/port/heslo se
  nepoužijí)"`. No new fields, no layout fork.
- **`read_only` flag — dual enforcement, decided:** the existing checkbox
  ("Pouze pro čtení") drives BOTH layers, mirroring sqlite exactly:
  (1) driver-side, `DuckdbConnection::new_with_options(path, true)` opens
  the shared root with `AccessMode::ReadOnly` — engine-enforced, proven by
  the driver's `read_only_connection_rejects_writes`/`…_execute_writes`
  tests; (2) app-side, the SAME shared `guard_not_read_only`/
  `is_read_statement` choke points every engine already flows through —
  no per-engine read-only logic anywhere (§3-novela pattern). Neither
  layer is optional; the driver's mixed-mode policy (§3) exists precisely
  so layer 1 can never be silently downgraded.
- **No vault involvement:** `on_dropdown_item_click`'s `needs_secret`
  predicate (`c.engine != Engine::Sqlite`) becomes a named helper in
  `connections_ui.rs` — `fn engine_is_file_based(e: Engine) -> bool
  { matches!(e, Engine::Sqlite | Engine::Duckdb) }` — used by
  `needs_secret` (`!engine_is_file_based(..)`), the §2 helper-row
  condition, and `backup_file_ext`-adjacent dispatch where useful.
  Rationale for the helper: this predicate is about to appear in three
  places and a missed site would mean a pointless master-password prompt
  (or worse, a skipped one) for the wrong engine.
- **`test_connect_spec`:** no change needed — it only special-cases
  `Engine::Mssql` (and G15 deletes even that); Duckdb flows through to
  `ConnectSpec::Config` and the Test button exercises the real
  `open_config` arm off the UI thread, including the `:memory:` refusal
  and mixed-mode/locked errors (§3), which surface in the dialog's
  existing `✗ {e}` test-result line.

## 3. connect.rs wiring (+ dbc-mcp)

- **`open_config` arm (the core of the phase):**
  ```rust
  Engine::Duckdb => {
      if is_in_memory_duckdb_path(&cfg.database) {
          return Err(QueryError::msg(
              "in-memory DuckDB databáze není podporována — zadejte cestu k souboru",
          ));
      }
      let conn = DuckdbConnection::new_with_options(cfg.database.clone(), cfg.read_only);
      Ok(OpenConnection { conn: Box::new(conn), _tunnel: None })
  }
  ```
  `dbc-ui/Cargo.toml` gains `dbc-driver-duckdb = { path = ... }` (the
  crate is already a workspace member; note the `bundled` duckdb build
  joins dbc-ui's dependency graph — a one-time compile-cost hit, already
  paid by anyone running the driver tests today).
- **`:memory:` — decided: NOT supported in v1, rejected loudly at
  `open_config`.** Grounded reason, not taste: the app's execution model
  opens a fresh connection per dispatch (`open_spec` per run/fetch, dropped
  when the stream drains) and the driver's registry holds only a `Weak` —
  the moment the last `DuckdbConnection` for a path drops, the root (and
  with it an in-memory database's entire contents) is torn down. Under
  this app, `:memory:` would be an empty database on every single query —
  a data-eating trap, not a feature. `is_in_memory_duckdb_path` matches
  the exact string `:memory:` plus the empty string (pure helper,
  unit-tested in connect.rs's test module). Revisit only if the app ever
  grows a held-connection mode. The error string above is the UI-side
  message; it fires before the driver is ever constructed.
- **`open` (CLI-arg URL path) — unchanged.** `engine_from_url` keeps its
  two-way split (postgres URL / sqlite file path). A `.duckdb` CLI arg is
  an explicit non-goal for v1 (same posture as MSSQL: no URL form) —
  saved connections are the only entry point. Flagged in §13 as trivially
  addable (extension sniff) if ever requested.
- **SSH block:** ignored, byte-for-byte the SQLite arm's behavior (that
  arm silently ignores `cfg.ssh` today; no new divergence, no new error).
- **Per-path registry semantics — what the UI inherits, documented as UX
  facts (all already implemented and tested driver-side, nothing to
  build):**
  - *Two app connections to the SAME file, same mode:* work. Every
    `DuckdbConnection` for a canonicalized path shares one process-wide
    root; `query`/`schema`/`execute` sessions are `try_clone`s with
    independent transaction state (driver tests: `two_plain_selects_…`,
    `schema_races_query_…`, `transaction_isolated_between_clones_…`). Two
    tabs, sandbox-Apply's dedicated write connection alongside the browse
    connection, CSV import — all fine.
  - *Two saved connections to the same file with DIFFERENT `read_only`
    flags:* the second dispatch fails with the driver's
    `mixed-access-mode` Czech error ("databáze je již otevřena v jiném
    režimu…") for as long as any instance of the first is alive. This is
    deliberate driver policy (silent downgrade from engine-enforced to
    app-enforced read-only is forbidden); the UI surfaces the message
    verbatim in the status line / test-result line, no rewording.
    Because app connections are per-dispatch and short-lived, the window
    is small in practice; a long-lived holder (an in-flight script run's
    `exec_conn`) extends it — acceptable, the error tells the user what
    to do.
  - *Another PROCESS holding the file* (a second app instance, a stray
    CLI): open fails with the driver's translated `locked` error
    ("databázový soubor je právě používán jiným procesem: {path}" — PID/
    exe-path scrubbed by `translate_open_error`). Two processes can
    coexist only if BOTH open read-only (engine-level allowance the
    driver documents).
- **dbc-mcp arm (`dbc-mcp/src/connect.rs`):** mirrors its Sqlite arm —
  `DuckdbConnection::new_with_options(cfg.database.clone(), true)`,
  read-only **unconditionally** (MCP has no write path; always at least
  as restrictive as `cfg.read_only`). `dbc-mcp/Cargo.toml` gains the
  driver dep. Documented limitation (README/tool description, one line):
  the MCP server is a separate process, so it can reach a DuckDB file
  concurrently with the app only when BOTH sides are read-only — the app
  holding a read-write root means MCP's open fails with the translated
  `locked` error, and vice versa. That error already tells the user which
  process holds the file in human terms; no additional handling.
- **Exhaustive-match sweep:** adding the variant breaks compilation at
  every non-wildcard `match` over `Engine` — that is the FEATURE: the
  compiler enumerates the full wiring checklist. Grounded count at design
  time: 15 files, ~290 occurrences, concentrated in `admin_sql.rs` (100),
  `admin_panel.rs` (56), `main.rs` (39), `plan.rs` (29), `runner.rs` (28),
  `connections_ui.rs` (11), `monitor_sql.rs` (8), `dbc-mcp` (6),
  `connect.rs` (3), rest small. Every new arm's DECISION is in §4's
  matrix; no arm may be silenced with a `_ =>` wildcard (house rule:
  wildcards over `Engine` would let the NEXT engine skip this checklist).

## 4. Feature matrix — what turns on vs. what stays gated

| Feature | Duckdb decision | Mechanism / arm |
|---|---|---|
| Grid, result streaming, export, row view, FK lookup | **ON** | `query()` path, engine-agnostic; FK lookup's `quote_qualified(schema, table)` with driver-provided `FkRef` (schema `None` → unqualified — resolves in DuckDB's default schema, sqlite precedent) |
| Editor incl. multi-statement, params, history, palette | **ON** | `dialect_for_engine(Duckdb) = Some(Dialect::Postgres)` (§5) |
| Sandbox grid editing (G5) + Apply | **ON** | `editable_decision`: Duckdb is NOT added to the `NotEditable` engine arm — `dbc_core::quote_ident` (pg-style `"…"` doubling) is exactly DuckDB's identifier quoting, `sql_value` emission is engine-neutral, driver populates `is_pk`. REQUIRED matrix test |
| Script runner (G12) | **ON** | same dialect mapping; per-statement dispatch matrix unchanged (§6) |
| CSV import (G12) | **ON** | `is_numeric_type_name` already classifies DuckDB's type names by substring (`HUGEINT`/`UTINYINT`/…`INT`-family, `DOUBLE`, `FLOAT`, `DECIMAL(x,y)`, `REAL` all hit existing fragments; `VARCHAR`/`BLOB`/`DATE`/`TIMESTAMP` correctly fall to quoted). REQUIRED test with DuckDB type-name spellings, no code change expected |
| Compare (G7) | **ON** | `SchemaSnapshot` + Arrow diff, engine-agnostic; DuckDB becomes a pickable side with zero compare-code change |
| ER diagram (G8) | **ON** | `SchemaSnapshot`-driven; FK edges resolve by table name (target schema `None`, driver doc'd) |
| Charts (G14) | **ON** | `ResultBuffer`-driven, engine-agnostic |
| Schema tree, autocomplete, DDL view | **ON** | driver `schema()`; sequences appear, routines/triggers legitimately empty (DuckDB has no `CREATE TRIGGER`; macros out of scope — driver doc) |
| Auto-limit | **ON** (widened, §5) | `apply_auto_limit` fires for leading `FROM` too |
| Plan view (G13) | **ON** (new parser, §8) | `EXPLAIN (FORMAT JSON)` capture-first; analyze via the existing BEGIN→EXPLAIN ANALYZE→ROLLBACK sanctioned path |
| Backup / restore (G11) | **ON** (new mechanics, §7) | SQL-statement backup + sniff-and-copy restore, sqlite-family shape |
| Server monitor (G9) | **GATED OFF** | `monitor_available(Duckdb) = false` — embedded engine, no server sessions/locks to monitor; `kill_sql(Duckdb) = None` (protocol interrupt ≠ server-side kill). Same posture as sqlite |
| Server admin (G10) | **GATED OFF (Hidden)** | `admin_entry_state`: Duckdb → `AdminEntry::Hidden`; every `admin_sql` builder's Duckdb arm returns empty `Vec` exactly like the Sqlite arms (DuckDB has no roles/privileges/logins). `quote_ident_for(Duckdb)` → `dbc_core::quote_ident` (grouped with `Postgres | Sqlite`) so the helper stays total |
| SSH tunnel | **N/A** | ignored (file-based; §3) |
| MCP | **ON, read-only** | §3 |

## 5. Dialect, guards, auto-limit (dbc-core + main.rs)

- **`dialect_for_engine(Engine::Duckdb) = Some(dbc_core::Dialect::
  Postgres)`** — this is the G12 curation item 2 decision landing at last:
  DuckDB accepts `$$`/`$tag$` dollar-quoting and pg-flavored syntax, so
  the Postgres splitter's rules (dollar-quote bodies opaque, no
  trigger-BEGIN tracking) are the correct split semantics. The curation's
  mandated test lands with it: a two-statement DuckDB-bound script whose
  first statement contains a `$body$ … ; … $body$` dollar-quoted literal
  splits into exactly two statements under the mapped dialect (would
  mis-split under `Dialect::Sqlite`).
- **`READ_LEADING_KEYWORDS` widened (guards.rs):** add `"FROM"`,
  `"DESCRIBE"`, `"SUMMARIZE"`, `"PIVOT"`, `"UNPIVOT"`. Why this belongs in
  the WIRING phase and not a polish followup: DuckDB's idiomatic
  `FROM t` / `DESCRIBE t` / `SUMMARIZE t` statements currently fail the
  leading-keyword allowlist, which doesn't just reject them on read-only
  connections — on a WRITABLE connection the G12 dispatch matrix routes
  "not provably read" statements to `execute()`, so a `FROM t` query
  would run row-less and render nothing. That's a wrong-result bug for
  the engine's most idiomatic query form. Safety argument for widening a
  security allowlist: the guard's second layer is untouched — the
  whole-statement `WRITE_KEYWORDS` blacklist scan still rejects any
  statement containing `INSERT`/`COPY`/`ATTACH`/`INTO`/… anywhere.
  **CORRECTION (G16 T2 review round 1, BLOCKER — the widening is
  dialect-gated, NOT engine-blind as this doc originally claimed:** the
  original "no engine in this app has a write statement that LEADS with
  any of the five new words" was WRONG for MSSQL. T-SQL executes the
  first statement of a batch as an implicit stored-procedure call with no
  EXEC keyword (the `sp_help t` convention), and DESCRIBE/SUMMARIZE are
  not T-SQL reserved words — `CREATE PROCEDURE DESCRIBE ...` is legal, so
  the batch `DESCRIBE t` would execute procedure [DESCRIBE] with 't' as
  an argument; on MSSQL the client-side guard is the ONLY read-only
  enforcement. FROM/PIVOT/UNPIVOT are T-SQL reserved words and are safe,
  but the implementation excludes ALL FIVE new keywords under
  `Dialect::Mssql` — byte-identical pre-G16 Mssql classification, pinned
  by test. Pg/sqlite keep the widened list: neither has any statement
  starting with one of the five (syntax errors the server refuses —
  fail-closed guard passing them is harmless) and Postgres has the
  server-side read-only backstop.) Fail-closed posture preserved.
  REQUIRED tests: each new keyword accepted bare (pg path); each refused
  under `Dialect::Mssql`; `FROM t` + write keyword later in the statement
  still rejected; existing suite untouched.
- **`apply_auto_limit` widened symmetrically:** fires when the first word
  is `SELECT` **or `FROM`** (unchanged skip conditions: any
  `LIMIT`/`OFFSET`/`FETCH`/`INTO` token). `FROM t LIMIT n` is valid
  DuckDB; a leading-FROM statement can't reach pg/sqlite (syntax error
  before the limit would matter). (Corrected per T2 review: "engine-safe
  without an engine parameter" holds here only because the widening lives
  in the pg/sqlite body `apply_auto_limit_pg` — the `Dialect::Mssql` arm
  of `apply_auto_limit_d` keeps its own `first_word == SELECT` check and
  never sees the widening, pinned by test.) REQUIRED tests: `FROM t`
  gains a limit; `FROM t LIMIT 5` untouched; `SELECT` behavior
  byte-identical to today; leading `FROM` never gains a `TOP` under
  `Dialect::Mssql`.

## 6. Transactional semantics — nothing to build, one thing to prove

DuckDB aborts an open transaction on the first failed statement, exactly
like Postgres and unlike SQLite — empirically pinned by the driver's
`mid_transaction_error_aborts_like_postgres` test (every post-error
statement fails with "current transaction is aborted" until `ROLLBACK`).
Every transaction-driving caller in this app — `run_write_transaction`
(sandbox Apply, admin), `run_script`'s per-file/whole-run scopes, CSV
import's single whole-import transaction, `run_analyze_write`'s
BEGIN→ROLLBACK — already follows the trait-doc-mandated
stop-at-first-error-and-roll-back discipline *uniformly across engines*
(G12 §2 deliberately refused per-engine exceptions). **Decision: zero
caller changes.** What G16 adds is proof at the UI layer, since the
embedded engine makes it nearly free (§10): a `runner.rs` test driving
`drive_write_sequence` against a temp `.duckdb` file where statement 2
fails — asserts rollback ran, nothing committed, connection usable after;
and a `drive_script` per-file-scope variant where a failing file rolls
back and (`Continue` policy) the next file still commits. These are the
same shapes the sqlite-backed tests already have — the DuckDB variants
exist to catch the pg-style divergence that sqlite's tolerance would mask.

## 7. Backup & restore (G11 extension)

- **Backup — decided: NOT a file copy; SQL-statement backup over the
  normal connection, mirroring sqlite's `VACUUM INTO` shape.** Copying a
  live DuckDB file (WAL + open writers) is exactly the corruption risk
  `VACUUM INTO` exists to avoid on sqlite, and DuckDB has no `VACUUM
  INTO` — its supported online single-file-copy idiom is:
  ```sql
  ATTACH '<dest>' AS __dbc_backup;
  COPY FROM DATABASE <src> TO __dbc_backup;
  DETACH __dbc_backup;
  ```
  New pure builder `backup::build_duckdb_backup_sql(src_db_name: &str,
  dest_path: &str) -> Vec<String>` (three statements; dest path
  single-quote-escaped via the same `'' `-doubling `build_vacuum_into_sql`
  already tests with `o'brien.sqlite`; `src_db_name` through
  `dbc_core::quote_ident`). `src_db_name` is fetched at run time with one
  `SELECT current_database()` on the dedicated backup connection (DuckDB
  names a file database after its file stem — computing it client-side
  from the path would duplicate engine logic; asking the engine is one
  cheap query). Runner method `run_duckdb_backup_inner` mirrors
  `run_sqlite_backup_inner` line for line: shared
  `guard_backup_restore_read_only(Backup, …)` first (Backup stays exempt
  per G11 curation item 2 — the existing predicate, no new logic), open
  ONE dedicated connection, the query, then the three `execute()` calls in
  order. These statements join `execute()`'s sanctioned-caller list under
  the EXISTING G11 backup entry (amended text, not a new entry).
  `backup_file_ext(Duckdb) = "duckdb"`. Dialog plumbing (`open_backup_
  dialog`'s Duckdb arm) mirrors the Sqlite arm: `command_line` shows the
  three statements verbatim (§3-novela: user sees exactly what will run).
  - *Read-only configs:* the dedicated backup connection opens in the
    config's own mode (never a sneaky rw open — that would trip the
    driver's mixed-mode policy against any concurrent ro root, and
    silently escalating privileges for a convenience feature is the wrong
    trade). If a read-only DuckDB instance refuses the write-mode `ATTACH`,
    the engine's error surfaces verbatim — the exact posture G11 curation
    item 2 already blessed for server-refused backups. REQUIRED test pins
    whichever behavior the engine actually has (see §10 — embedded, so
    this stops being a "needs live verification" and becomes a `#[test]`).
- **Restore — decided: sniff + `fs::copy`, the sqlite shape.**
  `plan_restore` gains `Engine::Duckdb => Ok(RestorePlan::Duckdb)`;
  runner-side `run_duckdb_restore_inner` mirrors
  `run_sqlite_restore_inner`: (1) `guard_backup_restore_read_only(Restore,
  …)` FIRST — restore stays hard-blocked on read-only, no exemption, no
  I/O before the guard; (2) magic sniff: a DuckDB database file carries
  the ASCII bytes `DUCK` at offset 8 of its main header (first 8 bytes are
  a checksum) — new `backup::DUCKDB_MAGIC_OFFSET`/`duckdb_magic_ok(bytes:
  &[u8]) -> bool` requiring `len >= 12 && &bytes[8..12] == b"DUCK"`,
  refusal message `"soubor není DuckDB databáze"`; the magic constant is
  verified against a freshly-created file in the test suite itself (§10),
  not trusted from documentation; (3) `fs::copy` over the target path.
  If any live root holds the target file (this process or another), the
  OS copy fails loudly — surfaced verbatim, acceptable (the driver's
  per-dispatch connection model makes a lingering root the exception).
  Typed-database-name confirm friction: unchanged, same modal as every
  restore.

## 8. Plan view (G13 follow-up, discharged here)

- **Estimated:** `explain_sql(Duckdb) = "EXPLAIN (FORMAT JSON) {sql}"`.
  **Actual:** `explain_analyze_sql(Duckdb) = Some("EXPLAIN (ANALYZE,
  FORMAT JSON) {sql}")`; `analyze_button_visible(Duckdb) = true`. Analyze
  on a write statement flows through the EXISTING `analyze_gate` +
  `AnalyzeWriteConfirm` modal + `run_analyze_write` (dedicated connection,
  BEGIN → wrapped statement → ROLLBACK) — no new write surface; DuckDB's
  pg-style tx semantics (§6) are exactly what that rollback path assumes.
- **Parser — capture-FIRST discipline, with the fallback pre-decided (so
  neither outcome is a TBD):** the implementing task's step 1 is a plain
  `#[test]` (no docker — embedded!) that creates a temp `.duckdb`, runs
  both EXPLAIN forms through the real `query()` path, and commits the
  captured output as fixtures — the same fixture-capture gate G13
  curation item 5 imposed on the pg parser, but paid in milliseconds.
  Then exactly one of two pre-decided branches:
  - *JSON confirmed at the vendored engine version* (expected: an array
    of operator nodes with `name`, `children`, and an `extra_info`
    string-map; ANALYZE adds per-operator timing/cardinality):
    `parse_duckdb_json` in `plan.rs` maps `name → operation`,
    `extra_info` entries → `target` (first table-ish key) + `extra`
    (rest, never dropped), timing/cardinality → the existing
    `PlanNode` actual-fields; `parse_plan` gains the `Duckdb` arm;
    hot-node fraction reuses the existing self-cost normalization.
  - *JSON absent/unusable at this version:* `explain_sql` falls back to
    plain `"EXPLAIN {sql}"`, and the Duckdb arm builds a single-root
    `PlanResult` (operation `"DuckDB plán"`, no children, no costs) whose
    `raw_text` is the concatenated `explain_key`/`explain_value` rows —
    the plan tab's existing raw-text surface becomes the primary view.
    The button works either way; a dead "Plán" button on a wired engine
    is not an acceptable exit state for G16.
- Fixture tests mirror the pg parser's suite (seq-scan-ish plan, a join,
  an ANALYZE plan with timings, malformed-JSON fail-closed) against the
  captured fixtures.

## 9. Schema tree / catalog — decision recorded: `duckdb_*()` table functions, NOT information_schema

Already settled driver-side, recorded here because the phase brief asks:
the driver's `schema()` uses `duckdb_tables()` / `duckdb_views()` /
`duckdb_columns()` / `duckdb_constraints()` / `duckdb_indexes()` /
`duckdb_sequences()`, not `information_schema`. Rationale (from the
driver's own comments, kept as the standing decision): the native
functions expose what `information_schema` can't — stable `*_oid` keys to
join the collections, original `CREATE …` DDL text (`sql` column),
`internal`/`temporary` flags for clean system-object exclusion, and
structured constraint column lists. UI work in G16: **none** — schema
tree, autocomplete, DDL view, favourites all consume `SchemaSnapshot`.

## 10. Live-validation tier — the embedded dividend

No docker, no `--ignored`, no external server: every DuckDB integration
test is a plain `#[test]`/`#[tokio::test]` over `tempfile` paths (the
driver's own 30-test suite is the template — note its fixture quirk:
delete the `NamedTempFile` before letting DuckDB create the database).
This is the cheapest live tier of any engine in the app, and the suites
below are REQUIRED, not aspirational — each converts one of this design's
factual claims into a pinned regression test:

- **dbc-state:** the three §1 serde tests.
- **connect.rs tests (dbc-ui):** Duckdb arm opens + `SELECT 1` round-trip;
  `:memory:` rejected before driver construction; read-only config →
  driver-level write refusal (proves the arm passes the flag through);
  mixed-mode error surfaces through `open_config` when a same-path
  opposite-mode instance is alive.
- **runner.rs:** `run_write_transaction` commit + mid-sequence-failure
  rollback over a temp `.duckdb` (§6); `drive_script` per-file scope with
  `Continue` policy (§6); CSV import end-to-end (mapped columns, one tx,
  bad-batch rollback leaves zero rows); read-only script rejects the
  write statement client-side via the SHARED guard (G12 curation item 4
  shape, DuckDB variant); `run_analyze_write` BEGIN→ROLLBACK leaves a
  write un-committed on DuckDB.
- **backup/restore (runner.rs + backup.rs):** pure builder-shape/quoting
  tests for `build_duckdb_backup_sql`; end-to-end backup of a seeded temp
  db → open the DEST file with the driver → seeded rows present; restore
  happy path; restore refuses wrong-magic file (no copy attempted);
  restore refuses read-only before any I/O; the §7 read-only-backup
  behavior pin; `duckdb_magic_ok` verified against a real freshly-created
  file (not just the constant).
- **guards/split (dbc-core):** §5's keyword/auto-limit tests; the §5
  dollar-quote dialect-mapping test.
- **plan.rs:** the §8 capture test + fixture parsers.
- **main.rs pure fns:** `dialect_for_engine`, `editable_decision`,
  `backup_file_ext`, `plan_restore`, `engine_is_file_based`,
  `admin_entry_state`, `monitor_available`, `kill_sql` — each existing
  per-engine table test gains its Duckdb row.
- **dbc-mcp:** temp-file DuckDB config → list/query succeed; write
  refused (always-read-only arm), mirroring the existing sqlite MCP tests.

## 11. Security invariants (restated for the new paths)

1. **Write choke point:** every DuckDB write reaches `Connection::
   execute` only through the already-sanctioned runner methods (sandbox
   Apply / admin `run_write_transaction`, `run_script`, CSV import,
   `run_analyze_write`, backup/restore) — G16 adds NO new write method;
   the DuckDB backup statements ride the existing G11 backup entry.
   `execute()`'s sanctioned-caller doc comment is amended once (after
   G15's own amendment) rather than re-declared.
2. **Read-only is dual-enforced:** shared `guard_not_read_only`/
   `is_read_statement` client-side AND `AccessMode::ReadOnly`
   engine-side; the driver's mixed-mode policy guarantees the engine-side
   layer can't be silently lost to root sharing. §5's allowlist widening
   never weakens this — the write-keyword blacklist still scans every
   token of every statement.
3. **Secrets:** DuckDB connections have no password; the
   `engine_is_file_based` predicate keeps the vault entirely out of the
   flow (no prompt, no `get_secret`, nothing to leak). No SQL built in
   this phase embeds a secret; the backup `ATTACH` embeds only a
   user-chosen file path, escaped by `''`-doubling.
4. **Error hygiene:** the driver already scrubs PID/exe-path from lock
   errors (`translate_open_error`) — the UI surfaces driver messages
   verbatim and adds nothing process-identifying. History entries for
   DuckDB runs carry connection NAME + file path only (existing
   `HistoryEntry` fields), never contents.
5. **Restore friction:** typed-database-name confirm + read-only
   hard-block + magic-header sniff, all before any byte is copied — the
   full G11 discipline, no DuckDB shortcut.

## 12. Task decomposition (hint, G15-style compile-driven)

- **T1 — `Engine::Duckdb` + serde tests (dbc-state).** Tiny; lands first
  and intentionally BREAKS every downstream `match` — the checklist.
- **T2 — dispatch sweep:** `connect.rs` arm (+ `:memory:` guard + tests),
  `dbc-mcp` arm, `dialect_for_engine`, `engine_label`/`next_engine`/
  `needs_secret`/`engine_is_file_based` + form hint row, and every
  gated-off arm from §4's matrix (`monitor_available`, `kill_sql`,
  `admin_entry_state`, `admin_sql` empty-Vec arms, `quote_ident_for`
  grouping, `editable_decision`). Compiles green = matrix implemented.
  Depends on T1.
- **T3 — guards + split (dbc-core):** §5 keyword/auto-limit widenings +
  the dialect-mapping dollar-quote test. Depends on T1 only; parallel
  with T2.
- **T4 — backup/restore (§7):** builders + runner methods + dialog arm +
  the §10 backup/restore suite. Depends on T2.
- **T5 — plan view (§8):** capture test first, then parser or fallback +
  fixtures. Depends on T2; parallel with T4.
- **T6 — integration tail:** §10's runner/mcp end-to-end suites,
  `execute()` doc amendment, zero-warnings sweep, version bump to 0.16.0.
  Depends on T2–T5.

## 13. Risks / needs-verification

- **`COPY FROM DATABASE` availability/behavior at the vendored duckdb
  crate (`~1.10504.0`)** — the backup mechanic's load-bearing assumption.
  Converted from risk to gate by T4's first test (embedded round-trip,
  §10); if the engine version predates the idiom, the pre-decided
  fallback is `EXPORT DATABASE` to a sibling *directory* — a real
  target-picker UX change, so surface it at T4 start rather than
  discovering it at review.
- **`EXPLAIN (FORMAT JSON)` shape at the vendored version** — same
  conversion: T5's capture test decides between the two §8 branches;
  neither branch is design-blocked.
- **Read-only backup (`ATTACH` for write from a read-only instance)** —
  behavior unknown by design (§7 decision covers both outcomes); the pin
  test documents whichever holds so the modal copy can say the truth.
- **Compound-type cells (LIST/STRUCT/MAP)** render via the driver's
  `Debug` fallback — accurate but ugly in the grid for DuckDB's nested
  types, which real DuckDB users hit more than sqlite users ever would.
  Known driver limitation, explicitly NOT expanded in this wiring phase;
  candidate polish follow-up.
- **Long-lived roots vs. restore/mixed-mode windows:** a running script/
  import holds its `exec_conn` (and root) for the run's duration, during
  which a restore `fs::copy` or an opposite-mode open fails loudly.
  Correct behavior, but worth one line in the restore modal copy if user
  reports confusion.
- **`FROM`-leading statements in HISTORY predate the §5 widening** — after
  T3, re-running an old history entry that previously errored now
  executes; same "capability unlock, regression-pass the starred entries"
  note G12 §7 recorded for the multi-statement unlock.
- **Build cost:** bundled libduckdb joins the dbc-ui/dbc-mcp dependency
  graphs (first clean build noticeably longer). One-time, already paid in
  CI by the driver's own suite; no action.
