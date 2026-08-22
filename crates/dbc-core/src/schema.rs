#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SchemaSnapshot {
    pub tables: Vec<TableInfo>,
    pub routines: Vec<RoutineInfo>,
    pub triggers: Vec<TriggerInfo>,
    pub sequences: Vec<SequenceInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FkRef {
    pub schema: Option<String>,
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConstraintInfo {
    pub name: String,
    // "PRIMARY KEY" | "FOREIGN KEY" | "UNIQUE" | "CHECK" | engine string
    pub kind: String,
    // human-readable body
    pub definition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
}

impl Default for RoutineKind {
    fn default() -> Self {
        RoutineKind::Function
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoutineInfo {
    pub schema: Option<String>,
    pub name: String,
    pub kind: RoutineKind,
    /// Display signature, e.g. "(integer, text) -> boolean"; empty string
    /// ok.
    pub signature: String,
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TriggerInfo {
    pub schema: Option<String>,
    pub name: String,
    pub table: String,
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SequenceInfo {
    pub schema: Option<String>,
    pub name: String,
}
