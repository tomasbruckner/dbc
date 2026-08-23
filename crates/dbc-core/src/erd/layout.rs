use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use super::{ErdGraph, ErdNode, TableKey};

pub const NODE_WIDTH: f32 = 220.0;
pub const HEADER_H: f32 = 24.0;
pub const ROW_H: f32 = 18.0;
pub const FOOTER_H: f32 = 16.0;
pub const LAYER_GAP: f32 = 60.0;
pub const COL_GAP: f32 = 40.0;
pub const ISOLATED_COLS_PER_ROW: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct PositionedNode {
    pub key: TableKey,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEdge {
    pub from: TableKey,
    pub to: TableKey,
    pub points: Vec<(f32, f32)>,
    pub is_self_loop: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiagramLayout {
    pub nodes: Vec<PositionedNode>,
    pub edges: Vec<RoutedEdge>,
}

fn node_height(n: &ErdNode) -> f32 {
    let rows = n.visible_cols.len().min(6) as f32;
    let footer = if n.hidden_col_count > 0 { FOOTER_H } else { 0.0 };
    HEADER_H + rows * ROW_H + footer
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

/// Iterative DFS (Global Constraints — deep-graph hazard: no recursion on a
/// user-controlled FK graph). Returns the set of (u, v) edges classified as
/// back edges. Iteration cap is a defensive backstop only — this algorithm
/// is provably O(V+E); the cap only ever trips on a bug.
fn classify_back_edges(nodes: &[TableKey], adj: &BTreeMap<TableKey, Vec<TableKey>>) -> HashSet<(TableKey, TableKey)> {
    let mut color: HashMap<TableKey, Color> = nodes.iter().cloned().map(|k| (k, Color::White)).collect();
    let mut back_edges = HashSet::new();
    let edge_count: usize = adj.values().map(|v| v.len()).sum();
    let cap = 10 * (nodes.len() + edge_count).max(1) + 10_000;
    let mut steps = 0usize;

    for start in nodes {
        if color.get(start).copied() != Some(Color::White) {
            continue;
        }
        let mut stack: Vec<(TableKey, usize)> = vec![(start.clone(), 0)];
        color.insert(start.clone(), Color::Gray);
        while let Some((node, idx)) = stack.last().cloned() {
            steps += 1;
            assert!(steps < cap, "erd layout: DFS exceeded iteration cap — graph invariant violated");
            let neighbours = adj.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
            if idx < neighbours.len() {
                stack.last_mut().unwrap().1 += 1;
                let next = neighbours[idx].clone();
                match color.get(&next).copied().unwrap_or(Color::Black) {
                    Color::White => {
                        color.insert(next.clone(), Color::Gray);
                        stack.push((next, 0));
                    }
                    Color::Gray => {
                        back_edges.insert((node.clone(), next));
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node.clone(), Color::Black);
                stack.pop();
            }
        }
    }
    back_edges
}

/// Kahn's algorithm doubling as longest-path layering: `layer[v]` is
/// relaxed on EVERY incoming edge, but `v` is only enqueued once its
/// indegree reaches zero — by then every predecessor has already relaxed
/// it, so the final value is the true longest-path layer. Iterative by
/// construction (a work queue, not recursion); same defensive cap as above.
fn assign_layers(nodes: &[TableKey], acyclic_edges: &[(TableKey, TableKey)]) -> HashMap<TableKey, usize> {
    let mut indegree: HashMap<TableKey, usize> = nodes.iter().cloned().map(|k| (k, 0)).collect();
    let mut succ: BTreeMap<TableKey, Vec<TableKey>> = BTreeMap::new();
    // Defensive: `acyclic_edges` is expected to only reference keys in
    // `nodes` (compute_layout guarantees this by filtering out edges with
    // an unknown endpoint before this is ever called), but an edge with a
    // stray endpoint is silently dropped here rather than panicking — this
    // function has no I/O and no way to recover otherwise, and a caller
    // bug shouldn't crash the whole app on layout.
    for (u, v) in acyclic_edges {
        if let Some(d) = indegree.get_mut(v) {
            *d += 1;
            succ.entry(u.clone()).or_default().push(v.clone());
        }
    }

    let mut layer: HashMap<TableKey, usize> = HashMap::new();
    let mut initial: Vec<TableKey> = nodes.iter().filter(|k| indegree[*k] == 0).cloned().collect();
    initial.sort();
    for k in &initial {
        layer.insert(k.clone(), 0);
    }
    let mut queue: VecDeque<TableKey> = initial.into();

    let cap = 10 * (nodes.len() + acyclic_edges.len()).max(1) + 10_000;
    let mut steps = 0usize;
    while let Some(u) = queue.pop_front() {
        steps += 1;
        assert!(steps < cap, "erd layout: layering exceeded iteration cap — graph invariant violated");
        let ul = layer[&u];
        if let Some(children) = succ.get(&u) {
            for v in children {
                let candidate = ul + 1;
                let better = layer.get(v).map_or(true, |&cur| candidate > cur);
                if better {
                    layer.insert(v.clone(), candidate);
                }
                let d = indegree.get_mut(v).expect("known node");
                *d -= 1;
                if *d == 0 {
                    queue.push_back(v.clone());
                }
            }
        }
    }
    layer
}

/// Barycenter crossing reduction, fixed 4 sweeps (no convergence loop —
/// Global Constraints/design §2). Ties (including "no neighbour in the
/// adjacent layer") broken by table name, ascending.
fn barycenter_reorder(
    layers: &mut [Vec<TableKey>],
    preds: &HashMap<TableKey, Vec<TableKey>>,
    succs: &HashMap<TableKey, Vec<TableKey>>,
) {
    fn position_index(layer: &[TableKey]) -> HashMap<TableKey, usize> {
        layer.iter().enumerate().map(|(i, k)| (k.clone(), i)).collect()
    }
    fn reorder_one(layer: &mut [TableKey], neighbour_pos: &HashMap<TableKey, usize>, neighbours_of: &HashMap<TableKey, Vec<TableKey>>) {
        let mut scored: Vec<(Option<f64>, TableKey)> = layer
            .iter()
            .map(|k| {
                let ns = neighbours_of.get(k).map(|v| v.as_slice()).unwrap_or(&[]);
                let idxs: Vec<f64> = ns.iter().filter_map(|n| neighbour_pos.get(n).map(|&i| i as f64)).collect();
                let bary = if idxs.is_empty() { None } else { Some(idxs.iter().sum::<f64>() / idxs.len() as f64) };
                (bary, k.clone())
            })
            .collect();
        scored.sort_by(|a, b| match (a.0, b.0) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.name.cmp(&b.1.name)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.1.name.cmp(&b.1.name),
        });
        for (slot, (_, k)) in layer.iter_mut().zip(scored.into_iter()) {
            *slot = k;
        }
    }

    for iter in 0..4 {
        if iter % 2 == 0 {
            for i in 1..layers.len() {
                let pos = position_index(&layers[i - 1]);
                reorder_one(&mut layers[i], &pos, preds);
            }
        } else {
            for i in (0..layers.len().saturating_sub(1)).rev() {
                let pos = position_index(&layers[i + 1]);
                reorder_one(&mut layers[i], &pos, succs);
            }
        }
    }
}

fn clip_to_rect_edge(center: (f32, f32), toward: (f32, f32), half_w: f32, half_h: f32) -> (f32, f32) {
    let dx = toward.0 - center.0;
    let dy = toward.1 - center.1;
    if dx == 0.0 && dy == 0.0 {
        return center;
    }
    let scale_x = if dx != 0.0 { half_w / dx.abs() } else { f32::INFINITY };
    let scale_y = if dy != 0.0 { half_h / dy.abs() } else { f32::INFINITY };
    let scale = scale_x.min(scale_y);
    (center.0 + dx * scale, center.1 + dy * scale)
}

fn self_loop_stub_points(n: &PositionedNode) -> Vec<(f32, f32)> {
    let right = n.x + n.w;
    let top = n.y + n.h * 0.3;
    let bottom = n.y + n.h * 0.7;
    vec![(right, top), (right + 30.0, n.y + n.h * 0.5), (right, bottom)]
}

pub fn compute_layout(graph: &ErdGraph) -> DiagramLayout {
    // Drop any FK edge whose endpoint (either side) isn't a table present
    // in this diagram's node set. `build_graph` doesn't validate `fk.table`/
    // `fk.schema` against the caller-selected slice (erd.rs: "tables is
    // caller-selected... this function has no opinion on selection"), so an
    // ordinary cross-schema FK pointing outside a single-schema slice
    // produces exactly this: an edge whose `to` isn't in `graph.nodes`. The
    // referenced table isn't on this diagram anyway, so the edge is simply
    // not drawn (mirrors export_svg's "no matching table -> silently
    // skipped" posture) instead of feeding a dangling reference into the
    // layering pipeline, where it would strand a node at an indegree that
    // never reaches zero and panic the layer lookup below.
    let known: HashSet<&TableKey> = graph.nodes.iter().map(|n| &n.key).collect();
    let known_edges: Vec<&super::FkEdge> =
        graph.edges.iter().filter(|e| known.contains(&e.from) && known.contains(&e.to)).collect();

    let self_loops: Vec<&super::FkEdge> = known_edges.iter().copied().filter(|e| e.from == e.to).collect();
    let plain_edges: Vec<&super::FkEdge> = known_edges.iter().copied().filter(|e| e.from != e.to).collect();

    let touched: HashSet<&TableKey> = known_edges.iter().flat_map(|e| [&e.from, &e.to]).collect();
    let mut connected: Vec<TableKey> = graph.nodes.iter().map(|n| n.key.clone()).filter(|k| touched.contains(k)).collect();
    let mut isolated: Vec<TableKey> = graph.nodes.iter().map(|n| n.key.clone()).filter(|k| !touched.contains(k)).collect();
    connected.sort();
    isolated.sort();

    let mut adj: BTreeMap<TableKey, Vec<TableKey>> = BTreeMap::new();
    for e in &plain_edges {
        adj.entry(e.from.clone()).or_default().push(e.to.clone());
    }
    let back_edges = classify_back_edges(&connected, &adj);

    // Layering DAG runs opposite to `FkEdge` direction: an FK edge points
    // child->parent (`from`=FK holder, `to`=referenced table), but we want
    // parents to land in lower layer numbers (rendered above their
    // children). So a normal (non-back) edge contributes a (parent, child)
    // = (to, from) layering edge; an edge that DFS classified as a back
    // edge (child->parent, i.e. (from, to), pointing at an on-stack node)
    // is instead kept in its original (from, to) direction here, which is
    // exactly what breaks the cycle in this reversed DAG.
    let acyclic: Vec<(TableKey, TableKey)> = plain_edges
        .iter()
        .map(|e| {
            if back_edges.contains(&(e.from.clone(), e.to.clone())) {
                (e.from.clone(), e.to.clone())
            } else {
                (e.to.clone(), e.from.clone())
            }
        })
        .collect();

    let layer_of = assign_layers(&connected, &acyclic);
    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<TableKey>> = vec![Vec::new(); max_layer + 1];
    for k in &connected {
        // `layer_of` is guaranteed to have an entry for every key in
        // `connected` now that `plain_edges`/`connected` are built from the
        // same known-endpoints-only edge set above — but default to layer 0
        // rather than panicking if that invariant is ever violated by a
        // future change.
        let l = layer_of.get(k).copied().unwrap_or(0);
        layers[l].push(k.clone());
    }

    let mut preds: HashMap<TableKey, Vec<TableKey>> = HashMap::new();
    let mut succs: HashMap<TableKey, Vec<TableKey>> = HashMap::new();
    for (u, v) in &acyclic {
        succs.entry(u.clone()).or_default().push(v.clone());
        preds.entry(v.clone()).or_default().push(u.clone());
    }
    barycenter_reorder(&mut layers, &preds, &succs);

    let node_by_key: HashMap<&TableKey, &ErdNode> = graph.nodes.iter().map(|n| (&n.key, n)).collect();
    let mut xy: HashMap<TableKey, (f32, f32, f32, f32)> = HashMap::new();

    let mut cursor_y = 0.0f32;
    for layer in &layers {
        let layer_h = layer
            .iter()
            .filter_map(|k| node_by_key.get(k).map(|n| node_height(n)))
            .fold(0.0f32, f32::max);
        let total_w = layer.len() as f32 * NODE_WIDTH + layer.len().saturating_sub(1) as f32 * COL_GAP;
        let mut x = -total_w / 2.0;
        for key in layer {
            let h = node_by_key.get(key).map(|n| node_height(n)).unwrap_or(HEADER_H);
            xy.insert(key.clone(), (x, cursor_y, NODE_WIDTH, h));
            x += NODE_WIDTH + COL_GAP;
        }
        cursor_y += layer_h + LAYER_GAP;
    }

    for (i, key) in isolated.iter().enumerate() {
        let row = i / ISOLATED_COLS_PER_ROW;
        let col = i % ISOLATED_COLS_PER_ROW;
        let h = node_by_key.get(key).map(|n| node_height(n)).unwrap_or(HEADER_H);
        let x = col as f32 * (NODE_WIDTH + COL_GAP);
        let y = cursor_y + row as f32 * (h + LAYER_GAP);
        xy.insert(key.clone(), (x, y, NODE_WIDTH, h));
    }

    let mut nodes: Vec<PositionedNode> = graph
        .nodes
        .iter()
        .filter_map(|n| xy.get(&n.key).map(|&(x, y, w, h)| PositionedNode { key: n.key.clone(), x, y, w, h }))
        .collect();
    nodes.sort_by(|a, b| a.key.cmp(&b.key));

    let pos_lookup: HashMap<&TableKey, &PositionedNode> = nodes.iter().map(|p| (&p.key, p)).collect();
    let mut edges: Vec<RoutedEdge> = Vec::with_capacity(graph.edges.len());
    for e in &plain_edges {
        if let (Some(a), Some(b)) = (pos_lookup.get(&e.from), pos_lookup.get(&e.to)) {
            let ca = (a.x + a.w / 2.0, a.y + a.h / 2.0);
            let cb = (b.x + b.w / 2.0, b.y + b.h / 2.0);
            let p1 = clip_to_rect_edge(ca, cb, a.w / 2.0, a.h / 2.0);
            let p2 = clip_to_rect_edge(cb, ca, b.w / 2.0, b.h / 2.0);
            edges.push(RoutedEdge { from: e.from.clone(), to: e.to.clone(), points: vec![p1, p2], is_self_loop: false });
        }
    }
    for e in &self_loops {
        if let Some(a) = pos_lookup.get(&e.from) {
            edges.push(RoutedEdge { from: e.from.clone(), to: e.to.clone(), points: self_loop_stub_points(a), is_self_loop: true });
        }
    }
    edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));

    DiagramLayout { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::erd::{build_graph, ErdColumnRow};
    use crate::schema::{ColumnInfo, FkRef, TableInfo};

    fn col(name: &str, pk: bool, fk: Option<(&str, &str)>) -> ColumnInfo {
        ColumnInfo {
            name: name.into(), data_type: "int4".into(), nullable: !pk, default: None, is_pk: pk,
            fk: fk.map(|(t, c)| FkRef { schema: None, table: t.into(), column: c.into() }),
        }
    }
    fn table(name: &str, cols: Vec<ColumnInfo>) -> TableInfo {
        TableInfo { schema: None, name: name.into(), columns: cols, ..Default::default() }
    }
    #[allow(dead_code)]
    fn key(name: &str) -> TableKey { TableKey { schema: None, name: name.into() } }
    fn layer_of<'a>(layout: &'a DiagramLayout, name: &str) -> f32 {
        layout.nodes.iter().find(|n| n.key.name == name).unwrap().y
    }

    #[test]
    fn single_table_no_edges() {
        let g = build_graph(&[table("t", vec![col("id", true, None)])]);
        let l = compute_layout(&g);
        assert_eq!(l.nodes.len(), 1);
        assert!(l.edges.is_empty());
    }

    #[test]
    fn simple_chain_layers_zero_one_two() {
        let a = table("a", vec![col("id", true, None)]);
        let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
        let c = table("c", vec![col("id", true, None), col("b_id", false, Some(("b", "id")))]);
        // Note: build_graph reads FK direction from the FK-holding side, so
        // edges are b->a and c->b; roots (indegree 0 in the reversed sense
        // used for layering) come from whichever side has no incoming FK.
        let g = build_graph(&[a, b, c]);
        let l = compute_layout(&g);
        let ya = layer_of(&l, "a");
        let yb = layer_of(&l, "b");
        let yc = layer_of(&l, "c");
        assert!(ya < yb && yb < yc, "a should layer above b above c (longest-path from the root)");
    }

    #[test]
    fn self_reference_does_not_affect_other_layers_and_is_marked() {
        let e = table("employees", vec![col("id", true, None), col("manager_id", false, Some(("employees", "id")))]);
        let g = build_graph(&[e]);
        let l = compute_layout(&g);
        assert_eq!(l.nodes.len(), 1);
        assert_eq!(l.edges.len(), 1);
        assert!(l.edges[0].is_self_loop);
        assert_eq!(l.edges[0].points.len(), 3);
    }

    #[test]
    fn bidirectional_pair_terminates_and_keeps_both_edges() {
        let a = table("a", vec![col("id", true, None), col("b_id", false, Some(("b", "id")))]);
        let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
        let g = build_graph(&[a, b]);
        let l = compute_layout(&g); // must return, not hang
        assert_eq!(l.edges.len(), 2);
    }

    #[test]
    fn composite_fk_edge_survives_layout_with_two_column_pairs() {
        let orders = table("orders", vec![
            col("id", true, None),
            col("addr_a", false, Some(("addresses", "a"))),
            col("addr_b", false, Some(("addresses", "b"))),
        ]);
        let addresses = table("addresses", vec![col("a", true, None), col("b", true, None)]);
        let g = build_graph(&[orders, addresses]);
        assert_eq!(g.edges[0].columns.len(), 2);
        let l = compute_layout(&g);
        assert_eq!(l.edges.len(), 1);
    }

    #[test]
    fn diamond_layers_sink_at_longest_path_not_first_arrival() {
        // a -> b -> d, a -> c -> d: FK direction is child->parent in this
        // codebase's schema model, so build the diamond as: b,c FK to a;
        // d FKs to BOTH b and c. Longest path to d must be 2 (via b or c),
        // not 1 (if d were laid out right after a on a first-arrival BFS).
        let a = table("a", vec![col("id", true, None)]);
        let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
        let c = table("c", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
        let d = table("d", vec![
            col("id", true, None),
            col("b_id", false, Some(("b", "id"))),
            col("c_id", false, Some(("c", "id"))),
        ]);
        let g = build_graph(&[a, b, c, d]);
        let l = compute_layout(&g);
        let ya = layer_of(&l, "a");
        let yb = layer_of(&l, "b");
        let yd = layer_of(&l, "d");
        assert!(ya < yb, "a above b");
        assert!(yb < yd, "b above d — d sinks to the longest path, not the first one found");
    }

    #[test]
    fn isolated_table_is_in_a_separate_row_below_connected_layers() {
        let a = table("a", vec![col("id", true, None)]);
        let b = table("b", vec![col("id", true, None), col("a_id", false, Some(("a", "id")))]);
        let lonely = table("lonely", vec![col("id", true, None)]);
        let g = build_graph(&[a, b, lonely]);
        let l = compute_layout(&g);
        let y_lonely = layer_of(&l, "lonely");
        let y_b = layer_of(&l, "b");
        assert!(y_lonely > y_b, "isolated row must sit below every connected layer");
    }

    #[test]
    fn dangling_cross_schema_fk_is_dropped_not_panicked() {
        // build_graph doesn't validate fk.table/fk.schema against the
        // caller-selected slice (erd.rs: "no opinion on selection"), so a
        // single-schema slice with an outbound cross-schema FK is ordinary
        // real-schema input: "parent" is referenced but never in `tables`.
        let child = table("child", vec![col("id", true, None), col("parent_id", false, Some(("parent", "id")))]);
        let g = build_graph(&[child]);
        assert_eq!(g.edges.len(), 1, "build_graph still records the dangling FK as an edge");
        let l = compute_layout(&g); // must not panic
        assert_eq!(l.nodes.len(), 1);
        assert_eq!(l.nodes[0].key.name, "child");
        assert!(l.edges.is_empty(), "dangling FK edge must not be drawn — the referenced table isn't on this diagram");
    }

    #[test]
    fn deterministic_same_input_twice_is_byte_identical() {
        let a = table("z_table", vec![col("id", true, None)]);
        let b = table("a_table", vec![col("id", true, None), col("z_id", false, Some(("z_table", "id")))]);
        let g = build_graph(&[a, b]);
        let l1 = compute_layout(&g);
        let l2 = compute_layout(&g);
        assert_eq!(l1, l2);
    }

    #[test]
    fn wide_node_column_row_metadata_is_preserved_through_graph_build() {
        // Sanity: layout doesn't need to inspect ErdColumnRow itself, but
        // confirms the type import above is exercised and the pipeline
        // compiles end to end with a non-trivial node.
        let t = table("t", vec![col("id", true, None)]);
        let g = build_graph(&[t]);
        let row: &ErdColumnRow = &g.nodes[0].visible_cols[0];
        assert!(row.is_pk);
    }
}
