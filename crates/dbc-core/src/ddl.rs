use crate::schema::TableInfo;

/// Synthesizes a CREATE TABLE statement from metadata for engines with no
/// server-side "get table DDL" (Postgres). Quoting via `quote_ident`.
pub fn synthesize_create_table(t: &TableInfo) -> String {
    let mut lines: Vec<String> = Vec::new();

    for col in &t.columns {
        let mut line = format!("  {} {}", quote_ident(&col.name), col.data_type);
        if !col.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(default) = &col.default {
            line.push_str(" DEFAULT ");
            line.push_str(default);
        }
        lines.push(line);
    }

    let pk_cols: Vec<String> =
        t.columns.iter().filter(|c| c.is_pk).map(|c| quote_ident(&c.name)).collect();
    if !pk_cols.is_empty() {
        lines.push(format!("  PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    for c in &t.constraints {
        if c.kind == "PRIMARY KEY" {
            continue;
        }
        lines.push(format!("  CONSTRAINT {} {}", quote_ident(&c.name), c.definition));
    }

    format!(
        "CREATE TABLE {} (\n{}\n);",
        quote_qualified(t.schema.as_deref(), &t.name),
        lines.join(",\n")
    )
}

/// `"name"` with embedded quotes doubled; used by ddl and by preview SQL
/// (T7).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `schema.table` with both parts quoted, schema optional.
pub fn quote_qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", quote_ident(s), quote_ident(name)),
        None => quote_ident(name),
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
}
