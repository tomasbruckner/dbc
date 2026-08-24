//! G10: pure admin SQL generation — catalog SELECT builders and (T2)
//! mutation WriteStatement builders. No GPUI, no I/O — same discipline as
//! sandbox.rs: the dialog/panel shows these strings verbatim, so every
//! builder is unit-tested for exact output.
//!
//! Allow dead_code at module scope: T1/T2 land ahead of T3-T6's consumers
//! (runner.rs's `fetch_admin_catalog`/`run_write_transaction` widening,
//! admin_panel.rs's staging) — every item here is unit-tested but not yet
//! called from `main`. Remove this allow once T3+ wire it in.
#![allow(dead_code)]

use dbc_state::Engine;

/// DEPRECATED-IN-COMMENT (G15 §2a): delegating wrapper over
/// `dbc_core::quote_ident_d` — dbc-core is the single bracket authority now;
/// remove this pair once all admin call sites take a `Dialect` directly.
/// Tests below stay as the contract proving delegation preserved behavior.
pub fn quote_ident_for(engine: Engine, name: &str) -> String {
    dbc_core::quote_ident_d(crate::sql_dialect(engine), name)
}

/// `schema.object`, both parts through quote_ident_for.
pub fn quote_qualified_for(engine: Engine, schema: &str, object: &str) -> String {
    format!(
        "{}.{}",
        quote_ident_for(engine, schema),
        quote_ident_for(engine, object)
    )
}

/// Single-quoted SQL string literal, `'` doubled — the same escaping
/// sandbox::sql_value applies on its quoted path, extracted here because
/// catalog filters and passwords need the literal WITHOUT the numeric
/// bare-path heuristic.
///
/// KNOWN GAP (flagged by the batch C review, not fixed here): dialect-
/// agnostic plain `'...'` — MSSQL admin statements built from this (e.g.
/// `ALTER ROLE ... PASSWORD = '...'`, catalog filter literals) don't get
/// the `N''` prefix `sandbox::sql_value_d`/`csv_import::generate_insert_batches_d`
/// use for MSSQL text. Admin SQL against MSSQL isn't live yet (gated until
/// T8) so this is inert today; T8's live admin validation must either
/// thread a dialect through here (an `_d` sibling, same pattern as every
/// other dialect-aware helper in this codebase) or confirm via the matrix
/// that plain `'...'` is acceptable for every caller (unlikely for
/// non-ASCII text under a non-UTF8 collation — the same class of bug batch
/// C's CSV import fix closed).
pub fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Labeled catalog SELECTs (label, sql) for the "Role a členství"
/// sub-view. Postgres: [("roles", …), ("membership", …)]. MSSQL:
/// [("server_principals", …), ("db_principals", …),
/// ("db_role_members", …), ("server_role_members", …)].
/// SQLite: empty (feature-exempt, defensive).
pub fn roles_catalog(engine: Engine) -> Vec<(&'static str, String)> {
    match engine {
        Engine::Postgres => vec![
            ("roles", "SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, \
                 rolcanlogin, rolreplication, rolconnlimit, rolvaliduntil, rolbypassrls \
                 FROM pg_catalog.pg_roles ORDER BY rolname".to_string()),
            ("membership", "SELECT g.rolname AS role, m.rolname AS member, am.admin_option \
                 FROM pg_catalog.pg_auth_members am \
                 JOIN pg_catalog.pg_roles g ON g.oid = am.roleid \
                 JOIN pg_catalog.pg_roles m ON m.oid = am.member \
                 ORDER BY g.rolname, m.rolname".to_string()),
        ],
        Engine::Mssql => vec![
            ("server_principals", "SELECT name, type_desc, is_disabled, create_date, modify_date, default_database_name \
                 FROM sys.server_principals WHERE type IN ('S','U','G') ORDER BY name".to_string()),
            ("db_principals", "SELECT name, type_desc, default_schema_name, create_date, is_fixed_role \
                 FROM sys.database_principals \
                 WHERE type IN ('S','U','G','R') AND name NOT IN ('public','guest','INFORMATION_SCHEMA','sys') \
                 ORDER BY name".to_string()),
            ("db_role_members", "SELECT rl.name AS role, mp.name AS member \
                 FROM sys.database_role_members drm \
                 JOIN sys.database_principals rl ON rl.principal_id = drm.role_principal_id \
                 JOIN sys.database_principals mp ON mp.principal_id = drm.member_principal_id \
                 ORDER BY rl.name, mp.name".to_string()),
            ("server_role_members", "SELECT rl.name AS role, mp.name AS member \
                 FROM sys.server_role_members srm \
                 JOIN sys.server_principals rl ON rl.principal_id = srm.role_principal_id \
                 JOIN sys.server_principals mp ON mp.principal_id = srm.member_principal_id \
                 ORDER BY rl.name, mp.name".to_string()),
        ],
        Engine::Sqlite => Vec::new(),
    }
}

/// "Oprávnění" sub-view, one schema at a time — `schema` is interpolated
/// via sql_string_literal, NEVER raw concatenation. Postgres:
/// [("object_acl", …), ("schema_acl", …), ("db_acl", …)] — hard PG ≥ 10
/// (acldefault; CURATION item 5: on error the sub-view shows the error,
/// there is NO fallback query). MSSQL: [("object_perms", …),
/// ("schema_perms", …)]. SQLite: empty.
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

/// "Databáze a schémata" sub-view. Postgres: [("current_db_size", …),
/// ("databases", …), ("schema_sizes", …)]. MSSQL: same three labels
/// (db size from sys.database_files, databases list, per-schema
/// partition-stats sizes). SQLite: empty.
pub fn sizes_catalog(engine: Engine) -> Vec<(&'static str, String)> {
    match engine {
        Engine::Postgres => vec![
            ("current_db_size", "SELECT pg_catalog.pg_database_size(current_database()) AS bytes, \
                 pg_catalog.pg_size_pretty(pg_catalog.pg_database_size(current_database())) AS pretty".to_string()),
            ("databases", "SELECT datname, pg_catalog.pg_database_size(datname) AS bytes \
                 FROM pg_catalog.pg_database WHERE datistemplate = false ORDER BY datname".to_string()),
            // G10 T6 live-docker fix: LEFT JOIN from pg_namespace (not an
            // INNER JOIN from pg_class) so a schema with zero tables/matviews
            // still gets a row (NULL SUM, parsed as 0 bytes by
            // admin_panel::parse_schema_sizes) instead of vanishing from the
            // list entirely — caught by a live PG run: a freshly
            // CREATE SCHEMA'd empty schema never appeared in a refetch under
            // the prior INNER JOIN shape, even though this file's OWN test
            // fixture (parse_schema_sizes_pg_bytes_mssql_kb's "empty" row)
            // already anticipated a NULL-bytes row for exactly this case.
            // Review finding M3: the LEFT JOIN alone would also surface
            // pg_toast/pg_temp_N/pg_toast_temp_N/pg_catalog/information_schema
            // as "selectable schemas" — filtered out with the SAME
            // exclusion `dbc-driver-postgres::SCHEMA_EXCLUDE` already uses
            // for the main schema tree (crates/dbc-driver-postgres/src/lib.rs)
            // — kept consistent rather than inventing a second filter, so
            // the Databases sub-view's schema list matches what Privileges'
            // schema selector (fed from that same SchemaSnapshot) already
            // shows.
            ("schema_sizes", "SELECT n.nspname AS schema, SUM(pg_catalog.pg_total_relation_size(c.oid)) AS bytes \
                 FROM pg_catalog.pg_namespace n \
                 LEFT JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relkind IN ('r','m') \
                 WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
                 AND n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%' \
                 AND n.nspname NOT LIKE 'pg\\_toast%' \
                 GROUP BY n.nspname ORDER BY n.nspname".to_string()),
        ],
        Engine::Mssql => vec![
            ("current_db_size", "SELECT DB_NAME() AS database_name, \
                 CAST(SUM(CASE WHEN type = 0 THEN size ELSE 0 END) * 8.0 / 1024 AS DECIMAL(15,2)) AS data_mb, \
                 CAST(SUM(CASE WHEN type = 1 THEN size ELSE 0 END) * 8.0 / 1024 AS DECIMAL(15,2)) AS log_mb \
                 FROM sys.database_files".to_string()),
            ("databases", "SELECT name, database_id, create_date, state_desc FROM sys.databases ORDER BY name".to_string()),
            // Review finding M1: the prior shape INNER JOINed
            // sys.dm_db_partition_stats -> sys.tables -> sys.schemas, so a
            // schema with zero tables never produced a row at all — the
            // exact empty-schema bug the pg LEFT JOIN above fixes, mirrored
            // here (never live-verified — no MSSQL docker available; this
            // is a static-SQL-shape fix only, honestly noted). Driven FROM
            // sys.schemas so every real schema gets a row (COALESCE'd to 0
            // when it owns no tables yet), filtering out MSSQL's fixed
            // database-role schemas (every one of which is also a `sys.
            // schemas` row) plus `sys`/`INFORMATION_SCHEMA` — the standard
            // "not a real user schema" set.
            ("schema_sizes", "SELECT s.name AS schema_name, \
                 COALESCE(SUM(ps.reserved_page_count), 0) * 8 AS reserved_kb, \
                 COALESCE(SUM(ps.used_page_count), 0) * 8 AS used_kb \
                 FROM sys.schemas s \
                 LEFT JOIN sys.tables t ON t.schema_id = s.schema_id \
                 LEFT JOIN sys.dm_db_partition_stats ps ON ps.object_id = t.object_id \
                 WHERE s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest', 'db_owner', 'db_accessadmin', \
                 'db_securityadmin', 'db_ddladmin', 'db_backupoperator', 'db_datareader', 'db_datawriter', \
                 'db_denydatareader', 'db_denydatawriter') \
                 GROUP BY s.name ORDER BY s.name".to_string()),
        ],
        Engine::Sqlite => Vec::new(),
    }
}

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

    // Review finding M1 (empty-schema visibility) + M3 (pg system-namespace
    // filter) — static-SQL shape checks only. M1's MSSQL side has NEVER run
    // against a live server (no MSSQL docker available in this
    // environment); the pg side WAS re-verified live (see runner.rs's
    // admin_pg_sizes_catalog_and_schema_ddl_round_trip_on_live_postgres).
    #[test]
    fn schema_sizes_drives_from_the_full_schema_list_not_an_inner_join_and_excludes_system_schemas() {
        let pg = sizes_catalog(Engine::Postgres);
        let pg_schema_sizes = &pg[2].1;
        // LEFT JOIN (not the prior INNER JOIN) — a schema with zero tables
        // must still produce a row.
        assert!(pg_schema_sizes.contains("FROM pg_catalog.pg_namespace n"));
        assert!(pg_schema_sizes.contains("LEFT JOIN pg_catalog.pg_class c"));
        // Same exclusion set dbc-driver-postgres::SCHEMA_EXCLUDE already
        // uses for the main schema tree (pg_catalog/information_schema +
        // the toast/temp implementation-detail namespaces).
        assert!(pg_schema_sizes.contains("NOT IN ('pg_catalog', 'information_schema')"));
        assert!(pg_schema_sizes.contains("pg\\_temp\\_%"));
        assert!(pg_schema_sizes.contains("pg\\_toast\\_temp\\_%"));
        assert!(pg_schema_sizes.contains("pg\\_toast%"));

        let ms = sizes_catalog(Engine::Mssql);
        let ms_schema_sizes = &ms[2].1;
        // Driven FROM sys.schemas (not FROM sys.dm_db_partition_stats) with
        // LEFT JOINs down to the stats — a schema with zero tables must
        // still produce a row (COALESCE'd to 0, not vanish).
        assert!(ms_schema_sizes.contains("FROM sys.schemas s"));
        assert!(ms_schema_sizes.contains("LEFT JOIN sys.tables t"));
        assert!(ms_schema_sizes.contains("LEFT JOIN sys.dm_db_partition_stats ps"));
        assert!(ms_schema_sizes.contains("COALESCE(SUM(ps.reserved_page_count), 0)"));
        // The standard fixed-database-role/system schema exclusion set.
        for excluded in [
            "sys",
            "INFORMATION_SCHEMA",
            "guest",
            "db_owner",
            "db_accessadmin",
            "db_securityadmin",
            "db_ddladmin",
            "db_backupoperator",
            "db_datareader",
            "db_datawriter",
            "db_denydatareader",
            "db_denydatawriter",
        ] {
            assert!(
                ms_schema_sizes.contains(&format!("'{excluded}'")),
                "expected {excluded} in the exclusion list: {ms_schema_sizes}"
            );
        }
    }
}

/// Design §0/§3: the widened statement type. `exec_sql` is what runs;
/// `display_sql` is what the confirm modal shows AND what history stores.
/// They differ ONLY for password-bearing statements (parallel
/// construction, never post-hoc replace). Lives here (pure module, no
/// GPUI) per design §5 T2 "mutation builders + WriteStatement";
/// runner.rs imports it in T3.
///
/// T1+T2 review carry-forward (BLOCKER 1): the plan's interface spec calls
/// for `#[derive(Debug, ...)]`, but `exec_sql` carries the REAL,
/// unredacted password for password-bearing statements — a derived
/// `{:?}` would print it verbatim into any log/panic/assert message that
/// happens to format a `WriteStatement`. That security requirement
/// supersedes the plan's derive: `Debug` is implemented BY HAND below,
/// printing `display_sql` (already '***'-redacted where it matters) and
/// never touching `exec_sql`.
#[derive(Clone, PartialEq, Eq)]
pub struct WriteStatement {
    pub exec_sql: String,
    pub display_sql: String,
    pub expected_affected: Option<u64>,
}

impl std::fmt::Debug for WriteStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteStatement")
            .field("display_sql", &self.display_sql)
            .field("exec_sql", &"<redacted>")
            .field("expected_affected", &self.expected_affected)
            .finish()
    }
}

/// G5's sandbox statements: exec == display, always.
impl From<(String, Option<u64>)> for WriteStatement {
    fn from((sql, expected_affected): (String, Option<u64>)) -> Self {
        Self { display_sql: sql.clone(), exec_sql: sql, expected_affected }
    }
}

/// The literal shown in display_sql wherever exec_sql carries a password.
const REDACTED: &str = "'***'";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoleFlags {
    pub login: bool,
    pub superuser: bool,
    pub createdb: bool,
    pub createrole: bool,
}

impl RoleFlags {
    fn render(&self) -> String {
        let mut out = String::new();
        if self.login {
            out.push_str(" LOGIN");
        }
        if self.superuser {
            out.push_str(" SUPERUSER");
        }
        if self.createdb {
            out.push_str(" CREATEDB");
        }
        if self.createrole {
            out.push_str(" CREATEROLE");
        }
        out
    }
}

/// pg: CREATE ROLE + PASSWORD + flags (1 stmt). MSSQL: CREATE LOGIN +
/// CREATE USER FOR LOGIN (2 stmts). SQLite: empty (exempt).
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

pub fn alter_password(engine: Engine, name: &str, password: &str) -> Vec<WriteStatement> {
    let ident = quote_ident_for(engine, name);
    match engine {
        Engine::Postgres => vec![WriteStatement {
            exec_sql: format!("ALTER ROLE {ident} PASSWORD {}", sql_string_literal(password)),
            display_sql: format!("ALTER ROLE {ident} PASSWORD {REDACTED}"),
            expected_affected: None,
        }],
        Engine::Mssql => vec![WriteStatement {
            exec_sql: format!("ALTER LOGIN {ident} WITH PASSWORD = {}", sql_string_literal(password)),
            display_sql: format!("ALTER LOGIN {ident} WITH PASSWORD = {REDACTED}"),
            expected_affected: None,
        }],
        Engine::Sqlite => Vec::new(),
    }
}

/// pg: DROP ROLE (1). MSSQL: DROP USER + DROP LOGIN (2). SQLite: empty.
pub fn drop_role(engine: Engine, name: &str) -> Vec<WriteStatement> {
    let ident = quote_ident_for(engine, name);
    match engine {
        Engine::Postgres => vec![(format!("DROP ROLE {ident}"), None).into()],
        Engine::Mssql => vec![
            (format!("DROP USER {ident}"), None).into(),
            (format!("DROP LOGIN {ident}"), None).into(),
        ],
        Engine::Sqlite => Vec::new(),
    }
}

/// `admin_option` pg-only (ignored on MSSQL); `server_role` MSSQL-only
/// (which membership list the role came from; ignored on pg).
pub fn add_membership(engine: Engine, role: &str, member: &str, admin_option: bool, server_role: bool) -> Vec<WriteStatement> {
    let r = quote_ident_for(engine, role);
    let m = quote_ident_for(engine, member);
    match engine {
        Engine::Postgres => {
            let opt = if admin_option { " WITH ADMIN OPTION" } else { "" };
            vec![(format!("GRANT {r} TO {m}{opt}"), None).into()]
        }
        Engine::Mssql => {
            let verb = if server_role { "ALTER SERVER ROLE" } else { "ALTER ROLE" };
            vec![(format!("{verb} {r} ADD MEMBER {m}"), None).into()]
        }
        Engine::Sqlite => Vec::new(),
    }
}

pub fn remove_membership(engine: Engine, role: &str, member: &str, server_role: bool) -> Vec<WriteStatement> {
    let r = quote_ident_for(engine, role);
    let m = quote_ident_for(engine, member);
    match engine {
        Engine::Postgres => vec![(format!("REVOKE {r} FROM {m}"), None).into()],
        Engine::Mssql => {
            let verb = if server_role { "ALTER SERVER ROLE" } else { "ALTER ROLE" };
            vec![(format!("{verb} {r} DROP MEMBER {m}"), None).into()]
        }
        Engine::Sqlite => Vec::new(),
    }
}

/// Privileges-matrix cell state. Postgres cells are BI-state (Denied is
/// unrepresentable through cycle_cell and refused by the builders);
/// MSSQL is TRI-state (design §2 — MSSQL alone has a real DENY).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellState {
    NotSet,
    Granted,
    Denied,
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

pub const PG_TABLE_PRIVS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];
pub const MSSQL_TABLE_PRIVS: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES"];
pub const SCHEMA_PRIVS: &[&str] = &["USAGE", "CREATE"];
pub const PG_DATABASE_PRIVS: &[&str] = &["CONNECT", "CREATE", "TEMP"];

/// Target state alone decides the verb: Granted→GRANT, Denied→DENY
/// (MSSQL only), NotSet→REVOKE. Err (errors are values, design §6's
/// DENY-divergence risk): (Postgres, Denied), SQLite, or empty `privs`.
pub fn object_privilege(
    engine: Engine,
    schema: &str,
    object: &str,
    privs: &[&str],
    grantee: &str,
    target: CellState,
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

/// pg: GRANT USAGE ON SCHEMA "s" TO "g". MSSQL: GRANT … ON SCHEMA::[s] TO [g].
pub fn schema_privilege(engine: Engine, schema: &str, priv_name: &str, grantee: &str, target: CellState) -> Result<WriteStatement, String> {
    if engine == Engine::Sqlite {
        return Err("SQLite nemá serverová oprávnění".to_string());
    }
    if engine == Engine::Postgres && target == CellState::Denied {
        return Err("DENY na PostgreSQL neexistuje".to_string());
    }
    let g = quote_ident_for(engine, grantee);
    let sql = match engine {
        Engine::Mssql => {
            let ident = quote_ident_for(engine, schema);
            match target {
                CellState::Granted => format!("GRANT {priv_name} ON SCHEMA::{ident} TO {g}"),
                CellState::Denied => format!("DENY {priv_name} ON SCHEMA::{ident} TO {g}"),
                CellState::NotSet => format!("REVOKE {priv_name} ON SCHEMA::{ident} FROM {g}"),
            }
        }
        Engine::Postgres => {
            let ident = quote_ident_for(engine, schema);
            match target {
                CellState::Granted => format!("GRANT {priv_name} ON SCHEMA {ident} TO {g}"),
                CellState::Denied => unreachable!("DENY on Postgres refused above"),
                CellState::NotSet => format!("REVOKE {priv_name} ON SCHEMA {ident} FROM {g}"),
            }
        }
        Engine::Sqlite => unreachable!("Sqlite refused above"),
    };
    Ok((sql, None).into())
}

/// pg-only (design §2: db-level row is pg only): GRANT CONNECT ON DATABASE "d" TO "g".
pub fn database_privilege_pg(database: &str, priv_name: &str, grantee: &str, target: CellState) -> Result<WriteStatement, String> {
    if target == CellState::Denied {
        return Err("DENY na PostgreSQL neexistuje".to_string());
    }
    let ident = quote_ident_for(Engine::Postgres, database);
    let g = quote_ident_for(Engine::Postgres, grantee);
    let sql = match target {
        CellState::Granted => format!("GRANT {priv_name} ON DATABASE {ident} TO {g}"),
        CellState::NotSet => format!("REVOKE {priv_name} ON DATABASE {ident} FROM {g}"),
        CellState::Denied => unreachable!("DENY refused above"),
    };
    Ok((sql, None).into())
}

pub fn create_schema(engine: Engine, name: &str) -> Vec<WriteStatement> {
    let ident = quote_ident_for(engine, name);
    match engine {
        Engine::Postgres | Engine::Mssql => vec![(format!("CREATE SCHEMA {ident}"), None).into()],
        Engine::Sqlite => Vec::new(),
    }
}

/// `cascade` is pg-only opt-in (design §2 — the confirm modal adds a red
/// warning line); T-SQL DROP SCHEMA has no CASCADE clause, the flag is
/// ignored for MSSQL (the engine refuses a non-empty schema itself).
pub fn drop_schema(engine: Engine, name: &str, cascade: bool) -> Vec<WriteStatement> {
    let ident = quote_ident_for(engine, name);
    match engine {
        Engine::Postgres => {
            let suffix = if cascade { " CASCADE" } else { "" };
            vec![(format!("DROP SCHEMA {ident}{suffix}"), None).into()]
        }
        Engine::Mssql => vec![(format!("DROP SCHEMA {ident}"), None).into()],
        Engine::Sqlite => Vec::new(),
    }
}

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

    // BLOCKER 1 carry-forward: WriteStatement's hand-written Debug must
    // never format the real password, whatever debug/log call formats it.
    #[test]
    fn write_statement_debug_never_contains_the_real_password() {
        let stmts = create_role(Engine::Postgres, "app_user", "s3cr'et", &RoleFlags::default());
        let debug = format!("{:?}", stmts[0]);
        assert!(!debug.contains("s3cr"), "Debug leaked the real password: {debug}");
        assert!(debug.contains("'***'"), "Debug should show the redacted display_sql: {debug}");
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
