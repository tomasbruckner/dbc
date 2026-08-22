# dbc GUI — Target Design & Phasing

Date: 2026-08-22
Status: awaiting user review
Scope: target ("fairly final") UI for the dbc client and its decomposition into
phases G1–G8. Each phase gets its own implementation plan; G5+ get their own
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
  password, "Test connection" button). Passwords ONLY in Windows Credential
  Manager — never on disk. Connection metadata (no secrets) in a config file
  in the user profile. Connecting happens off the UI thread (fixes the
  known block_on freeze follow-up).
- **Schema tree:** single click selects only; double-click on a table opens
  a data preview (`SELECT * LIMIT 1000`) as a result tab tagged PREVIEW;
  typing while the tree has focus starts speed search (DataGrip-style
  type-to-jump/filter). Columns show name, type, PK/FK markers.
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
  full-content popup; export of the full result or selection as CSV, JSON,
  or INSERT statements.
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
| **G1 Editor & connections** | Multiline editor (plain); connection manager (form dialog, Credential Manager vault, top-bar switcher); connect off the UI thread | Kills the two worst pains from the first human test; includes the block_on-freeze follow-up |
| **G2 Tabs & tree** | Result-tab infrastructure (buffer per tab); schema tree panel with speed search; double-click preview tabs; Ctrl+B | Also the natural home for the spill-off-UI-thread and byte-cap follow-ups (buffer work is touched anyway) |
| **G3 History & palette** | Persistent history (SQLite), right panel, fulltext, pins, click-to-load; Ctrl+K palette | History and palette share data sources |
| **G4 Grid+** | Local sort, column filters, cell detail popup, export CSV/JSON/INSERT | Pure additions over the existing buffer |
| **G5 Sandbox editing** | PK detection, local diff edits, Apply dialog with generated SQL, single transaction | First write path in the app; gets its own design pass before implementation |
| **G6 Editor pro** | Tree-sitter highlighting; schema autocomplete | Most expensive single feature, functionally least urgent |
| **G7 DB compare** | Schema diff between two saved connections (SchemaSnapshot-based); data diff via DuckDB over Arrow buffers | Future; own brainstorm when reached |
| **G8 ER diagram** | FK-graph rendering of a schema in GPUI canvas; export image | Future; own brainstorm when reached |

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
(autocomplete engine), G7 and G8 — each gets its own brainstorm + spec when
its turn comes. This spec fixes the target UX and the phase boundaries.
