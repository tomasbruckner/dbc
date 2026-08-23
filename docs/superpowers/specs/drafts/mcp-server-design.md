# dbc-mcp — MCP Server Design

Date: 2026-08-23
Status: draft, designed autonomously per the standing mandate — awaiting user
review
Scope: original roadmap phase 6 (orthogonal to the GUI G-phases). An MCP
server exposing the client's saved connections to LLM tools, READ-ONLY per
the binding architecture constraint ("MCP remains read-only",
`2026-08-22-gui-target-design.md` §3). Format follows the G5 design-pass
block: terse, one decision per bullet, no TBDs.

## 0. Research snapshot (Context7 + crates.io, 2026-08-23)

- SDK: `rmcp`, the official Rust MCP SDK. Current version **3.1.4**.
  Default features: `base64`, `macros`, `server` — already what a server
  needs for the `#[tool_router]`/`#[tool]` macros. Add `transport-io` for
  stdio.
- Server pattern (confirmed via docs.rs snippets): `#[tool_router]` on an
  `impl Block` generates a `tool_router()` fn from methods marked `#[tool]`;
  `#[tool_router(server_handler)]` auto-emits `#[tool_handler] impl
  ServerHandler for Self`. A tool method takes `Parameters<T>` where `T:
  serde::Deserialize + schemars::JsonSchema`, returns `Json<U>` (or a
  `Result` for fallible tools). `rmcp::transport::io::stdio()` returns
  `(Stdin, Stdout)`; a server is started with
  `service.serve(stdio()).await?.waiting().await?` (`ServiceExt::serve`,
  `server` feature).
- New deps beyond the current workspace: `rmcp`, `schemars` (required by the
  `#[tool]` macro for JSON-schema generation), `tracing`/
  `tracing-subscriber` (stderr logging — see §4). `tokio` needs the
  `io-std` feature added (workspace `tokio` currently has
  `rt-multi-thread, sync, time, macros`; `dbc-mcp`'s own `Cargo.toml` adds
  `io-std` on top, which Cargo permits per-crate).

## 1. Crate layout

- New binary crate `crates/dbc-mcp`, added to the workspace `members` list.
  Binary and crate name: `dbc-mcp`.
- Deps: `dbc-core` (path), `dbc-buffer` (path — for `ResultBuffer`, see §5),
  `dbc-state` (path — `AppConfig`, `Vault`, `ConnectionConfig`),
  `dbc-driver-sqlite` (path), `dbc-driver-postgres` (path), `rmcp = "3.1"`
  with `features = ["transport-io"]`, `tokio` (workspace + `io-std`),
  `serde_json`, `schemars`, `tracing` + `tracing-subscriber`. **No GPUI** —
  `dbc-mcp` is not `dbc-ui`, so the "no concrete drivers outside
  `connect.rs`" rule doesn't apply; it links both driver crates directly,
  same as `dbc-ui` does today.
- Modules: `main.rs` (arg parsing, vault unlock, server bootstrap, stdio
  serve loop), `connect.rs` (own minimal `open_for_mcp`, see §1a),
  `tools.rs` (the three `#[tool]` methods + their `Parameters` structs),
  `serialize.rs` (schema/row JSON shaping + size caps).
- MSSQL/DuckDB: neither driver is merged on `main` yet (in-progress on
  other branches). `dbc-mcp` ships with no dependency on either; a
  connection with `engine = Mssql` returns the same "driver not available"
  tool error `dbc-ui`'s `connect::open_config` already returns for the same
  case — no new stub needed, same message reused.
- **Non-goal (v1): SSH-tunneled connections.** A `ConnectionConfig` with
  `ssh: Some(_)` is excluded from `list_connections` and rejected by
  `get_schema`/`run_query` with a clear error. Rationale: `dbc-ui`'s tunnel
  lifecycle (`Tunnel::open`, child-process spawn, teardown on drop) is
  `dbc-ui`-only code tied to its own process lifetime; pulling it into a
  second binary crate is real work disproportionate to v1. Revisit if a
  user actually needs it.
- **Deliberate duplication (accepted):** `connect::open_for_mcp` duplicates
  the ~40-line shape of `dbc-ui/src/connect.rs::open_config` (Postgres
  `tokio_postgres::Config` builder, SQLite `new_with_options`) rather than
  extracting a shared crate. Rationale: `dbc-ui`'s version is entangled
  with `Tunnel`/GPUI-adjacent error formatting; a shared abstraction for
  ~40 lines used by exactly two callers isn't worth the indirection. The
  duplication is flagged with a doc comment in each file cross-referencing
  the other, so a fix to one path prompts checking the twin.

## 2. Transport

- **v1: stdio only.** `rmcp::transport::io::stdio()` + `ServiceExt::serve`.
  Matches Claude Desktop's and Claude Code's default MCP launch model
  (client spawns the server process, owns its stdin/stdout as the
  JSON-RPC channel).
- **stdout is sacred:** nothing but the MCP JSON-RPC stream may ever write
  to stdout. All logging goes to stderr (`tracing_subscriber::fmt().
  with_writer(std::io::stderr)`) — called out explicitly because it's a
  real footgun (a stray `println!` anywhere in `dbc-mcp` or a transitive
  dep corrupts the protocol stream).
- **User-facing setup (Claude Code), per the §3 curation override:** first
  run `dbc-mcp setup` once in a terminal (stores the derived key in the OS
  credential store), then register with NO secrets in the config:
  `claude mcp add dbc -- dbc-mcp`
  (optionally `--config <path>` / `--vault <path>` flags if the user keeps
  non-default file locations; defaults match `dbc-ui`'s own
  `dbc_state::config::default_config_path()` /
  `vault::default_vault_path()`, i.e. `%APPDATA%/dbc/{config.toml,
  vault.bin}` on Windows / `~/.config/dbc/...` elsewhere via the `dirs`
  crate).
- **Claude Desktop:** equivalent `claude_desktop_config.json` entry (no
  `env` block — nothing secret belongs in this file):
  ```json
  "dbc": { "command": "dbc-mcp" }
  ```
- No HTTP/SSE transport in v1 (non-goal — see §3 unlock-model discussion
  for why an "always-unlocked GUI-hosted server" would need a non-stdio
  transport and is deferred).

## 3. Vault unlock model (the critical decision)

Two options considered, per the brief:

- **Option A — server started from the GUI app, vault already unlocked
  in-process.** Rejected for v1. Stdio transport requires the *MCP
  client* (Claude Desktop/Code) to spawn the *server process* and own its
  stdin/stdout from the start; `dbc-ui` is not spawned by Claude, so this
  doesn't fit the stdio launch model at all. It would require `dbc-ui` to
  additionally host an HTTP/socket MCP transport as a background service
  while the GUI event loop runs, and Claude to connect to that instead of
  spawning a subprocess — a materially different (and larger) piece of
  work than a v1 stdio server. Flagged as a natural **v2** pairing (once a
  non-stdio transport exists), not designed further here.
- **Option B — standalone binary prompting once at startup.** Also
  rejected, for a sharper reason than "less convenient": it's not just
  awkward, it's **structurally broken** for the stdio case. Once Claude
  Desktop/Code spawns `dbc-mcp`, its stdin *is* the JSON-RPC pipe from
  process start — there is no TTY to prompt against, and reading stdin
  for a password would race/interleave with (or simply consume bytes
  belonging to) the MCP handshake itself. Opening `/dev/tty`/`CONIN$`
  directly would work only when launched from an interactive terminal,
  which is not Claude Desktop's launch model (no attached console) — a
  prompt there just hangs forever with nothing visible.
- **Decision (v1, CURATION OVERRIDE): derived-key in the OS credential
  store — NOT a password env var.** The originally-drafted
  `DBC_MCP_MASTER_PASSWORD` env var was rejected in curation: MCP client
  configs (`claude_desktop_config.json`, Claude Code's registry) persist
  env values in plaintext on disk, which violates the binding constraint
  "the master password is never stored". v1 model instead:
  - `dbc-mcp setup` (explicit subcommand, run MANUALLY from a terminal —
    a TTY exists there, unlike the MCP launch path): prompts for the
    master password (no echo), unlocks the vault to VERIFY it, then
    stores the Argon2id-DERIVED 32-byte vault key (never the password)
    in the Windows Credential Manager via the `keyring` crate (DPAPI-
    backed, per-user). `dbc-mcp setup --remove` deletes it (revocation).
  - MCP mode: reads the derived key from Credential Manager and opens the
    vault with it directly — new small additive `dbc-state::vault` API:
    `unlock_with_key(key)` alongside the existing password path, plus a
    key accessor on successful unlock for `setup` to store (zeroized on
    drop where practical). No key, wrong key, corrupt vault → fail
    closed (below).
  - Properties: the master password itself is never persisted anywhere
    (constraint holds); the derived key at rest is OS-encrypted and
    user-bound (DPAPI), revocable via `setup --remove` or by re-keying
    the vault; nothing secret ever appears in the MCP client's config —
    the registration is just `claude mcp add dbc -- dbc-mcp` with no env.
- **Fail closed:** missing/undecryptable Credential Manager entry, wrong
  key, or a corrupt vault file all produce a one-line stderr message and a
  non-zero exit *before* the stdio server loop starts — never a silent "no
  vault" mode with every connection unusable but the process pretending to
  be healthy. (Updated for the curation override — originally worded for
  the rejected env-var model.)
- **Explicit tradeoff vs. the GUI:** the GUI prompts interactively once
  per app start and never persists anything. The MCP path persists the
  DERIVED KEY (never the password) in the OS credential store,
  DPAPI-encrypted and user-bound, revocable via `dbc-mcp setup --remove`
  or by re-keying the vault. Nothing secret appears in the MCP client's
  config file. (This section originally described the rejected env-var
  model; superseded by the §3 curation override.)
- Per-connection secrets: once the vault is unlocked, `dbc-mcp` resolves a
  connection's password via `vault.get_secret(cfg.id)`, exactly like
  `dbc-ui`'s `main.rs` (`self.vault.as_ref().and_then(|v|
  v.get_secret(&cfg.id))`). Never included in any tool response, never
  logged (see §4).

## 4. Security model

Read-only is enforced by **three independent layers**, matching the
brief's "defense in depth" framing — any one failing still leaves the
other two:

1. **Client-side lexical gate.** Every `run_query` call runs
   `dbc_core::is_read_statement(sql)` before opening any connection —
   identical function `dbc-ui` already uses, already fail-closed and
   already tested (guards.rs's existing suite). Rejects without a
   connection ever being attempted.
2. **Driver-level read-only open flag, unconditional.** `dbc-mcp`'s
   `open_for_mcp` always opens SQLite with `SQLITE_OPEN_READ_ONLY`
   (`SqliteConnection::new_with_options(path, true)`) and Postgres with
   `options("-c default_transaction_read_only=on")` — **regardless of the
   saved `ConnectionConfig::read_only` value.** This is stricter than the
   GUI: the GUI honors `read_only = false` to allow the sandbox Apply
   write path; MCP has no write path at all, so it forces read-only at
   the driver layer unconditionally, on top of whatever the config says.
   This is also how the brief's "the `read_only` config flag on a
   connection is RESPECTED even for MCP" requirement is satisfied — a
   fortiori, since MCP is strictly more restrictive than that flag would
   ever require. An explicit `read_only = true` connection behaves
   identically through MCP to any other connection: both fully read-only.
3. **No write path wired at all.** `Connection::execute` is never called
   anywhere in `dbc-mcp`'s source — no tool handler has a reference to
   anything that could call it. Enforced by a regression test, not just
   code review discipline (T7, §7).
- **Logs — SQL text yes, results/secrets never.** `dbc-mcp` does **not**
  write to the shared `dbc-state` `HistoryDb` (that database is the GUI's
  interactive, human-facing history; mixing an AI agent's traffic into it
  with no "source" field to distinguish the two is a UX problem for the
  GUI, not a security one — the two logs are kept separate by design).
  Instead, every tool call emits one `tracing` INFO line to stderr:
  `tool=<name> connection=<name> rows=<n> duration_ms=<n> [error=<msg>]`
  — SQL text and the connection *name* are logged (matching the GUI's own
  `HistoryEntry` policy: SQL yes, connection name yes, never row data,
  never a password). Claude Desktop/Code capture server stderr to their
  own log files, giving an audit trail without touching the GUI's
  history DB.
- **Size/rate limits:** row cap and response-byte cap are covered in §3
  (tools) and §5 (serialization); no per-second rate limiter in v1
  (non-goal — a single LLM conversation against a local desktop tool
  isn't the multi-tenant scenario rate limiting defends against; §6's
  per-request connection model is the relevant bound instead).

## 5. Tools exposed

### `list_connections()`
- No arguments. Returns every non-SSH `ConnectionConfig` as
  `{id, name, engine, folder, read_only, favourite}`.
- **"Names + engine only, no secrets"** is the letter of the brief; `host`
  /`port`/`user`/`database`/`password` are never included. `read_only` and
  `folder` are added on top — neither is a secret, and `read_only`
  directly tells the LLM whether write attempts against that connection
  will be rejected (useful, not risky, to expose).
- Test asserts (string-search, mirroring `dbc-state`'s own
  `no_password_field_serialized` test) that the serialized JSON never
  contains `"host"`, `"user"`, `"database"`, or `"password"`.

### `get_schema(connection_id, schema: Option<String>, include_ddl: Option<bool>)`
- Opens via `open_for_mcp`, calls `Connection::schema()`, serializes
  `SchemaSnapshot` to JSON via new `#[derive(Serialize)]` impls added to
  every type in `dbc-core::schema` (T2, §7) — field-for-field, no
  reshaping: `{tables: [...], routines: [...], triggers: [...],
  sequences: [...]}`, each `TableInfo` carrying its `columns`/`indexes`/
  `constraints`/`ddl` as-is.
- `schema: Option<String>` filters `TableInfo::schema`/routine/trigger/
  sequence schema to one name (no-op for SQLite, which has none).
- `include_ddl` **defaults to `false`** — every `ddl: Option<String>`
  field (table/view/routine/trigger source text) is dropped from the
  response unless explicitly requested. Rationale: DDL bodies are the
  single largest contributor to payload size and are rarely needed for
  schema *exploration* (column names/types/PKs/FKs answer "what tables
  exist and how do they relate"); an LLM that specifically wants a
  table's DDL asks again with `include_ddl: true`.
- **Size guardrail:** serialize, and if the result exceeds **512 KB**,
  truncate the `tables` array (keep the first N by `(schema, name)` sort
  order) and add `{"truncated": true, "tables_returned": N,
  "tables_total": M}` at the top level rather than emitting a
  size-unbounded or broken-off blob. `routines`/`triggers`/`sequences`
  are dropped entirely (not truncated piecemeal) once the cap is hit, to
  keep the truncation logic single-pass and easy to reason about — a
  schema large enough to need truncation is large enough that the LLM
  should re-query with a `schema` filter anyway.

### `run_query(connection_id, sql, row_limit: Option<u32>, timeout_secs: Option<u32>)`
- Gate 1: `is_read_statement(sql)` — reject before connecting (§4 layer 1).
- Row limit: hard default **200**, caller-suppliable, hard ceiling
  **1000** (values above 1000 are clamped, not rejected — clamping is
  reported in the response, not silently applied).
- `apply_auto_limit(sql, effective_limit)` — same heuristic rewrite the
  GUI's Guard 2 uses (appends `LIMIT` to a bare `SELECT` with none). This
  is best-effort/heuristic exactly as `guards.rs` documents (a query that
  already carries its own `LIMIT 50000` is left alone) — so it is *not*
  the real hard cap.
- **The real hard cap is at result consumption:** rows are drained into a
  `dbc_buffer::ResultBuffer` (§6) up to `row_limit`; once hit, the stream
  is stopped/cancelled and `truncated: true` is set regardless of what
  the SQL itself requested or whether `apply_auto_limit` fired.
- Timeout: `timeout_secs` optional, default **30s**, ceiling **120s**,
  raced via `tokio::time::timeout` against the stream drain; on expiry
  the query's `CancelToken` fires (protocol-level cancel — pg
  `CancelRequest` / sqlite interrupt, the same mechanism the GUI uses) and
  the tool call returns a timeout error. v1 is all-or-nothing on timeout
  (no partial rows returned) — simplest, avoids ambiguous "timed out vs.
  truncated" semantics in the response shape.
- Response byte cap: **2 MB** serialized, independent of the row cap
  (very wide rows / large text cells can blow past it before `row_limit`
  rows are reached) — same `truncated: true` marker, one consistent field
  regardless of which cap tripped.
- Errors: `QueryError {code, message, position}` mapped into the tool's
  error result verbatim — SQL error text is not secret (the GUI already
  shows it directly), so no scrubbing.
- **Non-goals (v1): `table_sample`, `explain`.** `table_sample` needs no
  dedicated tool — it's `run_query` with `SELECT * FROM t LIMIT n`, and
  the LLM already has table names from `get_schema`. `explain` is
  deferred: wiring `EXPLAIN`/`EXPLAIN ANALYZE` through MCP ahead of any
  plan-rendering work (G13 in the GUI roadmap) is premature, and
  `EXPLAIN ANALYZE` on Postgres actually *executes* the statement —
  already flagged in `guards.rs` as a write-bypass vector closed by the
  blanket write-keyword scan — so it deliberately stays out of
  `run_query`'s allowlist rather than getting a bespoke, riskier tool.
- **No `execute`/write tool. Ever.** Per the binding constraint; see §4
  layer 3.

## 6. Result serialization

- Rows as **array-of-arrays**, not array-of-objects:
  `{"columns": [{"name": "id", "type": "Int64"}, ...], "rows": [["1",
  "Alice"], ["2", null]], "row_count": 2, "truncated": false,
  "duration_ms": 4}`. Positional rows avoid repeating column names per
  row (materially smaller for wide results); `columns` already carries
  the name↔position mapping, plus the underlying Arrow type name for
  anyone who wants it.
- Every cell is a JSON **string or `null`**, never a JSON number/bool —
  computed via `dbc_buffer::ResultBuffer::cell_text`/`cell_is_null`, the
  **existing, already-tested text-cell pipeline** `dbc-ui`'s grid uses
  (`crates/dbc-buffer/src/lib.rs`). `dbc-mcp` writes **zero** new
  Arrow-to-text conversion code — it pushes each `RecordBatch` from
  `QueryStream::batches` into a `ResultBuffer::with_cap(schema,
  row_limit)` and reads cells back out. `cell_is_null` distinguishes a
  real SQL `NULL` (→ JSON `null`) from an empty string (→ JSON `""`),
  mirroring G5's own NULL-vs-empty-string decision.
- `dbc-buffer` has no GPUI/UI dependency (`Cargo.toml`: `dbc-core` +
  `tempfile` only), so it's a legitimate `dbc-mcp` dependency — confirmed
  by reading `crates/dbc-buffer/Cargo.toml` before choosing this path.
- Size cap: 2 MB per response (§5); `get_schema`'s 512 KB cap is separate
  and smaller, reflecting that schema payloads are all-at-once metadata
  rather than a capped row stream.

## 7. Concurrency

- **One connection per tool call, opened fresh, closed at the end of that
  call. No pooling, no connection cache keyed by `connection_id`.**
- Rationale:
  - `Connection` implementations are per-connection objects with no
    internal pooling; building a pool (idle eviction, health checks,
    reacting to a saved config changing mid-session) is real work v1's
    traffic pattern doesn't justify — an LLM issuing occasional
    exploratory queries, not a hot loop.
  - Matches the SQLite driver's own per-call-open behavior for
    `query()`/`schema()` already; Postgres connects are already bounded
    by `connect_timeout`.
  - **Statelessness is the payoff:** since no connection outlives a
    single tool call, there is no shared mutable connection state across
    concurrent tool calls and therefore no mutex/lock needed anywhere in
    `dbc-mcp`. Two concurrent `run_query` calls against the same
    `connection_id` simply open two independent underlying connections —
    safe, because `execute()`/`BEGIN` is never used, so there's no
    transaction/session state that could ever be split across them.
  - Traded-off cost, stated explicitly: every tool call pays a fresh
    TCP+auth handshake (Postgres) or file-open (SQLite). Acceptable for
    an interactive/exploratory workload. A small idle-keepalive pool per
    `connection_id` is a plausible **v2** if MCP call volume/latency ever
    becomes a real complaint — not designed further here.
- Cancellation: each call's `CancelToken` is scoped to that call; it fires
  on the `timeout_secs` race (§5) and, if rmcp surfaces a client-side
  cancellation notification for the in-flight request, on that too —
  both drive the same protocol-level driver cancel already implemented
  in `dbc-core`/the drivers. No new cancellation plumbing needed.

## 8. Task decomposition

- **T1 — workspace scaffolding.** Add `crates/dbc-mcp` to workspace
  members; `Cargo.toml` with the deps from §1; `main.rs` parses
  `--config`/`--vault` flags (manual parsing, no new arg-parsing dep —
  matches this repo's existing minimal-deps style) with defaults from
  `dbc_state::config::default_config_path()`/
  `vault::default_vault_path()`; stderr logging bootstrap
  (`tracing_subscriber`). No tools yet. Test: `cargo build -p dbc-mcp`
  succeeds; binary starts and exits cleanly when stdin closes (rmcp's
  stdio transport ends on EOF).
- **T2 — schema serialization (dbc-core change).** Add
  `#[derive(serde::Serialize)]` to `SchemaSnapshot`, `TableInfo`,
  `TableKind`, `ColumnInfo`, `FkRef`, `IndexInfo`, `ConstraintInfo`,
  `RoutineKind`, `RoutineInfo`, `TriggerInfo`, `SequenceInfo` in
  `crates/dbc-core/src/schema.rs`; add `serde` as a `dbc-core` dependency
  (already a workspace-pinned version, used elsewhere in the workspace).
  Test: hand-build a `SchemaSnapshot`, serialize, assert the JSON shape
  matches §5's documented contract (field names, nesting) — this is the
  contract `get_schema`'s handler relies on.
- **T3 — `dbc-mcp` connect + vault unlock.** Credential-store key →
  `Vault::unlock_with_key` (fail closed, §3 curation override); `AppConfig::load`; `open_for_mcp(cfg,
  secret)` forcing read-only unconditionally at the driver layer (§4
  layer 2), erroring on `Engine::Mssql` and on any `ssh: Some(_)` config
  (§1 non-goal). No `Tunnel`/SSH dependency at all. Test (sqlite fixture,
  no network): unlock a throwaway vault + config pointing at a temp
  sqlite file; assert `open_for_mcp` returns a connection that can
  `SELECT` but errors on `INSERT` **even when the saved config has
  `read_only: false`** — the core assertion of layer 2.
- **T4 — `list_connections`.** Maps `AppConfig::connections` (minus any
  `ssh`-tunneled entries) to the §5 shape. Test: fixture `AppConfig` with
  2-3 mixed-engine connections including a favourite/folder/read_only one
  → assert response never contains `"host"`/`"user"`/`"database"`/
  `"password"` and does contain the expected `id`/`name`/`engine`/
  `read_only` fields.
- **T5 — `get_schema`.** Uses T2's `Serialize` impls + T3's
  `open_for_mcp`; applies the `schema` filter, `include_ddl` default, and
  512 KB truncation from §5. Tests (sqlite fixture, no network): known
  table set round-trips correctly; `include_ddl: false` (default) omits
  `ddl` fields; a hand-built oversized `SchemaSnapshot` (unit-tested
  directly against the truncation function, no need for an actual huge
  DB) trips the 512 KB truncation marker.
- **T6 — `run_query`.** `is_read_statement` gate → `apply_auto_limit` →
  `open_for_mcp` → drain `QueryStream` into a `ResultBuffer::with_cap(…,
  row_limit)` raced against `timeout_secs` → serialize via
  `cell_text`/`cell_is_null` into the §6 array-of-arrays shape → enforce
  the 2 MB byte cap. Tests (sqlite fixture, no network — this is the
  brief's explicitly-called-out "money" test): (a) a `SELECT` returns
  correct rows/columns/nulls; (b) an `INSERT`/`UPDATE`/`DELETE` is
  rejected by `is_read_statement` *before* any connection is opened
  (assert via a fixture DB file made read-only at the filesystem level,
  so a bypass would fail loudly rather than silently succeeding); (c) a
  query producing more than `row_limit` rows truncates with
  `truncated: true` and `row_count == row_limit`; (d) a deliberately slow
  query (same triple cross-join trick as
  `dbc-driver-sqlite`'s `cancel_interrupts_long_query` test) is cut off
  by the timeout, returning a timeout error rather than hanging; (e) a
  syntax-error SQL surfaces `QueryError`'s message/code/position in the
  tool's error result, not a panic.
- **T7 — "no write path" regression guard.** A test (in `dbc-mcp`'s own
  suite) that reads `dbc-mcp`'s own source files
  (`include_str!`/`std::fs`) and asserts no call site matches
  `.execute(` outside of comments — a cheap, mechanical enforcement of
  §4 layer 3 that survives future contributors adding tools without
  re-reading this design doc.
- **T8 — `ServerHandler` wiring.** `#[tool_router(server_handler)]` over
  T4/T5/T6 with per-tool `Parameters` structs
  (`ListConnectionsParams{}`, `GetSchemaParams{connection_id, schema:
  Option<String>, include_ddl: Option<bool>}`,
  `RunQueryParams{connection_id, sql, row_limit: Option<u32>,
  timeout_secs: Option<u32>}`, all `#[derive(Deserialize,
  schemars::JsonSchema)]`); `ServerHandler::get_info` advertising
  name/version; `main.rs` wires unlock (T3) → server construction →
  `.serve(stdio()).await?.waiting().await?`. Test: an in-process rmcp
  client/server pair (rmcp supports an in-memory duplex transport for
  exactly this) driving `list_tools` and one `call_tool` round-trip per
  tool against a sqlite fixture — end-to-end, no subprocess, no Claude
  Desktop install required. **Before building all three tools' final
  wiring, compile a minimal one-tool `#[tool_router]` example first** —
  the macro surface here is sourced from Context7 docs snapshotted today,
  not hand-verified against a compiled build (see §9 risks).
- **T9 — packaging.** `--help` text and a short usage note in `dbc-mcp`'s
  own crate-level doc comment covering the `claude mcp add`/Desktop-JSON
  setup from §2 (this design doc already contains the exact commands;
  T9 just puts them where a user running the binary will find them).

**Parallelization:** T1 first (blocks everything). Then T2 (dbc-core,
independent of dbc-mcp internals) and T3 (dbc-mcp connect logic, needs
only T1's Cargo.toml skeleton) run in parallel. Then T4 (needs T3), T5
(needs T2+T3), and T6 (needs T3; `dbc-buffer`'s `ResultBuffer` already
exists, no new task required for it) run in parallel. T7 is independent
of T4-T6 and can run any time after T1 (schedule alongside T4-T6 for
convenience). T8 depends on T4+T5+T6 all existing — the necessarily-serial
integration point. T9 last.

## 9. Risks / needs-verification

- **rmcp API surface not hand-compiled yet.** This design's macro usage
  (`#[tool_router]`, `#[tool]`, `Parameters<T>`, `stdio()`,
  `ServiceExt::serve`) is sourced from Context7's docs.rs snapshot for
  rmcp 3.1.4 (fetched 2026-08-23), not verified against an actual
  compile. Pin an exact/narrow version (`rmcp = "~3.1"`, not a bare
  `"3"`) and, per T8, compile a minimal one-tool example *before*
  wiring all three tools — catches macro-signature drift early rather
  than after three tools are written against a wrong shape.
- **`schemars` transitive version.** The `#[tool]` macro requires
  `T: schemars::JsonSchema`; `dbc-mcp`'s own `schemars` dependency version
  must match what rmcp 3.1.4 expects internally. A mismatch is a
  plausible build-time surprise — verify at T1/T8, not assumed.
- **512 KB / row-truncation thresholds are reasoned defaults, not
  measured** against a real large-schema fixture (e.g. a few-hundred-table
  warehouse schema). Flagged as tune-after-first-real-use, not a hard v1
  requirement to get exactly right.
- **Derived-key at rest in the OS credential store** (per the §3 curation
  override) is the design's main security tradeoff: DPAPI-encrypted and
  user-bound, but any process running as the user can read it via the
  keyring API. Accepted for v1 (same trust model as every OS-keychain-based
  credential helper); revocable via `dbc-mcp setup --remove` or re-keying
  the vault. The originally-drafted env-var password model (strictly
  weaker: plaintext at rest in the MCP client config) was rejected in
  curation and is NOT shipped.
- **Postgres read-only is session-level, not per-statement.**
  `default_transaction_read_only=on` is set once via `options()` at
  connect time — a known limitation already accepted by the GUI's own
  read-only path (`dbc-ui/src/connect.rs`), not something new introduced
  by this design, but worth restating since MCP leans on it
  unconditionally (§4 layer 2) rather than conditionally.
- **No automated test against a real Claude Desktop/Code client.** All
  tests in §8 are in-process or sqlite-fixture-based, per the brief's
  explicit ask ("testable end-to-end without network"). A manual
  smoke-test against an actual Claude Desktop/Code install is a pre-ship
  step, not part of the automated suite.
