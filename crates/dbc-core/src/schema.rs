use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    pub tables: Vec<TableInfo>,
    pub routines: Vec<RoutineInfo>,
    pub triggers: Vec<TriggerInfo>,
    pub sequences: Vec<SequenceInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
}

impl Default for TableKind {
    fn default() -> Self {
        TableKind::Table
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TableInfo {
    pub schema: Option<String>,
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub constraints: Vec<ConstraintInfo>,
    /// Engine-provided source (views: definition; sqlite tables:
    /// sqlite_master.sql). None => UI synthesizes via
    /// ddl::synthesize_create_table.
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub is_pk: bool,
    /// FK target, filled where the catalog knows it (feeds G4 joined
    /// columns).
    pub fk: Option<FkRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FkRef {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConstraintInfo {
    pub name: String,
    // "PRIMARY KEY" | "FOREIGN KEY" | "UNIQUE" | "CHECK" | engine string
    pub kind: String,
    // human-readable body
    pub definition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutineKind {
    Function,
    Procedure,
}

impl Default for RoutineKind {
    fn default() -> Self {
        RoutineKind::Function
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoutineInfo {
    pub schema: Option<String>,
    pub name: String,
    pub kind: RoutineKind,
    /// Display signature, e.g. "(integer, text) -> boolean"; empty string
    /// ok.
    pub signature: String,
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub schema: Option<String>,
    pub name: String,
    pub table: String,
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SequenceInfo {
    pub schema: Option<String>,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // T2: the JSON contract `dbc-mcp`'s `get_schema` handler relies on —
    // field names and nesting, hand-built and asserted directly rather than
    // via a driver, since this is a pure serialization contract.
    #[test]
    fn schema_snapshot_serializes_to_documented_shape() {
        let snap = SchemaSnapshot {
            tables: vec![TableInfo {
                schema: Some("public".into()),
                name: "users".into(),
                kind: TableKind::Table,
                columns: vec![ColumnInfo {
                    name: "id".into(),
                    data_type: "int4".into(),
                    nullable: false,
                    default: None,
                    is_pk: true,
                    fk: None,
                }],
                indexes: vec![IndexInfo { name: "users_pkey".into(), columns: vec!["id".into()], unique: true }],
                constraints: vec![ConstraintInfo {
                    name: "users_pkey".into(),
                    kind: "PRIMARY KEY".into(),
                    definition: "PRIMARY KEY (id)".into(),
                }],
                ddl: Some("CREATE TABLE users (id int4 PRIMARY KEY)".into()),
            }],
            routines: vec![RoutineInfo {
                schema: Some("public".into()),
                name: "f".into(),
                kind: RoutineKind::Function,
                signature: "() -> void".into(),
                ddl: None,
            }],
            triggers: vec![TriggerInfo {
                schema: Some("public".into()),
                name: "trg".into(),
                table: "users".into(),
                ddl: None,
            }],
            sequences: vec![SequenceInfo { schema: Some("public".into()), name: "users_id_seq".into() }],
        };

        let v = serde_json::to_value(&snap).unwrap();
        assert!(v.get("tables").unwrap().is_array());
        assert!(v.get("routines").unwrap().is_array());
        assert!(v.get("triggers").unwrap().is_array());
        assert!(v.get("sequences").unwrap().is_array());

        let t = &v["tables"][0];
        assert_eq!(t["schema"], "public");
        assert_eq!(t["name"], "users");
        assert_eq!(t["kind"], "Table");
        assert_eq!(t["columns"][0]["name"], "id");
        assert_eq!(t["columns"][0]["is_pk"], true);
        assert_eq!(t["indexes"][0]["name"], "users_pkey");
        assert_eq!(t["constraints"][0]["kind"], "PRIMARY KEY");
        assert_eq!(t["ddl"], "CREATE TABLE users (id int4 PRIMARY KEY)");

        assert_eq!(v["routines"][0]["kind"], "Function");
        assert_eq!(v["triggers"][0]["table"], "users");
        assert_eq!(v["sequences"][0]["name"], "users_id_seq");
    }
}
