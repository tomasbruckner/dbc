use crate::schema::TableInfo;
use crate::split::Dialect;

/// Synthesizes a CREATE TABLE statement from metadata for engines with no
/// server-side "get table DDL" (Postgres). Quoting via `quote_ident`.
pub fn synthesize_create_table(t: &TableInfo) -> String {
    synthesize_create_table_d(Dialect::Postgres, t)
}

/// Dialect-aware sibling of [`synthesize_create_table`] (G15 §2a). The
/// MSSQL driver reports `ddl: None` for tables, so schema-tree DDL and G7
/// text-diff fall back to synthesis -- it must bracket-quote for MSSQL.
pub fn synthesize_create_table_d(dialect: Dialect, t: &TableInfo) -> String {
    let mut lines: Vec<String> = Vec::new();

    for col in &t.columns {
        let mut line = format!("  {} {}", quote_ident_d(dialect, &col.name), col.data_type);
        if !col.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default {
            line.push_str(" DEFAULT ");
            line.push_str(default);
        }
        lines.push(line);
    }

    let pk_cols: Vec<String> = t
        .columns
        .iter()
        .filter(|c| c.is_pk)
        .map(|c| quote_ident_d(dialect, &c.name))
        .collect();
    if !pk_cols.is_empty() {
        lines.push(format!("  PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    for c in &t.constraints {
        if c.kind == "PRIMARY KEY" {
            continue;
        }
        lines.push(format!("  CONSTRAINT {} {}", quote_ident_d(dialect, &c.name), c.definition));
    }

    format!(
        "CREATE TABLE {} (\n{}\n);",
        quote_qualified_d(dialect, t.schema.as_deref(), &t.name),
        lines.join(",\n")
    )
}

/// `"name"` with embedded quotes doubled; used by ddl and by preview SQL
/// (T7). Thin pg-convention wrapper over [`quote_ident_d`] -- callers that
/// are pg/sqlite-only by construction keep compiling unchanged (G15 §2a).
pub fn quote_ident(name: &str) -> String {
    quote_ident_d(Dialect::Postgres, name)
}

/// `schema.table` with both parts quoted, schema optional. Thin
/// pg-convention wrapper over [`quote_qualified_d`].
pub fn quote_qualified(schema: Option<&str>, name: &str) -> String {
    quote_qualified_d(Dialect::Postgres, schema, name)
}

/// Dialect-aware identifier quoting (G15 §2a -- THE one bracket
/// implementation; `admin_sql::quote_ident_for` delegates here).
/// Mssql: brackets, `]` doubled -- valid in EVERY T-SQL session regardless
/// of QUOTED_IDENTIFIER (the unconditional choice for the write path, per
/// the driver's integration note 1). Others: ANSI double quotes.
pub fn quote_ident_d(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::Mssql => format!("[{}]", name.replace(']', "]]")),
        Dialect::Postgres | Dialect::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// Dialect-aware sibling of [`quote_qualified`].
pub fn quote_qualified_d(dialect: Dialect, schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", quote_ident_d(dialect, s), quote_ident_d(dialect, name)),
        None => quote_ident_d(dialect, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;

    fn col(name: &str, ty: &str, nullable: bool, default: Option<&str>, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable,
                     default: default.map(Into::into), is_pk: pk, fk: None }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_qualified(Some("public"), "t"), "\"public\".\"t\"");
        assert_eq!(quote_qualified(None, "t"), "\"t\"");
    }

    #[test]
    fn synthesizes_create_table() {
        let t = TableInfo {
            schema: Some("public".into()), name: "orders".into(), kind: TableKind::Table,
            columns: vec![
                col("id", "integer", false, None, true),
                col("note", "text", true, Some("'-'::text"), false),
            ],
            indexes: vec![],
            constraints: vec![ConstraintInfo {
                name: "orders_fk".into(), kind: "FOREIGN KEY".into(),
                definition: "FOREIGN KEY (cid) REFERENCES customers(id)".into(),
            }],
            ddl: None,
        };
        let sql = synthesize_create_table(&t);
        assert!(sql.starts_with("CREATE TABLE \"public\".\"orders\" ("));
        assert!(sql.contains("\"id\" integer NOT NULL"));
        assert!(sql.contains("\"note\" text DEFAULT '-'::text"));
        assert!(sql.contains("PRIMARY KEY (\"id\")"));
        assert!(sql.contains("CONSTRAINT \"orders_fk\" FOREIGN KEY (cid) REFERENCES customers(id)"));
        assert!(sql.trim_end().ends_with(");"));
    }

    // ---------- Mssql dialect-aware quoting (G15 T1) ----------

    #[test]
    fn quote_ident_d_mssql_brackets_and_doubles_closing() {
        assert_eq!(quote_ident_d(Dialect::Mssql, "we]ird"), "[we]]ird]");
        assert_eq!(quote_ident_d(Dialect::Mssql, "plain"), "[plain]");
        // pg unchanged.
        assert_eq!(quote_ident_d(Dialect::Postgres, "plain"), "\"plain\"");
        assert_eq!(quote_ident_d(Dialect::Sqlite, "plain"), "\"plain\"");
    }

    #[test]
    fn quote_qualified_d_mssql() {
        assert_eq!(
            quote_qualified_d(Dialect::Mssql, Some("dbo"), "t"),
            "[dbo].[t]"
        );
        assert_eq!(quote_qualified_d(Dialect::Mssql, None, "t"), "[t]");
    }

    #[test]
    fn synthesize_create_table_d_mssql_bracket_quotes_everything() {
        let t = TableInfo {
            schema: Some("dbo".into()), name: "orders".into(), kind: TableKind::Table,
            columns: vec![
                col("id", "int", false, None, true),
                col("we]ird", "nvarchar(50)", true, None, false),
            ],
            indexes: vec![],
            constraints: vec![ConstraintInfo {
                name: "orders_fk".into(), kind: "FOREIGN KEY".into(),
                definition: "FOREIGN KEY (cid) REFERENCES customers(id)".into(),
            }],
            ddl: None,
        };
        let sql = synthesize_create_table_d(Dialect::Mssql, &t);
        assert!(sql.starts_with("CREATE TABLE [dbo].[orders] ("));
        assert!(sql.contains("[id] int NOT NULL"));
        assert!(sql.contains("[we]]ird] nvarchar(50)"));
        assert!(sql.contains("PRIMARY KEY ([id])"));
        assert!(sql.contains("CONSTRAINT [orders_fk] FOREIGN KEY (cid) REFERENCES customers(id)"));
        assert!(sql.trim_end().ends_with(");"));
    }

    #[test]
    fn synthesize_create_table_wrapper_is_byte_identical_to_before() {
        let t = TableInfo {
            schema: Some("public".into()), name: "orders".into(), kind: TableKind::Table,
            columns: vec![
                col("id", "integer", false, None, true),
                col("note", "text", true, Some("'-'::text"), false),
            ],
            indexes: vec![],
            constraints: vec![ConstraintInfo {
                name: "orders_fk".into(), kind: "FOREIGN KEY".into(),
                definition: "FOREIGN KEY (cid) REFERENCES customers(id)".into(),
            }],
            ddl: None,
        };
        assert_eq!(synthesize_create_table(&t), synthesize_create_table_d(Dialect::Postgres, &t));
    }
}
