# DB Client — Design Spec, Phases 0–2

Date: 2026-08-21
Status: awaiting user review
Scope: phases 0–2 only. Phases 3–6 are listed for context and will get their own specs.

## 1. What we are building

A fast desktop database client for Windows, built in Rust with GPUI (Zed's UI
framework). Priorities, in order:

1. **Performance** — sub-second startup, first rows visible immediately,
   real query cancellation, million-row result sets without memory blowup.
2. **AI integration** — Claude Code drives the client via MCP. *Not in v1.*
   The architecture must allow an MCP server to be added later without
   touching the UI (see §3), but no MCP code is written in phases 0–2.

Target databases (eventually): PostgreSQL, MS SQL Server, SQLite, DuckDB.
Phases 0–2 cover SQLite and PostgreSQL only.

Explicitly rejected alternatives: DBeaver/DataGrip/QoreDB (user wants their
own), Electron/TS (conflicts with performance priority), .NET+WebView2
(user chose the Zed stack), Tauri (user chose GPUI).

## 2. Phase plan (risk-ordered)

| # | Phase | Proves | In this spec |
|---|-------|--------|--------------|
| 0 | rustup, workspace, empty GPUI window on Windows | The stack builds and draws at all | ✅ |
| 1 | SQLite → grid, one hard-coded connection | End-to-end pass through the whole stack | ✅ |
| 2 | Postgres: streaming, first-row-fast, real cancel, virtualized 1M-row grid | The performance priority | ✅ |
| 3 | MSSQL via odbc-api | Driver trait generalizes | later spec |
| 4 | Connection manager, credential vault, schema tree | Usability as a tool | later spec |
| 5 | SQL editor: tree-sitter highlighting, schema autocomplete | The most expensive single feature | later spec |
| 6 | DuckDB driver, MCP server over dbc-core | Extensions | later spec |

Phase 1 is deliberately tiny: it is a test of the GPUI bet, meant to produce
a verdict within days, not weeks. MSSQL is phase 3 (not last) because it is
the biggest unknown.

## 3. Architecture

```
db/
├── Cargo.toml                    # workspace
└── crates/
    ├── dbc-core/                 # traits, types, Arrow. No UI, no concrete DB.
    ├── dbc-driver-sqlite/        # rusqlite            (phase 1)
    ├── dbc-driver-postgres/      # tokio-postgres      (phase 2)
    ├── dbc-buffer/               # result storage & paging
    └── dbc-ui/                   # gpui binary
```

**The one architectural rule:** `dbc-core` never sees GPUI; `dbc-ui` never
sees a concrete driver. This is the entire cost of "MCP later" — in phase 6,
`dbc-mcp` becomes a second binary over the same core.

### Core trait

```rust
pub struct QueryStream {
    /// Columns known BEFORE the first row → grid draws its header instantly.
    pub columns: Arc<arrow::datatypes::Schema>,
    /// Columnar batches, not rows. Backpressure for free via bounded channel.
    pub batches: mpsc::Receiver<Result<RecordBatch, QueryError>>,
}

#[async_trait]
pub trait Connection: Send {
    async fn query(&mut self, sql: &str, cancel: CancelToken) -> Result<QueryStream, QueryError>;
    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError>;
}
```

### Decisions (fixed)

- **Arrow `RecordBatch` as the result format.** A million rows is a handful
  of contiguous buffers, not a million heap allocations. Also makes the
  future DuckDB driver and buffer spill nearly free (zero-copy Arrow).
- **GPUI pinned to a git commit** of zed-industries/zed, not crates.io.
  crates.io `gpui` 0.2.2 (Oct 2025) predates the `gpui_platform` standalone
  path; git-pin is the only sane mode for a pre-1.0, undocumented framework.
  Upgrades are deliberate, isolated commits.
- **MSSQL will use `odbc-api` + Microsoft ODBC Driver 18, not `tiberius`.**
  tiberius' last release is July 2024 (unmaintained); odbc-api is actively
  maintained and delegates protocol work to Microsoft's native driver.
  (Phase 3 concern; recorded here because it influenced trait design —
  the trait must not assume async-native drivers; ODBC work will run on
  `spawn_blocking`.)
- **Tokio** for all database I/O.

## 4. Data flow & performance

- **Threading.** The UI thread does GPUI only and never touches a socket.
  All DB work runs on a background tokio runtime. Invariant: *no `.await`
  on the UI thread, no drawing off it.* UI receives batches via a channel
  polled from the GPUI side.
- **First row fast.** Postgres uses `query_raw` / `RowStream` (the plain
  `query()` buffers the whole result). A batch is closed after **1024 rows
  or 16 ms**, whichever comes first — first rows appear near-instantly even
  for minute-long queries.
- **Cancellation** is protocol-level, verified available on every target:
  Postgres `cancel_token()` (CancelRequest on a second connection), SQLite
  `interrupt()`, ODBC `SQLCancel`. `Esc` kills the query for real.
- **Buffer (`dbc-buffer`).** Holds `Vec<RecordBatch>` with prefix-sum row
  counts → O(1) lookup of "rows 847 300–847 340" to (batch, offset). The
  grid requests only the visible window. Above a configurable cap
  (default 500k rows / 256 MB) excess batches spill to disk.
- **Errors are data, not panics.** `QueryError { code, message, position }`.
  `position` exists so the phase-5 editor can underline the exact spot.

## 5. UI (phases 0–2)

One window: plain SQL text input on top (no highlighting yet), grid below,
status bar with row count and elapsed time. `Ctrl+Enter` runs, `Esc`
cancels. Connection is entered as a raw connection string (a Postgres URL
or a SQLite file path; the manager UI is phase 4). Grid supports scrolling
through 1M rows, column resize, and copy of selected cells. Nothing else.

## 6. Security

Not a concern in phases 0–2 beyond: connection strings are never logged,
and no credential persistence exists yet (phase 4 adds an OS-keychain
vault). The read-only enforcement decision (MCP tools are read-only,
enforced both by rejecting non-SELECT and by DB-side read-only transactions)
is recorded for phase 6.

## 7. Testing

- `dbc-core` and `dbc-buffer`: unit tests against a fake driver.
- `dbc-driver-postgres`: integration tests via `testcontainers` (Docker),
  marked `#[ignore]` so plain `cargo test` stays fast.
- `criterion` benchmark: "1M rows into buffer" — guards the Arrow decision.
- `dbc-ui` stays deliberately dumb (no logic worth testing); pre-1.0 GPUI
  is not worth testing against.

## 8. Risks

| Risk | Mitigation |
|------|-----------|
| GPUI breaking changes, no docs | git-pin; upgrades are isolated commits; Zed source is the reference |
| GPUI unbearable to work with | Phase 1 is the cheap verdict; fallback stack (.NET+WebView2 or Tauri) already evaluated |
| Rust learning curve (first Rust project) | Phases sized small; core/driver/UI separation keeps units learnable |
| odbc-api ergonomics (phase 3) | Trait already allows blocking drivers via `spawn_blocking` |
