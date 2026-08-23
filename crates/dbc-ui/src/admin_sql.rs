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

/// Engine-aware identifier quoting (design §4): pg/sqlite delegate to
/// dbc_core::quote_ident (double quotes, `"` doubled); MSSQL brackets
/// (`[name]`, `]` doubled) — MSSQL must NEVER route through
/// dbc_core::quote_ident (CURATION item 2). Scoped to this module only;
/// ddl.rs/sandbox.rs are unchanged.
pub fn quote_ident_for(engine: Engine, name: &str) -> String {
    match engine {
        Engine::Mssql => format!("[{}]", name.replace(']', "]]")),
        Engine::Postgres | Engine::Sqlite => dbc_core::quote_ident(name),
    }
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
            ("schema_sizes", "SELECT n.nspname AS schema, SUM(pg_catalog.pg_total_relation_size(c.oid)) AS bytes \
                 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','m') GROUP BY n.nspname ORDER BY n.nspname".to_string()),
        ],
        Engine::Mssql => vec![
            ("current_db_size", "SELECT DB_NAME() AS database_name, \
                 CAST(SUM(CASE WHEN type = 0 THEN size ELSE 0 END) * 8.0 / 1024 AS DECIMAL(15,2)) AS data_mb, \
                 CAST(SUM(CASE WHEN type = 1 THEN size ELSE 0 END) * 8.0 / 1024 AS DECIMAL(15,2)) AS log_mb \
                 FROM sys.database_files".to_string()),
            ("databases", "SELECT name, database_id, create_date, state_desc FROM sys.databases ORDER BY name".to_string()),
            ("schema_sizes", "SELECT s.name AS schema_name, SUM(ps.reserved_page_count) * 8 AS reserved_kb, \
                 SUM(ps.used_page_count) * 8 AS used_kb \
                 FROM sys.dm_db_partition_stats ps \
                 JOIN sys.tables t ON t.object_id = ps.object_id \
                 JOIN sys.schemas s ON s.schema_id = t.schema_id \
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
}
