//! G8 T4: `ErDiagramView` — GPUI canvas rendering of a `DiagramLayout`.
//!
//! Read-only, pure paint: consumes T2's `DiagramLayout` (node positions,
//! routed edges) plus the raw `TableInfo` slice the schema tree already
//! fetched, and draws collapsed table boxes (header + PK/FK columns,
//! capped at `dbc_core::erd::MAX_VISIBLE_COLS`, matching T1's
//! `build_graph`/T3's `svg::node_to_svg` selection rule verbatim) with
//! straight-line/self-loop-bezier edges between them.
//!
//! T4 ships the static paint only — no interaction (pan/zoom drag,
//! hit-testing, click-to-DDL) — that's T5. `pan`/`zoom` fields already
//! exist so T5 can add input handlers without reshaping this struct.
//!
//! Spike (design CURATION point 3, plan Step 1): `canvas()`, `paint_quad`
//! and `PathBuilder::curve_to` were confirmed to rasterize correctly on
//! this project's Windows target at the pinned GPUI rev via a disposable
//! `examples/erd_canvas_spike.rs` (run once, screenshotted, deleted —
//! not part of this commit). No fallback to div-based Manhattan routing
//! was needed.

use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, App, Bounds, Context, PathBuilder, Pixels,
    Point, TextRun, Window,
};

use dbc_core::erd::layout::{DiagramLayout, PositionedNode, RoutedEdge};
use dbc_core::erd::MAX_VISIBLE_COLS;
use dbc_core::TableInfo;

const NODE_FILL: u32 = 0x313244;
const NODE_BORDER: u32 = 0x45475a;
const TEXT_COLOR: u32 = 0xcdd6f4;
const MUTED_COLOR: u32 = 0x6c7086;
const EDGE_COLOR: u32 = 0x89b4fa;

const HEADER_H: f32 = 24.0;
const ROW_H: f32 = 18.0;

pub struct ErDiagramView {
    pub(crate) layout: DiagramLayout,
    pub(crate) tables: Vec<TableInfo>,
    #[allow(dead_code)] // shown by T6's tab title / large-schema notice, not painted directly by T4
    pub(crate) schema_label: String,
    pub(crate) pan: Point<f32>,
    pub(crate) zoom: f32,
}

impl ErDiagramView {
    pub fn new(layout: DiagramLayout, tables: Vec<TableInfo>, schema_label: String) -> Self {
        Self { layout, tables, schema_label, pan: point(0.0, 0.0), zoom: 1.0 }
    }

    /// World-space (x, y) -> screen-space Pixels, given the current pan/
    /// zoom and the canvas element's own screen-space origin. Shared by
    /// paint (T4) and hit-testing (T5) so the two can never drift apart.
    fn world_to_screen(&self, origin: Point<Pixels>, world: (f32, f32)) -> Point<Pixels> {
        to_screen(origin, world, self.pan, self.zoom)
    }
}

impl Render for ErDiagramView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let layout = self.layout.clone();
        let tables = self.tables.clone();
        let pan = self.pan;
        let zoom = self.zoom;

        div().id("er-diagram-root").size_full().bg(rgb(0x1e1e2e)).child(
            canvas(
                move |bounds, _window, _app| bounds,
                move |_bounds, canvas_bounds: Bounds<Pixels>, window, app| {
                    paint_diagram(canvas_bounds, &layout, &tables, pan, zoom, window, app);
                },
            )
            .size_full(),
        )
    }
}

fn to_screen(origin: Point<Pixels>, world: (f32, f32), pan: Point<f32>, zoom: f32) -> Point<Pixels> {
    point(origin.x + px((world.0 + pan.x) * zoom), origin.y + px((world.1 + pan.y) * zoom))
}

fn header_text(t: &TableInfo) -> String {
    match &t.schema {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn paint_diagram(
    bounds: Bounds<Pixels>,
    layout: &DiagramLayout,
    tables: &[TableInfo],
    pan: Point<f32>,
    zoom: f32,
    window: &mut Window,
    app: &mut App,
) {
    for e in &layout.edges {
        paint_edge(bounds.origin, e, pan, zoom, window);
    }
    for n in &layout.nodes {
        if let Some(t) = tables.iter().find(|t| t.schema == n.key.schema && t.name == n.key.name) {
            paint_node(bounds.origin, n, t, pan, zoom, window, app);
        }
    }
}

fn paint_text_line(
    origin: Point<Pixels>,
    text: &str,
    color: u32,
    bold: bool,
    font_size: Pixels,
    line_height: Pixels,
    window: &mut Window,
    app: &mut App,
) {
    if text.is_empty() {
        return;
    }
    let mut font = window.text_style().font();
    if bold {
        font.weight = gpui::FontWeight::BOLD;
    }
    let run = TextRun {
        len: text.len(),
        font,
        color: rgb(color).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window.text_system().shape_line(text.to_string().into(), font_size, &[run], None);
    // Defensive: a hostile/unusual identifier (embedded control char, huge
    // string) must never panic the paint pass — worst case, that one
    // line is silently skipped, the rest of the diagram still renders.
    let _ = shaped.paint(origin, line_height, gpui::TextAlign::Left, None, window, app);
}

fn paint_node(
    origin: Point<Pixels>,
    n: &PositionedNode,
    t: &TableInfo,
    pan: Point<f32>,
    zoom: f32,
    window: &mut Window,
    app: &mut App,
) {
    let top_left = to_screen(origin, (n.x, n.y), pan, zoom);
    let sz = size(px(n.w * zoom), px(n.h * zoom));
    window.paint_quad(
        gpui::fill(Bounds::new(top_left, sz), rgb(NODE_FILL))
            .corner_radii(px(4.))
            .border_widths(px(1.))
            .border_color(rgb(NODE_BORDER)),
    );

    let rem_size = window.rem_size();
    let header_font_size = window.text_style().font_size.to_pixels(rem_size) * zoom;
    let row_font_size = header_font_size * (11.0 / 13.0);

    let pad_x = px(8.0 * zoom);
    let pad_top = px(4.0 * zoom);
    paint_text_line(
        top_left + point(pad_x, pad_top),
        &header_text(t),
        TEXT_COLOR,
        true,
        header_font_size,
        header_font_size,
        window,
        app,
    );

    // Same PK/FK-only, capped-at-MAX_VISIBLE_COLS selection T1's
    // `build_graph`/T3's `svg::node_to_svg` already use — kept in lockstep
    // by construction, not by a shared helper (dbc-core's erd module has
    // no GPUI dependency to hang one off of).
    let mut row_y = HEADER_H;
    let mut shown = 0usize;
    let mut total_pk_fk = 0usize;
    for c in t.columns.iter().filter(|c| c.is_pk || c.fk.is_some()) {
        total_pk_fk += 1;
        if shown >= MAX_VISIBLE_COLS {
            continue;
        }
        let marker = if c.is_pk { "PK " } else { "FK " };
        let line = format!("{marker}{}: {}", c.name, c.data_type);
        let row_origin = to_screen(origin, (n.x, n.y + row_y), pan, zoom);
        paint_text_line(row_origin + point(pad_x, px(0.0)), &line, TEXT_COLOR, false, row_font_size, row_font_size, window, app);
        row_y += ROW_H;
        shown += 1;
    }
    let hidden = total_pk_fk.saturating_sub(MAX_VISIBLE_COLS);
    if hidden > 0 {
        let footer = format!("+{hidden} dalších");
        let row_origin = to_screen(origin, (n.x, n.y + row_y), pan, zoom);
        paint_text_line(row_origin + point(pad_x, px(0.0)), &footer, MUTED_COLOR, false, row_font_size, row_font_size, window, app);
    }
}

fn paint_edge(origin: Point<Pixels>, e: &RoutedEdge, pan: Point<f32>, zoom: f32, window: &mut Window) {
    if e.points.len() < 2 {
        return;
    }
    let mut builder = PathBuilder::stroke(px(1.5));
    builder.move_to(to_screen(origin, e.points[0], pan, zoom));
    if e.is_self_loop && e.points.len() == 3 {
        builder.curve_to(to_screen(origin, e.points[2], pan, zoom), to_screen(origin, e.points[1], pan, zoom));
    } else {
        builder.line_to(to_screen(origin, e.points[1], pan, zoom));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, rgb(EDGE_COLOR));
    }
}
