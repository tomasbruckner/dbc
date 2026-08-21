#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSnapshot { pub tables: Vec<TableInfo> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo { pub name: String, pub data_type: String }
