# G10 Server Admin — Design Pass

Date: 2026-08-23
Status: draft, designed autonomously under the standing G5+ mandate; for user
review before implementation planning.
Scope (spec row G10): users and roles (create, password, membership),
privileges matrix (GRANT/REVOKE), database/schema DDL with sizes.
Reads: `docs/superpowers/specs/2026-08-22-gui-target-design.md` §1 "Grid
editing" + G5 design pass block (style model), §3 constraints;
`docs/superpowers/plans/2026-08-23-g5-sandbox-editing.md` Task 4
(`run_write_transaction` — the machinery this design reuses);
`crates/dbc-core/src/connection.rs` (`Connection::execute` + its invariants).

## 0. Amendment to §3 "Sandbox Apply is the ONLY write path"

- **Amended statement:** the Apply confirm-modal + `run_write_transaction`
  machinery (G5 Task 4) is the ONLY write path in the app. Sandbox grid edits
  and G10 admin actions are its two callers; nothing else may reach
  `Connection::execute`. This is additive, not a relaxation: every G10
  mutation still shows exact SQL in a confirm modal, still runs inside one
  BEGIN…COMMIT over one dedicated connection, still respects the read-only
  guard already in `run_write_transaction`.
- **Signature extension (needed):** G5 Task 4's contract is
  `run_write_transaction(spec, statements: Vec<(String, Option<u64>)>)`. G10
  needs a display string that differs from the executed string (password
  redaction — §3 below), so the statement type widens to a struct (see §3).
  If G5 Task 4 has already merged with the tuple form by the time G10 is
  implemented, G10's first task is a small additive refactor
  (tuple → struct, `From<(String, Option<u64>)>` impl so G5's call site is a
  one-line change) — not a redesign.

## 1. Catalog reads

No parameter binding exists on `Connection::query` (it takes a raw `&str`).
Every catalog SELECT below interpolates its schema/db filter through
`dbc_core::{quote_ident, quote_qualified}` or the same `sql_value`-style
string-literal escaping `sandbox.rs` already uses — never raw concatenation
of a user-typed filter. Catalog SELECTs run through the existing
`Connection::query` read path (a new `QueryRunner::fetch_admin_catalog`,
§3), never `execute`.

**SQLite: feature exempt.** SQLite has no server-side role/GRANT model (file
permissions only) — the "Správa serveru" entry point (tree node + palette
action) never appears for `Engine::Sqlite`, checked at the same call site
that gates preview/DDL tree visibility today.

### Postgres

```sql
-- Roles (cluster-wide)
SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb,
       rolcanlogin, rolreplication, rolconnlimit, rolvaliduntil, rolbypassrls
FROM pg_catalog.pg_roles ORDER BY rolname;

-- Membership
SELECT g.rolname AS role, m.rolname AS member, am.admin_option
FROM pg_catalog.pg_auth_members am
JOIN pg_catalog.pg_roles g ON g.oid = am.roleid
JOIN pg_catalog.pg_roles m ON m.oid = am.member
ORDER BY g.rolname, m.rolname;

-- Table/view privileges, one schema at a time
SELECT n.nspname AS schema, c.relname AS object,
       CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view'
                       WHEN 'm' THEN 'matview' WHEN 'p' THEN 'table'
                       ELSE c.relkind::text END AS kind,
       a.grantee::regrole::text AS grantee, a.privilege_type, a.is_grantable
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a
WHERE c.relkind IN ('r','v','m','p','f') AND n.nspname = '{schema}'
ORDER BY c.relname, a.grantee::regrole::text, a.privilege_type;

-- Schema-level ACL (USAGE/CREATE)
SELECT n.nspname AS schema, a.grantee::regrole::text AS grantee,
       a.privilege_type, a.is_grantable
FROM pg_catalog.pg_namespace n
CROSS JOIN LATERAL aclexplode(COALESCE(n.nspacl, acldefault('n', n.nspowner))) a
WHERE n.nspname = '{schema}'
ORDER BY a.grantee::regrole::text, a.privilege_type;

-- Database-level ACL (CONNECT/CREATE/TEMP)
SELECT d.datname AS database, a.grantee::regrole::text AS grantee,
       a.privilege_type, a.is_grantable
FROM pg_catalog.pg_database d
CROSS JOIN LATERAL aclexplode(COALESCE(d.datacl, acldefault('d', d.datdba))) a
WHERE d.datname = current_database()
ORDER BY a.grantee::regrole::text, a.privilege_type;

-- Current DB size
SELECT pg_catalog.pg_database_size(current_database()) AS bytes,
       pg_catalog.pg_size_pretty(pg_catalog.pg_database_size(current_database())) AS pretty;

-- All databases + sizes (DDL/sizes browser)
SELECT datname, pg_catalog.pg_database_size(datname) AS bytes
FROM pg_catalog.pg_database WHERE datistemplate = false ORDER BY datname;

-- Schema sizes (tables+indexes+toast)
SELECT n.nspname AS schema, SUM(pg_catalog.pg_total_relation_size(c.oid)) AS bytes
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r','m') GROUP BY n.nspname ORDER BY n.nspname;
```

`acldefault` requires PG ≥ 10 (assumed baseline; older PG falls back to
`relacl IS NULL` meaning "owner default", already handled by `COALESCE`
degrading gracefully if `acldefault` itself is unavailable — needs-verification,
§6).

### MSSQL

```sql
-- Server principals (logins)
SELECT name, type_desc, is_disabled, create_date, modify_date, default_database_name
FROM sys.server_principals WHERE type IN ('S','U','G') ORDER BY name;

-- Database principals (users + roles)
SELECT name, type_desc, default_schema_name, create_date, is_fixed_role
FROM sys.database_principals
WHERE type IN ('S','U','G','R') AND name NOT IN ('public','guest','INFORMATION_SCHEMA','sys')
ORDER BY name;

-- Database role membership
SELECT rl.name AS role, mp.name AS member
FROM sys.database_role_members drm
JOIN sys.database_principals rl ON rl.principal_id = drm.role_principal_id
JOIN sys.database_principals mp ON mp.principal_id = drm.member_principal_id
ORDER BY rl.name, mp.name;

-- Server role membership
SELECT rl.name AS role, mp.name AS member
FROM sys.server_role_members srm
JOIN sys.server_principals rl ON rl.principal_id = srm.role_principal_id
JOIN sys.server_principals mp ON mp.principal_id = srm.member_principal_id
ORDER BY rl.name, mp.name;

-- Object-level permissions, one schema at a time (class = 1: OBJECT_OR_COLUMN)
SELECT s.name AS schema_name, o.name AS object_name, dp.name AS grantee,
       perm.permission_name, perm.state_desc
FROM sys.database_permissions perm
JOIN sys.database_principals dp ON dp.principal_id = perm.grantee_principal_id
JOIN sys.objects o ON o.object_id = perm.major_id
JOIN sys.schemas s ON s.schema_id = o.schema_id
WHERE perm.class = 1 AND s.name = '{schema}'
ORDER BY o.name, dp.name, perm.permission_name;

-- Schema-level permissions (class = 3: SCHEMA)
SELECT s.name AS schema_name, dp.name AS grantee, perm.permission_name, perm.state_desc
FROM sys.database_permissions perm
JOIN sys.database_principals dp ON dp.principal_id = perm.grantee_principal_id
JOIN sys.schemas s ON s.schema_id = perm.major_id
WHERE perm.class = 3 AND s.name = '{schema}'
ORDER BY dp.name, perm.permission_name;

-- Current DB size (sp_helpdb equivalent, avoids the multi-result-set proc)
SELECT DB_NAME() AS database_name,
       CAST(SUM(CASE WHEN type = 0 THEN size ELSE 0 END) * 8.0 / 1024 AS DECIMAL(15,2)) AS data_mb,
       CAST(SUM(CASE WHEN type = 1 THEN size ELSE 0 END) * 8.0 / 1024 AS DECIMAL(15,2)) AS log_mb
FROM sys.database_files;

-- Databases list (server-wide)
SELECT name, database_id, create_date, state_desc FROM sys.databases ORDER BY name;

-- Per-schema table sizes
SELECT s.name AS schema_name, SUM(ps.reserved_page_count) * 8 AS reserved_kb,
       SUM(ps.used_page_count) * 8 AS used_kb
FROM sys.dm_db_partition_stats ps
JOIN sys.tables t ON t.object_id = ps.object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
GROUP BY s.name ORDER BY s.name;
```

`sys.database_permissions.state_desc` values `GRANT`/`GRANT_WITH_GRANT_OPTION`/
`DENY` map onto the tri-state matrix (§2); Postgres has no DENY concept — only
GRANT/absent (binary).

## 2. UI

- **Entry point — special tab, not a tree section.** Same mechanic as G9's
  monitor dashboard: "Správa serveru" opens as a singleton result tab (one
  per connection), reached via a palette action and a pinned, non-object top
  row in the schema tree (rendered above Favourites, not a real catalog
  object — parallel to how G9's monitor is reached). Hidden entirely for
  `Engine::Sqlite` and disabled (greyed, tooltip "pouze pro čtení") when
  `cfg.read_only` — belt-and-braces with the runner guard (§3).
- **Tab plumbing:** new `TabContent::Admin { view: Entity<AdminPanel> }`
  variant in `tabs.rs`. Singleton-per-connection dedup reuses the existing
  `preview_key` mechanism with a fixed sentinel (e.g. `"__admin__"`) instead
  of inventing new dedup plumbing — opening "Správa serveru" twice re-focuses
  the existing tab exactly like re-opening a table preview does.
- **Sub-navigation** inside the tab (left mini-tabs, not GPUI `Tabs`
  top-strip tabs — those are reserved for query results): **"Role a
  členství"**, **"Oprávnění"**, **"Databáze a schémata"**. Each sub-view owns
  its own staged-edits set and its own Apply bar (§3) — switching sub-view
  with unsaved changes prompts "Zahodit neuložené změny? / Zpět" rather than
  silently discarding.
- **Role a členství:** left list of roles/users (from §1's roles/principals
  query), right detail pane: flags (LOGIN/SUPERUSER/CREATEDB/… for pg;
  disabled/default-db for MSSQL), a "Členem v" checkbox list of every known
  role (checked = current membership), a "Nová role…" button and a "Smazat
  roli" button on the selected row, "Změnit heslo…" opens a small modal
  (TextField, masked input) separate from the confirm modal. Checkbox
  toggles stage a membership diff; the Apply bar reads "{n} změn ·
  Aplikovat · Zahodit" (same Czech pattern as G5).
- **Oprávnění (privileges matrix):** scope selector = one schema (dropdown,
  reuses schema list already in `SchemaSnapshot`) + one grantee (dropdown of
  roles/users) — v1 is single-grantee-at-a-time, not a 3D grantee × object ×
  privilege cube. Rows = tables/views in the selected schema; columns = a
  fixed privilege set per engine (pg: SELECT/INSERT/UPDATE/DELETE/TRUNCATE/
  REFERENCES/TRIGGER; MSSQL: SELECT/INSERT/UPDATE/DELETE/EXECUTE/REFERENCES).
  A small fixed row above the grid holds schema-level (USAGE/CREATE) and,
  pg only, database-level (CONNECT/CREATE/TEMP) checkboxes. Cell state
  cycling is **engine-aware**: Postgres cells are 2-state (granted / not
  granted → GRANT/REVOKE); MSSQL cells are 3-state (granted / denied / not
  set → GRANT/DENY/REVOKE), since MSSQL alone has a real DENY. Changed cells
  tint yellow (reuse the grid's diff-tint convention) until Apply.
- **Databáze a schémata:** read-only list of databases with size bars (pg:
  `pg_database_size`; MSSQL: `sys.database_files` sum) and, for the
  currently connected database, a schema list with per-schema size bars.
  Two mutations live here: "Nové schéma…" (name prompt → `CREATE SCHEMA`)
  and "Smazat schéma" on a selected schema (`DROP SCHEMA`, gated behind a
  checkbox "včetně CASCADE (smaže i obsah schématu)", default unchecked —
  unchecked emits plain `DROP SCHEMA`, which the engine itself refuses if
  non-empty; checked appends `CASCADE` and the confirm modal shows an extra
  red warning line "tato akce je nevratná a smaže i obsah schématu").
  Database create/drop is explicitly **out of scope** (§2 non-goals) — no UI
  for it at all.
- **Non-goals (v1, explicit):** column-level GRANT/REVOKE; row-level
  security policies; `ALTER DEFAULT PRIVILEGES`; function/procedure EXECUTE
  privileges in the matrix (only tables/views); `CREATE DATABASE` /
  `DROP DATABASE`; MSSQL server-level permissions beyond login/server-role
  membership; cross-database grants; password policy/expiry/login triggers;
  role-ownership transfer or any pre-flight "what depends on this role"
  check before DROP (the engine's own dependency error surfaces verbatim in
  the Apply modal instead); undo after a successful Apply (matches G5).

## 3. Mutation flows

- **Statement type widens** (§0): `run_write_transaction` takes
  `Vec<WriteStatement>` where
  ```rust
  pub struct WriteStatement {
      pub exec_sql: String,     // what actually runs
      pub display_sql: String,  // what the modal shows AND what history stores
      pub expected_affected: Option<u64>,
  }
  ```
  Sandbox Apply (G5 T4) constructs these with `exec_sql == display_sql`
  always (a trivial `From<(String, Option<u64>)>` impl). Only G10's
  password-bearing statements ever diverge. `record_history`'s `sql` param
  is fed the statements' `display_sql` joined by newline — `exec_sql` is
  **never** passed to history, never logged, never appears anywhere but the
  one `execute()` call. This is the single choke point that makes password
  redaction a type-level guarantee rather than a per-call-site discipline.
- **Redaction mechanism (concrete):** builders construct `exec_sql` and
  `display_sql` **in parallel from the same template**, substituting the
  real password into one and the literal `'***'` into the other — never a
  post-hoc string search-and-replace on the rendered SQL (a replace-based
  approach is fragile if the password happens to also match the username or
  another literal in the statement). Example (pg create role):
  ```rust
  fn create_role_pg(name: &str, password: &str, flags: &RoleFlags) -> WriteStatement {
      let ident = dbc_core::quote_ident(name);
      let flags_sql = flags.render(); // " LOGIN SUPERUSER CREATEDB" etc.
      WriteStatement {
          exec_sql: format!("CREATE ROLE {ident} PASSWORD {}{flags_sql}", sql_value(Some(password), false)),
          display_sql: format!("CREATE ROLE {ident} PASSWORD '***'{flags_sql}"),
          expected_affected: None,
      }
  }
  ```
  Every action that carries a password (create role/login, alter password)
  follows this shape. Password TextFields live only in the small per-action
  modal (§2), are read once into a local `String` used to build the
  statement, and are dropped when the modal closes — never stored in
  `AdminPanel` state beyond the single staged `WriteStatement`, never
  written to `dbc-state`.
- **Batching:** YES, matrix/membership edits stage locally (mirroring
  `sandbox::EditState`) and apply together in ONE transaction when
  "Aplikovat" is confirmed — consistent with the sandbox pattern and with
  "one transaction per user-visible action."
- **Per-action SQL (Postgres):**
  | Action | exec_sql shape |
  |---|---|
  | Create role | `CREATE ROLE "name" [LOGIN] PASSWORD '...' [SUPERUSER] [CREATEDB] [CREATEROLE]` |
  | Alter password | `ALTER ROLE "name" PASSWORD '...'` |
  | Drop role | `DROP ROLE "name"` |
  | Grant membership | `GRANT "role" TO "member" [WITH ADMIN OPTION]` |
  | Revoke membership | `REVOKE "role" FROM "member"` |
  | Grant table priv | `GRANT SELECT, INSERT ON "schema"."table" TO "grantee"` |
  | Revoke table priv | `REVOKE SELECT ON "schema"."table" FROM "grantee"` |
  | Grant schema/db priv | `GRANT USAGE ON SCHEMA "s" TO "g"` / `GRANT CONNECT ON DATABASE "d" TO "g"` |
  | Create schema | `CREATE SCHEMA "name"` |
  | Drop schema | `DROP SCHEMA "name" [CASCADE]` |
- **Per-action SQL (MSSQL)**, bracket-quoted (§4):
  | Action | exec_sql shape |
  |---|---|
  | Create login | `CREATE LOGIN [name] WITH PASSWORD = '...'` |
  | Create user for login | `CREATE USER [name] FOR LOGIN [name]` |
  | Alter login password | `ALTER LOGIN [name] WITH PASSWORD = '...'` |
  | Drop user / login | `DROP USER [name]` / `DROP LOGIN [name]` |
  | Add/remove db role member | `ALTER ROLE [role] ADD MEMBER [name]` / `... DROP MEMBER [name]` |
  | Add/remove server role member | `ALTER SERVER ROLE [role] ADD MEMBER [name]` / `... DROP MEMBER [name]` |
  | Grant/deny/revoke object priv | `GRANT SELECT ON [schema].[table] TO [grantee]` / `DENY ...` / `REVOKE ...` |
  | Grant schema priv | `GRANT USAGE ON SCHEMA::[s] TO [g]` |
  | Create/drop schema | `CREATE SCHEMA [name]` / `DROP SCHEMA [name]` |
- **`CREATE DATABASE`/`DROP DATABASE` excluded (§2 non-goal) for a
  correctness reason, not just scope discipline:** both Postgres and MSSQL
  refuse `CREATE DATABASE`/`DROP DATABASE` inside an explicit transaction
  block ("CREATE DATABASE cannot run inside a transaction block"). Wrapping
  every admin statement in the standard BEGIN…COMMIT (the whole point of
  reusing `run_write_transaction`) would make these two actions always fail
  — rather than special-casing the runner with a non-transactional escape
  hatch for two rarely-needed actions, v1 scopes them out entirely. Schema
  create/drop and every role/privilege statement above ARE fully
  transactional DDL on both engines, so they need no such escape hatch.
- **MSSQL `CREATE SCHEMA` batch rule:** T-SQL requires `CREATE SCHEMA` to be
  the only statement in its batch. `run_write_transaction` already issues
  one `execute()` call per `WriteStatement` sequentially (never
  concatenating statements into one string) — this constraint is already
  satisfied by construction; flagged here so no future optimization
  (e.g. batching same-kind statements into one `execute()` call) silently
  breaks it.

## 4. Engine abstraction

- **Location: `crates/dbc-ui/src/admin_sql.rs`, pure, no GPUI, no I/O** —
  same precedent as `sandbox.rs` (G5 Task 2): catalog-query string builders
  and mutation `WriteStatement` builders both live here, dispatched on
  `dbc_state::Engine` (already a `dbc-ui` dependency; no new crate edge).
  `dbc-core` gets **zero** changes for G10 — it has no concept of roles/
  privileges/engines today (`Engine` lives in `dbc-state::config`, and
  `dbc-core`/`dbc-state` are dependency-free siblings, confirmed via both
  `Cargo.toml`s), and inventing a mirror enum in `dbc-core` just to keep
  this pure module there would be pure ceremony — `sandbox.rs` already
  established that pure, exhaustively-unit-tested SQL-generation modules
  belong in `dbc-ui` even though they touch no GPUI type.
- **Identifier quoting is engine-aware, unlike the rest of the app:**
  `dbc_core::quote_ident`/`quote_qualified` are double-quote-only (correct
  for pg/sqlite, the only two engines G5's sandbox path targets). MSSQL
  conventionally brackets (`[name]`, `]` doubled) rather than relying on
  `QUOTED_IDENTIFIER ON` session state. `admin_sql.rs` adds its own
  `fn quote_ident_for(engine: Engine, name: &str) -> String` (pg/sqlite →
  delegate to `dbc_core::quote_ident`; MSSQL → bracket form) scoped to this
  module only — it does not change `ddl.rs` or `sandbox.rs`.
- **Every builder function is unit-tested for exact string output** per
  engine (weird identifiers, embedded quotes/brackets, password redaction
  pairs, tri-state → GRANT/DENY/REVOKE mapping) — the same "the dialog shows
  these strings verbatim" discipline `sandbox.rs`'s test module already
  applies.

## 5. Task decomposition

- **T1 — `admin_sql.rs`: catalog query builders.** Pure functions returning
  the exact SELECT strings from §1 (schema name interpolated via escaping,
  not concatenation). Unit tests assert exact SQL per engine + injection-safe
  interpolation (a schema literally named `weird"schema` or `weird]schema`).
- **T2 — `admin_sql.rs`: mutation builders + `WriteStatement`.** Every
  action in §3's tables, both engines, including the parallel exec/display
  construction for password-bearing statements and the tri-state →
  GRANT/DENY/REVOKE mapping for the privileges matrix. Unit tests: every
  action's exact string (both engines), redaction pairs (display has
  `'***'`/no real password substring anywhere, exec has the real value),
  CASCADE opt-in, weird identifiers.
- **T3 — runner extension.** `WriteStatement` struct + `From<(String,
  Option<u64>)>`; `run_write_transaction` signature widened (adjusting G5 T4
  if already merged, per §0); new `QueryRunner::fetch_admin_catalog(spec,
  queries: Vec<(&'static str, String)>) -> oneshot::Receiver<Result<Vec<(&'static
  str, AdminCatalogRows)>, QueryError>>` — opens one connection (`open_spec`,
  same dispatch as `fetch_schema`/`fetch_lookup`), runs each labeled SELECT
  sequentially, drains through `dbc_buffer::ResultBuffer` exactly like
  `fetch_lookup_inner` does today. Read-only guard reused unchanged from G5
  T4 (no new guard code — same choke point now has two callers). Unit tests:
  the pure guard fn (already exists per G5 T4), the tuple→struct conversion.
- **T4 — `admin_panel.rs` + tab plumbing.** `TabContent::Admin` variant,
  `"__admin__"` sentinel dedup via `preview_key`, tree pinned entry +
  palette action (hidden for sqlite/read-only), sub-nav shell, "Role a
  členství" sub-view (list+detail, membership checkboxes, staged diff, Apply
  bar wired to T3). Depends on T1–T3.
- **T5 — Oprávnění (privileges matrix) sub-view.** Schema+grantee selector,
  object×privilege grid with engine-aware tri/bi-state cycling, schema/db
  checkbox row, staged diff + Apply bar. Depends on T1–T3; independent of
  T4's membership sub-view beyond the shared `AdminPanel` shell (parallel
  once T4's shell exists).
- **T6 — Databáze a schémata sub-view.** Read-only size lists (T1's size
  queries) + create/drop schema actions (T2/T3). Depends on T1–T3; parallel
  with T5.
- **Order:** T1 ∥ T2 → T3 → T4 → {T5 ∥ T6}.
- **Cross-cutting tests:** a docker-pg integration test (mirroring G5 T1's
  `#[ignore]` pattern) exercising create role → grant → revoke → drop role
  end-to-end through `run_write_transaction`, asserting the history entry
  contains `'***'` and never the real password. No MSSQL integration test is
  possible in this environment (§6) — MSSQL builders get string-level unit
  tests only.

## 6. Risks / needs-verification

- **Privilege catalog queries need docker pg verification.** `aclexplode`
  over `pg_class.relacl`/`pg_namespace.nspacl`/`pg_database.datacl` and the
  `acldefault` fallback for null ACLs (owner-default privileges) are
  written from documentation, not yet run against a live instance — verify
  against the existing docker-pg fixture before T1 is considered done, and
  add a regression test for a table with a NULL `relacl` (never explicitly
  granted) to confirm the `acldefault` branch produces the expected
  owner-only row.
- **MSSQL is entirely untestable locally** — no MSSQL driver exists yet in
  this repo (orthogonal, unscheduled phase per the phasing table) and no
  MSSQL instance is available. Every MSSQL catalog/mutation string in this
  design is verified against documentation only; T1/T2's MSSQL tests are
  string-shape unit tests, not integration tests. **Hard prerequisite:** the
  MSSQL driver (`Connection` impl via odbc-api) must implement `query`,
  `schema`, and `execute` before any MSSQL admin action can run for real —
  if that driver phase hasn't landed by the time G10 is picked up, MSSQL
  sub-tasks are blocked (pg-only ships first, MSSQL follows once the driver
  exists).
- **`CREATE DATABASE`/`DROP DATABASE` transaction-block landmine** — scoped
  out of v1 entirely (§3); this is a correctness constraint, not a
  nice-to-have, and must not be silently reintroduced by a future "add
  database create/drop" ticket without re-deriving the non-transactional
  escape hatch it would need.
- **DENY vs REVOKE engine divergence** — the matrix's per-engine cycling
  (pg bi-state granted/not-granted vs MSSQL tri-state granted/denied/not-set,
  §2/§4) must be reviewed carefully; a bug here could silently apply the
  wrong statement kind (e.g. emitting `DENY` on Postgres, which has no such
  statement).
- **Password redaction is security-critical** — flag for a dedicated review
  pass on `admin_sql.rs`'s builders specifically (parallel exec/display
  construction, never substring-replace) and on the runner's choke point
  (`display_sql` is the only field reachable from `record_history`).
- **No pre-flight dependency check before DROP ROLE/USER/SCHEMA** — an
  engine error (e.g. "role cannot be dropped because some objects depend on
  it") surfaces verbatim in the Apply modal; acceptable for v1 (matches
  DataGrip's own posture of "let the server say no") but worth calling out
  as a possible follow-up UX ask.
