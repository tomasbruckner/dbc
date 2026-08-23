use std::collections::BTreeMap;

use crate::schema::TableInfo;

pub mod layout;
pub mod svg;

pub const MAX_VISIBLE_COLS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableKey {
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErdColumnRow {
    pub name: String,
    pub data_type: String,
    pub is_pk: bool,
    pub is_fk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErdNode {
    pub key: TableKey,
    /// PK columns first, then FK columns, capped at `MAX_VISIBLE_COLS`.
    /// A column that is both PK and FK appears once, with both flags set.
    pub visible_cols: Vec<ErdColumnRow>,
    /// Footer count for "+N dalších" (0 = no footer row).
    pub hidden_col_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkEdge {
    pub from: TableKey,
    pub to: TableKey,
    /// (from_column, to_column) pairs — every column of every FK
    /// constraint from `from` to `to` collapses into ONE edge (design §0).
    pub columns: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ErdGraph {
    pub nodes: Vec<ErdNode>,
    pub edges: Vec<FkEdge>,
}

/// Pure, no I/O. `tables` is caller-selected (design §3: always exactly
/// one schema's worth, scoped upstream by T6/T7 — this function has no
/// opinion on selection, it just builds a graph over whatever slice it's
/// given).
pub fn build_graph(tables: &[TableInfo]) -> ErdGraph {
    let mut nodes = Vec::with_capacity(tables.len());
    // BTreeMap keyed by the ordered (from, to) pair -> deterministic
    // iteration order for free, and collapses every FK column pair between
    // the same ordered table pair into one edge (design §0).
    let mut edge_map: BTreeMap<(TableKey, TableKey), Vec<(String, String)>> = BTreeMap::new();

    for t in tables {
        let key = TableKey { schema: t.schema.clone(), name: t.name.clone() };

        let mut rows: Vec<ErdColumnRow> = t
            .columns
            .iter()
            .filter(|c| c.is_pk || c.fk.is_some())
            .map(|c| ErdColumnRow {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                is_pk: c.is_pk,
                is_fk: c.fk.is_some(),
            })
            .collect();
        // Stable sort: PK rows first, FK-only rows after, original catalog
        // order preserved within each group.
        rows.sort_by_key(|c| !c.is_pk);
        let hidden_col_count = rows.len().saturating_sub(MAX_VISIBLE_COLS);
        rows.truncate(MAX_VISIBLE_COLS);

        nodes.push(ErdNode { key: key.clone(), visible_cols: rows, hidden_col_count });

        for c in &t.columns {
            if let Some(fk) = &c.fk {
                let to = TableKey { schema: fk.schema.clone(), name: fk.table.clone() };
                edge_map.entry((key.clone(), to)).or_default().push((c.name.clone(), fk.column.clone()));
            }
        }
    }

    nodes.sort_by(|a, b| a.key.cmp(&b.key));
    let edges = edge_map.into_iter().map(|((from, to), columns)| FkEdge { from, to, columns }).collect();

    ErdGraph { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColumnInfo, FkRef, TableInfo};

    fn col(name: &str, ty: &str, pk: bool, fk: Option<(&str, &str)>) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            default: None,
            is_pk: pk,
            fk: fk.map(|(table, column)| FkRef { schema: None, table: table.into(), column: column.into() }),
        }
    }
    fn table(name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
        TableInfo { schema: None, name: name.into(), columns: cols, ..Default::default() }
    }
    fn key(name: &str) -> TableKey {
        TableKey { schema: None, name: name.into() }
    }

    #[test]
    fn composite_fk_collapses_to_one_edge_with_two_column_pairs() {
        let orders = table(
            "orders",
            vec![
                col("id", "int4", true, None),
                col("addr_country", "text", false, Some(("addresses", "country"))),
                col("addr_id", "int4", false, Some(("addresses", "id"))),
            ],
        );
        let addresses = table("addresses", vec![col("id", "int4", true, None), col("country", "text", true, None)]);
        let g = build_graph(&[orders, addresses]);
        assert_eq!(g.edges.len(), 1, "two FK columns to the same table must collapse into one edge");
        let e = &g.edges[0];
        assert_eq!(e.from, key("orders"));
        assert_eq!(e.to, key("addresses"));
        assert_eq!(e.columns.len(), 2);
    }

    #[test]
    fn self_reference_is_a_normal_edge_with_from_equal_to() {
        let employees = table(
            "employees",
            vec![col("id", "int4", true, None), col("manager_id", "int4", false, Some(("employees", "id")))],
        );
        let g = build_graph(&[employees]);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].from, g.edges[0].to);
        assert_eq!(g.edges[0].from, key("employees"));
    }

    #[test]
    fn bidirectional_pair_is_two_distinct_edges() {
        let a = table("a", vec![col("id", "int4", true, None), col("b_id", "int4", false, Some(("b", "id")))]);
        let b = table("b", vec![col("id", "int4", true, None), col("a_id", "int4", false, Some(("a", "id")))]);
        let g = build_graph(&[a, b]);
        assert_eq!(g.edges.len(), 2);
        assert!(g.edges.iter().any(|e| e.from == key("a") && e.to == key("b")));
        assert!(g.edges.iter().any(|e| e.from == key("b") && e.to == key("a")));
    }

    #[test]
    fn isolated_table_is_present_with_zero_edges() {
        let lonely = table("lonely", vec![col("id", "int4", true, None)]);
        let g = build_graph(&[lonely]);
        assert_eq!(g.nodes.len(), 1);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn node_columns_cap_at_max_visible_with_footer_count() {
        // All 9 columns are PK-eligible (this test exercises the
        // cap/footer behavior on eligible columns, not the PK/FK filter
        // itself — that's covered by `non_pk_non_fk_columns_are_never_shown`).
        let cols: Vec<ColumnInfo> = (0..9).map(|i| col(&format!("c{i}"), "int4", true, None)).collect();
        let t = table("wide", cols);
        let g = build_graph(&[t]);
        assert_eq!(g.nodes[0].visible_cols.len(), MAX_VISIBLE_COLS);
        assert_eq!(g.nodes[0].hidden_col_count, 9 - MAX_VISIBLE_COLS);
    }

    #[test]
    fn non_pk_non_fk_columns_are_never_shown() {
        let t = table("t", vec![col("id", "int4", true, None), col("note", "text", false, None)]);
        let g = build_graph(&[t]);
        assert_eq!(g.nodes[0].visible_cols.len(), 1);
        assert_eq!(g.nodes[0].hidden_col_count, 0, "note isn't PK/FK, so it's just absent, not counted as hidden");
    }
}
