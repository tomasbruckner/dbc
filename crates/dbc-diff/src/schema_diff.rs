//! G7: schema-half diff model + matching engine (design §1). Pure data
//! structures and a sort-merge matcher — no I/O, no GPUI, no driver crates.

#[allow(unused_imports)] // consumed by diff_schema, T2
use dbc_core::{
    ColumnInfo, ConstraintInfo, IndexInfo, RoutineInfo, SchemaSnapshot, SequenceInfo, TableInfo,
    TableKind, TriggerInfo,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareMode {
    SameEngine,
    CrossEngine,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectDiff<T> {
    Added(T),
    Removed(T),
    Changed { left: T, right: T, fields: Vec<FieldChange> },
    Unchanged(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldChange {
    pub field: String,
    pub left: String,
    pub right: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStatus {
    Added,
    Removed,
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDiff {
    pub schema: Option<String>,
    pub name: String,
    pub status: TableStatus,
    pub table_fields: Vec<FieldChange>,
    pub columns: Vec<ObjectDiff<ColumnInfo>>,
    pub indexes: Vec<ObjectDiff<IndexInfo>>,
    pub constraints: Vec<ObjectDiff<ConstraintInfo>>,
    /// Full source object, present on whichever side(s) it exists —
    /// deviation from the design's field sketch, see Self-Review note 1:
    /// the UI's Added/Removed DDL panel needs the whole `TableInfo`
    /// (`.ddl`/`ddl::synthesize_create_table`), not just the field-diff
    /// summary. `Some(_)`/`Some(_)` for Changed/Unchanged,
    /// `Some(_)`/`None` for Removed, `None`/`Some(_)` for Added.
    pub left: Option<TableInfo>,
    pub right: Option<TableInfo>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaDiff {
    pub tables: Vec<TableDiff>,
    pub routines: Vec<ObjectDiff<RoutineInfo>>,
    pub triggers: Vec<ObjectDiff<TriggerInfo>>,
    pub sequences: Vec<ObjectDiff<SequenceInfo>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{ColumnInfo, TableKind};

    #[test]
    fn object_diff_variants_are_constructible_and_comparable() {
        let a = ColumnInfo { name: "id".into(), data_type: "int4".into(), ..Default::default() };
        let b = ColumnInfo { name: "id".into(), data_type: "int8".into(), ..Default::default() };
        let changed = ObjectDiff::Changed {
            left: a.clone(),
            right: b.clone(),
            fields: vec![FieldChange { field: "data_type".into(), left: "int4".into(), right: "int8".into() }],
        };
        assert_eq!(changed, changed.clone());
        assert_ne!(ObjectDiff::Added(a.clone()), ObjectDiff::Removed(a));
    }

    #[test]
    fn schema_diff_default_is_empty() {
        let d = SchemaDiff::default();
        assert!(d.tables.is_empty() && d.routines.is_empty() && d.triggers.is_empty() && d.sequences.is_empty());
    }

    #[test]
    fn table_diff_carries_the_right_side_presence_by_status() {
        let t = TableInfo { name: "t".into(), kind: TableKind::Table, ..Default::default() };
        let removed = TableDiff {
            schema: None, name: "t".into(), status: TableStatus::Removed,
            table_fields: vec![], columns: vec![], indexes: vec![], constraints: vec![],
            left: Some(t.clone()), right: None,
        };
        assert!(removed.left.is_some() && removed.right.is_none());
    }
}
