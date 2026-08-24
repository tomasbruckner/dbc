# G15 — MSSQL wiring: Design Pass

Date: 2026-08-24
Status: designed autonomously per the G5-style standing mandate; decisions
recorded here for later user review.
Scope: making the existing-but-unreachable MSSQL support real. The driver
crate (`dbc-driver-mssql`, odbc-api, v0.5.0) exists and is unit-tested but
unwired: `connect::open_config`'s `Engine::Mssql` arm hard-errors ("MSSQL
driver zatím není k dispozici"), and a backlog of MSSQL-shaped code written
across G9–G13 (monitor SQL, admin catalogs/builders, backup/restore T-SQL,
Showplan parser, bracket quoting in `admin_sql`) has never touched a live
server. G15 wires the driver in, dialectizes the app's SQL generation, and
runs every never-live MSSQL string against a real dockerized SQL Server.

Read before implementing: `crates/dbc-driver-mssql/src/lib.rs` (FULL — the
module doc's five "Integration notes (things this crate does NOT fix)" are
this phase's requirements list: quote_ident dialectization, Encrypt/Trust
dialog options, XACT_ABORT/mid-tx-error verification, ODBC Driver 17/18
runtime prerequisite, no server-side read-only mode); `config.rs` there
(`MssqlConfig`, `escape_odbc_value` brace rules); `tests/mssql_integration.rs`
(the `#[ignore]`d, never-run driver tests — "the first thing to run against
a live server"); `crates/dbc-ui/src/connect.rs` (`open_config` — the arm
being replaced, plus the pg arm's timeout/read-only/tunnel discipline this
mirrors); `crates/dbc-core/src/{ddl.rs,guards.rs,split.rs,connection.rs}`;
`crates/dbc-ui/src/{sandbox.rs,csv_import.rs,fk_join.rs,admin_sql.rs,
monitor_sql.rs,monitor.rs,plan.rs,runner.rs,backup.rs,connections_ui.rs}`;
the G9 §3/§7, G10, G11, G12 §1/§2, G13 §1b/§5/T7 design/plan sections named
inline below; `docs/superpowers/2026-08-21-phase-3-follow-ups.md` (the
runner-seam fake-driver test gap "worth closing before MSSQL driver work").

> **CURATION (2026-08-24, binding — reconciles stale claims in earlier
> drafts; where they conflict, THIS section wins):**
> 1. **G13's `explain_sql` MSSQL arm is superseded.** `plan.rs` today emits
>    `"SET SHOWPLAN_XML ON; {sql}"` as one string (explicitly marked
>    unreachable/T7). That form cannot work: per Microsoft's documented
>    rule, `SET SHOWPLAN_XML` must be the ONLY statement in its batch, and
>    it is session-scoped while `MssqlConnection::query()` opens a fresh
>    connection per call. §2e below replaces it with a driver-level
>    session-prelude mechanism. G13 §1b's needs-verification flags close
>    against real captures in T-PLAN (§7).
> 2. **G9 §7's "no other monitor-side change needed" is superseded.** That
>    sentence covered only the connect layer. `monitor.rs`'s refresh is
>    hard-wired to `monitor_sql::pg`'s 8-statement shape; MSSQL is 11
>    statements with different tile mapping. Wiring the monitor is a real
>    task (§4, T-MON), not a one-line `monitor_available` flip.
> 3. **`split.rs`'s "GO belongs in a separate line-based pre-pass" comment
>    is superseded in mechanism, upheld in substance:** `Dialect::Mssql` is
>    added INSIDE `StatementSplitter` (not as a bolt-on pre-pass), but its
>    split trigger is GO-lines, never `;` — see §2c for why `;`-splitting
>    T-SQL is actively dangerous, not just unsupported.
> 4. **The driver crate itself is NOT frozen:** G15 adds exactly two public
>    items to `dbc-driver-mssql` (`MssqlConnection::probe()`,
>    `MssqlConnection::query_with_session(...)`, §1/§2e) and changes
>    nothing else there. The `Connection` trait in `dbc-core` is untouched.
> 5. **dbc-mcp stays gated.** Its own design (mcp-server-design.md) errors
>    `Engine::Mssql` and `ssh: Some(_)` at its layer 2 by explicit decision;
>    lifting that is a separate follow-up with its own security review, not
>    a G15 side effect. The UI and the MCP server may legitimately differ
>    here for one release.

## 0. Architecture up front: wire, dialectize, verify — no trait changes

Three moves, in dependency order. (1) **Wire:** `open_config`'s
`Engine::Mssql` arm builds an `MssqlConfig` from the saved
`ConnectionConfig` + vault secret and returns an `MssqlConnection` — the
driver is blocking-internally (`spawn_blocking` per operation, per its
module doc) and already implements the full `Connection` trait, so the
runner/UI plumbing needs zero changes to *transport* MSSQL results. (2)
**Dialectize:** every place the APP composes SQL text (sandbox Apply, CSV
import, preview/fk-join/diff SELECTs, auto-limit, transaction control,
script splitting, plans) today assumes pg/sqlite conventions; a single
`Dialect` authority in `dbc-core` (§2) gains an `Mssql` variant and every
composer takes it as a parameter. (3) **Verify:** the accumulated
never-live MSSQL SQL (G9 monitor, G10 admin incl. the schema_sizes LEFT
JOIN, G11 backup/restore, driver integration tests) plus the new
transactional semantics (§3's XACT_ABORT matrix) run against a dockerized
SQL Server via testcontainers (§5) before any feature-ON flip merges.

Non-goals, decided (each with its Czech refusal where user-visible):
Windows integrated auth (`Trusted_Connection`) — SQL auth only in v1, empty
user errors with "MSSQL: zadejte uživatele — ověření přes Windows účet
zatím není podporováno"; SSH tunnel for MSSQL (§1d); DSN-based connections
(§1b); `GO <n>` repeat counts (§2c); dbc-mcp MSSQL (curation item 5);
multi-result-set rendering for one batch (§2c, documented limitation).

## 1. Connection wiring (`connect.rs`, `dbc-state`, `connections_ui.rs`)

### 1a. `open_config`'s `Engine::Mssql` arm

Replaces the permanent error with (all inside the existing blocking-legal
context — `open_config` is only ever called from `spawn_blocking` via
`runner::connect_and_run`/`open_spec`):

- Refuse `ssh: Some(_)` first (§1d), before touching the vault-provided
  secret or building anything.
- Build `MssqlConfig::new(cfg.host, cfg.port.unwrap_or(1433), cfg.database,
  cfg.user, secret.unwrap_or_default())`, then apply
  `.encrypt(opts.encrypt)`, `.trust_server_certificate(opts.trust_server_certificate)`,
  `.driver(...)` when overridden (§1c), and
  `.connect_timeout_sec(cfg.timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS)
  .min(u32::MAX as u64) as u32)` — the same 15s fallback bound the pg arm
  uses, rendered as ODBC `Connection Timeout` so an unreachable host fails
  inside the same envelope instead of hanging for the OS TCP timeout.
- **Eager handshake:** `MssqlConnection::new` is lazy (connects per
  operation), but `open_config`'s contract — relied on by `test_connect`
  and by the status bar's connect-vs-query error split — is "bad
  host/credentials fail HERE." New driver method **`MssqlConnection::
  probe(&self) -> Result<(), QueryError>`** (blocking; opens one ODBC
  connection via the existing `connect(&self.conn_str)` and drops it),
  called synchronously by the arm. No `block_on` needed — probe is plain
  blocking code and the arm already runs on a blocking-legal thread.
- **Read-only:** there is NO server-side read-only mode to set (driver
  integration note 5: `ApplicationIntent=ReadOnly` only routes AG
  secondaries; on a standalone instance it accepts writes). The arm sets
  nothing server-side and the doc comment states, verbatim posture:
  client-side `is_read_statement` + the shared runner guard are the ONLY
  enforcement for MSSQL, unlike pg (`default_transaction_read_only=on`)
  and sqlite (`SQLITE_OPEN_READ_ONLY`) — exactly the call-out the driver's
  integration note 5 demands "wherever the sqlite driver's read-only
  connection guarantee is currently assumed to generalize". UI half of
  the same honesty: the
  connection dialog's read-only checkbox gains, for MSSQL only, the hint
  text "u MSSQL vynuceno pouze na straně klienta". REQUIRED test: a
  read-only MSSQL config still refuses a write via the shared guard
  (extends `runner.rs`'s existing read-only test set to the new arm).

### 1b. Connection string / DSN shape — decided: connection string only

No DSN support, permanently for v1: a User/System DSN is on-disk state
(registry / odbc.ini) that invites storing the password outside the vault,
and `MssqlConfig::to_connection_string` already renders everything needed.
The string is built in memory at open time from vault-held parts, held
only in `MssqlConnection.conn_str` for the connection's lifetime, and is
never persisted, never logged, never formatted into any error
(`odbc_err` renders driver diagnostic records only). `escape_odbc_value`'s
brace-wrapping already makes hostile passwords round-trip. REQUIRED test
(mirrors the pg arm's SECURITY note): a failed `probe()` against a wrong
password must produce an error message that does NOT contain the password
text (assert on the docker-gated test, §5).

### 1c. ODBC Driver 18 requirement

The driver links the platform ODBC driver manager; "ODBC Driver 18 for SQL
Server" (msodbcsql18) must be installed machine-side. Decisions:

- Default driver name stays `"ODBC Driver 18 for SQL Server"`; the saved
  config can override it (§1e's `driver` field) to target 17.
- A connect failure whose diagnostics carry SQLSTATE `IM002` ("data source
  name not found and no default driver specified" — the exact failure an
  uninstalled driver produces) is wrapped once, at the `open_config` arm
  (not in the driver crate, which stays app-agnostic), with the actionable
  Czech line: `"ODBC Driver 18 for SQL Server není nainstalován —
  nainstalujte balíček msodbcsql18 (nebo v nastavení připojení zadejte
  název nainstalovaného driveru)"` + the original diagnostic appended.
  Detection is a substring match on `IM002` in the driver-produced message
  — best-effort sugar, never load-bearing (the raw error still shows).
- `TrustServerCertificate` handling: defaults stay secure
  (`encrypt: true`, `trust: false`, Driver 18's own posture, already the
  `MssqlConfig::new` default). The dev-server/self-signed case (which
  includes EVERY docker test instance, §5) requires the user to tick the
  dialog checkbox — we never silently degrade. A cert-validation failure
  (SQLSTATE 08001 with the "certificate chain" text) is left verbatim; no
  auto-retry-with-trust, ever (that would be a silent MITM downgrade).

### 1d. SSH tunnel — decided: refused for MSSQL in v1

`Tunnel` is engine-agnostic TCP forwarding and would mechanically work,
but `Encrypt=yes` + a `127.0.0.1` tunnel endpoint makes the server
certificate's hostname never match, so a tunneled MSSQL connection only
works with `TrustServerCertificate=yes` — a security-relevant interaction
nobody has live-tested. Fail honest rather than ship an untested
encryption downgrade path: the arm errors with `"SSH tunel pro MSSQL zatím
není podporován — použij přímé připojení"` before opening any tunnel.
(Same message pattern as the existing backup-over-tunnel gates in
main.rs.) Follow-up if demanded: tunnel + forced trust + an explicit
warning modal — out of G15.

### 1e. Saved config + dialog (`dbc-state/config.rs`, `connections_ui.rs`)

- `ConnectionConfig` gains `#[serde(default)] pub mssql:
  Option<MssqlOptions>` with `MssqlOptions { #[serde(default =
  "default_true")] encrypt: bool, #[serde(default)]
  trust_server_certificate: bool, #[serde(default)] driver:
  Option<String> }`. `None` ⇒ all defaults (encrypt on, trust off, Driver
  18) — old TOML files load unchanged, non-MSSQL configs never serialize
  the table. No password field anywhere in TOML, unchanged invariant.
- Dialog: two checkbox rows, rendered only while `engine == Mssql`
  (same conditional-row pattern the SQLite path uses to hide host/port):
  `"Šifrovat připojení (Encrypt)"` and `"Důvěřovat certifikátu serveru
  (TrustServerCertificate)"`; one text row `"ODBC driver (volitelné)"` for
  the name override. Czech labels, English keyword in parentheses since
  the keyword is what appears in Microsoft docs/errors.
- `test_connect_spec`'s MSSQL short-circuit is deleted — Test now goes
  through the runner to `open_config` → `probe()` like every engine.
- Password flow recap (unchanged rules, new consumer): dialog password box
  → vault (`Vault::set_secret`, AES-GCM on disk) → `secret:
  Option<String>` through `ConnectSpec::Config` → `MssqlConfig.password`
  → in-memory conn string. The Zeroizing discipline used by G10's
  password modals applies to the dialog's transient copy.

## 2. Dialect story — one enum, one authority

**Decision: `dbc_core::Dialect` (today `{Postgres, Sqlite}`, owned by
`split.rs`) is promoted to THE app-wide dialect authority.** It gains
`Mssql`, moves to its own `dbc-core` module position in the re-export list
(still the same type — no second enum anywhere), and
`main.rs::dialect_for_engine` becomes total: `Postgres → Postgres, Sqlite
→ Sqlite, Mssql → Mssql` (its "refuses mssql" test flips). Every composer
below takes `Dialect`, not `Engine` — `Engine` stays a config/state
concept, `Dialect` a SQL-text concept, and the one mapping function
between them lives in `main.rs` as today.

### 2a. Identifier quoting

- New in `dbc_core::ddl`: `quote_ident_d(dialect, name)` and
  `quote_qualified_d(dialect, schema, name)`. `Mssql` ⇒ brackets with `]`
  doubled (`[we]]ird]`), others ⇒ existing double-quote behavior. The
  existing `quote_ident`/`quote_qualified` remain as thin pg-convention
  wrappers (callers that are pg/sqlite-only by construction keep
  compiling), and `admin_sql::quote_ident_for`/`quote_qualified_for` —
  the only bracket implementation in the repo today — become delegating
  wrappers over the core functions with their tests kept as the contract
  (one implementation, two names during transition; the admin_sql pair is
  marked `#[deprecated]`-in-comment for eventual removal).
- Why brackets and not "double quotes + rely on `QUOTED_IDENTIFIER ON`":
  the ODBC driver does default that session setting on, but the driver
  crate's own integration note 1 names "relying on a driver default rather
  than an explicit dialect" as the gap. Brackets are valid in EVERY T-SQL
  session regardless of settings — the unconditional choice wins for the
  write path.
- Dialectized call sites (each takes `Dialect` threaded from the active
  connection's engine): `sandbox::generate_statements` (via a new
  `TableMeta.dialect` field — Apply is the app's only user-data write
  path, quoting here is CRITICAL per the module doc),
  `csv_import::generate_insert_batches`, `main.rs::preview_sql`,
  `fk_join::build_join_sql` (its inline `t."col"` fragments too),
  `runner::compose_diff_select` (G7 data compare),
  `dbc_core::ddl::synthesize_create_table` (the MSSQL driver reports
  `ddl: None` for tables, so schema-tree DDL and G7 text-diff fall back to
  synthesis — it must bracket-quote for MSSQL).
- REQUIRED tests: every dialectized composer gets an Mssql-variant unit
  test with a `we]ird` identifier, mirroring the existing `we"ird` pg
  tests one-for-one.

### 2b. String literals — `N'...'` for MSSQL

`sandbox::sql_value` grows a dialect-aware sibling `sql_value_d(v,
numeric, dialect)`: for `Mssql`, quoted (non-numeric-passthrough) values
render as `N'...'` (same `''` doubling). Rationale: a bare `'...'` literal
is `varchar` in T-SQL and transcodes through the database collation's code
page — Czech diacritics staged in the grid would corrupt exactly the way
`wide.rs` exists to prevent on the read side. `N''` is harmless for ASCII
and correct for everything else. Existing `sql_value` delegates with pg
behavior; csv_import and sandbox both switch to the `_d` form. Non-finite
floats keep the existing quote-and-let-the-server-decide posture — MSSQL
will reject `N'NaN'` for a float column server-side, error surfaces
verbatim (documented, not special-cased).

### 2c. `split.rs` — `Dialect::Mssql` splits on GO batches, never on `;`

**Decided: support scripts, via GO-batch splitting inside the existing
state machine; `;` is NOT a statement separator for the Mssql dialect.**

- Why not `;`-splitting: T-SQL DDL bodies (`CREATE PROCEDURE ... AS BEGIN
  ... END`) have no pg-style dollar-quoting — their interior `;`s are
  top-level to a lexer, so `;`-splitting would shred any procedure/trigger
  script into garbage. Real MSSQL tools (sqlcmd, SSMS) send batch-by-batch
  on `GO`; the server happily executes a multi-statement batch as one
  unit. Splitting on GO is both the safe and the idiomatic contract.
- Mechanics (inside `StatementSplitter`, reusing the existing
  string/comment state machine): for `Dialect::Mssql`, (i) `;` never
  triggers `emit_statement`; (ii) a new bracket-ident mode is added
  (`[` opens, `]` closes, `]]` is an escaped `]`) mirroring
  `InDoubleIdent` — so a `GO` or quote inside `[a GO b]` can't confuse the
  scan, and `UnterminatedKind` gains `BracketIdent` for the fail-closed
  EOF case; (iii) a finalized bare word `GO` (case-insensitive) that is
  the FIRST non-whitespace content since the last newline, followed on its
  line only by whitespace or a `--` comment, emits the accumulated batch.
  A `GO` anywhere else (mid-line, inside strings/comments/brackets) is
  ordinary text — it reaches the server, which errors verbatim
  ("Incorrect syntax near 'GO'"), the honest outcome for malformed input.
  (iv) `GO <n>` (repeat count) is refused: new
  `SplitError::UnsupportedGoCount`, surfaced in the script UI as
  `"GO s počtem opakování není podporováno"` — same fail-closed posture as
  `UnterminatedAtEof`, and rare enough that emulating the loop isn't worth
  a bespoke execution path. (v) No dollar-quote, no sqlite trigger
  tracking for Mssql (both stay dialect-gated exactly as today).
- Script runner semantics on top: unchanged machinery — "statement" simply
  means "batch" for MSSQL. Per-batch read-only guard via
  `is_read_statement` on the whole batch text (guards.rs already splits on
  interior `;` internally and requires EVERY sub-statement to pass, its
  doc comment even names "MSSQL/odbc-api" batch drivers as the
  future-proofing case — it composes correctly here with no change).
  `TxScope::PerFile`/`WholeRun` wrap batches in the §3 transaction
  helpers; T-SQL transactions legally span batches. Statements that
  refuse to run inside an explicit transaction (`BACKUP`, `ALTER
  DATABASE`, fulltext DDL) error verbatim — documented, not detected.
- Editor multi-statement: `dialect_for_engine` now returns
  `Some(Dialect::Mssql)`, so the editor path splits on GO too. A typical
  `SELECT 1; SELECT 2` is therefore ONE batch; the driver's `query()`
  returns the FIRST result set of a batch and drops the rest — documented
  v1 limitation (surfacing `more_results()` as multiple tabs is a
  follow-up, noted in §8), status text unaffected.
- `count_statements_in_file` and the pre-scan modal work unchanged (they
  are dialect-parameterized already).

### 2d. Auto-LIMIT → `TOP {n}`

`guards::apply_auto_limit` gains a dialect parameter (existing signature
kept as a pg/sqlite delegating wrapper). For `Mssql`: instead of appending
`LIMIT`, insert `TOP {n}` immediately after the leading `SELECT` — after
`DISTINCT`/`ALL` when one of those is the next word (T-SQL grammar order:
`SELECT [ALL|DISTINCT] TOP n ...`). Skip (return unchanged) when the
statement already contains a `TOP`, `OFFSET`, `FETCH`, or `INTO` token —
same flat, depth-unaware scan as today's `LIMIT`/`OFFSET`/`FETCH`/`INTO`
check, preserving the documented "can only under-apply, never over-apply"
posture. Applies per split batch via `auto_limit_each` (a multi-statement
batch's first word is still `SELECT` only when the batch leads with one;
interior SELECTs are untouched — under-application again, accepted).
Status suffix: `" · auto-TOP {n}"` (its own string so the user sees the
actual rewrite vocabulary). BOTH call paths are dialectized — `auto_limit_
each` (split path) AND `run_query_with`'s single-statement fallback (the
path a `dialect_for_engine` `None` would take): an MSSQL connection must
never reach the `LIMIT`-appending form on any branch-intermediate state.
REQUIRED tests: bare SELECT, `SELECT DISTINCT`, already-`TOP`,
`OFFSET ... FETCH`, and the trailing-semicolon form, mirroring the
existing pg auto-limit test rows.

### 2e. EXPLAIN — G13 T7 delivered (Showplan via session preludes)

Grounded in G13 §1b's decisions (estimated = `SET SHOWPLAN_XML`, actual =
`SET STATISTICS XML`; both column-named `"Microsoft SQL Server 2005 XML
Showplan"`) and correcting the delivery mechanism per curation item 1:

- **New driver method** `MssqlConnection::query_with_session(prelude:
  &[String], sql: &str, postlude: &[String], cancel: CancelToken) ->
  Result<QueryStream, QueryError>`: opens ONE fresh connection, executes
  each prelude string as its own batch (satisfying "SET SHOWPLAN_XML must
  be the only statement in a batch"), then executes `sql` on that SAME
  connection and walks its result sets via `Cursor`/`more_results()` (G13
  curation already confirmed odbc-api exposes this), returning the result
  set whose single column is named `Microsoft SQL Server 2005 XML
  Showplan` — falling back to the LAST result set if no column matches
  (fail-open on the name, needs-verification flag carried from G13 §1b
  resolves against live captures). After the result sets are drained (or
  the main batch errored), each `postlude` batch runs best-effort, ALWAYS
  — the `let _ =` discard posture — and the connection then drops, which
  is itself the backstop: an ODBC disconnect rolls back any still-open
  transaction, and the session settings can never leak (the same
  connection-ownership rationale G13 §1b already recorded).
- **Runner method** `run_mssql_plan(spec, sql, analyze: bool)` — the
  runner constructs the concrete `MssqlConnection` for this flow (it
  already has engine-specific methods `run_mssql_backup`/`run_mssql_restore`;
  same precedent, no `Connection`-trait change, no downcasting).
  - Estimated: prelude `["SET SHOWPLAN_XML ON"]`, empty postlude, then
    `{sql}` — the server returns the plan XML INSTEAD of executing;
    inherently safe on any connection including read-only (G13 §5's
    "Explain is always safe" holds).
  - Actual: prelude `["SET STATISTICS XML ON", tx_begin_sql(Mssql)]`
    (`SET STATISTICS XML` has no only-statement restriction — it's a
    run-time setting), postlude `[tx_rollback_sql(Mssql)]` — i.e. the
    plan XML is collected from `{sql}`'s trailing result set with the
    transaction still open, then ROLLBACK runs ALWAYS: the exact
    `drive_analyze_write` discipline expressed through
    `query_with_session`'s prelude/postlude shape. Gating is UNCHANGED:
    `analyze_gate`'s three cases (run / blocked-on-read-only /
    `AnalyzeWriteConfirm` modal) already dispatch by classification, not
    engine.
- `plan.rs` changes: `explain_sql`/`explain_analyze_sql`'s Mssql arms are
  deleted (their SQL text moves into `run_mssql_plan` as the prelude
  constants); `dispatch_plan_query`/`on_confirm_analyze_write` gain the
  `Engine::Mssql` branch routing to `run_mssql_plan`; `parse_mssql_xml`
  and its hand-authored fixtures are corrected against real captures
  (§5), closing every needs-verification flag G13 T3 carried
  (`RunTimeCountersPerThread` aggregation, `<MissingIndexes>` shape,
  result-set column name, `loops: Some(1)` convention).

## 3. Transactional semantics — XACT_ABORT ON, dialectized tx control

### 3a. Transaction-control helpers (fixes G12's bare-`BEGIN` bug)

New in `dbc_core` (beside `guards.rs`): `tx_begin_sql(dialect) -> &'static
str`, `tx_commit_sql(dialect)`, `tx_rollback_sql(dialect)`.

- Postgres/Sqlite: `"BEGIN"` / `"COMMIT"` / `"ROLLBACK"` — byte-identical
  to today's literals, zero behavior change.
- Mssql: `tx_begin_sql` = **`"SET XACT_ABORT ON; BEGIN TRANSACTION"`**
  (one batch — `SET XACT_ABORT` has no only-statement restriction, and
  keeping it fused to BEGIN means no sequence anywhere can open an MSSQL
  transaction without it); `"COMMIT"` / `"ROLLBACK"` are already valid
  T-SQL and stay identical.
- Every sanctioned write sequence switches from the literal `"BEGIN"` to
  the helper, threaded with the connection's dialect:
  `drive_write_sequence` (+ its bounded variant) — sandbox Apply and G10
  admin; the G12 script runner's `TxScope` BEGIN/COMMIT sites;
  `run_csv_import_inner` (whose bare `BEGIN` is invalid T-SQL today —
  the G12-noted bug this section exists to fix); `drive_analyze_write`
  (+ bounded) for §2e's actual-plan path. No new write paths, no new
  guard logic — the §3-novela's sanctioned-caller list is unchanged in
  membership, each member just becomes dialect-correct.

### 3b. Why `XACT_ABORT ON` (the semantics decision)

The driver's integration note 3 documents the three-way engine divergence:
sqlite leaves a failed transaction open and usable; pg dooms it until
ROLLBACK; T-SQL's default (`XACT_ABORT OFF`) is a mess — most runtime
errors roll back only the failed statement and leave the transaction OPEN
with earlier statements' effects intact, while compile/severity errors
abort variously. The app's write discipline everywhere is "stop at first
error, roll back everything" — under `XACT_ABORT OFF` a mid-sequence
error followed by our best-effort `ROLLBACK` would *usually* still be
correct (ROLLBACK undoes the open transaction), but the failure classes
that kill the batch-vs-statement differently make "usually" untestable.
`SET XACT_ABORT ON` collapses T-SQL to the pg-like contract: ANY runtime
error dooms and rolls back the whole transaction. Our sequences already
tolerate the consequence — the subsequent explicit `ROLLBACK` failing with
"no corresponding BEGIN TRANSACTION" is swallowed by the existing `let _
=` discard posture that every sequence documents.

### 3c. Empirical verification matrix — REQUIRED, docker-gated (§5), must
pass before any feature-ON flip merges

Over one `MssqlConnection` per case, driving `execute()` exactly as the
app does (persistent `exec_conn`, statements as separate calls):

Session-state assertions use an execute-compatible probe (no result set,
so it's legal on the persistent `exec_conn` the app actually uses):
`IF @@TRANCOUNT <> {n} THROW 50000, 'trancount mismatch', 1` — errors
exactly when the assertion fails. Data-visibility assertions use a SECOND
connection's `query()`.

1. `tx_begin` → INSERT ok → INSERT violating a PK constraint → assert the
   error, then probe `@@TRANCOUNT = 0` on the same session and assert the
   first INSERT's row is GONE via the second connection (XACT_ABORT
   aborted + rolled back the whole transaction).
2. Same shape with a conversion error (`CAST('x' AS int)`) and an
   arithmetic error — the classes that diverge under `XACT_ABORT OFF`
   must all behave identically under ON.
3. After case 1's abort, issue the app's best-effort `ROLLBACK` → assert
   it errors AND the connection remains usable for a next statement
   (the `let _ =` discard is safe, not masking a poisoned session).
4. Autocommit interference (driver note 3's second open question):
   `tx_begin` → probe `@@TRANCOUNT = 1` → INSERT → from the second
   connection assert the row is NOT visible → `COMMIT` → now visible.
   Proves ODBC's `SQL_ATTR_AUTOCOMMIT = ON` does not commit between
   `execute()` calls once a literal `BEGIN TRANSACTION` is open.
5. Session persistence: `SET XACT_ABORT ON` issued in the `tx_begin`
   batch still governs a LATER `tx_begin` on the same `exec_conn`
   (harmless redundancy either way, but characterizes the session).
6. CSV-import end-to-end: a CSV whose last batch violates a constraint
   imports ZERO rows (the all-or-nothing contract holds on MSSQL).

Also verified here, non-transactional: `CSV_IMPORT_BATCH_SIZE (500)` is
under T-SQL's 1000-row `VALUES` clause cap — add a unit-level
`const_assert`-style test (`assert!(CSV_IMPORT_BATCH_SIZE <= 1000)` with a
comment naming the T-SQL limit) so a future bump can't silently break
MSSQL imports.

## 4. Feature matrix — what turns ON, what stays gated

Per feature, decided from what SQL it emits and what §1–§3 fix. "ON" means
the existing engine gate is removed in the same task that makes its SQL
dialect-correct — never before.

| Feature | Decision | Grounds |
|---|---|---|
| Editor query run (single batch) | **ON** | driver `query()` complete (UTF-16 binding, streaming, cancellation-at-batch-granularity documented) |
| Auto-limit | **ON** (`TOP`, §2d) | guards change only |
| Multi-statement editor + script runner (G12) | **ON** (GO batches, §2c) | splitter + tx helpers |
| Grid preview + FK join | **ON** | `preview_sql`/`fk_join` dialectized (§2a) |
| Sandbox editing / Apply (G5) | **ON** — `detect_editable_pk` drops its `engine == Mssql` exclusion | brackets (§2a) + `N''` (§2b) + tx helpers/XACT_ABORT (§3); the driver doc's "MUST NOT be wired into Apply until ddl.rs is dialectized" precondition is exactly what §2a discharges |
| CSV import | **ON** | §2a/§2b/§3a + batch-size assert |
| Admin panel (G10) | **ON** — automatic: `admin_entry_state` already returns `Enabled` for writable MSSQL; the gate that held it back was `open_config` itself | admin_sql's MSSQL catalogs/builders exist but are never-live — §5 makes their validation REQUIRED, incl. the honestly-flagged never-verified `schema_sizes` LEFT JOIN empty-schema shape |
| Backup & restore (G11) | **ON** — automatic: `run_mssql_backup`/`run_mssql_restore` exist and fail today only at `open_spec` | §5 validates `BACKUP DATABASE`/`SINGLE_USER→RESTORE→MULTI_USER` T-SQL live; note the path in the dialog is a SERVER-side path (docker: inside the container) — the G11 UI already presents it as the T-SQL it will run |
| Server monitor (G9) | **ON, with real wiring work (T-MON)** | `monitor_sql::mssql`'s 11 statements exist (CI-smoke-tested) but `monitor.rs`'s refresh/parse is pg-shaped (curation item 2); T-MON adds the per-engine statement set + tile mapping: CONNECTIONS+CONNECTIONS_MAX → connections tile (`value_in_use = 0` ⇒ max `None`), LOCKS_WAITING+DEADLOCKS → locks, SIZE (one row, two cols) → data/log, CACHE_HIT+UPTIME+XACT_TOTAL → perf (cumulative-delta logic reused), RUNNING/BLOCKING/TABLES → existing parse contracts (column order already matches by construction — REQUIRED live test). Per-statement failure degrades its tile to "n/a", same posture. `monitor_available` flips to include Mssql. `kill_sql`'s `KILL {pid}` already routes through `execute` (sanctioned, G9 §0 rationale). |
| Execution plans (G13) | **ON** via §2e | T7 delivered; fixtures corrected live |
| ER diagram (G8) | **ON** — automatic | pure `SchemaSnapshot` consumer; the MSSQL driver's `schema.rs` fills columns/PKs/FKs/constraints/indexes |
| Schema/data compare (G7) | **ON** | `compose_diff_select` dialectized (§2a); schema half is snapshot-pure, cross-engine diff already suppresses type-text noise; synthesized-DDL text diff uses dialectized `synthesize_create_table` |
| History, saved params, favourites, export | **ON** — automatic | engine-agnostic (no SQL composed) |
| SSH tunnel | **gated** — `"SSH tunel pro MSSQL zatím není podporován — použij přímé připojení"` | §1d |
| Windows integrated auth | **gated** — `"MSSQL: zadejte uživatele — ověření přes Windows účet zatím není podporováno"` | §0 non-goals |
| dbc-mcp | **gated** — keeps its own `"MSSQL driver zatím není k dispozici"` | curation item 5 |

The stale "MSSQL driver zatím není k dispozici" copies outside the two
surviving gates (`connections_ui::test_connect_spec`, `connect.rs`,
runner tests asserting on it) are deleted/retargeted in the wiring task —
a repo-wide grep for the string is part of that task's checklist so no
dead Czech gate message survives.

## 5. Live validation — docker MSSQL, honestly tiered

- **Container:** `testcontainers-modules` with the `mssql_server` feature
  (image `mcr.microsoft.com/mssql/server`, Linux-only — viable on this
  Windows host because Docker Desktop runs Linux containers under WSL2,
  same daemon the existing pg-gated tests already require). The module
  handles `ACCEPT_EULA` + SA password. Startup is slow (30–60 s) and the
  image is large (~1.5 GB) — same `#[ignore]` + explicit-invocation tier
  as `monitor_pg_tests`/`backup_docker_tests`, run with
  `cargo test -p dbc-ui -- --ignored mssql_docker_tests::` (and
  `-p dbc-driver-mssql -- --ignored` for the driver crate's own suite).
- **Host prerequisite that docker cannot provide:** ODBC Driver 18 must be
  installed on the HOST (the tests connect from the host through the
  mapped port). Tests probe `odbc_api::Environment::drivers()` first and
  SKIP with an explicit message when no `ODBC Driver 1[78] for SQL Server`
  is present — the exact `"SKIP …: install …"` posture
  `backup_docker_tests` already uses for a missing `pg_dump`. Never a
  silent green.
- **Connection posture in tests:** `trust_server_certificate(true)`
  (self-signed dev cert — the documented dialog path, §1c), plus the
  driver crate keeps honoring `DBC_MSSQL_TEST_CONN` as an escape hatch
  (existing convention) with testcontainers as the default when unset.
- **What MUST run live (the never-live backlog, by owner):**
  1. `dbc-driver-mssql/tests/mssql_integration.rs` — all of it (the file's
     own doc comment: "the first thing to run against a live server").
  2. §3c's XACT_ABORT/autocommit matrix (new, driver crate).
  3. G10 admin: `roles_catalog`, `privileges_catalog`, `sizes_catalog`
     (incl. the flagged `schema_sizes` LEFT JOIN against a freshly created
     EMPTY schema — the exact case its comment admits was never verified),
     and one mutation round-trip per builder family (CREATE LOGIN/USER →
     GRANT/DENY/REVOKE → sp_addrolemember path → DROP) against parse
     functions `parse_db_sizes`/`parse_schema_sizes`/`RoleRow`.
  4. G9 monitor: all 11 `monitor_sql::mssql` constants through T-MON's
     real refresh, plus a genuine blocking chain and a `KILL` round-trip
     (mirroring `monitor_pg_tests`' shape).
  5. G11: `BACKUP DATABASE` to a container path → `RESTORE` round-trip
     through `run_mssql_backup`/`run_mssql_restore` (verifying the
     SINGLE_USER/MULTI_USER bracketing and that data survives).
  6. Sandbox Apply end-to-end: bracket-quoted UPDATE/INSERT/DELETE with a
     `we]ird` column name and a Czech-diacritics `N''` value, staged →
     applied → re-read.
  7. §2c/§2d: a GO-batched script (incl. a `CREATE PROCEDURE` body with
     interior semicolons) through the script runner with `PerFile` tx
     scope; TOP auto-limit visible in a real result.
  8. §2e: capture real `SHOWPLAN_XML` and `STATISTICS XML` output for the
     G13 fixture set (seq/index scan, join, missing-index case) and
     correct `parse_mssql_xml` + fixtures against them.
- **Test placement:** driver-level cases in `dbc-driver-mssql/tests/`;
  app-level cases in-crate under `#[cfg(test)] mod mssql_docker_tests` in
  `runner.rs`/`plan.rs` (dbc-ui is a binary crate — same reasoning as the
  G9/G13 precedent), plain `#[test]` + `runner.handle().block_on` (NOT
  `#[tokio::test]` — the nested-runtime panic documented at
  `monitor_pg_tests`' module doc applies identically).

## 6. Security invariants (restated for the new code paths)

- **§3-novela (binding, unchanged):** every write reaches
  `Connection::execute` only through (a) a confirm modal showing the
  exact SQL, or (b) a sanctioned runner-owned method with explicit
  transaction discipline, and (c) the SHARED read-only guard at the
  runner choke point. G15 adds NO new sanctioned member — it makes
  existing members dialect-correct (§3a) and adds one engine branch to an
  existing member (`run_mssql_plan` is the MSSQL face of the
  already-sanctioned analyze-write sequence; `execute()`'s
  sanctioned-caller doc list gains that one line).
- **Read-only guard:** for MSSQL, client-side enforcement is the ONLY
  enforcement (no server-side mode exists — §1a). Consequence, stated
  where the driver's integration note 5 demands it: every place that
  documents
  "server-side backstop" for pg/sqlite (e.g. `run_query_with` Guard 1's
  comment, G13 §5's defense-in-depth paragraph) gets an MSSQL exception
  note in the wiring task. The guard itself, `is_read_statement`, already
  fails closed on T-SQL's sharp edges (`EXEC`/`EXECUTE`/`MERGE`/`INTO` in
  `WRITE_KEYWORDS`; multi-statement batches require every sub-statement
  to pass). Backup-on-read-only stays exempt via the ONE documented G11
  predicate; restore stays hard-blocked.
- **Password rules:** vault-only at rest; in memory it lives in the
  ODBC connection string (`MssqlConnection.conn_str`) for the
  connection's lifetime — never persisted, never logged, never formatted
  into errors (REQUIRED negative test, §1b); no DSN so it can never leak
  into registry/odbc.ini; `TrustServerCertificate` is never auto-enabled
  (§1c). TOML gains only non-secret fields (§1e).
- **No silent encryption downgrade:** `encrypt` defaults on; disabling it
  or trusting a certificate is always an explicit, persisted user choice
  visible in the dialog.

## 7. Task decomposition hint (for the plan author)

Pure-first, serialized-by-file after — the G13 batching shape:

- **T-CORE** (`dbc-core`: `Dialect::Mssql` + bracket lexing in `split.rs`
  incl. GO rules, `quote_ident_d`/`quote_qualified_d`,
  `synthesize_create_table` dialect param, `apply_auto_limit` dialect,
  `tx_*_sql` helpers; unit tests only). Foundation; solo first.
- **T-DRV** (`dbc-driver-mssql`: `probe()`, `query_with_session()`, §3c
  test matrix authored — runnable once §5 infra lands). Parallel with
  T-CORE (no file overlap).
- Then in parallel, each depending only on T-CORE (+T-DRV where noted):
  **T-CONN** (`connect.rs` arm, `dbc-state` `MssqlOptions`,
  `connections_ui` dialog rows + short-circuit removal, IM002/ssh/auth
  messages; needs T-DRV's `probe`); **T-SQLGEN** (`sandbox.rs`
  `TableMeta.dialect` + `sql_value_d`, `csv_import`, `preview_sql`,
  `fk_join`, `compose_diff_select`, `admin_sql` delegation to core);
  **T-TX** (`runner.rs` tx-helper adoption across all sequences + csv
  BEGIN fix); **T-MON** (`monitor.rs` per-engine refresh/mapping).
- **T-PLAN** (`plan.rs` + `runner.rs::run_mssql_plan`; needs T-DRV +
  T-TX) — serialized after T-TX (shares `runner.rs`).
- **T-LIVE** (§5: testcontainers infra, driver-probe SKIP helper, the
  full live backlog 1–8, fixture re-capture) — last, integrates
  everything; the feature-ON flips (`detect_editable_pk`'s Mssql
  exclusion, `dialect_for_engine`'s `Mssql → Some(Dialect::Mssql)` in
  `main.rs`, `monitor_available`, gate-message grep sweep) land HERE,
  gated on the §3c matrix passing, in the same commit tier as the
  integration sweep + version bump to v0.15.0. (T-CORE adds the enum
  variant; the `main.rs` mapping that makes it reachable is deliberately
  a T-LIVE flip so nothing turns on before the live tier exists.)

`runner.rs` is the contention file (T-TX, T-PLAN, T-LIVE touch it) —
serialize those three; everything else is disjoint.

## 8. Risks / needs-verification (consolidated)

- **XACT_ABORT matrix is the phase's keystone** — if case 4 (autocommit
  interference) fails, the driver needs `set_autocommit(false)`-based
  transaction control instead of literal `BEGIN TRANSACTION`, which is a
  driver-contract change (its lib.rs note 3 pre-documents this exact
  fork). Everything in §3/§4's write column is gated on the matrix.
- **`query_with_session` result-set selection** — the plan-XML column
  name (`Microsoft SQL Server 2005 XML Showplan`) is documented-historic
  but unverified here; the last-result-set fallback bounds the damage to
  "wrong text handed to a parser that fails closed".
- **Monitor DMV permissions** — `sys.dm_exec_*`/`dm_os_*` need
  `VIEW SERVER STATE`; a plain login degrades tiles to "n/a" (posture
  exists), but the live tests run as `sa` and won't catch a bad
  degradation path — add one deliberate low-privilege-login case to
  backlog item 4.
- **GO-only splitting changes editor semantics for MSSQL** (`;`-separated
  text is one batch, first result set only) — correct but potentially
  surprising; if user feedback demands it, multi-result-set tabs via
  `more_results()` are the follow-up, not `;`-splitting.
- **Collation/codepage edge in `N''`** — §2b fixes literals, but bare
  numeric-passthrough and server-side conversions still follow database
  collation; live test 6's diacritics round-trip is the canary.
- **testcontainers `mssql_server` module version pin** — confirm the
  module exists in the workspace's `testcontainers-modules` 0.13 at
  implementation time; if not, bump the crate (it unions with the pg
  feature) rather than hand-rolling a GenericImage.
- **Driver-18-absent developer machines** — every gated test SKIPs loudly
  (§5); CI without the ODBC driver stays green-with-skips, which is the
  honest tier, not proof. The matrix must run on at least one real
  machine before the flips merge (same standing rule as G10's live-pg
  sweep).
