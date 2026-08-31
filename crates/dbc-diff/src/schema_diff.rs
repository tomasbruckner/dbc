//! G7: schema-half diff model + matching engine (design §1). Pure data
//! structures and a sort-merge matcher — no I/O, no GPUI, no driver crates.

use std::cmp::Ordering;
use dbc_core::{
    ColumnInfo, ConstraintInfo, FkRef, IndexInfo, RoutineInfo, RoutineKind, SchemaSnapshot,
    SequenceInfo, TableInfo, TriggerInfo,
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

/// The crate's one entry point for the schema half (design §1).
/// Deterministic by construction: every match pass sorts both sides by
/// their key once, then merge-walks with two cursors — output order is
/// therefore always ascending-by-key regardless of the snapshots'
/// original catalog-query order.
pub fn diff_schema(left: &SchemaSnapshot, right: &SchemaSnapshot, mode: CompareMode) -> SchemaDiff {
    SchemaDiff {
        tables: diff_tables(&left.tables, &right.tables, mode),
        routines: diff_by_key(&left.routines, &right.routines, routine_key, diff_routine_fields),
        triggers: diff_by_key(&left.triggers, &right.triggers, trigger_key, diff_trigger_fields),
        sequences: diff_by_key(&left.sequences, &right.sequences, sequence_key, |_, _| Vec::new()),
    }
}

fn routine_key(r: &RoutineInfo) -> (Option<String>, String, u8) {
    (r.schema.clone(), r.name.clone(), match r.kind { RoutineKind::Function => 0, RoutineKind::Procedure => 1 })
}
fn trigger_key(t: &TriggerInfo) -> (Option<String>, String, String) {
    (t.schema.clone(), t.table.clone(), t.name.clone())
}
fn sequence_key(s: &SequenceInfo) -> (Option<String>, String) {
    (s.schema.clone(), s.name.clone())
}
fn table_key(t: &TableInfo) -> (Option<String>, String) {
    (t.schema.clone(), t.name.clone())
}
fn column_key(c: &ColumnInfo) -> String { c.name.clone() }
fn index_key(i: &IndexInfo) -> String { i.name.clone() }
fn constraint_key(c: &ConstraintInfo) -> String { c.name.clone() }

/// Generic sort-merge matcher: O(n log n) via one sort per side, O(n) merge.
/// `field_diff` returning `vec![]` for a matched pair means Unchanged;
/// non-empty means Changed. Used for every flat (non-nested) object list —
/// tables use their own hand-rolled version below since a table match must
/// ALSO recurse into columns/indexes/constraints.
fn diff_by_key<T, K, KF, F>(left: &[T], right: &[T], key_fn: KF, field_diff: F) -> Vec<ObjectDiff<T>>
where
    T: Clone,
    K: Ord,
    KF: Fn(&T) -> K,
    F: Fn(&T, &T) -> Vec<FieldChange>,
{
    let mut li: Vec<&T> = left.iter().collect();
    let mut ri: Vec<&T> = right.iter().collect();
    li.sort_by_key(|t| key_fn(t));
    ri.sort_by_key(|t| key_fn(t));

    let mut out = Vec::with_capacity(li.len().max(ri.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < li.len() && j < ri.len() {
        match key_fn(li[i]).cmp(&key_fn(ri[j])) {
            Ordering::Less => { out.push(ObjectDiff::Removed(li[i].clone())); i += 1; }
            Ordering::Greater => { out.push(ObjectDiff::Added(ri[j].clone())); j += 1; }
            Ordering::Equal => {
                let fields = field_diff(li[i], ri[j]);
                out.push(if fields.is_empty() {
                    ObjectDiff::Unchanged(li[i].clone())
                } else {
                    ObjectDiff::Changed { left: li[i].clone(), right: ri[j].clone(), fields }
                });
                i += 1; j += 1;
            }
        }
    }
    while i < li.len() { out.push(ObjectDiff::Removed(li[i].clone())); i += 1; }
    while j < ri.len() { out.push(ObjectDiff::Added(ri[j].clone())); j += 1; }
    out
}

fn diff_table_top_fields(l: &TableInfo, r: &TableInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.kind != r.kind {
        out.push(FieldChange { field: "kind".into(), left: format!("{:?}", l.kind), right: format!("{:?}", r.kind) });
    }
    out
}

fn fmt_fk(fk: &Option<FkRef>) -> String {
    match fk {
        None => String::new(),
        Some(f) => format!("{}.{}.{}", f.schema.as_deref().unwrap_or(""), f.table, f.column),
    }
}

fn diff_column_fields(l: &ColumnInfo, r: &ColumnInfo, mode: CompareMode) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if mode == CompareMode::SameEngine {
        if l.data_type != r.data_type {
            out.push(FieldChange { field: "data_type".into(), left: l.data_type.clone(), right: r.data_type.clone() });
        }
        if l.nullable != r.nullable {
            out.push(FieldChange { field: "nullable".into(), left: l.nullable.to_string(), right: r.nullable.to_string() });
        }
        if l.default != r.default {
            out.push(FieldChange {
                field: "default".into(),
                left: l.default.clone().unwrap_or_default(),
                right: r.default.clone().unwrap_or_default(),
            });
        }
    }
    // Structural, compared in BOTH modes (design §1).
    if l.is_pk != r.is_pk {
        out.push(FieldChange { field: "is_pk".into(), left: l.is_pk.to_string(), right: r.is_pk.to_string() });
    }
    if l.fk != r.fk {
        out.push(FieldChange { field: "fk".into(), left: fmt_fk(&l.fk), right: fmt_fk(&r.fk) });
    }
    out
}

fn diff_index_fields(l: &IndexInfo, r: &IndexInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.columns != r.columns {
        out.push(FieldChange { field: "columns".into(), left: l.columns.join(", "), right: r.columns.join(", ") });
    }
    if l.unique != r.unique {
        out.push(FieldChange { field: "unique".into(), left: l.unique.to_string(), right: r.unique.to_string() });
    }
    out
}

fn diff_constraint_fields(l: &ConstraintInfo, r: &ConstraintInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.kind != r.kind {
        out.push(FieldChange { field: "kind".into(), left: l.kind.clone(), right: r.kind.clone() });
    }
    if l.definition != r.definition {
        out.push(FieldChange { field: "definition".into(), left: l.definition.clone(), right: r.definition.clone() });
    }
    out
}

fn diff_routine_fields(l: &RoutineInfo, r: &RoutineInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.kind != r.kind {
        out.push(FieldChange { field: "kind".into(), left: format!("{:?}", l.kind), right: format!("{:?}", r.kind) });
    }
    if l.signature != r.signature {
        out.push(FieldChange { field: "signature".into(), left: l.signature.clone(), right: r.signature.clone() });
    }
    out
}

fn diff_trigger_fields(l: &TriggerInfo, r: &TriggerInfo) -> Vec<FieldChange> {
    let mut out = Vec::new();
    if l.table != r.table {
        out.push(FieldChange { field: "table".into(), left: l.table.clone(), right: r.table.clone() });
    }
    if l.ddl != r.ddl {
        out.push(FieldChange {
            field: "ddl".into(),
            left: l.ddl.clone().unwrap_or_default(),
            right: r.ddl.clone().unwrap_or_default(),
        });
    }
    out
}

fn table_diff_removed(t: &TableInfo) -> TableDiff {
    TableDiff {
        schema: t.schema.clone(), name: t.name.clone(), status: TableStatus::Removed,
        table_fields: Vec::new(),
        columns: t.columns.iter().map(|c| ObjectDiff::Removed(c.clone())).collect(),
        indexes: t.indexes.iter().map(|x| ObjectDiff::Removed(x.clone())).collect(),
        constraints: t.constraints.iter().map(|x| ObjectDiff::Removed(x.clone())).collect(),
        left: Some(t.clone()), right: None,
    }
}
fn table_diff_added(t: &TableInfo) -> TableDiff {
    TableDiff {
        schema: t.schema.clone(), name: t.name.clone(), status: TableStatus::Added,
        table_fields: Vec::new(),
        columns: t.columns.iter().map(|c| ObjectDiff::Added(c.clone())).collect(),
        indexes: t.indexes.iter().map(|x| ObjectDiff::Added(x.clone())).collect(),
        constraints: t.constraints.iter().map(|x| ObjectDiff::Added(x.clone())).collect(),
        left: None, right: Some(t.clone()),
    }
}
fn table_diff_matched(l: &TableInfo, r: &TableInfo, mode: CompareMode) -> TableDiff {
    let table_fields = diff_table_top_fields(l, r);
    let columns = diff_by_key(&l.columns, &r.columns, column_key, |a, b| diff_column_fields(a, b, mode));
    let indexes = diff_by_key(&l.indexes, &r.indexes, index_key, diff_index_fields);
    let constraints = diff_by_key(&l.constraints, &r.constraints, constraint_key, diff_constraint_fields);
    let any_change = !table_fields.is_empty()
        || columns.iter().any(|c| !matches!(c, ObjectDiff::Unchanged(_)))
        || indexes.iter().any(|c| !matches!(c, ObjectDiff::Unchanged(_)))
        || constraints.iter().any(|c| !matches!(c, ObjectDiff::Unchanged(_)));
    TableDiff {
        schema: l.schema.clone(), name: l.name.clone(),
        status: if any_change { TableStatus::Changed } else { TableStatus::Unchanged },
        table_fields, columns, indexes, constraints,
        left: Some(l.clone()), right: Some(r.clone()),
    }
}

fn diff_tables(left: &[TableInfo], right: &[TableInfo], mode: CompareMode) -> Vec<TableDiff> {
    let mut li: Vec<&TableInfo> = left.iter().collect();
    let mut ri: Vec<&TableInfo> = right.iter().collect();
    li.sort_by_key(|t| table_key(t));
    ri.sort_by_key(|t| table_key(t));

    let mut out = Vec::with_capacity(li.len().max(ri.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < li.len() && j < ri.len() {
        match table_key(li[i]).cmp(&table_key(ri[j])) {
            Ordering::Less => { out.push(table_diff_removed(li[i])); i += 1; }
            Ordering::Greater => { out.push(table_diff_added(ri[j])); j += 1; }
            Ordering::Equal => { out.push(table_diff_matched(li[i], ri[j], mode)); i += 1; j += 1; }
        }
    }
    while i < li.len() { out.push(table_diff_removed(li[i])); i += 1; }
    while j < ri.len() { out.push(table_diff_added(ri[j])); j += 1; }
    out
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

#[cfg(test)]
mod diff_schema_tests {
    use super::*;
    use dbc_core::{TableKind, RoutineKind};

    fn table(schema: Option<&str>, name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
        TableInfo { schema: schema.map(String::from), name: name.into(), kind: TableKind::Table, columns: cols, indexes: vec![], constraints: vec![], ddl: None }
    }
    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !pk, default: None, is_pk: pk, fk: None }
    }
    fn snap(tables: Vec<TableInfo>) -> SchemaSnapshot {
        SchemaSnapshot { tables, routines: vec![], triggers: vec![], sequences: vec![] }
    }

    #[test]
    fn table_added_removed_unchanged() {
        let left = snap(vec![table(Some("public"), "a", vec![]), table(Some("public"), "b", vec![])]);
        let right = snap(vec![table(Some("public"), "b", vec![]), table(Some("public"), "c", vec![])]);
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        assert_eq!(d.tables.len(), 3);
        assert_eq!(d.tables[0].name, "a"); assert_eq!(d.tables[0].status, TableStatus::Removed);
        assert_eq!(d.tables[1].name, "b"); assert_eq!(d.tables[1].status, TableStatus::Unchanged);
        assert_eq!(d.tables[2].name, "c"); assert_eq!(d.tables[2].status, TableStatus::Added);
        // Added/Removed carry the full source object for DDL rendering.
        assert!(d.tables[0].left.is_some() && d.tables[0].right.is_none());
        assert!(d.tables[2].left.is_none() && d.tables[2].right.is_some());
    }

    #[test]
    fn table_changed_on_kind_field() {
        let mut r_table = table(Some("public"), "v", vec![]);
        r_table.kind = TableKind::View;
        let left = snap(vec![table(Some("public"), "v", vec![])]);
        let right = snap(vec![r_table]);
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        assert_eq!(d.tables[0].status, TableStatus::Changed);
        assert_eq!(d.tables[0].table_fields, vec![FieldChange { field: "kind".into(), left: "Table".into(), right: "View".into() }]);
    }

    #[test]
    fn column_data_type_change_detected_same_engine() {
        let left = snap(vec![table(Some("p"), "t", vec![col("id", "int4", true)])]);
        let right = snap(vec![table(Some("p"), "t", vec![col("id", "int8", true)])]);
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        assert_eq!(d.tables[0].status, TableStatus::Changed);
        assert!(matches!(&d.tables[0].columns[0], ObjectDiff::Changed { fields, .. } if fields.iter().any(|f| f.field == "data_type")));
    }

    #[test]
    fn cross_engine_suppresses_column_field_diff_but_keeps_existence() {
        let left = snap(vec![table(Some("p"), "t", vec![col("id", "int4", true), col("gone", "text", false)])]);
        let right = snap(vec![table(Some("p"), "t", vec![col("id", "integer", true), col("new_col", "text", false)])]);
        let d = diff_schema(&left, &right, CompareMode::CrossEngine);
        // "id" present both sides, different data_type text — but cross-engine
        // never flags a type-text difference as Changed.
        assert!(d.tables[0]
            .columns
            .iter()
            .any(|c| matches!(c, ObjectDiff::Unchanged(c) if c.name == "id")));
        // Existence-level diff still fires fully.
        assert!(d.tables[0].columns.iter().any(|c| matches!(c, ObjectDiff::Removed(c) if c.name == "gone")));
        assert!(d.tables[0].columns.iter().any(|c| matches!(c, ObjectDiff::Added(c) if c.name == "new_col")));
    }

    #[test]
    fn cross_engine_still_flags_is_pk_structural_change() {
        let left = snap(vec![table(Some("p"), "t", vec![col("id", "int4", true)])]);
        let right = snap(vec![table(Some("p"), "t", vec![col("id", "integer", false)])]);
        let d = diff_schema(&left, &right, CompareMode::CrossEngine);
        assert!(matches!(&d.tables[0].columns[0], ObjectDiff::Changed { fields, .. } if fields.iter().any(|f| f.field == "is_pk")));
    }

    #[test]
    fn none_schema_never_matches_a_named_schema() {
        let left = snap(vec![table(None, "t", vec![])]);
        let right = snap(vec![table(Some("public"), "t", vec![])]);
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        assert_eq!(d.tables.len(), 2, "None-schema must NOT match Some(\"public\") — CURATION binding decision");
        assert!(d.tables.iter().any(|t| t.status == TableStatus::Removed && t.schema.is_none()));
        assert!(d.tables.iter().any(|t| t.status == TableStatus::Added && t.schema.as_deref() == Some("public")));
    }

    #[test]
    fn routine_overload_split_not_paired() {
        fn routine(name: &str, sig: &str) -> RoutineInfo {
            RoutineInfo { schema: Some("p".into()), name: name.into(), kind: RoutineKind::Function, signature: sig.into(), ddl: None }
        }
        // Left has TWO overloads of "f"; right has ONE. Design §1: no
        // signature-aware pairing — the excess entry is a plain Removed.
        let left = SchemaSnapshot { tables: vec![], routines: vec![routine("f", "(int) -> int"), routine("f", "(text) -> int")], triggers: vec![], sequences: vec![] };
        let right = SchemaSnapshot { tables: vec![], routines: vec![routine("f", "(int) -> int")], triggers: vec![], sequences: vec![] };
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        let removed = d.routines.iter().filter(|r| matches!(r, ObjectDiff::Removed(_))).count();
        let matched = d.routines.iter().filter(|r| matches!(r, ObjectDiff::Unchanged(_) | ObjectDiff::Changed { .. })).count();
        assert_eq!((matched, removed), (1, 1), "one overload pairs, the excess is Removed — never re-paired by signature");
    }

    #[test]
    fn sequences_are_never_changed_presence_only() {
        let left = SchemaSnapshot { tables: vec![], routines: vec![], triggers: vec![], sequences: vec![SequenceInfo { schema: Some("p".into()), name: "s".into() }] };
        let right = left.clone();
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        assert!(matches!(d.sequences[0], ObjectDiff::Unchanged(_)));
    }

    #[test]
    fn deterministic_output_order_regardless_of_input_order() {
        let left = snap(vec![table(Some("p"), "zeta", vec![]), table(Some("p"), "alpha", vec![])]);
        let right = left.clone();
        let d = diff_schema(&left, &right, CompareMode::SameEngine);
        let names: Vec<&str> = d.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"], "output must be sorted by (schema, name), not input order");
    }
}
