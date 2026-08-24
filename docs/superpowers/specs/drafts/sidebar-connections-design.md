# Sidebar Connections Rework — Design Pass

Date: 2026-08-24
Status: designed autonomously per the G5-style standing mandate; decisions
recorded here for later user review.
Scope: replace the single-connection schema tree with a DataGrip-style
multi-root sidebar — every saved connection is an expandable node
(user requirement (a), 2026-08-24: „spojení mají být normálně rozklikávací
v menu vlevo"), and each connection expands into ALL databases on that
server (requirement (b): „pro každé spojení chci vidět všechny db"),
each database expanding into the existing schema → section → table →
column tree. This forces the app's single biggest architectural change
since G5: the active context widens from "a connection" to
"(connection, database)", and the connection-identity guard convention
(`conn_identity_matches`, main.rs:1160) widens with it — §7 carries the
full audit of every stamp/guard site.

**ORDERING (binding): this phase is serialized AFTER G15 (MSSQL wiring)
and G16 (DuckDB wiring).** All three phases edit the same
`match cfg.engine`/`match engine` arms in `connect.rs`,
`connections_ui.rs` (`test_connect_spec`, `on_dropdown_item_click`'s
`needs_secret`, `engine_label`) and `main.rs`. Specific constraints:

- (a) The expand/switch path this design routes through
  `test_connect_spec` (connections_ui.rs:2333) currently hard-refuses
  MSSQL („MSSQL driver zatím není k dispozici") — G15 deletes that
  refusal; writing this phase's per-engine database-listing dispatch
  (§3) against the pre-G15 3-variant enum would just be churn.
- (b) §3's per-engine DB-list match must be exhaustive over the FINAL
  `Engine` enum (`Postgres | Mssql | Sqlite | Duckdb`) — the `Duckdb`
  variant only exists after G16.
- (c) The MSSQL DB-list/switch mechanics (§3) assume G15's
  `Database=<name>` connection-string handling; this design deliberately
  does NOT depend on `USE <db>` (see §3, binding).
- (d) Versioning: one minor bump per phase (gui-target spec §Versioning).
  Note the g16 draft's "G15 → 0.15.0, G16 → 0.16.0" numbering is STALE —
  main already shipped 0.15.0/0.16.0 (grid tasks / UX polish). G15 and
  G16 take the next two free minors at their merge time; this phase takes
  the one after G16's actual number.
- (e) G16 §4's `editable_decision` and gating sweep may reshape
  `dialect_for_engine`/`detect_editable_pk` — §5's feature matrix here is
  written against behaviour *contracts* (per-feature rows), not line
  numbers, precisely so it survives that sweep.

Read before implementing: `crates/dbc-ui/src/schema_tree.rs` (FULL — the
`NodeId` path-stability contract, pure `flatten` + `prune_stale_ids`,
`uniform_list` rendering, the pinned `AdminRoot`/`FavouriteSection`
rows); `crates/dbc-ui/src/main.rs` §§ `active_connection_id` (1010),
`conn_spec_key` (1138), `conn_identity_matches` (1160) + the stamp/guard
sites enumerated in §7 below, `run_query_with`'s inline spec resolution
(1395-1422), `resolve_spec_for_explain` (3166), `active_conn_spec`
(4254), `apply_conn_spec` (5439), `current_conn_identity` (5010),
`trigger_schema_fetch` (5780); `crates/dbc-ui/src/connections_ui.rs`
(`on_dropdown_item_click`/`switch_to_connection`/`PendingAfterUnlock`/
`test_connect_spec`/`group_connections`, and `confirm_compare_dialog`);
`crates/dbc-ui/src/runner.rs` (`ConnectSpec`, `fetch_schema`,
`test_connect` — note EVERY operation opens a fresh connection from a
`ConnectSpec`; there is no persistent pool); `crates/dbc-ui/src/connect.rs`
(`open_config` — where `cfg.database` becomes the pg `dbname`, SQLite
file path, and read-only session options); `crates/dbc-state/src/config.rs`
(`ConnectionConfig`, `FavouriteObject`), `view_prefs.rs`/`params.rs`
(the `\u{1F}` `encode_key` convention this design reuses);
`crates/dbc-ui/src/admin_sql.rs:158` (the existing pg `pg_database`
listing §3's query is a sibling of).

## 0. Grounding facts the design leans on

1. **There is no live connection to "keep open" per tree node.** The
   runner opens a fresh connection per operation (`fetch_schema(spec)`,
   `connect_and_run(spec, …)`); only the monitor tab holds one for its
   lifetime. So "expand = connect" really means "expand = one bounded
   metadata fetch over a short-lived connection", and the tree's
   connected/disconnected visuals are about *cached metadata + active
   context*, not sockets. This makes the lazy model cheap and safe.
2. **A Postgres connection is per-database.** Listing all DBs = one
   catalog query on the connected (default) DB; browsing another DB =
   a NEW `ConnectSpec` with the `database` field swapped. The same
   mechanic works for MSSQL post-G15 (`Database=<name>` in the
   connection string) — no `USE` dependency. SQLite/DuckDB are
   one-file-one-database.
3. **The identity guard is a plain string equality with one deliberate
   change point** (`conn_identity_matches`, doc'd "single place to
   change"). Today the string is `active_connection_id` or the literal
   `"cli"` — no database component. Identities are stamped on in-memory
   objects only (tabs, panels, modal states); nothing persists them, so
   widening the format has zero migration cost.
4. Specs are re-derived on demand from `config` + `vault` in FOUR
   near-duplicate places (`run_query_with` inline,
   `resolve_spec_for_explain`, `apply_conn_spec`, `active_conn_spec`).
   This phase must touch all four anyway (db swap), so it consolidates
   them (§2.4) rather than teaching each one the swap separately.

## 1. Tree model

### 1.1 Hierarchy and row types

The left sidebar becomes a multi-root tree:

```
Správa serveru                      (pinned, active context — unchanged)
Oblíbené (n)                        (pinned, active context — unchanged)
<složka>/                           (folder grouping rows, from group_connections)
  ● prod-pg (pg)                    ← Connection row
      inventory                     ← Database row
      sales  (výchozí)  ●           ← Database row, active context
        public                      ← existing Schema/Section/… rows,
          Tabulky (12)                 rendered by the EXISTING flatten
            orders
              id: integer PK
  ○ analytics (duckdb)
      analytics.duckdb              ← single Database row (file engines)
```

The existing per-database tree (`NodeId`, `flatten`, all seven sections,
speed search semantics) is REUSED UNCHANGED as the subtree under each
Database row. Two new layers wrap it:

```rust
// schema_tree.rs — new outer row type. The existing `NodeId` stays
// exactly as-is (path-stable within one database); scope travels
// alongside it, not inside it, so none of flatten's emit_* helpers,
// prune_stale_ids, or the NodeId hash-stability tests change.
pub enum SidebarRow {
    Folder { path: Vec<String> },                       // grouping only, from group_connections
    Connection { conn_id: String },
    Database { conn_id: String, db: String },
    Inner { conn_id: String, db: String, node: NodeId }, // existing NodeId, depth-shifted
    Notice { conn_id: String, db: Option<String>, text: String }, // "Načítám…"/error/truncation rows
}
```

### 1.2 State machine (per node, lazy)

`SchemaTree` (renamed field-wise, entity kept — main.rs keeps one
`Entity<SchemaTree>`) holds:

```rust
struct ConnNode {
    id: String,               // ConnectionConfig::id
    // name/engine/folder re-read from AppView's config on every push —
    // the tree never owns a second copy of the connection list.
    dbs: DbListState,
}
enum DbListState {
    NotLoaded,                          // collapsed, nothing fetched yet
    Loading { generation: u64 },
    Error(String),                      // Czech driver/runner error text, retry on re-expand
    Loaded { dbs: Vec<DbNode>, truncated: bool },
}
struct DbNode {
    name: String,
    is_default: bool,                   // == cfg.database
    schema: DbSchemaState,
}
enum DbSchemaState {
    NotLoaded,
    Loading { generation: u64 },
    Error(String),
    Loaded { snapshot: SchemaSnapshot, expanded: HashSet<NodeId> },
}
```

- **Expanding a Connection row** with `dbs == NotLoaded` → emits
  `TreeEvent::LoadDatabases { conn_id }`; main.rs dispatches §3's
  DB-list fetch over ONE short-lived connection to the *default*
  database. NOT eager: no other connection is touched, no schema is
  fetched yet. File engines (SQLite/DuckDB) skip the fetch entirely —
  main.rs answers synchronously with the single `DbNode` (name = file
  stem of `cfg.database`, `is_default: true`).
- **Expanding a Database row** with `schema == NotLoaded` → emits
  `TreeEvent::LoadSchema { conn_id, db }`; main.rs dispatches
  `runner.fetch_schema(spec_for_database(cfg, db, secret))` (§3).
  Expanding does NOT switch the active context (§2) — browsing is free
  of side effects on the editor.
- Both fetches are generation-guarded per slot (last-dispatched-wins,
  same shape as `schema_fetch_generation`); a stale result is dropped.
  Collapsing a node keeps its cache (re-expand is instant); the ⟳
  header button refreshes the ACTIVE context's slot via the existing
  `prune_stale_ids` carry-forward, exactly like today's same-connection
  refresh.
- `schema_tree_connection_key: Option<String>` (main.rs:1047) is
  DELETED — its "same connection or not" job is subsumed by per-slot
  state (a refresh lands in its own `(conn_id, db)` slot by
  construction).

### 1.3 Vault unlock on expand

Expanding a Connection row is the new "first touch" of a server, so it
inherits `on_dropdown_item_click`'s exact gate: if
`cfg.engine` needs a secret (post-G16: everything except
`Sqlite`/`Duckdb`) AND the vault is locked AND the vault file exists →
open the master-password prompt with a new variant:

```rust
// connections_ui.rs
enum PendingAfterUnlock {
    Connect(String),
    ExpandConnection(String),                 // NEW — resume the expand
    SwitchDatabase { conn_id: String, db: String },  // NEW — §2.2
    SaveConnection(..), TestConnection(..),   // unchanged
}
```

`resume_pending` routes `ExpandConnection` back into the LoadDatabases
dispatch and `SwitchDatabase` into §2.2's switch. Cancelling the prompt
collapses the row back to `NotLoaded` (no error state — the user just
declined). `test_needs_vault_prompt`'s `engine != Engine::Sqlite`
predicate is widened to the shared `engine_needs_secret(engine)` helper
G15/G16 will already have introduced (or introduce it here if they
didn't — one function, used by all three gates).

### 1.4 Visual states (Czech, honest about fact 0.1)

- Connection row: `{name}` + dim engine chip (`pg`/`mssql`/`sqlite`/
  `duckdb`, reusing `engine_label`). Left indicator: `●` in accent when
  this connection is the active context's connection; `○` in
  `text_disabled` otherwise. A read-only connection appends the
  existing shield convention: dim „(pouze pro čtení)".
- Database row: `{db}`, plus „ (výchozí)" suffix when `is_default`;
  `●` accent when `(conn_id, db)` IS the active context; the active
  row also gets `bg_selected`-family emphasis.
- `Notice` rows: „Načítám databáze…", „Načítám schéma…" in
  `text_muted`; errors as `error: {msg}` in `danger` with the row
  clickable to retry (re-emits the Load event); truncation row per §6.
- There is deliberately NO green/red "connected/disconnected" lamp —
  per fact 0.1 there is no live socket to report. The two indicators
  are *active context* (●) and *metadata cached* (children present).

## 2. Active context: (connection, database)

### 2.1 The decision

**Clicking a Database row does switch the active context** — that is
the entire point of requirement (b): the editor, admin, monitor,
backup default, sandbox stamps, autocomplete and history all follow the
selected database. Precisely:

- **Single click** on any row: selects it (visual only), as today.
- **Double-click on a Database row**: switches the active context to
  `(conn_id, db)` (§2.2).
- **Double-click on a Connection row**: switches to
  `(conn_id, cfg.database)` — the exact semantics of today's dropdown
  item click, so the dropdown and the tree agree.
- Expanding (chevron) never switches. Browsing ≠ switching.

### 2.2 Mechanics: `switch_to_database`

New `AppView` field:

```rust
/// The active database WITHIN active_connection_id. `None` = the
/// saved config's `database` (the default). Always `None` when
/// `active_connection_id` is `None` (CLI path has no db switching).
active_database: Option<String>,
```

`connections_ui.rs::switch_to_connection(id)` becomes a thin wrapper
over the new
`switch_to_database(&mut self, id: &str, db: Option<String>, cx)`:
identical body (cancel running backup defensively, build spec — now via
`spec_for_database` when `db` is `Some` —, `runner.test_connect`,
`switch_generation` last-dispatched-wins, status
„connecting…" → „Připojeno ({engine})"), with the success arm setting
BOTH `active_connection_id = Some(id)` and `active_database = db`,
closing autocomplete, and re-triggering the schema fetch into the
`(id, effective_db)` slot. The vault gate of §1.3 fronts it
(`PendingAfterUnlock::SwitchDatabase`). A failed test_connect leaves
the previous context untouched — same contract as today.

**Queued action after switch (one-shot):** double-click on a
table/preview row in a NON-active `(conn, db)` subtree (§5 row 1)
first calls `switch_to_database`, storing a
`pending_after_switch: Option<PendingTreeAction>` (open-preview only —
the single queued kind) that the success arm replays and any failure or
superseding switch clears. This keeps "ONE active context drives
everything" while making cross-database browsing feel direct.

### 2.3 Identity widening

```rust
// main.rs — replaces the bare active_connection_id read.
/// `\u{1F}` (unit separator) joins id and database — the same
/// convention dbc-state's view_prefs/params encode_key already uses;
/// it cannot collide with either component (ids are `conn-{hex}`,
/// database names/file paths never contain control characters) and it
/// is never rendered raw (conn_name_for_identity translates).
fn conn_identity_for(conn_id: &str, database: &str) -> String {
    format!("{conn_id}\u{1F}{database}")
}

fn current_conn_identity(&self) -> String {
    match (&self.active_connection_id, ...) {
        Some(id) => conn_identity_for(id, &self.effective_database()), // db = active_database.unwrap_or(cfg.database)
        None => CLI_CONN_IDENTITY.to_string(),                          // "cli", unchanged
    }
}
```

- `conn_identity_matches` (1160) stays a trivial `==` — the widening
  happens entirely in what gets stamped/compared, which is exactly why
  the convention kept a single change point.
- **Behavioural consequence (the safety win):** switching database on
  the SAME connection now invalidates every pending write guard — an
  Apply dialog, staged admin edits, a script-run modal, a CSV mapping
  captured against `sales` refuse to dispatch after a switch to
  `inventory`. Today's identity would have silently passed. This
  closes the exact gap the audit (§7, "does not include database")
  documents.
- `conn_name_for_identity(identity)` learns to split on `\u{1F}` and
  render „{name} / {db}" (db part shown only when ≠ default) —
  mismatch messages like „změny pocházejí z jiného připojení ({from})"
  stay human-readable.
- `conn_spec_key` (1138) widens identically:
  `"cfg:{id}\u{1F}{database}"` / `"url:{u}"` — its one remaining job
  (autocomplete/schema caching identity) must distinguish databases.
- CLI-arg path: `"cli"` unchanged, sentinel semantics unchanged (the
  URL bakes its own db; the CLI session gets no db switching — §3.4).

REQUIRED tests: `conn_identity_for` composition; `current_conn_identity`
with `active_database` `None`/`Some`/CLI; the existing
`conn_identity_matches_tests` gain a case proving
`(id, "sales") != (id, "inventory")`; one integration-shaped test per
guard family (Apply refusal + script refusal after an in-connection db
switch) — mirroring the existing `csv_import_dispatch_allowed`/
`script_run_dispatch_allowed` pure-fn test style.

### 2.4 Spec resolution consolidates

The four duplicate `active_connection_id → cfg + secret` lookups all
need the db swap, so they collapse into one:

```rust
struct ActiveConn {
    cfg: ConnectionConfig,      // database ALREADY swapped to effective_database()
    secret: Option<String>,
    read_only: bool,            // cfg.read_only (inherited by every db — §4)
    engine: Engine,
    timeout_secs: Option<u64>,
    auto_limit: Option<u64>,
    identity: String,           // conn_identity_for(..) of the same snapshot
}
fn resolve_active(&self) -> Option<ActiveConn>;   // None = missing conn; CLI handled by callers as today
```

`run_query_with`'s inline block, `resolve_spec_for_explain` (which
keeps its status-writing wrapper), `apply_conn_spec` and
`active_conn_spec` become thin projections of `resolve_active`. This
is the single site where "the database the app talks to" is decided —
an invariant worth a doc comment: **no other code path may build a
`ConnectSpec::Config` from `active_connection_id` directly.**
(Compare and backup build specs from EXPLICIT configs by design and are
exempt — §5.)

### 2.5 Top-bar switcher: KEPT, demoted to status + quick switch

Decision: the dropdown stays. Reasons: (1) it is the sole home of the
per-row 🗄/♻ backup/restore entry points and the manager's
add/edit/favourite affordances — moving those is out of scope; (2) a
keyboard-fast "switch default db of another server" flow is worth
keeping; (3) the palette's `Connection` items route through the same
path. Changes: `current_connection_label` widens to
„{name} ({engine}) · {db}" (db segment only when ≠ default), and a
dropdown item click now goes through `switch_to_database(id, None)` —
i.e. the dropdown always targets the DEFAULT database, matching its
pre-rework meaning. The tree is the only UI that reaches non-default
databases. („replacing/extending" from the requirement resolves to:
extending; the tree is primary, the dropdown is auxiliary.)

## 3. Per-database spec derivation and DB listing

### 3.1 The derivation function (the security-critical five lines)

```rust
// main.rs (next to resolve_active)
/// SECURITY: the derived spec inherits EVERYTHING from the saved
/// config except `database` — same id (⇒ same vault secret, same
/// favourites/prefs bucket root), same read_only (⇒ open_config still
/// applies default_transaction_read_only / ODBC read-only), same
/// ssh/timeout/auto_limit. No new secret storage, no new config entry.
fn spec_for_database(cfg: &ConnectionConfig, db: &str, secret: Option<String>) -> ConnectSpec {
    let mut cfg = cfg.clone();
    cfg.database = db.to_string();
    ConnectSpec::Config { cfg: Box::new(cfg), secret }
}
```

### 3.2 Listing databases per engine

New `runner.fetch_database_list(spec: ConnectSpec, engine: Engine) ->
oneshot::Receiver<Result<Vec<String>, QueryError>>` — same
open-spec/run/drop shape as `fetch_schema`, executing one SELECT:

| Engine | Query (LIMIT 2001 appended — §6) | Notes |
|---|---|---|
| Postgres | `SELECT datname FROM pg_catalog.pg_database WHERE NOT datistemplate AND datallowconn ORDER BY datname` | Excludes templates AND `datallowconn = false` (deliberately stricter than admin_sql.rs:158's sizes query, which only excludes templates — a db you cannot connect to must not render as expandable). |
| Mssql (post-G15) | `SELECT name FROM sys.databases WHERE state = 0 ORDER BY name` | ONLINE only. System DBs (master/msdb/model/tempdb) are INCLUDED — DataGrip precedent, and hiding them would surprise admins; they are just rows until expanded. |
| Sqlite / Duckdb | none — synchronous single node | `DbNode { name: file_stem(cfg.database), is_default: true }`. No `ATTACH` support: ATTACH is a guarded write keyword (`guards.rs`) and multi-file topology is out of scope. |

The listing runs over `spec_for_database(cfg, cfg.database, secret)` —
i.e. the default database, under the connection's own privileges and
read-only session. Zero-privilege-escalation: if the server denies the
catalog read, the node shows the error row and the connection remains
usable exactly as before this phase (default db via dropdown).

### 3.3 Browsing/switching a database

Everything — LoadSchema fetches, the §2.2 switch's `test_connect`, and
every subsequent operation once active — uses `spec_for_database`.
**Binding: no `USE`/`\c` is ever sent.** Session-level database
switching would desynchronize the identity (§2.3) from the wire state
and is unavailable on Postgres anyway; a new connection per database is
the uniform, engine-agnostic mechanic, and fact 0.1 (per-operation
connections) makes it free.

### 3.4 CLI-arg path

The sidebar renders one synthetic root row for a CLI session: label
`current_connection_label()`'s URL form, expandable straight into the
single fetched snapshot (its db list is the one implied database) — no
listing query, no db switching, identity `"cli"` untouched. It
disappears once a saved connection is switched to, same as today's
`conn_url = None`.

## 4. Security invariants (restated + new)

1. Plaintext secrets never touch disk; derived specs hold the vault
   secret in memory for the duration of one dispatch, exactly like
   every existing spec build. `spec_for_database` introduces NO new
   storage, logging, or formatting of the secret (it moves a field it
   never reads).
2. `read_only` is a property of the SAVED CONNECTION and inherits into
   every derived database spec — server-side enforcement
   (`default_transaction_read_only=on`, SQLite/DuckDB read-only open
   modes, G15's ODBC equivalent) applies uniformly. There is no
   per-database read_only override (rejected: it would be a second
   place write policy lives).
3. Write paths remain exactly the sanctioned set (sandbox Apply, admin
   apply, script runner, CSV import, monitor kill), all behind the
   identity guards — which this phase STRENGTHENS (§2.3): a database
   switch now invalidates pending writes captured against another db.
4. The vault unlock prompt fronts every first-touch of a server
   (dropdown click today; tree expand and tree switch after this
   phase) via the same `needs_secret ∧ vault locked ∧ vault exists`
   gate — no path fetches metadata with an empty secret as a fallback.
5. The DB-list query is an ordinary SELECT under the connection's own
   role; failure degrades to an error row, never to a retry loop or a
   privilege prompt.
6. History keeps recording names, never URLs/credentials (§5 row 8's
   label extension is display text only).

## 5. Interaction with every existing feature

| # | Feature | Behaviour after this phase |
|---|---|---|
| 1 | Tree ops: preview / DDL / CSV / ER / ★ | `TreeEvent::{OpenPreview, OpenDdl, OpenErDiagram, ImportCsv, ToggleFavourite}` gain the `(conn_id, db)` scope of the row that emitted them. Handlers compare scope to the active context: matching → act exactly as today. Non-matching: double-click preview → switch-then-open via the §2.2 one-shot queue; the ICON affordances (★/⊞/⇪) and the DDL header button are rendered ONLY on active-scope rows (flatten already receives the active identity — it now also receives the active scope), so cross-connection ambient actions simply don't exist. Rationale: preview is read-only and cheap to queue; CSV import/ER/favourites against a non-active db would multiply the guard surface for no user story. |
| 2 | Editor dispatch | Unchanged call shape; `run_query_with` resolves via `resolve_active` (§2.4) so it transparently runs against the active database. Guards (read-only, auto-limit, multi-statement dialect) unchanged. |
| 3 | Sandbox identity stamps | Zero site changes — stamps call `current_conn_identity()`, guards call `conn_identity_matches`; both widen centrally (§2.3). The Apply bar's „(jiné připojení — přepni se zpět)" dim-out now also fires on a same-connection db switch, which is correct and is the audit's headline fix. |
| 4 | Admin (server-level-ish) | Opens against the active context via `apply_conn_spec`→`resolve_active`. Identity widening means a db switch triggers `admin_open_decision::Replace` (staged edits dropped with the existing warning) — correct, because admin_sql's `current_db_size`/`distinct_schemas` are database-relative even though roles are cluster-level. `admin_sql.rs`'s own queries are untouched. |
| 5 | Monitor | pg-only; opens with the active context's spec, so its held connection sits on the selected db. `pg_stat_activity`/locks are cluster-wide (unchanged value); the DATA_SIZE tile (`current_database()`) becomes per-selected-db — an improvement. Tab singleton key `"monitor:{identity}"` widens automatically → one monitor tab per (conn, db); acceptable and consistent with the tab's data. No identity guard added (its connection is held, not re-resolved — unchanged posture). |
| 6 | Backup / restore | **Deliberately UNCHANGED: default-database only, this phase.** It already operates on an explicit `connection_id` handed in by the dropdown row / palette, re-resolves via `resolve_conn_for_backup` (⇒ `cfg.database`), and its existence-only `backup_dispatch_allowed` posture is independent of the active context. Backing up / restoring a NON-default database is a real feature (pg_dump dbname swap is trivial; MSSQL `RESTORE … MOVE` is not) and gets its own safety pass — a one-line follow-up is recorded in §9. The tree offers no backup affordance. |
| 7 | Compare (G7) | Natural fit, minimally taken: `ModalState::CompareDialog`'s sides widen from `Option<String>` to `Option<(String, Option<String>)>` (connection id, db; `None` db = default). The picker columns list each connection with its CACHED database list indented beneath it (from the sidebar's `DbListState::Loaded` — the dialog never triggers fetches; connections without a cached list offer only their default row). `confirm_compare_dialog` builds both specs via `spec_for_database`. Same-connection-two-databases becomes expressible — the flagship new capability. The compare tab's inert `conn_identity` stamp stays inert. |
| 8 | History | `active_connection_name_for_history()` returns „{name}/{db}" when the active db ≠ default, plain `{name}` otherwise. Pure display string, no schema migration; dedup (`sql + connection + window`) naturally scopes per db. The known name-collision lossiness (rename/delete → "cli") is unchanged and out of scope. |
| 9 | Favourites (★) | `FavouriteObject` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub database: Option<String>` — `None` means the connection's default db, so every existing config.toml entry keeps meaning what it meant. The Oblíbené section filters by `(connection_id, database-or-default)` of the ACTIVE context; the pinned section stays active-context-only (unchanged posture). |
| 10 | View prefs / param values | Same back-compat rule as favourites, applied at the key level: for the DEFAULT database the legacy `encode_key` (no db component) is used unchanged — existing prefs survive byte-for-byte; a non-default db appends one more `\u{1F}`-separated component. One helper per store, two tests each (legacy key round-trip; non-default isolation). |
| 11 | Autocomplete | Cache keyed by `conn_spec_key` → widens automatically (§2.3); a db switch closes the popup via the existing `close_autocomplete` in the switch success arm. No further change. |
| 12 | Script runner / CSV import | Resolve via `resolve_active`; their capture-then-recheck identity guards (`script_run_dispatch_allowed`, `csv_import_dispatch_allowed`) widen automatically. |
| 13 | Explain / plans | `resolve_spec_for_explain` → `resolve_active` projection; per-engine plan gating (G13/G15/G16 shape) untouched. |
| 14 | Palette | `Connection` items keep routing through the switch path (now `switch_to_database(id, None)`); a new item kind is NOT added this phase (palette-searchable databases would require eager listing — rejected, violates lazy contract). |

## 6. Bounded rendering & recursion rules

- **`uniform_list` stays.** The new pure fn
  `flatten_sidebar(&[ConnNode], folders, outer_expanded, per-db state,
  filter, active_scope, favourites, admin) -> Vec<SidebarRow>` is a
  plain loop over connections that, for each expanded Loaded database,
  splices the EXISTING `flatten(snapshot, …)` output with a fixed depth
  offset (+1 folder depth, +1 connection, +1 database; file engines
  may collapse the single-db level visually but KEEP the Database row —
  one uniform hierarchy beats a special case, and the row doubles as
  the switch target). No recursion anywhere: outer loop + the existing
  iterative emit_* helpers; depth = folder-path length (unbounded in
  principle — `parse_folder` splits on `/` with no limit — but only an
  indent multiplier, never a recursion depth) + conn + db + schema +
  section + table + column — no stack risk regardless.
- **Snapshot cache cap:** `const LOADED_SNAPSHOT_CAP: usize = 8` —
  LRU-evict `DbSchemaState::Loaded` slots beyond the cap back to
  `NotLoaded`, NEVER evicting the active context's slot. Rationale: a
  `SchemaSnapshot` can be thousands of objects; eight covers real
  cross-db work while bounding memory on a hoarder server.
- **DB-list cap:** the listing query carries `LIMIT 2001`; > 2000 rows
  ⇒ store 2000 + `truncated: true`, rendered as a final Notice row
  „… zobrazeno prvních 2000 databází — použijte výchozí databázi nebo
  filtr". (Hundreds of DBs render fine — rows are virtualized — the cap
  guards the pathological thousands case.)
- **Speed search filters LOADED content only** (binding): the filter
  narrows connection/database/inner rows already in memory and NEVER
  triggers a fetch — typing must stay allocation-cheap and
  side-effect-free, same contract as today. Collapsed/NotLoaded
  subtrees match on their own row labels only.
- Per-slot generations make every in-flight fetch last-dispatched-wins;
  there is no fan-out (one expand = at most one fetch).

REQUIRED tests (pure, GPUI-free, `flatten_tests` style): folder/conn/db
depth composition; lazy states render their Notice rows; active-scope
gating of icon affordances; LRU cap evicts oldest-not-active;
truncation row; filter-never-fetches (flatten_sidebar is pure — assert
output only, the no-fetch property holds by construction and gets a doc
comment, not a test).

## 7. Conn-identity widening — the audit

Identity today: `active_connection_id` or `"cli"`; plain `==` via
`conn_identity_matches` (main.rs:1160, the deliberate single change
point). After §2.3 it is `"{id}\u{1F}{db}"` or `"cli"`. Every site,
verified against main @ v0.16.0:

**Stamp sites (write an identity; all pick up the widening for free —
they call `current_conn_identity()` or receive its value):**
`run_query_with` capture (1506) → `ResultTab.conn_identity` (1765);
`open_adhoc_result_tab` param (1992/2010) fed by `run_many`'s capture
(2047); `start_script_pick` (2290) → `ModalState::ScriptRun` (2404) →
script progress tab (2540/2546); `start_csv_import` (2769) →
`ModalState::CsvImport` (2890) → CSV progress tab (3039/3045);
`dispatch_plan_query` (3261/3358) and `on_confirm_analyze_write`
(3428/3447) → Plan tabs; chart tab (4815/4821); `open_admin_tab` →
`AdminPanel::new` + admin tab (5083/5111); monitor `preview_key`
`"monitor:{identity}"` (5213) + monitor tab (5235/5241); ER tab (5291)
and its DDL child (5313); `ApplyDialogState.conn_identity` from tab
(5534) and from admin panel (5759); tree-DDL text tab (5957, inert);
compare tab (connections_ui.rs:1766, inert).

**Guard sites (compare identities; all inherit the stricter semantics
with zero code change):** `admin_open_decision` (1183 — db switch now
⇒ Replace); script pick continuation (2388) and enforcement
`script_run_dispatch_allowed` (2492); CSV continuation (2872) and
enforcement `csv_import_dispatch_allowed` (3006);
`fetch_admin_catalog_into` M2 gap (5172); `on_open_apply_dialog`
(5488); `on_confirm_apply` backstop (5569); `open_admin_apply_dialog`
(5746); `trigger_schema_fetch`'s distinct_schemas push (5846);
`render_apply_bar` dim-out (6244).

**Sites needing ACTIVE changes (the real diff):**
1. `current_conn_identity` (5010) — composes via `conn_identity_for`.
2. `conn_spec_key` (1138) — `"cfg:{id}\u{1F}{db}"`.
3. `conn_name_for_identity` (5019) — splits on `\u{1F}`, renders
   „{name} / {db}" for non-default dbs (mismatch messages, admin/apply
   dials).
4. `view_prefs` key derivation (4378/4477) — currently raw
   `active_connection_id`; moves to the §5-row-10 helper (legacy key
   for default db).
5. `param_values` keying (1263/1331) — already `current_conn_identity()`
   based ⇒ widens automatically, BUT the identity now contains `\u{1F}`
   which `params::encode_key` also uses as its separator — collision
   check: keys become `id␟db␟name`, still unambiguous (fixed component
   count is gone though). DECISION: params adopt the same
   legacy-for-default rule as view_prefs instead of blindly embedding
   the composite identity — preserves existing stored values and keeps
   `encode_key` two-component for default dbs.
   **CORRECTION (sidebar T1 review round 1, MINOR — the "database names
   cannot contain control chars" claim here/§2.3/§10 is overbroad):**
   Postgres identifiers may contain ANY character except NUL, so a
   database literally named `x\u{1F}y` CAN exist, and an unescaped key
   would let one scope's prefs/params bucket alias another's (preference
   bleed only, not a security hole — identities are compared atomically
   by `conn_identity_matches`, never split for authorization). Closure:
   `connection_scope_key` (dbc-state scope.rs, T1) ESCAPES the database
   component before joining — `\` → `\\` first, then U+001F → the
   literal 6-char sequence backslash + "u001F". The escape is injective
   (a legitimate name already containing that backslash + "u001F" text
   gains a doubled backslash and stays distinct), so the emitted key
   contains exactly one raw
   U+001F — the separator. Connection ids are app-generated `conn-{hex}`
   and genuinely cannot contain the separator, so that half of the claim
   stands. Pinned by scope.rs tests
   (`hostile_database_name_cannot_smuggle_a_separator`,
   `escape_sequence_in_a_real_name_stays_distinct_from_escaped_separator`).
6. `active_connection_name_for_history` (history_panel.rs:173) — §5
   row 8.
7. `emit_favourites_section` / `favourite_object_for`
   (schema_tree.rs) — take the active scope, filter per §5 row 9.

**Intentionally guard-free surfaces (posture unchanged, re-affirmed):**
monitor (holds its own connection; identity is only a tab key), backup
(explicit id + existence check by design), compare (self-contained
configs — the §5-row-7 widening keeps it self-contained), read-only
artifact tabs (plan/chart/DDL/ER — stamped, never checked).

## 8. Migration / compat summary

- `config.toml`: NO shape change for existing data. `ConnectionConfig`
  untouched; `database` is re-labeled in the connection dialog as
  „Databáze (výchozí)" — label only. `FavouriteObject.database` is
  additive with `serde(default)` + skip-on-None ⇒ old files load AND
  round-trip byte-identically until a non-default favourite is created
  (same posture as G16 §1's variant addition; REQUIRED test mirroring
  `old_config_without_theme_loads`).
- view_prefs/params: legacy keys = default database (no rewrite, no
  loss); new component only for non-default dbs.
- History DB: no migration (display-string change only).
- Folders: the manager's `folder: Vec<String>` is REUSED as the tree's
  grouping (`group_connections` ordering, verbatim: favourites first,
  then the BTreeMap-of-paths — which puts LOOSE connections (empty
  path) before named folders, parents before children, alphabetical
  within siblings) — one grouping model, zero new config. Folder rows
  in the tree are expand/collapse only, never connect.
- The dropdown keeps working mid-migration users' muscle memory; no
  saved state references the removed `schema_tree_connection_key`.

## 9. Task decomposition (serialization explicit)

| T | Content | Files (owner) | Depends on |
|---|---|---|---|
| T1 | dbc-state additive: `FavouriteObject.database`, view_prefs/params legacy-key helpers + back-compat tests | dbc-state (isolated) | — |
| T2 | Identity core: `conn_identity_for`, `current_conn_identity`, `conn_spec_key`, `conn_name_for_identity`, `active_database` field, `resolve_active` consolidation (all four resolution sites) + tests | main.rs | — |
| T3 | Runner: `fetch_database_list` + per-engine SQL consts + `spec_for_database` + tests (sqlite/duckdb synchronous path, pg query text) | runner.rs, small main.rs seam | can run parallel to T1; serializes with T2 only at the main.rs seam — land T2 first |
| T4 | schema_tree.rs multi-root rework: `SidebarRow`, node state machines, `flatten_sidebar`, caps/LRU, scope-carrying `TreeEvent`s, full pure-fn test suite | schema_tree.rs | T2 (scope/identity types) |
| T5 | main.rs wiring: Load* event handlers, per-slot fetch plumbing (replaces `schema_tree_connection_key`), `switch_to_database` + one-shot queue, feature-matrix rows 1-5, 8, 11-13 | main.rs | T2, T3, T4 |
| T6 | connections_ui.rs: `PendingAfterUnlock::{ExpandConnection, SwitchDatabase}`, dropdown → `switch_to_database(id, None)`, top-bar label, compare dialog db sub-pick (§5 row 7) | connections_ui.rs | T5 (switch fn exists) |
| T7 | Sweep + curation: history label, favourites scope in tree, docs/memory update, full-workspace test run, version bump | cross-cutting | T5, T6 |

`main.rs`, `schema_tree.rs`, `connections_ui.rs` are single-owner
files; the chain T2 → T4 → T5 → T6 → T7 SERIALIZES (every pair shares a
file or a type), with only T1 and T3 parallelizable at the edges.
Follow-up recorded (NOT this phase): backup/restore of non-default
databases (§5 row 6); palette items for databases (§5 row 14); MSSQL
`USE`-based session switching stays permanently rejected (§3.3).

## 10. Self-review notes (pre-hand-off)

- Checked: no decision above depends on a persistent connection pool —
  every mechanic reduces to "build a spec, run one bounded operation"
  (fact 0.1), which is what the runner already sells.
- Checked: the `\u{1F}` identity separator against real data shapes —
  ids are `conn-{nanos:x}` (generate_connection_id), pg/mssql db names
  cannot contain control chars, sqlite/duckdb paths on Windows cannot
  either; and the identity string is never rendered raw (grep'd the
  stamp/guard table — only `conn_name_for_identity` produces display
  text). The params-store collision (§7 item 5) was found in this
  review pass and resolved by NOT embedding the composite identity in
  store keys.
- Checked: §5 covers every feature G1-G14 + mcp posture (dbc-mcp builds
  its own specs from config and is default-db by construction — no
  change; its read-only invariant is untouched).
- Deliberately rejected alternatives, for the record: eager
  connect-to-all-on-startup (violates lazy contract, hostile to vaulted
  servers); per-database tree entities (one entity per db explodes GPUI
  state for zero gain over per-slot structs); putting scope INSIDE
  `NodeId` (churns every variant, every test, and the path-stability
  contract for no benefit over the `SidebarRow::Inner` wrapper);
  removing the top-bar dropdown (loses backup entry points and the
  palette's switch route in the same phase that rewrites the tree —
  too much blast radius).
- Honesty check on requirement (b): "ALL databases" is bounded by
  `datallowconn`/ONLINE filters and the 2000-row cap — both are
  disclosed in-UI (§1.4, §6), not silent.
