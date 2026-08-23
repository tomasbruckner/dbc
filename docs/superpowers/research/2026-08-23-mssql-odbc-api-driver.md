# Research: MSSQL Driver on odbc-api

> Research-pass output (2026-08-23, subagent). Blueprint for `dbc-driver-mssql`. Decision already made: odbc-api, NOT tiberius.

## Feasibility verdict

`odbc-api` is viable for our `Connection` trait, but it is a strictly lower-level, blocking API — the MSSQL driver will look architecturally like **`dbc-driver-sqlite` (blocking bridge), not `dbc-driver-postgres`** (native async). Every trait need has a counterpart (blocking execute/fetch, `SQLRowCount` via `Statement::row_count()`, transactions via one handle, catalog via raw SQL against `sys.*`), but two things need real engineering: (1) `Connection<'c>` is `Send + !Sync` and borrows from a process-wide singleton `Environment`; (2) odbc-api exposes **no safe cancel API** — real cancellation needs the raw handle (`handles::AnyHandle`) + unsafe `SQLCancelHandle` from a watcher thread (spec-sanctioned cross-thread call).

## Dependencies + Windows runtime prereqs

- **Crate**: `odbc-api = "29"` (latest as of research; re-check before pinning — this crate moves fast between majors). Add `odbc-sys` directly only if the raw-handle cancel path is implemented.
- **Windows runtime**: the ODBC Driver Manager (`odbc32.dll`) ships with Windows, but the **Microsoft ODBC Driver 17/18 for SQL Server** must be installed separately (winget `msodbcsql18`). Packaging/setup-docs prerequisite.
- **Connection string** (Driver 18):
  `Driver={ODBC Driver 18 for SQL Server};Server=tcp:host,1433;Database=mydb;Uid=user;Pwd=pass;Encrypt=yes;TrustServerCertificate=no;Connection Timeout=30;`
- **TLS gotcha**: Driver 18 defaults `Encrypt=yes` **with cert validation** — self-signed/dev servers fail unless `TrustServerCertificate=yes` (or `Encrypt=no`/`optional`). `Encrypt=Strict` ignores `TrustServerCertificate` entirely. Must be a first-class connect-dialog option.
- `Environment` must be a single process-wide instance (`OnceLock`), itself `Send + Sync`; every connection borrows `Connection<'static>` from it.

## Architecture (mirrors dbc-driver-sqlite's blocking bridge)

- `Connection<'c>` is `Send + !Sync` → movable into `spawn_blocking` and back, never shared. `MssqlConnection` carries `exec_conn: Option<odbc_api::Connection<'static>>` — take-out/put-back, identical to sqlite's `exec_conn`.
- `query()`: fresh connection per call (sqlite model), spawn_blocking, prepare/execute → cursor, column metadata over oneshot, then batch-fetch loop pushing RecordBatches over the bounded mpsc (`tx.blocking_send`), honoring `BATCH_ROWS`/`CHANNEL_CAPACITY`.
- `execute()`: reuse `exec_conn`; BEGIN/statement/COMMIT sequentially over the SAME handle (trait invariant, connection.rs:22-42). Row count via `Statement::row_count()` (SQLRowCount) — returns **-1 = "unknown"** for some statement types; must map to error/sentinel, never blind-cast to u64.
- **Cancellation**: no safe API. Documented mechanism: grab raw handle before the blocking fetch, watcher task calls `odbc_sys::SQLCancelHandle` unsafely on `cancel.cancelled()` — the ONE operation ODBC contractually allows cross-thread despite `!Sync`. Fallback/defense-in-depth: cooperative `cancel.is_cancelled()` between batch fetches (bounds latency to one rowset fetch).
- **Batching**: columnar `BlockCursor`/`RowSetBuffer` (e.g. 1024-row rowsets into a reused buffer) over row-wise `next_row()` (documented slow). Typed builders for clean types (Int32/Int64/Float64/Boolean), text fallback with placeholder for DECIMAL/DATETIME2/UNIQUEIDENTIFIER/VARBINARY — mirroring pg's `text_value`/sqlite's `value_to_text`.

## Catalog query sketches (SchemaSnapshot v2)

Schema-exclusion analog of pg's `SCHEMA_EXCLUDE`: `is_ms_shipped = 0` + exclude schemas `'sys'`, `'INFORMATION_SCHEMA'`.

- **Tables/views + columns**: `sys.tables`/`sys.views` + `sys.schemas` + `sys.columns` + `sys.types` (respect `user_type_id` for precision/scale) + `sys.default_constraints`; `is_nullable` off `sys.columns`.
- **PK**: `sys.key_constraints` `type='PK'` + `sys.index_columns` (order by `key_ordinal`).
- **FK**: `sys.foreign_keys` + `sys.foreign_key_columns` (resolve via `sys.columns`).
- **Constraints**: union key_constraints (PK/UNIQUE) + foreign_keys + `sys.check_constraints` (has `definition` text directly).
- **Indexes**: `sys.indexes` (`is_primary_key = 0`) + ordered columns + `is_unique`.
- **View DDL**: `sys.sql_modules.definition` / `OBJECT_DEFINITION(object_id)`. Tables: `None` ddl → UI synthesizes.
- **Routines**: `sys.objects` `type IN ('P','FN','TF','IF')` + `sys.parameters` signature + `OBJECT_DEFINITION` (NULL for encrypted modules tolerated per-row).
- **Triggers**: `sys.triggers` + parent table + definition. **Sequences**: `sys.sequences` + `sys.schemas`.
- Generic ODBC catalog functions (`SQLTables` etc.) exist but are far poorer — use raw `sys.*` SQL exclusively (parity with pg's native-catalog approach).

## Integration concerns (for later wiring tasks — NOT the driver crate)

1. **Identifier quoting**: `dbc-core/ddl.rs::quote_ident` hard-codes `"double quotes"`; MSSQL wants `[brackets]` (double quotes only under `QUOTED_IDENTIFIER ON`, not guaranteed). Quoting must be dialect-parameterized before MSSQL joins the sandbox write path — **blocking dependency** for that wiring, owned outside the driver crate.
2. **TLS/TrustServerCertificate** → connect-dialog option.
3. **Affected rows**: -1 = unknown must not feed the Apply expectation check as a real count.
4. **Tx-error divergence — NEEDS EMPIRICAL VERIFICATION**: SQL Server default (`XACT_ABORT OFF`) likely = statement error does NOT abort the tx — a **third behavior** (neither pg always-aborts nor sqlite always-open). `SET XACT_ABORT ON` switches to abort-and-rollback. Verify against a real instance before writing the trait-conformance doc.
5. **Multi-statement batches**: `guards.rs` already splits on top-level `;` and requires every sub-statement to pass — design already accounts for ODBC batch execution.

## Risks / needs-verification

1. Exact odbc-api method/signature for the raw-handle cancel path (spike before design commit).
2. `row_count()` reachability from `Connection::execute()`'s `Option<CursorImpl>` return (`None` = no result set) — `Preallocated`/`into_stmt()` route needs a hands-on check.
3. Tx-error divergence (above).
4. `Environment` singleton under parallel test execution — serialize tests or share a fixture.
5. odbc-api version drift.
