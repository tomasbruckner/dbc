# dbc GUI — Target Design & Phasing

Date: 2026-08-22
Status: awaiting user review
Scope: target ("fairly final") UI for the dbc client and its decomposition into
phases G1–G14. Each phase gets its own implementation plan; G5+ get their own
design pass before implementation. This spec supersedes the UI portions of the
phase 4/5 roadmap sketch in the 2026-08-21 spec; driver phases (MSSQL, DuckDB,
MCP) are unchanged and orthogonal.

## 1. Target UI (validated via mockups 2026-08-22)

One window, hybrid layout (user choice "C"):

```
┌────────────────────────────────────────────────────────────────────┐
│ ● demo-postgres ▾          Ctrl+K palette · Ctrl+Enter · Esc       │
├──────────┬──────────────────────────────────────────┬──────────────┤
│ schema   │ multiline SQL editor (highlight, autoc.) │ history      │
│ tree     ├──────────────────────────────────────────┤ (persistent, │
│ (speed   │ result tabs: run1 | run2 | preview: t    │ fulltext, ★) │
│ search)  ├──────────────────────────────────────────┤              │
│ Ctrl+B   │ grid: sort · filter · cell detail ·      │ click loads  │
│ toggle   │ sandbox edits (diff colors) · export     │ SQL into     │
│          ├──────────────────────────────────────────┤ editor       │
│          │ apply bar (when dirty) / status bar      │              │
└──────────┴──────────────────────────────────────────┴──────────────┘
```

### Decisions (each validated with the user)

- **Layout:** left schema tree + right history panel, both collapsible
  (Ctrl+B for tree; history toggle analogous), plus a Ctrl+K command palette.
- **Connections:** top-bar dropdown listing saved connections; "New
  connection…" opens a full form dialog (name, type, host, database, user,
  password, "Test connection" button). Connections can be organised into
  user-defined folders (tree in both the manager and the dropdown; folder
  is metadata only, no behaviour). Passwords live in an encrypted vault
  file: keys derived from a master password via Argon2id, payload encrypted
  with an AEAD cipher (ChaCha20-Poly1305 or AES-GCM); the master password
  is prompted once per app start to unlock the vault and is never stored.
  Plaintext secrets never touch disk; the vault file is portable between
  machines. Connection metadata (no secrets) in a separate config file in
  the user profile. Connecting happens off the UI thread (fixes the known
  block_on freeze follow-up).
- **Schema tree:** shows the full object catalog per schema — tables, views
  (incl. materialized), functions, procedures, triggers, indexes,
  sequences, constraints. Columns show name, type, nullability, default
  value, PK/FK markers. Single click selects only; double-click on a
  table/view opens a data preview (`SELECT * LIMIT 1000`) as a result tab
  tagged PREVIEW; double-click on a function/procedure/trigger opens its
  source (DDL) in a read-only text tab (same tab mechanism, text instead
  of grid). Typing while the tree has focus starts speed search
  (DataGrip-style type-to-jump/filter) across all object types. Object
  availability per engine differs (SQLite: no procedures; MSSQL/pg map
  their own catalogs) — the tree renders what the driver reports.
- **Editor:** one multiline editor (no editor tabs). Target state includes
  tree-sitter syntax highlighting and schema-driven autocomplete (G6);
  earlier phases ship it plain. Ctrl+Enter runs, Esc cancels (unchanged).
- **Result tabs:** every execution opens a new result tab (SQL snippet,
  row-count badge, close button); preview tabs from the tree are the same
  mechanism. Each tab owns its ResultBuffer. Tabs close manually; a cap
  (default 10) closes oldest-unpinned first.
- **History:** right panel; persistent across restarts (SQLite file in the
  user profile); stores SQL text, timestamp, connection name, row count,
  duration — never result data. Click loads SQL into the editor without
  running it. Fulltext search box over SQL text. Star pins an entry to the
  top (lightweight saved queries).
- **Palette (Ctrl+K):** one fuzzy-search entry point over: tables (open
  preview), history entries (load SQL), connections (switch), and app
  actions (toggle panels, export, new connection).
- **Grid additions:** click on header sorts locally over the Arrow buffer
  (second click reverses); per-column filter row filters locally
  (contains/=/range); Enter or double-click on a cell opens a read-only
  full-content popup; export of the full result or selection as CSV, TSV,
  JSON, or INSERT statements.
- **Column visibility:** a ☰ menu on the grid header opens a checkbox list
  of columns; unchecked columns are hidden locally (buffer unchanged,
  display-only).
- **Favourites (★ on anything):** starring a schema-tree object (table,
  view, function, …) adds it to a "Favourites" section at the top of the
  tree (cross-schema); starring a connection pins it to the top of the
  dropdown and manager regardless of folders; history stars keep working
  as before. The palette ranks favourites first. All persisted in
  dbc-state, keyed per connection for objects.
- **Per-table view memory:** keyed by (connection, schema, table), the app
  remembers column visibility, column widths, local sort, and the chosen
  FK joined columns, and reapplies them to every new preview tab of that
  table. Ad-hoc query results keep per-tab settings only (an arbitrary
  result set cannot reliably be identified as "the same table").
- **FK joined columns (user choice "B"):** on a column that is a foreign
  key, the ☰ menu offers "add columns from <referenced table>" with a
  checkbox list; selected columns appear inline next to the FK column,
  visually tinted as joined. Mechanics: for preview tabs the underlying
  query is rewritten with a LEFT JOIN; for arbitrary user queries the
  values are fetched locally by key lookup (batched `WHERE pk IN (…)` over
  the visible window). Detailed design of the lookup path lands in the G4
  plan.
- **Grid editing — sandbox with diff (user choice):** edits never touch the
  database directly. Local changes accumulate with diff colouring (yellow
  edited / green new row / red deleted row). "Apply…" opens a dialog showing
  the exact generated SQL (UPDATE/INSERT/DELETE, PK-based WHERE) for review;
  confirming runs it in a single transaction. Editing requires a detected
  primary key; tables without one are read-only with a status-bar notice.
  "Discard" drops all local changes.

## 2. Phasing

Each phase is independently shippable; order minimises risk (first DB write
lands last) and pays the biggest pains first.

| Phase | Contents | Notes |
|---|---|---|
| **G1 Editor & connections** | Multiline editor (plain); connection manager (form dialog, folders, Argon2id master-password vault, top-bar switcher); connect off the UI thread; per-connection options: SSH tunnel (host/user/key, app-managed), read-only flag (blocks every write path app-wide: sandbox Apply, admin, script runner), query timeout; auto-LIMIT guard (bare SELECT gets a configurable LIMIT, overridable per run) | Kills the two worst pains from the first human test; includes the block_on-freeze follow-up |
| **G2 Tabs & tree** | Result-tab infrastructure (buffer per tab); schema tree panel with speed search; double-click preview/DDL tabs; Ctrl+B; `SchemaSnapshot` in dbc-core grows from tables+columns to the full object model (views, functions, procedures, triggers, indexes, sequences, constraints, column defaults) with per-driver catalog queries; "Generate DDL" on tables/views (CREATE statement into a read-only tab) | Also the natural home for the spill-off-UI-thread and byte-cap follow-ups (buffer work is touched anyway) |
| **G3 History & palette** | Persistent history (SQLite), right panel, fulltext, pins, click-to-load; Ctrl+K palette; generalized favourites (★ on tree objects and connections, Favourites tree section, palette ranking) | History and palette share data sources |
| **G4 Grid+** | Local sort, column filters, cell detail popup, export CSV/TSV/JSON/INSERT; column visibility menu; FK joined columns; Ctrl+F search within the fetched result; per-table view memory (visibility, widths, sort, FK joins) in dbc-state | Mostly local additions over the buffer; FK joins need FK metadata from G2 and are the meaty half of this phase |
| **G5 Sandbox editing** | PK detection, local diff edits, Apply dialog with generated SQL, single transaction | First write path in the app; gets its own design pass before implementation |
| **G6 Editor pro** | Tree-sitter highlighting; schema autocomplete; parametrized queries (:name placeholders prompt a values dialog before run, last values remembered) | Most expensive single feature, functionally least urgent |
| **G7 DB compare** | Schema diff between two saved connections (SchemaSnapshot-based); data diff via DuckDB over Arrow buffers | Future; own brainstorm when reached |
| **G8 ER diagram** | FK-graph rendering of a schema in GPUI canvas; export image | Future; own brainstorm when reached |
| **G9 Server monitor** | Dashboard opening as a special result tab, auto-refresh 5 s (pausable). Tiles: connections (active/idle/max), locks & waiting + today's deadlock count, DB size split data vs WAL/logs, cache hit + uptime + TPS. Sections: running queries (pid, user, application, client, state, duration colour-coded, query text, kill) sorted by duration; blocking chains as a who-waits-on-whom tree with wait times (click pid = query detail); per-table sizes (data + indexes + toast, row estimate, size bar). Postgres via pg_stat_activity/pg_locks/pg_stat_database/pg_total_relation_size; MSSQL via sys.dm_exec_requests, sys.dm_tran_locks, sp_spaceused equivalents; SQLite has no monitor tab. Read-only except kill | Mockup approved 2026-08-22; may be pulled forward (after G4) since it is useful while debugging |
| **G10 Server admin** | Users and roles (create, password, membership), privileges matrix (GRANT/REVOKE), database/schema DDL with sizes | Pure DDL write paths — lands after G5 establishes the "show SQL → confirm → transaction" pattern; heavily engine-specific (pg roles vs MSSQL logins; SQLite exempt); privileges matrix gets its own design pass |
| **G11 Backup & restore** | Whole-DB export and restore per engine: Postgres via pg_dump/pg_restore (external binaries orchestrated by the app, streamed progress, format/compression options), MSSQL via BACKUP/RESTORE DATABASE (server-side, writes to the server's disk — path picker must reflect that), SQLite via file copy/VACUUM INTO | Two fundamentally different mechanics behind one UI; requires detecting client tools on the machine; own design pass when reached |
| **G12 Script runner** | Run an external .sql file (streamed statement-by-statement, never loaded into the editor) or a whole folder (files in name order, per-file progress) against a chosen connection; requires a statement splitter (also unlocks multi-statement SQL in the editor); error policy (stop/continue), transaction scope and progress UI get their own design pass; CSV import into a table (column mapping, batched INSERTs, respects read-only flag) | Future; own brainstorm when reached |
| **G13 Execution plans** | "Explain" action next to Run: estimated plan and REAL plan (EXPLAIN ANALYZE / MSSQL actual execution plan) rendered as a tree visualization — per node: operation, cost, estimated vs actual rows, timing, buffers; hot nodes highlighted; engine-provided hints (e.g. missing index) surfaced; raw plan text available | Future; own brainstorm when reached |
| **G14 Polish & extras** | Theme system with light/dark toggle; charts from a result tab (bar/line over selected columns) | Future; lowest priority by agreement |

Orthogonal, unscheduled here: MSSQL driver (odbc-api), DuckDB driver, MCP
server (original phases 3/6 — unchanged).

## 3. Architecture constraints (carried over, still binding)

- `dbc-core` never sees GPUI; `dbc-ui` never sees concrete driver crates.
- New persistent state (connections config, history DB) lives in a new
  `dbc-state` crate consumed by `dbc-ui` — core and drivers stay stateless.
- Errors are values; no panics on DB or user-data paths (sandbox Apply
  errors surface in the dialog, not as crashes).
- Sandbox Apply is the ONLY write path in the app; MCP (future) remains
  read-only per the original spec.
- GPUI stays git-pinned; upgrades are isolated commits.

## 4. Out of scope of this spec

Detailed design of G5 (SQL generation rules, conflict handling), G6
(autocomplete engine), and G7–G14 — each gets its own brainstorm + spec when
its turn comes. This spec fixes the target UX and the phase boundaries.
