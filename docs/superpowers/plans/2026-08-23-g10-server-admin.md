# G10 Server Admin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** "Správa serveru" — users and roles (create, password, membership), a privileges matrix (GRANT/REVOKE, plus DENY on MSSQL only), and a databases/schemas browser with sizes and schema CREATE/DROP — as a singleton per-connection tab, with every mutation flowing through the existing confirm-modal + `run_write_transaction` write path, widened from `(String, Option<u64>)` tuples to a `WriteStatement` struct whose `exec_sql`/`display_sql` split makes password redaction a type-level guarantee.

**Architecture:** One new pure module (`crates/dbc-ui/src/admin_sql.rs` — catalog SELECT builders + mutation `WriteStatement` builders, dispatched on `dbc_state::Engine`, exhaustively string-unit-tested, same precedent as `sandbox.rs`), one runner extension (`runner.rs`: `run_write_transaction` widened to `Vec<WriteStatement>`, statement-failure errors paired with `display_sql` only, plus a new labeled multi-SELECT one-shot `fetch_admin_catalog`), and one new GPUI panel (`crates/dbc-ui/src/admin_panel.rs` — sub-nav shell with three sub-views, each owning its own staged-edit set and Apply bar, all applying through `main.rs`'s existing — now target-generalized — Apply confirm dialog). `dbc-core` gets **zero** changes (design §4: `Engine` lives in `dbc-state`, and `sandbox.rs` already established that pure SQL-generation modules belong in `dbc-ui`).

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`), `zeroize` (already a workspace dep at root `Cargo.toml:24`, used by `dbc-state`'s vault and `dbc-mcp` — new to `dbc-ui`), tokio (existing). No other new dependencies.

**Spec:** `docs/superpowers/specs/drafts/g10-server-admin-design.md` — binding design for this phase, **including its CURATION block, which partially supersedes the body**: (1) the §0 write-path amendment is approved (see Global Constraints for the G12-reconciled wording); (2) `dbc-driver-mssql` exists but is unwired — MSSQL builders ship string-unit-tested only, bracket-quoted, never through `dbc_core::quote_ident`; (3) redaction hardening: the runner never interpolates `exec_sql` into any surfaced error/status/log — plus a mandated unit test; (4) modal passwords are read into `zeroize::Zeroizing<String>`; (5) `acldefault` has NO graceful fallback — hard PG ≥ 10, on error the privileges sub-view shows the error, no fallback query; (6) required read-only refusal tests — one at the runner guard, one UI-level (entry point disabled).

## Global Constraints

- Cargo lives at `%USERPROFILE%\.cargo\bin\cargo.exe`; every invocation uses explicit `-p <crate>` flags, never a bare workspace-wide build/test.
- Zero warnings — `cargo build`/`cargo test` output must be warning-free for every crate touched.
- Errors are values; no panics on DB or user-data paths. An engine refusal (dependency error on DROP ROLE, non-empty schema on plain DROP SCHEMA), a failed catalog fetch, or an impossible builder request (DENY on Postgres) surfaces as an error string in the panel/dialog — never a crash.
- `dbc-core` never sees GPUI (untouched this phase — zero `dbc-core` changes at all). `dbc-ui` imports no concrete driver crate outside `connect.rs` (unaffected — no driver code changes; live MSSQL admin lights up only when the orthogonal MSSQL wiring task lands).
- **Write-path invariant (G12 CURATION item 1's §3-novela wording, which supersedes this design's §0 wording where they conflict):** the app-wide write invariant is the PATTERN, not one function: *every* write reaches `Connection::execute` only through (a) a confirm modal showing the exact SQL that will run, (b) a runner-owned method with explicit transaction discipline, and (c) the SHARED read-only guard at the runner choke point. After G10, `run_write_transaction` has two sanctioned callers — sandbox Apply and admin Apply — both through `main.rs`'s one confirm dialog, both behind the one `guard_not_read_only` choke point (`runner.rs:256`). No fresh read-only logic anywhere in this phase.
- GPUI stays pinned at rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`.
- **Passwords:** read from the masked modal `TextField` into `zeroize::Zeroizing<String>` (same discipline as `dbc-state/src/vault.rs:158`'s `export_key` and `dbc-mcp/src/main.rs:173`); `exec_sql`/`display_sql` are constructed **in parallel from the same template** (never post-hoc search-and-replace); `display_sql` is the ONLY string reachable by the confirm modal, `record_history`, status lines, and error messages — `exec_sql` exists solely for the one `execute()` call and dies with the statement `Vec` when the transaction future completes (never cached in panel state, never written to `dbc-state`).
- **SQLite is fully exempt from this feature** — the "Správa serveru" entry point (tree row, palette action) never appears for `Engine::Sqlite`; every `admin_sql` builder defensively returns empty/`Err` for it anyway.
- UI strings are Czech (labels, statuses, errors) — English only in code/comments/tests.
- Tests green before every commit: `cargo test -p dbc-ui` must pass with each task's new tests included; each task leaves `dbc-ui` at least as green as it found it. `dbc-core`/`dbc-state` are untouched (no test-count movement expected there).
- Version bump to `0.10.0` in `crates/dbc-ui/Cargo.toml` at merge (per the phasing table's `G<n> → 0.<n>.0` convention; `dbc-ui` is `0.5.0` as of writing — confirm the intervening phases' bumps have landed first, don't skip a version out of order).

### Task dependency graph (design §5, refined)

| Task | Depends on | Files touched |
|---|---|---|
| T1 catalog query builders | — | `admin_sql.rs` (new), `main.rs` (+1 line `mod`) |
| T2 `WriteStatement` + mutation builders | T1 (same file exists) | `admin_sql.rs` |
| T3 runner extension | T2 (`WriteStatement` type) | `runner.rs`, `main.rs` (call site), `Cargo.toml` |
| T4 panel shell + Roles + plumbing | T1–T3 | `admin_panel.rs` (new), `main.rs`, `tabs.rs`, `schema_tree.rs`, `palette.rs`, `Cargo.toml` (zeroize) |
| T5 privileges matrix sub-view | T4 | `admin_panel.rs`, `main.rs` (minor) |
| T6 databases & schemas sub-view | T4 | `admin_panel.rs`, `main.rs` (minor), `Cargo.toml` (version) |

See the **Task ordering** section at the end for the parallel/serial split.

---

### Task 1 (T1): `admin_sql.rs` — catalog query builders

**Files:**
- Create: `crates/dbc-ui/src/admin_sql.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `mod admin_sql;` next to the existing `mod sandbox;`)

**Interfaces:**
- Consumes: `dbc_state::Engine` (`crates/dbc-state/src/config.rs:23` — `Postgres | Mssql | Sqlite`), `dbc_core::quote_ident`/`quote_qualified` (`crates/dbc-core/src/ddl.rs:42/47`) for the pg/sqlite quoting delegate.
- Produces (consumed by T2 internally, T3's `fetch_admin_catalog`, T4/T5/T6's fetch events):
  ```rust
  /// Engine-aware identifier quoting (design §4): pg/sqlite delegate to
  /// dbc_core::quote_ident (double quotes, `"` doubled); MSSQL brackets
  /// (`[name]`, `]` doubled) — MSSQL must NEVER route through
  /// dbc_core::quote_ident (CURATION item 2). Scoped to this module only;
  /// ddl.rs/sandbox.rs are unchanged.
  pub fn quote_ident_for(engine: Engine, name: &str) -> String;
  /// `schema.object`, both parts through quote_ident_for.
  pub fn quote_qualified_for(engine: Engine, schema: &str, object: &str) -> String;
  /// Single-quoted SQL string literal, `'` doubled — the same escaping
  /// sandbox::sql_value applies on its quoted path, extracted here because
  /// catalog filters and passwords need the literal WITHOUT the numeric
  /// bare-path heuristic.
  pub fn sql_string_literal(s: &str) -> String;

  /// Labeled catalog SELECTs (label, sql) for the "Role a členství"
  /// sub-view. Postgres: [("roles", …), ("membership", …)]. MSSQL:
  /// [("server_principals", …), ("db_principals", …),
  /// ("db_role_members", …), ("server_role_members", …)].
  /// SQLite: empty (feature-exempt, defensive).
  pub fn roles_catalog(engine: Engine) -> Vec<(&'static str, String)>;
  /// "Oprávnění" sub-view, one schema at a time — `schema` is interpolated
  /// via sql_string_literal, NEVER raw concatenation. Postgres:
  /// [("object_acl", …), ("schema_acl", …), ("db_acl", …)] — hard PG ≥ 10
  /// (acldefault; CURATION item 5: on error the sub-view shows the error,
  /// there is NO fallback query). MSSQL: [("object_perms", …),
  /// ("schema_perms", …)]. SQLite: empty.
  pub fn privileges_catalog(engine: Engine, schema: &str) -> Vec<(&'static str, String)>;
  /// "Databáze a schémata" sub-view. Postgres: [("current_db_size", …),
  /// ("databases", …), ("schema_sizes", …)]. MSSQL: same three labels
  /// (db size from sys.database_files, databases list, per-schema
  /// partition-stats sizes). SQLite: empty.
  pub fn sizes_catalog(engine: Engine) -> Vec<(&'static str, String)>;
  ```

**Grounding:** the SQL text is design §1's, verbatim, single-lined, with every `'{schema}'` placeholder replaced by `sql_string_literal(schema)` interpolation. Catalog SELECTs run through the read path only (T3's `fetch_admin_catalog` → `Connection::query`), never `execute`. `sandbox.rs`'s test module is the discipline to mirror: exact-string assertions on everything parameterized, plus injection cases with hostile identifiers.

- [ ] **Step 1: Write the failing tests** (`crates/dbc-ui/src/admin_sql.rs`, `#[cfg(test)] mod catalog_tests`):

```rust
#[cfg(test)]
mod catalog_tests {
    use super::*;
    use dbc_state::Engine;

    #[test]
    fn quote_ident_for_pg_doubles_quotes_mssql_doubles_brackets() {
        assert_eq!(quote_ident_for(Engine::Postgres, "we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident_for(Engine::Sqlite, "plain"), "\"plain\"");
        assert_eq!(quote_ident_for(Engine::Mssql, "we]ird"), "[we]]ird]");
        assert_eq!(quote_ident_for(Engine::Mssql, "plain"), "[plain]");
    }

    #[test]
    fn quote_qualified_for_both_engines() {
        assert_eq!(quote_qualified_for(Engine::Postgres, "s", "t"), "\"s\".\"t\"");
        assert_eq!(quote_qualified_for(Engine::Mssql, "s", "t"), "[s].[t]");
    }

    #[test]
    fn sql_string_literal_doubles_single_quotes() {
        assert_eq!(sql_string_literal("O'Brien"), "'O''Brien'");
        assert_eq!(sql_string_literal(""), "''");
    }

    #[test]
    fn sqlite_is_feature_exempt_every_catalog_is_empty() {
        assert!(roles_catalog(Engine::Sqlite).is_empty());
        assert!(privileges_catalog(Engine::Sqlite, "any").is_empty());
        assert!(sizes_catalog(Engine::Sqlite).is_empty());
    }

    #[test]
    fn pg_roles_catalog_labels_and_sources() {
        let qs = roles_catalog(Engine::Postgres);
        assert_eq!(qs.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec!["roles", "membership"]);
        assert!(qs[0].1.contains("FROM pg_catalog.pg_roles"));
        assert!(qs[1].1.contains("pg_catalog.pg_auth_members"));
    }

    #[test]
    fn mssql_roles_catalog_labels_and_sources() {
        let qs = roles_catalog(Engine::Mssql);
        assert_eq!(
            qs.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
            vec!["server_principals", "db_principals", "db_role_members", "server_role_members"]
        );
        assert!(qs[0].1.contains("sys.server_principals"));
        assert!(qs[2].1.contains("sys.database_role_members"));
    }

    // Injection-safe interpolation (design §5 T1): a schema literally named
    // weird"schema / weird]schema / O'Brien goes into the STRING-LITERAL
    // filter position with '' doubling — quotes/brackets are inert inside a
    // string literal, only the ' matters.
    #[test]
    fn pg_privileges_catalog_escapes_schema_literal() {
        let qs = privileges_catalog(Engine::Postgres, "O'Brien\"s");
        assert_eq!(qs.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec!["object_acl", "schema_acl", "db_acl"]);
        assert!(qs[0].1.contains("n.nspname = 'O''Brien\"s'"));
        assert!(qs[1].1.contains("n.nspname = 'O''Brien\"s'"));
        // db_acl is scoped to current_database(), no schema interpolation.
        assert!(qs[2].1.contains("current_database()"));
        assert!(!qs[2].1.contains("O''Brien"));
        // Hard PG >= 10 (CURATION item 5): acldefault stays in the query —
        // its COALESCE arm is the NULL-acl owner-default semantics, not a
        // version fallback. No alternative query exists.
        assert!(qs[0].1.contains("aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner)))"));
        assert!(qs[1].1.contains("acldefault('n', n.nspowner)"));
        assert!(qs[2].1.contains("acldefault('d', d.datdba)"));
    }

    #[test]
    fn mssql_privileges_catalog_escapes_schema_literal() {
        let qs = privileges_catalog(Engine::Mssql, "we]ird'schema");
        assert_eq!(qs.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec!["object_perms", "schema_perms"]);
        assert!(qs[0].1.contains("s.name = 'we]ird''schema'"));
        assert!(qs[0].1.contains("perm.class = 1"));
        assert!(qs[1].1.contains("perm.class = 3"));
    }

    #[test]
    fn sizes_catalog_labels_per_engine() {
        let pg = sizes_catalog(Engine::Postgres);
        assert_eq!(pg.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec!["current_db_size", "databases", "schema_sizes"]);
        assert!(pg[0].1.contains("pg_database_size(current_database())"));
        assert!(pg[1].1.contains("datistemplate = false"));
        assert!(pg[2].1.contains("pg_total_relation_size"));

        let ms = sizes_catalog(Engine::Mssql);
        assert_eq!(ms.iter().map(|(l, _)| *l).collect::<Vec<_>>(), vec!["current_db_size", "databases", "schema_sizes"]);
        assert!(ms[0].1.contains("sys.database_files"));
        assert!(ms[1].1.contains("sys.databases"));
        assert!(ms[2].1.contains("sys.dm_db_partition_stats"));
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui catalog_tests::`
Expected: compile error (module doesn't exist).

- [ ] **Step 3: Implement** the module header + helpers + the three catalog builders. Helpers:

```rust
//! G10: pure admin SQL generation — catalog SELECT builders and (T2)
//! mutation WriteStatement builders. No GPUI, no I/O — same discipline as
//! sandbox.rs: the dialog/panel shows these strings verbatim, so every
//! builder is unit-tested for exact output.

use dbc_state::Engine;

pub fn quote_ident_for(engine: Engine, name: &str) -> String {
    match engine {
        Engine::Mssql => format!("[{}]", name.replace(']', "]]")),
        Engine::Postgres | Engine::Sqlite => dbc_core::quote_ident(name),
    }
}

pub fn quote_qualified_for(engine: Engine, schema: &str, object: &str) -> String {
    format!("{}.{}", quote_ident_for(engine, schema), quote_ident_for(engine, object))
}

pub fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
```

Catalog bodies are design §1's SQL, single-lined. The two parameterized pg queries, exactly:

```rust
pub fn privileges_catalog(engine: Engine, schema: &str) -> Vec<(&'static str, String)> {
    match engine {
        Engine::Postgres => {
            let lit = sql_string_literal(schema);
            vec![
                ("object_acl", format!(
                    "SELECT n.nspname AS schema, c.relname AS object, \
                     CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' \
                     WHEN 'm' THEN 'matview' WHEN 'p' THEN 'table' \
                     ELSE c.relkind::text END AS kind, \
                     a.grantee::regrole::text AS grantee, a.privilege_type, a.is_grantable \
                     FROM pg_catalog.pg_class c \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a \
                     WHERE c.relkind IN ('r','v','m','p','f') AND n.nspname = {lit} \
                     ORDER BY c.relname, a.grantee::regrole::text, a.privilege_type"
                )),
                ("schema_acl", format!(
                    "SELECT n.nspname AS schema, a.grantee::regrole::text AS grantee, \
                     a.privilege_type, a.is_grantable \
                     FROM pg_catalog.pg_namespace n \
                     CROSS JOIN LATERAL aclexplode(COALESCE(n.nspacl, acldefault('n', n.nspowner))) a \
                     WHERE n.nspname = {lit} \
                     ORDER BY a.grantee::regrole::text, a.privilege_type"
                )),
                ("db_acl",
                    "SELECT d.datname AS database, a.grantee::regrole::text AS grantee, \
                     a.privilege_type, a.is_grantable \
                     FROM pg_catalog.pg_database d \
                     CROSS JOIN LATERAL aclexplode(COALESCE(d.datacl, acldefault('d', d.datdba))) a \
                     WHERE d.datname = current_database() \
                     ORDER BY a.grantee::regrole::text, a.privilege_type".to_string()),
            ]
        }
        Engine::Mssql => {
            let lit = sql_string_literal(schema);
            vec![
                ("object_perms", format!(
                    "SELECT s.name AS schema_name, o.name AS object_name, dp.name AS grantee, \
                     perm.permission_name, perm.state_desc \
                     FROM sys.database_permissions perm \
                     JOIN sys.database_principals dp ON dp.principal_id = perm.grantee_principal_id \
                     JOIN sys.objects o ON o.object_id = perm.major_id \
                     JOIN sys.schemas s ON s.schema_id = o.schema_id \
                     WHERE perm.class = 1 AND s.name = {lit} \
                     ORDER BY o.name, dp.name, perm.permission_name"
                )),
                ("schema_perms", format!(
                    "SELECT s.name AS schema_name, dp.name AS grantee, perm.permission_name, perm.state_desc \
                     FROM sys.database_permissions perm \
                     JOIN sys.database_principals dp ON dp.principal_id = perm.grantee_principal_id \
                     JOIN sys.schemas s ON s.schema_id = perm.major_id \
                     WHERE perm.class = 3 AND s.name = {lit} \
                     ORDER BY dp.name, perm.permission_name"
                )),
            ]
        }
        Engine::Sqlite => Vec::new(),
    }
}
```

`roles_catalog`/`sizes_catalog` follow the same shape with design §1's remaining queries (pg roles/membership; MSSQL `sys.server_principals WHERE type IN ('S','U','G')`, `sys.database_principals WHERE type IN ('S','U','G','R') AND name NOT IN ('public','guest','INFORMATION_SCHEMA','sys')`, the two role-member joins; pg `pg_database_size` triple; MSSQL `sys.database_files` data/log MB sum, `sys.databases` list, `sys.dm_db_partition_stats` per-schema KB) — no parameters, no interpolation, `.to_string()` constants.

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui admin_sql::`
Expected: all pass, zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/admin_sql.rs crates/dbc-ui/src/main.rs
git commit -m "feat: admin catalog query builders (admin_sql.rs)"
```

---

### Task 2 (T2): `admin_sql.rs` — `WriteStatement` + mutation builders

**Files:**
- Modify: `crates/dbc-ui/src/admin_sql.rs` (append — disjoint from T1's functions)

**Interfaces:**
- Consumes: T1's `quote_ident_for`/`quote_qualified_for`/`sql_string_literal`.
- Produces (consumed by T3's runner signature, T4/T5/T6's staging, and — via `From` — G5's existing sandbox Apply):
  ```rust
  /// Design §0/§3: the widened statement type. `exec_sql` is what runs;
  /// `display_sql` is what the confirm modal shows AND what history stores.
  /// They differ ONLY for password-bearing statements (parallel
  /// construction, never post-hoc replace). Lives here (pure module, no
  /// GPUI) per design §5 T2 "mutation builders + WriteStatement";
  /// runner.rs imports it in T3.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct WriteStatement {
      pub exec_sql: String,
      pub display_sql: String,
      pub expected_affected: Option<u64>,
  }
  /// G5's sandbox statements: exec == display, always.
  impl From<(String, Option<u64>)> for WriteStatement { /* below */ }

  #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
  pub struct RoleFlags { pub login: bool, pub superuser: bool, pub createdb: bool, pub createrole: bool }

  /// pg: CREATE ROLE + PASSWORD + flags (1 stmt). MSSQL: CREATE LOGIN +
  /// CREATE USER FOR LOGIN (2 stmts). SQLite: empty (exempt).
  pub fn create_role(engine: Engine, name: &str, password: &str, flags: &RoleFlags) -> Vec<WriteStatement>;
  pub fn alter_password(engine: Engine, name: &str, password: &str) -> Vec<WriteStatement>;
  /// pg: DROP ROLE (1). MSSQL: DROP USER + DROP LOGIN (2). SQLite: empty.
  pub fn drop_role(engine: Engine, name: &str) -> Vec<WriteStatement>;
  /// `admin_option` pg-only (ignored on MSSQL); `server_role` MSSQL-only
  /// (which membership list the role came from; ignored on pg).
  pub fn add_membership(engine: Engine, role: &str, member: &str, admin_option: bool, server_role: bool) -> Vec<WriteStatement>;
  pub fn remove_membership(engine: Engine, role: &str, member: &str, server_role: bool) -> Vec<WriteStatement>;

  /// Privileges-matrix cell state. Postgres cells are BI-state (Denied is
  /// unrepresentable through cycle_cell and refused by the builders);
  /// MSSQL is TRI-state (design §2 — MSSQL alone has a real DENY).
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub enum CellState { NotSet, Granted, Denied }
  pub fn cycle_cell(engine: Engine, s: CellState) -> CellState;

  pub const PG_TABLE_PRIVS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];
  pub const MSSQL_TABLE_PRIVS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES"];
  pub const SCHEMA_PRIVS: &[&str] = &["USAGE", "CREATE"];
  pub const PG_DATABASE_PRIVS: &[&str] = &["CONNECT", "CREATE", "TEMP"];

  /// Target state alone decides the verb: Granted→GRANT, Denied→DENY
  /// (MSSQL only), NotSet→REVOKE. Err (errors are values, design §6's
  /// DENY-divergence risk): (Postgres, Denied), SQLite, or empty `privs`.
  pub fn object_privilege(engine: Engine, schema: &str, object: &str, privs: &[&str], grantee: &str, target: CellState) -> Result<WriteStatement, String>;
  /// pg: GRANT USAGE ON SCHEMA "s" TO "g". MSSQL: GRANT … ON SCHEMA::[s] TO [g].
  pub fn schema_privilege(engine: Engine, schema: &str, priv_name: &str, grantee: &str, target: CellState) -> Result<WriteStatement, String>;
  /// pg-only (design §2: db-level row is pg only): GRANT CONNECT ON DATABASE "d" TO "g".
  pub fn database_privilege_pg(database: &str, priv_name: &str, grantee: &str, target: CellState) -> Result<WriteStatement, String>;

  pub fn create_schema(engine: Engine, name: &str) -> Vec<WriteStatement>;
  /// `cascade` is pg-only opt-in (design §2 — the confirm modal adds a red
  /// warning line); T-SQL DROP SCHEMA has no CASCADE clause, the flag is
  /// ignored for MSSQL (the engine refuses a non-empty schema itself).
  pub fn drop_schema(engine: Engine, name: &str, cascade: bool) -> Vec<WriteStatement>;
  ```

**Grounding — redaction (design §3, security-critical, flagged for dedicated review in §6):** `exec_sql` and `display_sql` are built by two `format!` calls over the same template — the real password (via `sql_string_literal`) into one, the literal `'***'` into the other. Never a substring replace on rendered SQL. Password-bearing builders take `password: &str` — the `Zeroizing<String>` lives at the modal call site (T4) and derefs in.

- [ ] **Step 1: Write the failing tests** (`#[cfg(test)] mod mutation_tests` in the same file):

```rust
#[cfg(test)]
mod mutation_tests {
    use super::*;
    use dbc_state::Engine;

    #[test]
    fn from_tuple_is_exec_eq_display() {
        let ws: WriteStatement = ("UPDATE \"t\" SET \"a\" = 1".to_string(), Some(1)).into();
        assert_eq!(ws.exec_sql, ws.display_sql);
        assert_eq!(ws.exec_sql, "UPDATE \"t\" SET \"a\" = 1");
        assert_eq!(ws.expected_affected, Some(1));
    }

    // Redaction pairs (CURATION item 3 + design §3): display has '***' and
    // never any form of the real password; exec has the real, escaped value.
    #[test]
    fn create_role_pg_redaction_pair() {
        let flags = RoleFlags { login: true, createdb: true, ..Default::default() };
        let stmts = create_role(Engine::Postgres, "app_user", "s3cr'et", &flags);
        assert_eq!(stmts.len(), 1);
        assert_eq!(
            stmts[0].exec_sql,
            "CREATE ROLE \"app_user\" PASSWORD 's3cr''et' LOGIN CREATEDB"
        );
        assert_eq!(
            stmts[0].display_sql,
            "CREATE ROLE \"app_user\" PASSWORD '***' LOGIN CREATEDB"
        );
        assert!(!stmts[0].display_sql.contains("s3cr"));
        assert_eq!(stmts[0].expected_affected, None);
    }

    #[test]
    fn create_role_mssql_is_login_plus_user() {
        let stmts = create_role(Engine::Mssql, "app_user", "pw", &RoleFlags::default());
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].exec_sql, "CREATE LOGIN [app_user] WITH PASSWORD = 'pw'");
        assert_eq!(stmts[0].display_sql, "CREATE LOGIN [app_user] WITH PASSWORD = '***'");
        assert_eq!(stmts[1].exec_sql, "CREATE USER [app_user] FOR LOGIN [app_user]");
        assert_eq!(stmts[1].exec_sql, stmts[1].display_sql);
    }

    #[test]
    fn alter_password_both_engines_redacts() {
        let pg = alter_password(Engine::Postgres, "bob", "tajne");
        assert_eq!(pg[0].exec_sql, "ALTER ROLE \"bob\" PASSWORD 'tajne'");
        assert_eq!(pg[0].display_sql, "ALTER ROLE \"bob\" PASSWORD '***'");
        let ms = alter_password(Engine::Mssql, "bob", "tajne");
        assert_eq!(ms[0].exec_sql, "ALTER LOGIN [bob] WITH PASSWORD = 'tajne'");
        assert_eq!(ms[0].display_sql, "ALTER LOGIN [bob] WITH PASSWORD = '***'");
    }

    #[test]
    fn drop_role_shapes() {
        assert_eq!(drop_role(Engine::Postgres, "bob")[0].exec_sql, "DROP ROLE \"bob\"");
        let ms = drop_role(Engine::Mssql, "bob");
        assert_eq!(ms[0].exec_sql, "DROP USER [bob]");
        assert_eq!(ms[1].exec_sql, "DROP LOGIN [bob]");
    }

    #[test]
    fn membership_statements() {
        assert_eq!(
            add_membership(Engine::Postgres, "readers", "bob", false, false)[0].exec_sql,
            "GRANT \"readers\" TO \"bob\""
        );
        assert_eq!(
            add_membership(Engine::Postgres, "readers", "bob", true, false)[0].exec_sql,
            "GRANT \"readers\" TO \"bob\" WITH ADMIN OPTION"
        );
        assert_eq!(
            remove_membership(Engine::Postgres, "readers", "bob", false)[0].exec_sql,
            "REVOKE \"readers\" FROM \"bob\""
        );
        assert_eq!(
            add_membership(Engine::Mssql, "db_datareader", "bob", false, false)[0].exec_sql,
            "ALTER ROLE [db_datareader] ADD MEMBER [bob]"
        );
        assert_eq!(
            add_membership(Engine::Mssql, "sysadmin", "bob", false, true)[0].exec_sql,
            "ALTER SERVER ROLE [sysadmin] ADD MEMBER [bob]"
        );
        assert_eq!(
            remove_membership(Engine::Mssql, "db_datareader", "bob", false)[0].exec_sql,
            "ALTER ROLE [db_datareader] DROP MEMBER [bob]"
        );
    }

    // Engine-aware cycling (design §2/§6's DENY-divergence risk): pg is
    // bi-state and can NEVER reach Denied; MSSQL is tri-state.
    #[test]
    fn cycle_cell_pg_bi_state_mssql_tri_state() {
        assert_eq!(cycle_cell(Engine::Postgres, CellState::NotSet), CellState::Granted);
        assert_eq!(cycle_cell(Engine::Postgres, CellState::Granted), CellState::NotSet);
        // Defensive: a (never-constructible) pg Denied normalizes out.
        assert_eq!(cycle_cell(Engine::Postgres, CellState::Denied), CellState::NotSet);
        assert_eq!(cycle_cell(Engine::Mssql, CellState::NotSet), CellState::Granted);
        assert_eq!(cycle_cell(Engine::Mssql, CellState::Granted), CellState::Denied);
        assert_eq!(cycle_cell(Engine::Mssql, CellState::Denied), CellState::NotSet);
    }

    #[test]
    fn object_privilege_grant_revoke_deny() {
        assert_eq!(
            object_privilege(Engine::Postgres, "public", "users", &["SELECT", "INSERT"], "bob", CellState::Granted).unwrap().exec_sql,
            "GRANT SELECT, INSERT ON \"public\".\"users\" TO \"bob\""
        );
        assert_eq!(
            object_privilege(Engine::Postgres, "public", "users", &["SELECT"], "bob", CellState::NotSet).unwrap().exec_sql,
            "REVOKE SELECT ON \"public\".\"users\" FROM \"bob\""
        );
        assert_eq!(
            object_privilege(Engine::Mssql, "dbo", "users", &["SELECT"], "bob", CellState::Denied).unwrap().exec_sql,
            "DENY SELECT ON [dbo].[users] TO [bob]"
        );
        assert_eq!(
            object_privilege(Engine::Mssql, "dbo", "users", &["SELECT"], "bob", CellState::NotSet).unwrap().exec_sql,
            "REVOKE SELECT ON [dbo].[users] FROM [bob]"
        );
        // The errors-are-values backstop: DENY must be impossible on pg.
        assert!(object_privilege(Engine::Postgres, "public", "users", &["SELECT"], "bob", CellState::Denied).is_err());
        assert!(object_privilege(Engine::Sqlite, "s", "t", &["SELECT"], "b", CellState::Granted).is_err());
        assert!(object_privilege(Engine::Postgres, "s", "t", &[], "b", CellState::Granted).is_err());
    }

    #[test]
    fn schema_and_database_privileges() {
        assert_eq!(
            schema_privilege(Engine::Postgres, "public", "USAGE", "bob", CellState::Granted).unwrap().exec_sql,
            "GRANT USAGE ON SCHEMA \"public\" TO \"bob\""
        );
        assert_eq!(
            schema_privilege(Engine::Postgres, "public", "USAGE", "bob", CellState::NotSet).unwrap().exec_sql,
            "REVOKE USAGE ON SCHEMA \"public\" FROM \"bob\""
        );
        assert_eq!(
            schema_privilege(Engine::Mssql, "dbo", "USAGE", "bob", CellState::Granted).unwrap().exec_sql,
            "GRANT USAGE ON SCHEMA::[dbo] TO [bob]"
        );
        assert_eq!(
            database_privilege_pg("appdb", "CONNECT", "bob", CellState::Granted).unwrap().exec_sql,
            "GRANT CONNECT ON DATABASE \"appdb\" TO \"bob\""
        );
        assert!(database_privilege_pg("appdb", "CONNECT", "bob", CellState::Denied).is_err());
    }

    #[test]
    fn schema_ddl_and_cascade_opt_in() {
        assert_eq!(create_schema(Engine::Postgres, "rep\"orts")[0].exec_sql, "CREATE SCHEMA \"rep\"\"orts\"");
        assert_eq!(create_schema(Engine::Mssql, "reports")[0].exec_sql, "CREATE SCHEMA [reports]");
        assert_eq!(drop_schema(Engine::Postgres, "reports", false)[0].exec_sql, "DROP SCHEMA \"reports\"");
        assert_eq!(drop_schema(Engine::Postgres, "reports", true)[0].exec_sql, "DROP SCHEMA \"reports\" CASCADE");
        // T-SQL has no DROP SCHEMA … CASCADE — the flag never leaks.
        assert_eq!(drop_schema(Engine::Mssql, "reports", true)[0].exec_sql, "DROP SCHEMA [reports]");
    }

    #[test]
    fn sqlite_mutation_builders_are_empty() {
        assert!(create_role(Engine::Sqlite, "x", "p", &RoleFlags::default()).is_empty());
        assert!(alter_password(Engine::Sqlite, "x", "p").is_empty());
        assert!(drop_role(Engine::Sqlite, "x").is_empty());
        assert!(add_membership(Engine::Sqlite, "r", "m", false, false).is_empty());
        assert!(create_schema(Engine::Sqlite, "s").is_empty());
        assert!(drop_schema(Engine::Sqlite, "s", true).is_empty());
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui mutation_tests::`
Expected: compile error (types don't exist).

- [ ] **Step 3: Implement.** Core pieces:

```rust
impl From<(String, Option<u64>)> for WriteStatement {
    fn from((sql, expected_affected): (String, Option<u64>)) -> Self {
        Self { display_sql: sql.clone(), exec_sql: sql, expected_affected }
    }
}

/// The literal shown in display_sql wherever exec_sql carries a password.
const REDACTED: &str = "'***'";

impl RoleFlags {
    fn render(&self) -> String {
        let mut out = String::new();
        if self.login { out.push_str(" LOGIN"); }
        if self.superuser { out.push_str(" SUPERUSER"); }
        if self.createdb { out.push_str(" CREATEDB"); }
        if self.createrole { out.push_str(" CREATEROLE"); }
        out
    }
}

pub fn create_role(engine: Engine, name: &str, password: &str, flags: &RoleFlags) -> Vec<WriteStatement> {
    let ident = quote_ident_for(engine, name);
    match engine {
        Engine::Postgres => {
            let flags_sql = flags.render();
            vec![WriteStatement {
                exec_sql: format!("CREATE ROLE {ident} PASSWORD {}{flags_sql}", sql_string_literal(password)),
                display_sql: format!("CREATE ROLE {ident} PASSWORD {REDACTED}{flags_sql}"),
                expected_affected: None,
            }]
        }
        Engine::Mssql => vec![
            WriteStatement {
                exec_sql: format!("CREATE LOGIN {ident} WITH PASSWORD = {}", sql_string_literal(password)),
                display_sql: format!("CREATE LOGIN {ident} WITH PASSWORD = {REDACTED}"),
                expected_affected: None,
            },
            (format!("CREATE USER {ident} FOR LOGIN {ident}"), None).into(),
        ],
        Engine::Sqlite => Vec::new(),
    }
}

pub fn cycle_cell(engine: Engine, s: CellState) -> CellState {
    match (engine, s) {
        (Engine::Mssql, CellState::NotSet) => CellState::Granted,
        (Engine::Mssql, CellState::Granted) => CellState::Denied,
        (Engine::Mssql, CellState::Denied) => CellState::NotSet,
        (_, CellState::NotSet) => CellState::Granted,
        (_, _) => CellState::NotSet, // pg/sqlite bi-state; Denied normalizes out
    }
}

pub fn object_privilege(
    engine: Engine, schema: &str, object: &str, privs: &[&str], grantee: &str, target: CellState,
) -> Result<WriteStatement, String> {
    if privs.is_empty() {
        return Err("žádná oprávnění ke změně".to_string());
    }
    if engine == Engine::Sqlite {
        return Err("SQLite nemá serverová oprávnění".to_string());
    }
    if engine == Engine::Postgres && target == CellState::Denied {
        return Err("DENY na PostgreSQL neexistuje".to_string());
    }
    let list = privs.join(", ");
    let obj = quote_qualified_for(engine, schema, object);
    let g = quote_ident_for(engine, grantee);
    let sql = match target {
        CellState::Granted => format!("GRANT {list} ON {obj} TO {g}"),
        CellState::Denied => format!("DENY {list} ON {obj} TO {g}"),
        CellState::NotSet => format!("REVOKE {list} ON {obj} FROM {g}"),
    };
    Ok((sql, None).into())
}
```

Remaining builders follow the exact strings the tests pin down (`alter_password`, `drop_role`, `add_membership`/`remove_membership` — the MSSQL arms use `ALTER ROLE`/`ALTER SERVER ROLE … ADD|DROP MEMBER`; `schema_privilege` with pg `ON SCHEMA {ident}` vs MSSQL `ON SCHEMA::{ident}`; `database_privilege_pg` with `ON DATABASE {ident}`; `create_schema`/`drop_schema` with the pg-only `" CASCADE"` suffix). All admin statements carry `expected_affected: None` — the optimistic affected-rows check is a sandbox-UPDATE/DELETE concept; DDL/DCL row counts are driver-defined noise (`affected_mismatch(None, _)` never fires, `runner.rs:281`).

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui admin_sql::`
Expected: all pass (T1 + T2 modules), zero warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/admin_sql.rs
git commit -m "feat: WriteStatement + admin mutation builders with password redaction"
```

---

### Task 3 (T3): runner extension — `WriteStatement` widening + `fetch_admin_catalog`

**Files:**
- Modify: `crates/dbc-ui/src/runner.rs`
- Modify: `crates/dbc-ui/src/main.rs` (the one G5 call site, `main.rs:2434`)

**Interfaces:**
- Consumes: `crate::admin_sql::WriteStatement` (T2), `crate::admin_sql` builders (the docker test + redaction test), everything already in `runner.rs`.
- Produces (consumed by T4-T6 and by the — adjusted — G5 Apply flow):
  ```rust
  impl QueryRunner {
      /// Widened from Vec<(String, Option<u64>)> (design §0). Still the
      /// app's ONLY write path; still one dedicated connection, one
      /// BEGIN…COMMIT, still guard_not_read_only FIRST (unchanged, shared —
      /// now with two callers, per the §3-novela in Global Constraints).
      pub fn run_write_transaction(
          &self,
          spec: ConnectSpec,
          statements: Vec<crate::admin_sql::WriteStatement>,
          timeout_secs: Option<u64>,
      ) -> tokio::sync::oneshot::Receiver<Result<u64, QueryError>>;

      /// Design §5 T3: one connection (open_spec, same dispatch as
      /// fetch_schema/fetch_lookup), each labeled SELECT run sequentially
      /// through the READ path (Connection::query) and drained via
      /// dbc_buffer::ResultBuffer exactly like fetch_lookup_inner. First
      /// error aborts the whole fetch (CURATION item 5: the privileges
      /// sub-view shows the error — no fallback). No read-only guard: this
      /// is a read.
      pub fn fetch_admin_catalog(
          &self,
          spec: ConnectSpec,
          queries: Vec<(&'static str, String)>,
      ) -> tokio::sync::oneshot::Receiver<Result<Vec<(&'static str, AdminCatalogRows)>, QueryError>>;
  }

  /// (column names, rows); rows[r][c] None = SQL NULL — the same shape
  /// fetch_lookup's private LookupResult already has (which becomes an
  /// alias of this).
  pub type AdminCatalogRows = (Vec<String>, Vec<Vec<Option<String>>>);
  ```

**Grounding — the widening is mechanical:** `drive_write_sequence` (`runner.rs:314`), `drive_write_sequence_bounded` (`:383`), `run_write_transaction_inner` (`:418`) change their `statements: &[(String, Option<u64>)]` parameter to `&[WriteStatement]`; the loop body reads `st.exec_sql` / `st.expected_affected`. `guard_not_read_only` (`:256`), `spec_is_read_only` (`:267`), `affected_mismatch` (`:281`), the timeout/rollback machinery, and every doc-commented invariant are untouched. The runner's "decoupled from sandbox's types" note (`:218-224`) is updated to name `admin_sql::WriteStatement` as the shared statement type (still zero coupling to `sandbox::EditState`/GPUI).

**Grounding — redaction hardening (CURATION item 3):** the ONE place the runner pairs an error with SQL context is the per-statement failure arm — it appends `display_sql`, never `exec_sql`:

```rust
for st in statements {
    match conn.execute(&st.exec_sql, cancel.clone()).await {
        Ok(affected) => {
            if affected_mismatch(st.expected_affected, affected) {
                let _ = conn.execute("ROLLBACK", cancel.clone()).await;
                return Err(QueryError::msg(AFFECTED_MISMATCH_MSG));
            }
            total += affected;
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", cancel.clone()).await;
            // CURATION item 3: pair the surfaced error with display_sql
            // ONLY — exec_sql is used exactly once, in the execute() call
            // above, and appears in no error/status/log/history string.
            return Err(QueryError::msg(format!("{} — příkaz: {}", e.message, st.display_sql)));
        }
    }
}
```

(For sandbox statements `display_sql == exec_sql`, so G5's error surface just gains helpful statement context; the two existing assertions on error text — `assert_eq!` against `AFFECTED_MISMATCH_MSG` at `runner.rs:668` and `assert_ne!` at `:688` — still hold.)

**Grounding — `fetch_admin_catalog`:** extract the drain half of `fetch_lookup_inner` (`runner.rs:442-474`) into a shared helper so admin doesn't re-implement arrow draining:

```rust
/// Runs `sql` on an open connection and drains the FULL result into
/// materialized rows via a throwaway ResultBuffer, capped at `cap` —
/// shared by fetch_lookup_inner (cap = LOOKUP_ROW_CAP) and
/// fetch_admin_catalog_inner (same cap; catalog results are small).
async fn drain_all_rows(
    conn: &mut dyn Connection,
    sql: &str,
    cap: usize,
) -> Result<AdminCatalogRows, QueryError> { /* body moved verbatim from fetch_lookup_inner */ }

async fn fetch_admin_catalog_inner(
    spec: ConnectSpec,
    queries: Vec<(&'static str, String)>,
    handle: tokio::runtime::Handle,
) -> Result<Vec<(&'static str, AdminCatalogRows)>, QueryError> {
    let mut opened = open_spec(spec, handle).await?;
    let mut out = Vec::with_capacity(queries.len());
    for (label, sql) in queries {
        out.push((label, drain_all_rows(&mut *opened.conn, &sql, LOOKUP_ROW_CAP).await?));
    }
    Ok(out)
}
```

**Grounding — the G5 call site** (`main.rs:2434`, inside `on_confirm_apply`; `ApplyDialogState.statements` stays `Vec<(String, Option<u64>)>` in THIS task — T4 moves the conversion earlier when it generalizes the dialog):

```rust
let rx = self.runner.run_write_transaction(
    spec,
    statements.into_iter().map(admin_sql::WriteStatement::from).collect(),
    timeout_secs,
);
```

- [ ] **Step 1: Write the failing tests** (extend `runner.rs`'s existing `mod write_transaction_tests`; add a tuple-conversion helper so the six existing `drive_write_sequence`/`run_write_transaction_inner` tests need only their statement-vec construction lines touched):

```rust
// In mod write_transaction_tests:
use crate::admin_sql::{self, WriteStatement};

fn ws(sql: &str, expected: Option<u64>) -> WriteStatement {
    (sql.to_string(), expected).into()
}
// …existing tests: each `vec![("…".to_string(), Some(1)), …]` becomes
// `vec![ws("…", Some(1)), …]`; assertions unchanged.

/// CURATION item 3's REQUIRED test: a failing password-bearing statement's
/// surfaced error context contains '***' and NEVER the real password. The
/// mock fails the password statement with a generic driver message; the
/// runner's own pairing must attach display_sql, not exec_sql.
struct FailsOnAlter;

#[async_trait::async_trait]
impl Connection for FailsOnAlter {
    async fn query(&mut self, _sql: &str, _cancel: CancelToken) -> Result<dbc_core::QueryStream, QueryError> {
        Err(QueryError::msg("not exercised"))
    }
    async fn schema(&mut self) -> Result<SchemaSnapshot, QueryError> {
        Err(QueryError::msg("not exercised"))
    }
    async fn execute(&mut self, sql: &str, _cancel: CancelToken) -> Result<u64, QueryError> {
        if sql.starts_with("ALTER ROLE") {
            return Err(QueryError::msg("syntax error"));
        }
        Ok(0) // BEGIN / ROLLBACK
    }
}

#[tokio::test]
async fn statement_failure_pairs_display_sql_never_exec_sql() {
    let mut conn = FailsOnAlter;
    let stmts = admin_sql::alter_password(dbc_state::Engine::Postgres, "app_user", "s3cr'et");
    let err = drive_write_sequence(&mut conn, &stmts, CancelToken::new()).await.unwrap_err();
    assert!(err.message.contains("'***'"), "error must carry the redacted display_sql: {}", err.message);
    assert!(err.message.contains("ALTER ROLE \"app_user\""));
    assert!(!err.message.contains("s3cr"), "real password leaked into surfaced error: {}", err.message);
}

/// CURATION item 6's REQUIRED guard-level test: admin statements over a
/// read_only cfg are refused by the SHARED guard before any driver call —
/// same choke point G5's own refusal test already exercises, now proven
/// with admin-built statements.
#[tokio::test]
async fn admin_statements_refused_on_read_only_before_any_driver_call() {
    let cfg = dbc_state::ConnectionConfig {
        id: "x".into(), name: "x".into(), folder: Vec::new(),
        engine: dbc_state::Engine::Postgres,
        host: String::new(), port: None, database: "\0invalid".into(), user: String::new(),
        read_only: true, timeout_secs: None, auto_limit: None, ssh: None, favourite: false,
    };
    let spec = ConnectSpec::Config { cfg: Box::new(cfg), secret: None };
    let stmts = admin_sql::drop_role(dbc_state::Engine::Postgres, "bob");
    let handle = tokio::runtime::Handle::current();
    let err = run_write_transaction_inner(spec, stmts, None, handle).await.unwrap_err();
    assert_eq!(err.message, "připojení je jen pro čtení");
}
```

And a new `mod admin_catalog_tests` for the drain path over the sqlite temp-file driver (same `open_sqlite_test_conn` pattern, exercising `drain_all_rows` + the sequential-labels contract of `fetch_admin_catalog_inner` without docker — the SQL is generic SELECTs, the labels/order/abort-on-first-error logic is what's under test):

```rust
#[cfg(test)]
mod admin_catalog_tests {
    use super::*;

    #[tokio::test]
    async fn drains_labeled_queries_in_order() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let handle = tokio::runtime::Handle::current();
        let mut conn = crate::connect::open(f.path().to_str().unwrap(), &handle).unwrap();
        conn.execute("CREATE TABLE t(a TEXT)", CancelToken::new()).await.unwrap();
        conn.execute("INSERT INTO t VALUES ('x'), (NULL)", CancelToken::new()).await.unwrap();

        let (cols, rows) = drain_all_rows(&mut *conn, "SELECT a FROM t ORDER BY a IS NULL", 100).await.unwrap();
        assert_eq!(cols, vec!["a".to_string()]);
        assert_eq!(rows, vec![vec![Some("x".to_string())], vec![None]]);
    }

    #[tokio::test]
    async fn first_error_aborts_whole_catalog_fetch() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let handle = tokio::runtime::Handle::current();
        let mut conn = crate::connect::open(f.path().to_str().unwrap(), &handle).unwrap();
        // No fallback (CURATION item 5): an erroring catalog SELECT is a
        // hard Err for the whole labeled batch.
        let err = drain_all_rows(&mut *conn, "SELECT * FROM no_such_catalog", 100).await;
        assert!(err.is_err());
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui write_transaction_tests:: admin_catalog_tests::`
Expected: compile errors (signature mismatch, missing `drain_all_rows`).

- [ ] **Step 3: Implement** the widening (three signatures + loop body + the `display_sql` error pairing shown above), `pub type AdminCatalogRows` (+ `type LookupResult = AdminCatalogRows;` keeping `fetch_lookup`'s public surface identical), `drain_all_rows` extraction, `fetch_admin_catalog`/`fetch_admin_catalog_inner`, the `main.rs:2434` call-site map, and the existing-test `ws(…)` conversions. Update `run_write_transaction`'s and `drive_write_sequence`'s doc comments (two sanctioned callers; `exec_sql` used exactly once).

- [ ] **Step 4: Add the docker-pg end-to-end `#[ignore]` test** (design §5's cross-cutting test — mirrors `crates/dbc-driver-postgres/tests/integration.rs`'s "Docker required" `#[ignore]` convention, but env-var-driven since `dbc-ui` carries no testcontainers dep):

```rust
/// Docker/pg required. Run:
///   DBC_PG_ADMIN_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres \
///     cargo test -p dbc-ui -- --ignored admin_roundtrip
/// End-to-end create role → grant → revoke → drop through the real write
/// path, asserting the history-bound display join is redacted.
#[tokio::test]
#[ignore]
async fn admin_roundtrip_create_grant_revoke_drop_pg() {
    let url = std::env::var("DBC_PG_ADMIN_URL").expect("set DBC_PG_ADMIN_URL");
    let engine = dbc_state::Engine::Postgres;
    let password = "tajne'heslo";
    let role = "g10_plan_test_role";

    let mut stmts = admin_sql::drop_role(engine, role); // idempotence best-effort: ignore failure by running it alone first
    let handle = tokio::runtime::Handle::current();
    let _ = run_write_transaction_inner(ConnectSpec::Url(url.clone()), stmts, None, handle.clone()).await;

    stmts = admin_sql::create_role(engine, role, password, &admin_sql::RoleFlags { login: true, ..Default::default() });
    stmts.extend(admin_sql::database_privilege_pg("postgres", "CONNECT", role, admin_sql::CellState::Granted));
    stmts.extend(admin_sql::database_privilege_pg("postgres", "CONNECT", role, admin_sql::CellState::NotSet));
    stmts.extend(admin_sql::drop_role(engine, role));

    // What record_history/the confirm modal would show — display only.
    let shown = stmts.iter().map(|s| s.display_sql.as_str()).collect::<Vec<_>>().join("\n");
    assert!(shown.contains("'***'"));
    assert!(!shown.contains("tajne"));

    let total = run_write_transaction_inner(ConnectSpec::Url(url), stmts, None, handle).await.unwrap();
    let _ = total; // DDL affected counts are driver-defined; success is the assertion
}
```

(`database_privilege_pg` returns `Result<WriteStatement, _>` — `stmts.extend(result)` works because `Result` iterates its `Ok` value; the test unwraps implicitly via the final transaction success.)

- [ ] **Step 5: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass (the `#[ignore]` test skipped), zero warnings. Optionally run the ignored test against a local docker pg.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/runner.rs crates/dbc-ui/src/main.rs
git commit -m "feat: WriteStatement runner widening + fetch_admin_catalog"
```

---

### Task 4 (T4): `admin_panel.rs` shell, "Role a členství", tab/tree/palette plumbing

**Files:**
- Create: `crates/dbc-ui/src/admin_panel.rs`
- Modify: `crates/dbc-ui/src/main.rs` (`mod admin_panel;`, open/fetch/apply wiring, `ApplyDialogState` generalization)
- Modify: `crates/dbc-ui/src/tabs.rs` (`TabContent::Admin` variant)
- Modify: `crates/dbc-ui/src/schema_tree.rs` (pinned entry row + `TreeEvent::OpenAdmin`)
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::OpenServerAdmin` + gated fixed action)
- Modify: `crates/dbc-ui/Cargo.toml` (add `zeroize.workspace = true`)

**Interfaces:**
- Consumes: `admin_sql::{roles_catalog, create_role, alter_password, drop_role, add_membership, remove_membership, WriteStatement, RoleFlags}` (T1/T2), `QueryRunner::{fetch_admin_catalog, run_write_transaction}` + `AdminCatalogRows` (T3), `connections_ui::TextField` (`connections_ui.rs:235`, `new(cx, placeholder, masked)`), `zeroize::Zeroizing`.
- Produces (consumed by T5/T6, which extend the same panel):
  ```rust
  pub const ADMIN_PREVIEW_KEY: &str = "__admin__";

  /// The entry-point gate (design §2), pure and unit-tested — the
  /// UI-level half of CURATION item 6.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum AdminEntry { Hidden, Disabled, Enabled }
  pub fn admin_entry_state(engine: Option<Engine>, read_only: bool) -> AdminEntry;

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum AdminSubView { Roles, Privileges, Databases }

  /// Generic parsed role row: first result column is the name, remaining
  /// columns become (header, value) detail pairs (NULL → "—") — works for
  /// pg_roles and both MSSQL principal queries without per-engine structs.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct RoleRow { pub name: String, pub detail: Vec<(String, String)> }
  pub fn parse_roles(rows: &AdminCatalogRows) -> Vec<RoleRow>;

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Membership { pub role: String, pub member: String, pub server_role: bool }
  /// Columns role, member[, admin_option] (extras ignored).
  pub fn parse_memberships(rows: &AdminCatalogRows, server_role: bool) -> Vec<Membership>;

  /// Staged membership diff (mirrors sandbox::EditState's staging idiom).
  #[derive(Default)]
  pub struct MembershipEdits {
      pub add: std::collections::BTreeSet<(String, String, bool)>,    // (role, member, server_role)
      pub remove: std::collections::BTreeSet<(String, String, bool)>,
  }
  impl MembershipEdits {
      pub fn toggle(&mut self, role: &str, member: &str, server_role: bool, currently_member: bool);
      /// Effective checkbox state after staging.
      pub fn is_checked(&self, role: &str, member: &str, server_role: bool, currently_member: bool) -> bool;
      pub fn change_count(&self) -> usize;
      pub fn is_dirty(&self) -> bool;
      pub fn clear(&mut self);
      pub fn to_statements(&self, engine: Engine) -> Vec<WriteStatement>;
  }

  pub struct AdminPanel { /* engine, conn_identity, sub_view, roles,
      memberships, selected_role, membership_edits,
      staged_role_actions: Vec<WriteStatement>, loading, error,
      modal: Option<AdminModal>, discard_confirm: Option<AdminSubView>,
      focus_handle; T5/T6 add their fields */ }
  impl AdminPanel {
      pub fn new(engine: Engine, conn_identity: String, cx: &mut Context<Self>) -> Self;
      pub fn set_loading(&mut self, cx: &mut Context<Self>);
      pub fn set_error(&mut self, msg: &str, cx: &mut Context<Self>);
      /// Routes each labeled result to its parser by label.
      pub fn apply_catalog(&mut self, rows: Vec<(&'static str, AdminCatalogRows)>, cx: &mut Context<Self>);
      /// Post-Apply-success: clear staged sets + re-request the active
      /// sub-view's catalog.
      pub fn on_apply_success(&mut self, cx: &mut Context<Self>);
  }

  /// Panel → main.rs (main owns the runner and the confirm dialog).
  pub enum AdminEvent {
      /// Panel-built labeled SELECTs (panel knows its engine) → main
      /// forwards to QueryRunner::fetch_admin_catalog.
      FetchCatalog { queries: Vec<(&'static str, String)> },
      /// Staged statements → main opens the (generalized) Apply confirm
      /// dialog. `warning` is T6's red CASCADE line; None elsewhere.
      RequestApply { statements: Vec<WriteStatement>, warning: Option<String> },
  }
  impl gpui::EventEmitter<AdminEvent> for AdminPanel {}
  ```

**Grounding — tab plumbing:** `tabs.rs:29-37`'s `TabContent` gains `Admin { view: Entity<AdminPanel> }` (same "typed handle in plain data" note as `Grid`). Singleton-per-connection dedup reuses `preview_key` (`tabs.rs:53`) with the `ADMIN_PREVIEW_KEY` sentinel — no new `Tabs` API: `main.rs` finds the existing admin tab by key; **same `conn_identity` → `Tabs::activate` (re-focus, staged edits preserved — design §2 "re-focuses the existing tab"); different `conn_identity` → `Tabs::close` + fresh open** (stale staged admin edits must never survive a connection switch — the same posture as `conn_identity_matches`' BLOCKER-1 rationale at `tabs.rs:54-66`). This open/activate/replace decision is a pure function over `&Tabs`, unit-testable since `Tabs` is GPUI-free plain data.

**Grounding — tree + palette entry:** `schema_tree.rs:78-87`'s `TreeEvent` gains `OpenAdmin`; `NodeId` (`:65-73`) gains a unit `AdminRoot` variant; `flatten` (`:406`) takes one more param `admin: AdminEntry` and, when not `Hidden`, pushes `(NodeId::AdminRoot, 0, "Správa serveru".to_string(), false)` FIRST — above `emit_favourites_section`'s output (`:325`), matching design §2 "rendered above Favourites, not a real catalog object". `SchemaTree` stores `admin_entry: AdminEntry` (+ `set_admin_entry`, called from `main.rs` everywhere `set_favourites` (`:614`) is already called on connection switch); the row renders greyed with tooltip "pouze pro čtení" when `Disabled` and its click emits `TreeEvent::OpenAdmin` only when `Enabled`. `palette.rs:98`'s `PaletteAction` gains `OpenServerAdmin`; `fixed_actions` (`:135`) takes `admin: AdminEntry` and appends `("Správa serveru".to_string(), PaletteAction::OpenServerAdmin)` only when `Enabled` (Hidden AND Disabled both omit the row — a palette has no greyed-row idiom; the tree row is where the disabled state is explained). `main.rs`'s `PaletteAction` match (`:1618`) gains the `OpenServerAdmin => self.open_admin_tab(cx)` arm; `open_admin_tab` re-checks `admin_entry_state` defensively (belt-and-braces with the runner guard).

**Grounding — `main.rs` wiring:**

```rust
// AppView helper: engine/read_only facts for the CURRENT connection — the
// same three-way lookup apply_conn_spec/detect_editable_pk already do
// (saved config → cfg.engine/cfg.read_only; CLI URL → engine_from_url,
// never read-only; neither → None).
fn admin_entry_meta(&self) -> (Option<dbc_state::Engine>, bool) {
    if let Some(id) = &self.active_connection_id {
        match self.config.connections.iter().find(|c| &c.id == id) {
            Some(cfg) => (Some(cfg.engine), cfg.read_only),
            None => (None, false),
        }
    } else if let Some(url) = &self.conn_url {
        (Some(engine_from_url(url)), false)
    } else {
        (None, false)
    }
}

fn open_admin_tab(&mut self, cx: &mut Context<Self>) {
    let (engine, read_only) = self.admin_entry_meta();
    if admin_panel::admin_entry_state(engine, read_only) != admin_panel::AdminEntry::Enabled {
        self.status = "správa serveru není pro toto připojení dostupná".to_string();
        cx.notify();
        return;
    }
    let engine = engine.expect("Enabled implies an engine");
    let identity = self.current_conn_identity();
    let existing = self
        .tabs
        .iter()
        .find(|t| t.preview_key.as_deref() == Some(admin_panel::ADMIN_PREVIEW_KEY))
        .map(|t| (t.id, t.conn_identity == identity));
    match existing {
        Some((id, true)) => {
            self.tabs.activate(id);
            cx.notify();
            return;
        }
        Some((id, false)) => self.tabs.close(id),
        None => {}
    }
    let panel = cx.new(|cx| admin_panel::AdminPanel::new(engine, identity.clone(), cx));
    cx.subscribe(&panel, Self::on_admin_event).detach();
    self.tabs.open(ResultTab {
        id: 0,
        title: "Správa serveru".to_string(),
        pinned: false,
        preview_key: Some(admin_panel::ADMIN_PREVIEW_KEY.to_string()),
        conn_identity: identity,
        content: TabContent::Admin { view: panel.clone() },
    });
    self.fetch_admin_catalog_into(panel, admin_sql::roles_catalog(engine), cx);
    cx.notify();
}

fn on_admin_event(
    &mut self,
    panel: Entity<admin_panel::AdminPanel>,
    event: &admin_panel::AdminEvent,
    cx: &mut Context<Self>,
) {
    match event {
        admin_panel::AdminEvent::FetchCatalog { queries } => {
            self.fetch_admin_catalog_into(panel, queries.clone(), cx);
        }
        admin_panel::AdminEvent::RequestApply { statements, warning } => {
            self.open_admin_apply_dialog(panel, statements.clone(), warning.clone(), cx);
        }
    }
}

fn fetch_admin_catalog_into(
    &mut self,
    panel: Entity<admin_panel::AdminPanel>,
    queries: Vec<(&'static str, String)>,
    cx: &mut Context<Self>,
) {
    let Some((spec, _timeout)) = self.apply_conn_spec() else {
        panel.update(cx, |p, cx| p.set_error("Bez připojení — vyberte připojení nahoře.", cx));
        return;
    };
    panel.update(cx, |p, cx| p.set_loading(cx));
    let rx = self.runner.fetch_admin_catalog(spec, queries);
    cx.spawn(async move |_this, cx| {
        let result = rx.await;
        let _ = panel.update(cx, |p, cx| match result {
            Ok(Ok(rows)) => p.apply_catalog(rows, cx),
            Ok(Err(e)) => p.set_error(&e.to_string(), cx),
            Err(_) => p.set_error("dotaz zrušen", cx),
        });
    })
    .detach();
}
```

**Grounding — `ApplyDialogState` generalization** (`main.rs:437/:648`): the dialog becomes target-aware and `WriteStatement`-native, making the display-only choke point structural:

```rust
/// Which staged-edit owner this dialog applies for — drives the
/// success-arm cleanup only; the confirm/dispatch/error mechanics are
/// identical for both (the whole point of the shared write path).
enum ApplyTarget {
    SandboxTab { tab_id: u64, preview_identity: (Option<String>, String) },
    Admin { panel: Entity<admin_panel::AdminPanel> },
}

struct ApplyDialogState {
    target: ApplyTarget,
    statements: Vec<admin_sql::WriteStatement>,
    /// display_sql joined by newline — the ONLY SQL string the modal
    /// renders and record_history receives.
    sql_text: String,
    /// T6's red CASCADE warning line; None elsewhere.
    warning: Option<String>,
    conn_identity: String,
    running: bool,
    error: Option<String>,
    focus_handle: FocusHandle,
}
```

- `on_open_apply_dialog` (`main.rs:2317`, sandbox path): converts `generate_statements`' tuples via `From` at construction, `sql_text` from `display_sql` (identical strings for sandbox), `target: ApplyTarget::SandboxTab { tab_id, preview_identity }`, `warning: None`. T3's call-site `.map(From::from)` moves here and disappears from `on_confirm_apply`.
- New `open_admin_apply_dialog(panel, statements, warning, cx)`: same guards as the sandbox opener (`self.modal`/`discard_confirm`/`apply_dialog` occupancy, `conn_identity_matches(&panel.read(cx).conn_identity, &self.current_conn_identity())` with the same Czech mismatch status), `sql_text` = display join, `target: ApplyTarget::Admin { panel }`, focus handling identical (`window` variant of the listener where needed, mirroring `main.rs:2373`).
- `on_confirm_apply` (`main.rs:2391`): dispatch is unchanged (`self.runner.run_write_transaction(spec, statements, timeout_secs)` — already `Vec<WriteStatement>` now); `record_history` keeps receiving `&sql_text` (display-only — CURATION items 3/4 satisfied at the single choke point); the success arm matches on `target`: `SandboxTab` keeps the existing clear-edits + preview-re-run + history block verbatim (`main.rs:2439-2489`); `Admin` does `panel.update(cx, |p, cx| p.on_apply_success(cx))` (which clears staged sets and emits `FetchCatalog` for the active sub-view) + the same status/history calls.
- `render_apply_dialog_overlay` (`main.rs:2951`): renders `ad.sql_text` as today, plus a red (`rgb(0xf38ba8)`) line above the buttons when `ad.warning` is `Some`.
- `render_tab_content` (`main.rs:2728/2739` arms): new `TabContent::Admin { view } => view.clone().into_any_element()` arm.

**Grounding — "Role a členství" sub-view (design §2):** left `uniform_list` of `RoleRow.name`s; right detail pane shows `RoleRow.detail` pairs, the "Členem v" checkbox list (every known role; checked = `membership_edits.is_checked(role, selected, server_role, currently_member)`; MSSQL shows the server-role list under a second heading), buttons "Nová role…" / "Smazat roli" / "Změnit heslo…", and the Apply bar "{n} změn · Aplikovat · Zahodit" (G5's Czech pattern) where `n = membership_edits.change_count() + staged_role_actions.len()`. "Aplikovat" emits `RequestApply { statements: staged_role_actions ++ membership_edits.to_statements(engine), warning: None }`; "Zahodit" clears both. Sub-view switching with a dirty state sets `discard_confirm: Some(target)` and renders the "Zahodit neuložené změny? / Zpět" prompt instead of switching silently. The two password modals (panel-local overlay, same visual idiom as the grid cell editor):

```rust
enum AdminModal {
    NewRole {
        name: Entity<connections_ui::TextField>,
        password: Entity<connections_ui::TextField>, // TextField::new(cx, "", true) — masked
        login: bool, superuser: bool, createdb: bool, createrole: bool,
    },
    ChangePassword { role: String, password: Entity<connections_ui::TextField> },
    // T6 adds NewSchema/DropSchema here.
}
```

Confirm handlers read the password ONCE into `Zeroizing` (CURATION item 4) and stage the built statements:

```rust
fn confirm_new_role(&mut self, cx: &mut Context<Self>) {
    let Some(AdminModal::NewRole { name, password, login, superuser, createdb, createrole }) = &self.modal else { return };
    let role_name = name.read(cx).text();
    if role_name.trim().is_empty() {
        return;
    }
    // CURATION item 4: modal-local password lives in Zeroizing<String>;
    // it derefs into the builder and is overwritten on drop at the end of
    // this function. Only the staged WriteStatement's exec_sql keeps the
    // value, and that dies with the Vec when the transaction completes.
    let password: zeroize::Zeroizing<String> = zeroize::Zeroizing::new(password.read(cx).text());
    let flags = admin_sql::RoleFlags {
        login: *login, superuser: *superuser, createdb: *createdb, createrole: *createrole,
    };
    self.staged_role_actions.extend(admin_sql::create_role(self.engine, role_name.trim(), &password, &flags));
    self.modal = None;
    cx.notify();
}
```

(`confirm_change_password` is the same shape over `alter_password`; "Smazat roli" stages `drop_role(self.engine, name)` directly, no modal.)

- [ ] **Step 1: Write the failing tests** (`admin_panel.rs` `#[cfg(test)]` — pure model only, per this repo's "no GPUI entity tests" precedent; plus one plain-data `Tabs` test in `main.rs`'s test area and a `flatten` test in `schema_tree.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbc_state::Engine;

    // CURATION item 6's REQUIRED UI-level test: entry point hidden for
    // SQLite, disabled for read-only, enabled otherwise.
    #[test]
    fn admin_entry_state_matrix() {
        assert_eq!(admin_entry_state(Some(Engine::Sqlite), false), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(Some(Engine::Sqlite), true), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(None, false), AdminEntry::Hidden);
        assert_eq!(admin_entry_state(Some(Engine::Postgres), true), AdminEntry::Disabled);
        assert_eq!(admin_entry_state(Some(Engine::Mssql), true), AdminEntry::Disabled);
        assert_eq!(admin_entry_state(Some(Engine::Postgres), false), AdminEntry::Enabled);
        assert_eq!(admin_entry_state(Some(Engine::Mssql), false), AdminEntry::Enabled);
    }

    fn rows(cols: &[&str], data: &[&[Option<&str>]]) -> crate::runner::AdminCatalogRows {
        (
            cols.iter().map(|c| c.to_string()).collect(),
            data.iter()
                .map(|r| r.iter().map(|c| c.map(|s| s.to_string())).collect())
                .collect(),
        )
    }

    #[test]
    fn parse_roles_first_col_is_name_rest_is_detail() {
        let r = rows(
            &["rolname", "rolsuper", "rolcanlogin"],
            &[&[Some("alice"), Some("true"), Some("false")], &[Some("bob"), None, Some("true")]],
        );
        let roles = parse_roles(&r);
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].name, "alice");
        assert_eq!(roles[0].detail, vec![
            ("rolsuper".to_string(), "true".to_string()),
            ("rolcanlogin".to_string(), "false".to_string()),
        ]);
        assert_eq!(roles[1].detail[0], ("rolsuper".to_string(), "—".to_string()));
    }

    #[test]
    fn parse_memberships_reads_role_member_pairs() {
        let r = rows(
            &["role", "member", "admin_option"],
            &[&[Some("readers"), Some("bob"), Some("false")]],
        );
        let m = parse_memberships(&r, false);
        assert_eq!(m, vec![Membership { role: "readers".into(), member: "bob".into(), server_role: false }]);
        let s = parse_memberships(&r, true);
        assert!(s[0].server_role);
    }

    #[test]
    fn membership_toggle_stage_unstage_and_statements() {
        let mut e = MembershipEdits::default();
        // Not a member → first toggle stages an add, second unstages.
        e.toggle("readers", "bob", false, false);
        assert!(e.is_checked("readers", "bob", false, false));
        assert_eq!(e.change_count(), 1);
        e.toggle("readers", "bob", false, false);
        assert!(!e.is_checked("readers", "bob", false, false));
        assert_eq!(e.change_count(), 0);
        // A member → toggle stages a removal.
        e.toggle("writers", "bob", false, true);
        assert!(!e.is_checked("writers", "bob", false, true));
        e.toggle("readers", "bob", false, false);
        assert_eq!(e.change_count(), 2);
        assert!(e.is_dirty());

        let stmts = e.to_statements(Engine::Postgres);
        let sql: Vec<&str> = stmts.iter().map(|s| s.exec_sql.as_str()).collect();
        assert_eq!(sql, vec!["GRANT \"readers\" TO \"bob\"", "REVOKE \"writers\" FROM \"bob\""]);

        let ms = e.to_statements(Engine::Mssql);
        let sql: Vec<&str> = ms.iter().map(|s| s.exec_sql.as_str()).collect();
        assert_eq!(sql, vec!["ALTER ROLE [readers] ADD MEMBER [bob]", "ALTER ROLE [writers] DROP MEMBER [bob]"]);

        e.clear();
        assert!(!e.is_dirty());
    }
}
```

`schema_tree.rs` (extend its existing pure-`flatten` test module):

```rust
#[test]
fn admin_root_renders_first_when_not_hidden_and_never_when_hidden() {
    let snapshot = SchemaSnapshot::default();
    let expanded = HashSet::new();
    let out = flatten(&snapshot, &expanded, "", &[], None, AdminEntry::Enabled);
    assert_eq!(out.first().map(|(id, depth, label, _)| (id.clone(), *depth, label.clone())),
        Some((NodeId::AdminRoot, 0, "Správa serveru".to_string())));
    let out = flatten(&snapshot, &expanded, "", &[], None, AdminEntry::Disabled);
    assert!(matches!(out.first(), Some((NodeId::AdminRoot, ..))));
    let out = flatten(&snapshot, &expanded, "", &[], None, AdminEntry::Hidden);
    assert!(out.iter().all(|(id, ..)| *id != NodeId::AdminRoot));
}
```

`main.rs` (plain-data `Tabs` decision test, new `#[cfg(test)] mod admin_open_tests` beside `conn_identity_matches_tests` at `main.rs:3669` — extract the find/decide step of `open_admin_tab` as a pure helper so it's testable):

```rust
/// Pure: what open_admin_tab should do given the current tab set — reuse
/// (activate), replace (stale connection), or open fresh.
#[derive(Debug, PartialEq, Eq)]
enum AdminOpenDecision { Activate(u64), Replace(u64), OpenFresh }

fn admin_open_decision(tabs: &Tabs, current_identity: &str) -> AdminOpenDecision {
    match tabs
        .iter()
        .find(|t| t.preview_key.as_deref() == Some(admin_panel::ADMIN_PREVIEW_KEY))
        .map(|t| (t.id, t.conn_identity == current_identity))
    {
        Some((id, true)) => AdminOpenDecision::Activate(id),
        Some((id, false)) => AdminOpenDecision::Replace(id),
        None => AdminOpenDecision::OpenFresh,
    }
}

#[cfg(test)]
mod admin_open_tests {
    use super::*;

    fn admin_tab(identity: &str) -> ResultTab {
        ResultTab {
            id: 0,
            title: "Správa serveru".into(),
            pinned: false,
            preview_key: Some(admin_panel::ADMIN_PREVIEW_KEY.to_string()),
            conn_identity: identity.to_string(),
            content: TabContent::Text { text: String::new(), scroll_lines: 0 },
        }
    }

    #[test]
    fn admin_tab_is_singleton_per_connection() {
        let mut tabs = Tabs::new();
        assert_eq!(admin_open_decision(&tabs, "conn-a"), AdminOpenDecision::OpenFresh);
        let id = tabs.open(admin_tab("conn-a"));
        assert_eq!(admin_open_decision(&tabs, "conn-a"), AdminOpenDecision::Activate(id));
        assert_eq!(admin_open_decision(&tabs, "conn-b"), AdminOpenDecision::Replace(id));
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui admin_panel:: admin_open_tests::`
Expected: compile errors (module/types don't exist).

- [ ] **Step 3: Implement the pure model** (`admin_entry_state`, `parse_roles`, `parse_memberships`, `MembershipEdits`, `AdminOpenDecision`/`admin_open_decision`, the `flatten` param + `NodeId::AdminRoot` + `TreeEvent::OpenAdmin`) and run the Step-1 tests to green before touching any render code.

- [ ] **Step 4: Implement the GPUI glue**: `TabContent::Admin`, `AdminPanel` entity + `Render` (sub-nav mini-tabs on the left — plain clickable divs, NOT the result-tab strip; roles `uniform_list`; detail pane; checkbox rows reusing the grid's diff-tint yellow `rgb(0xf9e2af)` for staged rows; Apply bar; `AdminModal` overlays with masked `TextField` and the `Zeroizing` confirm handlers shown above; `discard_confirm` prompt), `main.rs`'s `open_admin_tab`/`on_admin_event`/`fetch_admin_catalog_into`/`admin_entry_meta` + the `ApplyDialogState` generalization (target enum, `warning` line, success-arm match) + `render_tab_content` arm + palette arm + `set_admin_entry` calls on connection switch, `palette.rs`'s action + gated `fixed_actions` (update its call site and tests for the new parameter), and `zeroize.workspace = true` in `crates/dbc-ui/Cargo.toml`. Czech labels: "Správa serveru", "Role a členství", "Oprávnění", "Databáze a schémata", "Nová role…", "Smazat roli", "Změnit heslo…", "Členem v", "{n} změn", "Aplikovat", "Zahodit", "Zahodit neuložené změny?", "Zpět", "pouze pro čtení".

- [ ] **Step 5: Run to green + sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass, zero warnings. Manual launch against docker pg: tree shows "Správa serveru" on top; opening it twice re-focuses one tab; roles list populates; toggling a membership checkbox stages "{1} změn"; "Nová role…" + Apply shows the confirm modal with `'***'` in the SQL and the history panel entry afterwards contains `'***'`. Against the SQLite fixture: no entry anywhere. With a read-only pg connection: greyed tree row, no palette row.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/admin_panel.rs crates/dbc-ui/src/main.rs crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/palette.rs crates/dbc-ui/Cargo.toml
git commit -m "feat: server admin panel shell + roles/membership sub-view"
```

---

### Task 5 (T5): "Oprávnění" — privileges matrix sub-view

**Files:**
- Modify: `crates/dbc-ui/src/admin_panel.rs`
- Modify: `crates/dbc-ui/src/main.rs` (only if the schema list handoff needs a new panel setter — see Grounding)

**Interfaces:**
- Consumes: `admin_sql::{privileges_catalog, object_privilege, schema_privilege, database_privilege_pg, cycle_cell, CellState, PG_TABLE_PRIVS, MSSQL_TABLE_PRIVS, SCHEMA_PRIVS, PG_DATABASE_PRIVS, WriteStatement}` (T1/T2), T4's shell (`AdminPanel`, `AdminEvent::{FetchCatalog, RequestApply}`, discard-confirm, Apply bar idiom).
- Produces (pure, unit-tested):
  ```rust
  /// One (schema, grantee) scope's matrix — committed state parsed from
  /// §1's ACL rows, staged state as a diff map (only cells that differ).
  #[derive(Default)]
  pub struct MatrixState {
      pub objects: Vec<String>,
      pub current: std::collections::HashMap<(String, String), CellState>,  // (object, privilege)
      pub staged: std::collections::HashMap<(String, String), CellState>,
      pub schema_current: std::collections::HashMap<String, CellState>,     // privilege → state
      pub schema_staged: std::collections::HashMap<String, CellState>,
      pub db_current: std::collections::HashMap<String, CellState>,         // pg only
      pub db_staged: std::collections::HashMap<String, CellState>,
  }
  impl MatrixState {
      /// Postgres rows (object_acl/schema_acl/db_acl): grantee-filtered,
      /// privilege_type → Granted. MSSQL rows (object_perms/schema_perms):
      /// state_desc GRANT/GRANT_WITH_GRANT_OPTION → Granted, DENY → Denied.
      /// `objects` = every distinct object in the object rows (aclexplode
      /// over acldefault yields owner rows for EVERY object, so unlisted
      /// objects still appear).
      pub fn from_catalog(engine: Engine, grantee: &str, labeled: &[(&'static str, AdminCatalogRows)]) -> MatrixState;
      pub fn effective(&self, object: &str, privilege: &str) -> CellState;
      /// Cycle one cell via admin_sql::cycle_cell; staging back to the
      /// committed state REMOVES the diff entry (yellow tint clears).
      pub fn click_cell(&mut self, engine: Engine, object: &str, privilege: &str);
      pub fn click_schema_cell(&mut self, engine: Engine, privilege: &str);
      pub fn click_db_cell(&mut self, privilege: &str); // pg only — bi-state by construction
      pub fn change_count(&self) -> usize;
      pub fn is_dirty(&self) -> bool;
      pub fn clear(&mut self);
      /// Groups staged object cells by (object, target state) so multi-priv
      /// changes emit "GRANT SELECT, INSERT ON …" (design §3's table), in
      /// deterministic (object, privilege-column) order; schema/db cells one
      /// statement each. Err bubbles admin_sql's own refusals (unreachable
      /// via the UI cycles; errors-are-values backstop).
      pub fn to_statements(&self, engine: Engine, schema: &str, grantee: &str, database: &str) -> Result<Vec<WriteStatement>, String>;
  }
  ```

**Grounding:** scope selectors — schema dropdown fed from the schema list `AppView` already holds in its `SchemaSnapshot` (T4's `open_admin_tab` passes the distinct schema list into `AdminPanel::new`; a `set_schemas(Vec<String>, cx)` setter refreshes it when the snapshot refreshes), grantee dropdown fed from T4's parsed `roles`. Selecting both emits `AdminEvent::FetchCatalog { queries: admin_sql::privileges_catalog(engine, &schema) }`; `apply_catalog` routes the `object_acl`/`schema_acl`/`db_acl` (pg) or `object_perms`/`schema_perms` (MSSQL) labels into `MatrixState::from_catalog`. Columns are `PG_TABLE_PRIVS`/`MSSQL_TABLE_PRIVS` per engine; the fixed row above the grid holds `SCHEMA_PRIVS` and (pg only) `PG_DATABASE_PRIVS` checkboxes (design §2). Changed cells tint yellow (`rgb(0xf9e2af)`, the grid's diff-tint convention). Changing schema/grantee with `is_dirty()` goes through T4's `discard_confirm` prompt. A failed privileges fetch shows the error in the sub-view (CURATION item 5 — this is the surface where a PG < 10 `acldefault` error lands, by design). Cell glyphs: `✓` Granted, `✗` Denied (MSSQL only), empty NotSet.

- [ ] **Step 1: Write the failing tests** (`admin_panel.rs`, `#[cfg(test)] mod matrix_tests`):

```rust
#[cfg(test)]
mod matrix_tests {
    use super::*;
    use crate::admin_sql::CellState;
    use dbc_state::Engine;

    fn pg_catalog(grantee: &str) -> Vec<(&'static str, crate::runner::AdminCatalogRows)> {
        let object_acl = (
            vec!["schema".into(), "object".into(), "kind".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
            vec![
                vec![Some("public".into()), Some("users".into()), Some("table".into()), Some(grantee.into()), Some("SELECT".into()), Some("false".into())],
                vec![Some("public".into()), Some("orders".into()), Some("table".into()), Some("owner".into()), Some("SELECT".into()), Some("true".into())],
            ],
        );
        let schema_acl = (
            vec!["schema".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
            vec![vec![Some("public".into()), Some(grantee.into()), Some("USAGE".into()), Some("false".into())]],
        );
        let db_acl = (
            vec!["database".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
            vec![vec![Some("appdb".into()), Some(grantee.into()), Some("CONNECT".into()), Some("false".into())]],
        );
        vec![("object_acl", object_acl), ("schema_acl", schema_acl), ("db_acl", db_acl)]
    }

    #[test]
    fn from_catalog_filters_grantee_and_lists_every_object() {
        let m = MatrixState::from_catalog(Engine::Postgres, "bob", &pg_catalog("bob"));
        // orders has only an owner row, but still appears as a matrix row.
        assert_eq!(m.objects, vec!["orders".to_string(), "users".to_string()]);
        assert_eq!(m.effective("users", "SELECT"), CellState::Granted);
        assert_eq!(m.effective("orders", "SELECT"), CellState::NotSet);
        assert_eq!(m.schema_current.get("USAGE"), Some(&CellState::Granted));
        assert_eq!(m.db_current.get("CONNECT"), Some(&CellState::Granted));
    }

    #[test]
    fn mssql_state_desc_maps_deny() {
        let object_perms = (
            vec!["schema_name".into(), "object_name".into(), "grantee".into(), "permission_name".into(), "state_desc".into()],
            vec![
                vec![Some("dbo".into()), Some("users".into()), Some("bob".into()), Some("SELECT".into()), Some("DENY".into())],
                vec![Some("dbo".into()), Some("users".into()), Some("bob".into()), Some("INSERT".into()), Some("GRANT_WITH_GRANT_OPTION".into())],
            ],
        );
        let schema_perms = (
            vec!["schema_name".into(), "grantee".into(), "permission_name".into(), "state_desc".into()],
            vec![],
        );
        let m = MatrixState::from_catalog(Engine::Mssql, "bob", &[("object_perms", object_perms), ("schema_perms", schema_perms)]);
        assert_eq!(m.effective("users", "SELECT"), CellState::Denied);
        assert_eq!(m.effective("users", "INSERT"), CellState::Granted);
    }

    #[test]
    fn click_cycles_and_reverting_clears_the_stage() {
        let mut m = MatrixState::from_catalog(Engine::Postgres, "bob", &pg_catalog("bob"));
        m.click_cell(Engine::Postgres, "orders", "SELECT"); // NotSet -> Granted
        assert_eq!(m.effective("orders", "SELECT"), CellState::Granted);
        assert_eq!(m.change_count(), 1);
        m.click_cell(Engine::Postgres, "orders", "SELECT"); // Granted -> NotSet == committed
        assert_eq!(m.change_count(), 0);
        assert!(!m.is_dirty());
        // pg bi-state: no click sequence ever reaches Denied.
        for _ in 0..6 {
            m.click_cell(Engine::Postgres, "users", "SELECT");
            assert_ne!(m.effective("users", "SELECT"), CellState::Denied);
        }
    }

    #[test]
    fn to_statements_groups_same_object_same_target() {
        let mut m = MatrixState::from_catalog(Engine::Postgres, "bob", &pg_catalog("bob"));
        m.click_cell(Engine::Postgres, "orders", "SELECT");  // grant
        m.click_cell(Engine::Postgres, "orders", "INSERT");  // grant
        m.click_cell(Engine::Postgres, "users", "SELECT");   // revoke (was granted)
        m.click_schema_cell(Engine::Postgres, "CREATE");     // grant
        m.click_db_cell("TEMP");                              // grant
        let stmts = m.to_statements(Engine::Postgres, "public", "bob", "appdb").unwrap();
        let sql: Vec<&str> = stmts.iter().map(|s| s.exec_sql.as_str()).collect();
        assert_eq!(sql, vec![
            "GRANT SELECT, INSERT ON \"public\".\"orders\" TO \"bob\"",
            "REVOKE SELECT ON \"public\".\"users\" FROM \"bob\"",
            "GRANT CREATE ON SCHEMA \"public\" TO \"bob\"",
            "GRANT TEMP ON DATABASE \"appdb\" TO \"bob\"",
        ]);
    }

    #[test]
    fn to_statements_mssql_emits_deny() {
        let m0 = (
            vec!["schema_name".into(), "object_name".into(), "grantee".into(), "permission_name".into(), "state_desc".into()],
            vec![vec![Some("dbo".into()), Some("users".into()), Some("bob".into()), Some("SELECT".into()), Some("GRANT".into())]],
        );
        let s0 = (vec!["schema_name".into(), "grantee".into(), "permission_name".into(), "state_desc".into()], vec![]);
        let mut m = MatrixState::from_catalog(Engine::Mssql, "bob", &[("object_perms", m0), ("schema_perms", s0)]);
        m.click_cell(Engine::Mssql, "users", "SELECT"); // Granted -> Denied
        let stmts = m.to_statements(Engine::Mssql, "dbo", "bob", "").unwrap();
        assert_eq!(stmts[0].exec_sql, "DENY SELECT ON [dbo].[users] TO [bob]");
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui matrix_tests::`
Expected: compile error (`MatrixState` doesn't exist).

- [ ] **Step 3: Implement** `MatrixState` (pure) to green, then the sub-view render: selectors (schema from `set_schemas`, grantee from `roles`; both plain dropdown overlays, dirty-guarded), header row of privilege columns per engine, the fixed schema/db checkbox row, the object grid (`uniform_list` rows; cells clickable, staged cells tinted `rgb(0xf9e2af)`), the Apply bar wired to `RequestApply { statements: matrix.to_statements(…)?, warning: None }` (an `Err` — unreachable via UI — lands in the panel's `error` line rather than panicking), and `on_apply_success` re-emitting the privileges fetch for the current scope.

- [ ] **Step 4: Run to green + sanity launch**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass, zero warnings. Manual: against docker pg, pick `public` + a role, toggle a SELECT cell, Apply → confirm modal shows the exact GRANT, re-fetch shows it committed; REVOKE round-trips the same way.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/admin_panel.rs crates/dbc-ui/src/main.rs
git commit -m "feat: privileges matrix sub-view (engine-aware GRANT/REVOKE/DENY)"
```

---

### Task 6 (T6): "Databáze a schémata" sub-view + version bump

**Files:**
- Modify: `crates/dbc-ui/src/admin_panel.rs`
- Modify: `crates/dbc-ui/Cargo.toml` (version `0.10.0` — see Global Constraints' don't-skip caveat)

**Interfaces:**
- Consumes: `admin_sql::{sizes_catalog, create_schema, drop_schema}` (T1/T2), T4's shell (`AdminModal`, `AdminEvent::RequestApply`'s `warning` field, `FetchCatalog`).
- Produces (pure, unit-tested):
  ```rust
  /// "1.2 GB" / "340.5 MB" / "512 B" — binary units, one decimal above B.
  pub fn format_bytes(bytes: u64) -> String;
  /// Bar width fraction in [0,1]; 0 when max is 0.
  pub fn bar_fraction(bytes: u64, max: u64) -> f32;
  /// pg "databases" rows → (datname, Some(bytes)); MSSQL "databases" rows
  /// have NO per-db size (sys.databases carries none) → (name, None).
  pub fn parse_db_sizes(engine: Engine, rows: &AdminCatalogRows) -> Vec<(String, Option<u64>)>;
  /// pg "schema_sizes" (schema, bytes); MSSQL (schema, reserved_kb → bytes).
  pub fn parse_schema_sizes(engine: Engine, rows: &AdminCatalogRows) -> Vec<(String, u64)>;
  ```

**Grounding:** read-only lists per design §2 — databases with size bars (bar width = `bar_fraction` against the max in the list; MSSQL's `None` sizes render the name with "—" and no bar), and for the current database a schema list with per-schema bars; `current_db_size` renders as a headline line ("Aktuální databáze: {pretty}"). Sub-view activation emits `FetchCatalog { queries: admin_sql::sizes_catalog(engine) }`. The two mutations are direct-to-confirm (no local staging — each opens the Apply confirm dialog with its one statement, matching "one transaction per user-visible action"): "Nové schéma…" adds `AdminModal::NewSchema { name: Entity<TextField> }` whose confirm emits `RequestApply { statements: admin_sql::create_schema(engine, name.trim()), warning: None }`; "Smazat schéma" on the selected schema row, with the checkbox "včetně CASCADE (smaže i obsah schématu)" (default unchecked), emits `RequestApply { statements: admin_sql::drop_schema(engine, &schema, cascade), warning: cascade.then(|| "tato akce je nevratná a smaže i obsah schématu".to_string()) }` — T4's dialog renders that line red. Unchecked plain `DROP SCHEMA` failing on a non-empty schema surfaces the engine's own error in the dialog (design §2/§6 "let the server say no"). `CREATE DATABASE`/`DROP DATABASE` have NO UI (design §3's transaction-block landmine — not silently reintroducible). On Apply success `on_apply_success` re-emits the sizes fetch.

- [ ] **Step 1: Write the failing tests** (`admin_panel.rs`, `#[cfg(test)] mod sizes_tests`):

```rust
#[cfg(test)]
mod sizes_tests {
    use super::*;
    use dbc_state::Engine;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn bar_fraction_clamps_and_handles_zero_max() {
        assert_eq!(bar_fraction(0, 0), 0.0);
        assert_eq!(bar_fraction(50, 100), 0.5);
        assert_eq!(bar_fraction(100, 100), 1.0);
    }

    fn rows(cols: &[&str], data: &[&[Option<&str>]]) -> crate::runner::AdminCatalogRows {
        (
            cols.iter().map(|c| c.to_string()).collect(),
            data.iter().map(|r| r.iter().map(|c| c.map(|s| s.to_string())).collect()).collect(),
        )
    }

    #[test]
    fn parse_db_sizes_pg_has_bytes_mssql_has_none() {
        let pg = rows(&["datname", "bytes"], &[&[Some("appdb"), Some("1048576")]]);
        assert_eq!(parse_db_sizes(Engine::Postgres, &pg), vec![("appdb".to_string(), Some(1_048_576))]);
        let ms = rows(
            &["name", "database_id", "create_date", "state_desc"],
            &[&[Some("appdb"), Some("5"), Some("2026-01-01"), Some("ONLINE")]],
        );
        assert_eq!(parse_db_sizes(Engine::Mssql, &ms), vec![("appdb".to_string(), None)]);
    }

    #[test]
    fn parse_schema_sizes_pg_bytes_mssql_kb() {
        let pg = rows(&["schema", "bytes"], &[&[Some("public"), Some("2048")], &[Some("empty"), None]]);
        // NULL SUM (schema with no tables) → 0, not a crash.
        assert_eq!(parse_schema_sizes(Engine::Postgres, &pg), vec![("public".to_string(), 2048), ("empty".to_string(), 0)]);
        let ms = rows(&["schema_name", "reserved_kb", "used_kb"], &[&[Some("dbo"), Some("16"), Some("8")]]);
        assert_eq!(parse_schema_sizes(Engine::Mssql, &ms), vec![("dbo".to_string(), 16 * 1024)]);
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui sizes_tests::`
Expected: compile error (functions don't exist).

- [ ] **Step 3: Implement** the four pure functions to green, then the sub-view render (headline size, two bar lists, the `NewSchema`/`DropSchema` modal variants + cascade checkbox + `RequestApply` emissions per the Grounding), and the sizes re-fetch on activation/success.

- [ ] **Step 4: Version bump**

`crates/dbc-ui/Cargo.toml`: `version = "0.10.0"` — first confirming the intervening phases' bumps have landed on the integration branch (Global Constraints).

- [ ] **Step 5: Run to green + full sanity pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui` and `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui`
Expected: all pass, zero warnings. Manual against docker pg: sizes render with bars; "Nové schéma…" → confirm modal → schema appears in the list AND the schema tree after refresh; "Smazat schéma" without CASCADE on a non-empty schema shows the engine's error in the dialog; with CASCADE checked the dialog shows the red warning line and succeeds.

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/admin_panel.rs crates/dbc-ui/Cargo.toml
git commit -m "feat: databases & schemas sub-view with sizes + schema DDL"
```

---

## Self-Review Notes

**Spec coverage** (design doc sections → tasks):
- §0 write-path amendment (WriteStatement widening, `From` tuple impl, single choke point): T2 (struct + `From`) + T3 (runner widening, G5 call-site one-liner, display-only error pairing) + T4 (dialog generalization makes `sql_text`/`record_history` display-only structurally). The G12 §3-novela wording supersedes §0's phrasing — copied into Global Constraints.
- §1 catalog reads: T1 (all queries, both engines, escaped interpolation, SQLite exemption at the builder level) + T3 (`fetch_admin_catalog` read path, first-error abort) + T4/T5/T6 (label routing). CURATION item 5 (acldefault hard PG ≥ 10, no fallback): T1 keeps `acldefault` unconditionally + asserts it; T3's abort-on-error + T5's error surface implement "the sub-view shows the error".
- §2 UI: entry point (tree pinned row above Favourites, palette action, sqlite hidden / read-only disabled) → T4 (`admin_entry_state` + `flatten`/`fixed_actions` gating, the CURATION-6 UI-level test); tab plumbing (`TabContent::Admin`, `"__admin__"` `preview_key` singleton, re-focus semantics + stale-connection replace) → T4 (`admin_open_decision` test); sub-nav + dirty-switch prompt → T4; Role a členství → T4; Oprávnění (engine-aware bi/tri-state, yellow tint, schema/db row) → T5; Databáze a schémata (sizes, create/drop schema, CASCADE opt-in + red warning, no database DDL) → T6; non-goals list honored (nothing in any task builds column-level grants, ALTER DEFAULT PRIVILEGES, CREATE/DROP DATABASE, etc.).
- §3 mutation flows: statement type + redaction mechanism (parallel construction) → T2; batching-in-one-transaction → T4/T5 Apply bars → the unchanged `run_write_transaction`; per-action SQL tables (both engines) → T2's builders + exact-string tests; CREATE/DROP DATABASE exclusion → no UI (T6 Grounding) ; MSSQL CREATE SCHEMA batch rule → already satisfied (one `execute()` per statement — noted in T3's doc-comment update).
- §4 engine abstraction: `admin_sql.rs` pure module, `quote_ident_for` bracket-form MSSQL (CURATION item 2), zero `dbc-core` changes → T1/T2.
- §5 task decomposition → T1-T6 as refined here; cross-cutting docker-pg `#[ignore]` roundtrip with redaction assertion → T3 Step 4.
- §6 risks: acldefault verification → T3's abort test + the docker test path + T5 error surface; MSSQL untestable → string-unit-tests only throughout (no task wires an MSSQL connection); DENY divergence → T2's `cycle_cell`/`object_privilege` refusals + T5's bi-state test; redaction review flag → T2's parallel construction + T3's pairing test; no pre-flight dependency checks → engine errors surface verbatim in the dialog (T3's error pairing, T4/T6 Grounding).
- CURATION items: (1) Global Constraints (G12 wording); (2) T1/T2 MSSQL bracket quoting + string-only tests, no driver wiring; (3) T3's `statement_failure_pairs_display_sql_never_exec_sql`; (4) T4's `Zeroizing` confirm handlers + `staged_role_actions` dying with the dialog's statement Vec; (5) T1 asserts acldefault stays, T3 aborts, T5 surfaces; (6) T3's guard-level admin refusal test + T4's `admin_entry_state_matrix`.

**Placeholder scan:** every step shows real code (full test modules, key implementation bodies) or a concrete cargo command. T4's panel `Render` tree, T5's grid render, and T6's bar render are described by contract (fields, labels, colors, event emissions) rather than full GPUI trees — the same precedent the G5/G6 plans set for `render_cell_editor_overlay`/the values dialog; every piece of logic those renders call is specified and tested. The "remaining builders" sentence in T2 Step 3 is bounded by the exact-string tests in T2 Step 1, which pin every output byte.

**Type-name consistency across tasks:** `admin_sql::WriteStatement { exec_sql, display_sql, expected_affected }` (T2) matches T3's runner signatures/loop, T4's `ApplyDialogState.statements`/`staged_role_actions`/`MembershipEdits::to_statements`, T5's `MatrixState::to_statements`, T6's `RequestApply` payloads. `AdminCatalogRows = (Vec<String>, Vec<Vec<Option<String>>>)` (T3) matches T4's `parse_roles`/`parse_memberships`, T5's `from_catalog`, T6's parsers. `CellState`/`cycle_cell`/`object_privilege`/`schema_privilege`/`database_privilege_pg` (T2) match T5's usage exactly (including the `Result` bubbling). `AdminEntry`/`admin_entry_state`/`ADMIN_PREVIEW_KEY`/`AdminEvent::{FetchCatalog, RequestApply}` (T4) match the tree/palette/main wiring and T5/T6's emissions. `roles_catalog`/`privileges_catalog`/`sizes_catalog` labels (T1) match the `apply_catalog` routing named in T4/T5/T6.

**Resolved design ambiguities (flagged for controller review, not vetoed unilaterally):**
1. **`WriteStatement`'s home is `admin_sql.rs` (T2), not `runner.rs`.** Design §5 lists it under both T2 and T3; defining it in the pure module keeps T2 self-contained/parallelizable and makes T3's runner edit purely mechanical. `runner.rs` importing a crate-local pure module is no layering change (it already imports `crate::connect`); its "decoupled from sandbox's types" doc note is updated, not violated — there is still zero `EditState`/GPUI coupling.
2. **Statement-failure errors now carry `" — příkaz: {display_sql}"` context for ALL callers**, including G5 sandbox applies (where display == exec). CURATION item 3 demands errors be *paired with display_sql*; doing it unconditionally at the one choke point is simpler and safer than a per-statement conditional, and G5's existing error-text assertions still pass. Slight sandbox error-surface change, flagged.
3. **Singleton semantics:** design says re-opening "re-focuses the existing tab exactly like re-opening a table preview does" — but table previews actually close-and-replace (`close_by_preview_key`). Replacing would destroy staged admin edits, so this plan re-focuses for the SAME connection and close-and-replaces only when the existing admin tab belongs to a DIFFERENT connection (stale staged edits must not survive a connection switch — same rationale as G5's BLOCKER-1 `conn_identity` guard).
4. **Read-only/disabled palette row is omitted, not greyed** — the palette has no disabled-row idiom; the tree row (greyed + "pouze pro čtení") is where the disabled state is visible, and `open_admin_tab` re-checks defensively either way.
5. **MSSQL schema-privilege names follow the design's own table (`GRANT USAGE ON SCHEMA::[s]`)** even though `USAGE` is doubtful T-SQL — MSSQL is documentation-verified-only this phase (§6/CURATION item 2); the builder is generic over `priv_name`, so correcting the fixed set at MSSQL wiring time is a constant change, not a builder change. Flagged.
6. **T6's schema mutations skip local staging** (direct-to-confirm, one statement per action) where T4/T5 batch — matches design §2's phrasing ("name prompt → CREATE SCHEMA") and "one transaction per user-visible action"; a one-item batch through the same dialog is still the same write path.
7. **Admin `expected_affected` is always `None`** — the design's tables never specify expectations for DDL/DCL and drivers report engine-defined counts for them; `affected_mismatch(None, _)` never fires, so the optimistic check remains a sandbox-only concern.

## Task ordering

- **Parallel batch (worktree-friendly):** T1 and T2 — the pure `admin_sql.rs` halves. They touch no shared function: T1 creates the module (helpers + catalog builders, plus the one-line `mod admin_sql;` in `main.rs`); T2 appends the `WriteStatement`/mutation half with its own test module. Two worktrees merge append-only; a single worker just runs T1 → T2 back-to-back. Nothing else in the repo is touched, so this batch is also parallel with any unrelated in-flight phase work.
- **Serial chain:** T3 → T4 → T5 → T6. T3 depends on T2's `WriteStatement` and rewrites `runner.rs` + one `main.rs` line; T4 depends on T1-T3 and makes the big `main.rs`/`tabs.rs`/`schema_tree.rs`/`palette.rs` edits; T5 and T6 both extend `admin_panel.rs` (and lightly touch `main.rs`), so although the design marks them logically parallel (T5 ∥ T6), they conflict textually on the same files — serialize them (same author, or rebase one onto the other), exactly like the G6 plan's T3/T7 `main.rs` caveat.
- Everything after T2 serializes on `runner.rs`/`main.rs` — do not run T3-T6 concurrently with each other or with any other phase's `main.rs` work without an explicit rebase plan.
