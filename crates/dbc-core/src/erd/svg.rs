use std::collections::HashMap;

use super::layout::{DiagramLayout, PositionedNode, RoutedEdge};
use super::TableKey;
use crate::schema::TableInfo;

const BG: &str = "#1e1e2e";
const NODE_FILL: &str = "#313244";
const NODE_BORDER: &str = "#45475a";
const TEXT_COLOR: &str = "#cdd6f4";
const EDGE_COLOR: &str = "#89b4fa";
const MUTED: &str = "#6c7086";

/// Escapes the five XML-significant characters. The ONE function every
/// interpolated string (table/column/schema name) MUST pass through
/// before reaching the output string — CURATION-binding (Global
/// Constraints).
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // C0 control chars are illegal in XML 1.0 even as numeric
            // character references (only #x9/#xA/#xD are permitted) — a NUL
            // in an identifier would make the document unparseable. Drop
            // them rather than emit spec-invalid output.
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            _ => out.push(c),
        }
    }
    out
}

fn header_text(t: &TableInfo) -> String {
    match &t.schema {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

/// Defensive guard (security-critical, non-negotiable per curation): a
/// `DiagramLayout` produced by a correct `compute_layout` is always
/// finite, but this is the single choke point every numeric coordinate
/// passes through before reaching the SVG string, so a future layout bug
/// (or a hand-built `DiagramLayout`, as the `missing_table_for_a_positioned_node_is_skipped_not_a_panic`
/// test constructs) can never leak `NaN`/`inf` into the exported XML.
fn fmt_coord(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn node_to_svg(pos: &PositionedNode, t: &TableInfo) -> String {
    let (x, y, w, h) = (fmt_coord(pos.x), fmt_coord(pos.y), fmt_coord(pos.w), fmt_coord(pos.h));
    let mut s = String::new();
    s.push_str(&format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"4\" fill=\"{NODE_FILL}\" stroke=\"{NODE_BORDER}\" stroke-width=\"1\"/>\n",
        x, y, w, h
    ));
    s.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{TEXT_COLOR}\" font-weight=\"bold\" font-family=\"sans-serif\" font-size=\"13\">{}</text>\n",
        x + 8.0,
        y + 16.0,
        escape_xml(&header_text(t))
    ));
    let mut row_y = y + 24.0 + 13.0;
    for c in t.columns.iter().filter(|c| c.is_pk || c.fk.is_some()).take(6) {
        let marker = if c.is_pk { "PK " } else if c.fk.is_some() { "FK " } else { "" };
        let line = format!("{marker}{}: {}", c.name, c.data_type);
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"{TEXT_COLOR}\" font-family=\"sans-serif\" font-size=\"11\">{}</text>\n",
            x + 8.0, row_y, escape_xml(&line)
        ));
        row_y += 18.0;
    }
    s
}

fn edge_to_svg(e: &RoutedEdge) -> String {
    if e.is_self_loop && e.points.len() == 3 {
        let (x0, y0) = (fmt_coord(e.points[0].0), fmt_coord(e.points[0].1));
        let (cx, cy) = (fmt_coord(e.points[1].0), fmt_coord(e.points[1].1));
        let (x1, y1) = (fmt_coord(e.points[2].0), fmt_coord(e.points[2].1));
        format!("<path d=\"M {x0:.1} {y0:.1} Q {cx:.1} {cy:.1} {x1:.1} {y1:.1}\" fill=\"none\" stroke=\"{EDGE_COLOR}\" stroke-width=\"1.5\"/>\n")
    } else if e.points.len() == 2 {
        let (x0, y0) = (fmt_coord(e.points[0].0), fmt_coord(e.points[0].1));
        let (x1, y1) = (fmt_coord(e.points[1].0), fmt_coord(e.points[1].1));
        format!("<line x1=\"{x0:.1}\" y1=\"{y0:.1}\" x2=\"{x1:.1}\" y2=\"{y1:.1}\" stroke=\"{EDGE_COLOR}\" stroke-width=\"1.5\"/>\n")
    } else {
        String::new() // defensive: malformed RoutedEdge, never emitted by compute_layout
    }
}

/// Builds a complete, standalone SVG document from the SAME
/// `DiagramLayout` the canvas paints from (T4/T5) — screen and export
/// can never visually diverge. `tables` is looked up by `TableKey` to
/// recover full column lists/types for node text; a `PositionedNode`
/// with no matching `TableInfo` is silently skipped (defensive, should
/// never happen given both are derived from the same snapshot).
pub fn export_svg(layout: &DiagramLayout, tables: &[TableInfo]) -> String {
    let by_key: HashMap<TableKey, &TableInfo> =
        tables.iter().map(|t| (TableKey { schema: t.schema.clone(), name: t.name.clone() }, t)).collect();

    let (w, h) = layout.nodes.iter().fold((800.0f32, 600.0f32), |(mw, mh), n| {
        (mw.max(fmt_coord(n.x + n.w + 40.0)), mh.max(fmt_coord(n.y + n.h + 40.0)))
    });

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.1}\" height=\"{h:.1}\" viewBox=\"0 0 {w:.1} {h:.1}\">\n"
    ));
    svg.push_str(&format!("<rect width=\"{w:.1}\" height=\"{h:.1}\" fill=\"{BG}\"/>\n"));
    let _ = MUTED; // reserved for the footer "+N dalších" row (T6 visual polish, not load-bearing on export correctness)

    for e in &layout.edges {
        svg.push_str(&edge_to_svg(e));
    }
    for n in &layout.nodes {
        if let Some(t) = by_key.get(&n.key) {
            svg.push_str(&node_to_svg(n, t));
        }
    }
    svg.push_str("</svg>\n");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::erd::{build_graph, layout::compute_layout};
    use crate::schema::{ColumnInfo, FkRef, TableInfo};

    fn col(name: &str, pk: bool, fk: Option<(&str, &str)>) -> ColumnInfo {
        ColumnInfo {
            name: name.into(), data_type: "int4".into(), nullable: !pk, default: None, is_pk: pk,
            fk: fk.map(|(t, c)| FkRef { schema: None, table: t.into(), column: c.into() }),
        }
    }

    #[test]
    fn escape_xml_covers_all_five_characters() {
        assert_eq!(escape_xml("a<b>c&d\"e'f"), "a&lt;b&gt;c&amp;d&quot;e&apos;f");
        assert_eq!(escape_xml("plain"), "plain");
    }

    #[test]
    fn escape_xml_drops_illegal_c0_controls_but_keeps_tab_lf_cr() {
        // NUL and other C0 controls are unrepresentable in XML 1.0 — dropped.
        assert_eq!(escape_xml("evil\u{0}name\u{1}\u{b}"), "evilname");
        // The three XML-legal whitespace controls survive.
        assert_eq!(escape_xml("a\tb\nc\rd"), "a\tb\nc\rd");
    }

    // REQUIRED test (Global Constraints, CURATION-binding): a table named
    // `we"ird<x>` — plus a column and a schema each carrying a different
    // dangerous character — must never leak an un-escaped `<`, `>`, `&`,
    // `"`, or `'` into the output outside the fixed SVG syntax this
    // serializer itself emits.
    #[test]
    fn hostile_identifiers_produce_fully_escaped_svg() {
        let evil_table = TableInfo {
            schema: Some("sch&ema".into()),
            name: "we\"ird<x>".into(),
            columns: vec![
                col("id", true, None),
                ColumnInfo {
                    name: "a'b".into(),
                    data_type: "text".into(),
                    nullable: false,
                    default: None,
                    is_pk: false,
                    fk: Some(FkRef { schema: None, table: "we\"ird<x>".into(), column: "id".into() }),
                },
            ],
            ..Default::default()
        };
        let g = build_graph(&[evil_table.clone()]);
        let l = compute_layout(&g);
        let svg = export_svg(&l, &[evil_table]);

        // Escaped forms present.
        assert!(svg.contains("we&quot;ird&lt;x&gt;"), "table name must be escaped: {svg}");
        assert!(svg.contains("sch&amp;ema"), "schema name must be escaped: {svg}");
        assert!(svg.contains("a&apos;b"), "column name must be escaped: {svg}");

        // Extract only the <text>...</text> payload substrings (the one
        // place hostile identifier text is interpolated) and assert none
        // of them contain a raw dangerous character — the fixed SVG
        // syntax around them (attribute quotes, tag brackets) legitimately
        // contains '<'/'>'/'"' and must not be flagged as a false
        // positive, so this check is scoped to text-node payloads only.
        // Scoped by the "<text" tag marker itself (not just the next '>'
        // anywhere in the document) — other elements (the background/node
        // `<rect>`s) also close with '>' before the first `<text>` starts,
        // which would otherwise pull unrelated markup into the "payload".
        let mut idx = 0;
        let mut payloads = Vec::new();
        while let Some(tag_start) = svg[idx..].find("<text").map(|p| idx + p) {
            if let Some(gt_offset) = svg[tag_start..].find('>') {
                let start = tag_start + gt_offset + 1;
                if let Some(end_offset) = svg[start..].find("</text>") {
                    let end = start + end_offset;
                    payloads.push(&svg[start..end]);
                    idx = end + "</text>".len();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        assert!(!payloads.is_empty(), "sanity: expected at least one <text> payload to check");
        for p in &payloads {
            assert!(!p.contains('<') && !p.contains('>'), "text payload must never contain a raw angle bracket: {p}");
        }

        // Well-formedness sanity (no XML parser dependency added to
        // dbc-core for this — a balanced open/close <text> tag count is a
        // cheap, dependency-free proxy that would fail if escaping ever
        // let a raw '<' truncate/split a tag).
        let opens = svg.matches("<text").count();
        let closes = svg.matches("</text>").count();
        assert_eq!(opens, closes, "every <text> must be balanced — a raw '<' in payload would break this");
    }

    #[test]
    fn export_contains_expected_shape_and_coordinates() {
        let t = TableInfo { schema: None, name: "t".into(), columns: vec![col("id", true, None)], ..Default::default() };
        let g = build_graph(&[t.clone()]);
        let l = compute_layout(&g);
        let svg = export_svg(&l, &[t]);
        assert!(svg.starts_with("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains("<rect"));
        assert!(svg.contains(&format!("x=\"{:.1}\"", l.nodes[0].x)));
    }

    #[test]
    fn self_loop_edge_renders_as_a_quadratic_path() {
        let t = TableInfo {
            schema: None, name: "employees".into(),
            columns: vec![col("id", true, None), col("manager_id", false, Some(("employees", "id")))],
            ..Default::default()
        };
        let g = build_graph(&[t.clone()]);
        let l = compute_layout(&g);
        let svg = export_svg(&l, &[t]);
        assert!(svg.contains("<path d=\"M"));
    }

    #[test]
    fn missing_table_for_a_positioned_node_is_skipped_not_a_panic() {
        let l = crate::erd::layout::DiagramLayout {
            nodes: vec![crate::erd::layout::PositionedNode {
                key: crate::erd::TableKey { schema: None, name: "ghost".into() },
                x: 0.0, y: 0.0, w: 220.0, h: 24.0,
            }],
            edges: vec![],
        };
        let svg = export_svg(&l, &[]); // no matching TableInfo — must not panic
        // `export_svg` always emits exactly one background `<rect>` (the
        // canvas fill) regardless of node content, so "no rect at all" is
        // unsatisfiable by construction; the real assertion for "skipped,
        // not a panic" is that no EXTRA (node) rect was added for the
        // unmatched `ghost` node — i.e. still exactly one `<rect>`.
        assert_eq!(svg.matches("<rect").count(), 1, "only the background rect should be present — the unmatched node must contribute no rect: {svg}");
    }
}
