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
    canvas, div, point, prelude::*, px, rgb, size, App, Bounds, Context, EventEmitter,
    PathBuilder, Pixels, Point, ScrollDelta, TextRun, Window,
};

use dbc_core::erd::layout::{DiagramLayout, PositionedNode, RoutedEdge};
use dbc_core::erd::{TableKey, MAX_VISIBLE_COLS};
use dbc_core::TableInfo;

use crate::schema_tree::TreeEvent;

const NODE_FILL: u32 = 0x313244;
const NODE_BORDER: u32 = 0x45475a;
const TEXT_COLOR: u32 = 0xcdd6f4;
const MUTED_COLOR: u32 = 0x6c7086;
const EDGE_COLOR: u32 = 0x89b4fa;
const ACCENT_COLOR: u32 = 0xf5c2e7;

/// Anchored-zoom clamp bounds (T5 grounding, Constraints: "clamp zoom to a
/// sane range — no zoom->0 or infinity producing NaN"). `0.2`/`3.0` per the
/// plan's own `zoom_at` sketch (inside the stricter `[0.1, 5.0]` the brief
/// allows).
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 3.0;

const HEADER_H: f32 = 24.0;
const ROW_H: f32 = 18.0;

/// G8 T7 (design §3): past this many tables in the scoped selection,
/// `AppView::open_er_diagram` truncates (alphabetical, via `cap_tables`)
/// rather than laying out an unreadable/slow graph. No viewport culling in
/// v1 — GPUI repaints whole scenes every frame regardless, so culling buys
/// nothing below this cap without real profiling evidence of a problem.
pub const DIAGRAM_TABLE_CAP: usize = 150;

pub struct ErDiagramView {
    pub(crate) layout: DiagramLayout,
    pub(crate) tables: Vec<TableInfo>,
    pub(crate) schema_label: String,
    pub(crate) pan: Point<f32>,
    pub(crate) zoom: f32,
    /// T5: the currently-selected node, if any. Cleared on an empty-canvas
    /// click; set (and `TreeEvent::OpenDdl` emitted) on a node hit.
    pub(crate) selected: Option<TableKey>,
    /// T5: screen-space (window-absolute, post pan/zoom) node boxes,
    /// recomputed every paint pass in the canvas's `prepaint` closure and
    /// written back onto `self` via the entity handle (canvas closures only
    /// get `&mut App`, not `&mut Context<Self>` — the entity is not
    /// borrowed during prepaint/paint, only during `render` itself, so
    /// `Entity::update` here is the standard GPUI "measure during paint,
    /// store for the next input event" idiom). Linear scan on hit-test is
    /// O(n) in node count — fine at this app's schema sizes (`T7`'s
    /// `DIAGRAM_TABLE_CAP` bounds it further); documented, not optimized.
    pub(crate) hit_boxes: Vec<(TableKey, Bounds<Pixels>)>,
    /// T5: the canvas element's own screen-space origin, refreshed
    /// alongside `hit_boxes` — `zoom_at` needs the cursor position in
    /// canvas-local space (`mouse_pos - canvas_origin`), matching the
    /// `to_screen`/`world_to_screen` convention above.
    pub(crate) canvas_origin: Point<Pixels>,
    /// T5: `(start mouse pos, start pan)`, captured on an empty-canvas
    /// mouse-down and cleared on mouse-up — the same drag-capture shape
    /// `grid.rs`'s column-resize drag uses.
    pub(crate) drag_state: Option<(Point<Pixels>, Point<f32>)>,
    /// T7: set by `AppView::open_er_diagram` when the scoped table list was
    /// truncated to `DIAGRAM_TABLE_CAP` — `render` shows this as a one-line
    /// warning banner above the canvas whenever it's `Some`.
    pub(crate) truncated_notice: Option<String>,
    /// T6: export status ("volím cíl exportu…" / "exportováno: {path}" /
    /// "export zrušen" / "error: …"), same "status_note, shown once" idiom
    /// `ResultGrid` already establishes (`main.rs::render_tab_content`).
    pub(crate) status_note: Option<String>,
}

impl ErDiagramView {
    pub fn new(layout: DiagramLayout, tables: Vec<TableInfo>, schema_label: String) -> Self {
        Self {
            layout,
            tables,
            schema_label,
            pan: point(0.0, 0.0),
            zoom: 1.0,
            selected: None,
            hit_boxes: Vec::new(),
            canvas_origin: point(px(0.0), px(0.0)),
            drag_state: None,
            truncated_notice: None,
            status_note: None,
        }
    }

    /// G8 T6: "Export…" button — save-as dialog (no filter; the pinned
    /// GPUI's `prompt_for_new_path` doesn't take one at all, see the export
    /// pattern already established at `grid.rs::start_export`), then a
    /// lossless SVG export of the CURRENT (possibly T7-truncated) layout via
    /// `dbc_core::erd::svg::export_svg`. Simpler than `grid.rs`'s CSV/TSV/JSON
    /// export: no chunking, no background-executor split — an SVG string for
    /// at most `DIAGRAM_TABLE_CAP` nodes is small.
    fn start_export_svg(&mut self, cx: &mut Context<Self>) {
        let suggested_name = format!("{}.svg", self.schema_label);
        self.status_note = Some("volím cíl exportu…".to_string());
        cx.notify();
        let dialog = cx.prompt_for_new_path(&std::path::PathBuf::new(), Some(&suggested_name));
        let layout = self.layout.clone();
        let tables = self.tables.clone();
        cx.spawn(async move |this, cx| {
            let path = match dialog.await {
                Ok(Ok(Some(p))) => p,
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |v, cx| {
                        v.status_note = Some("export zrušen".to_string());
                        cx.notify();
                    });
                    return;
                }
                _ => {
                    let _ = this.update(cx, |v, cx| {
                        v.status_note = Some("error: export dialog selhal".to_string());
                        cx.notify();
                    });
                    return;
                }
            };
            let svg = dbc_core::erd::svg::export_svg(&layout, &tables);
            let result = std::fs::write(&path, svg);
            let _ = this.update(cx, |v, cx| {
                v.status_note = Some(match result {
                    Ok(()) => format!("exportováno: {}", path.display()),
                    Err(e) => format!("error: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// World-space (x, y) -> screen-space Pixels, given the current pan/
    /// zoom and the canvas element's own screen-space origin. Shared by
    /// paint (T4) and hit-testing (T5) so the two can never drift apart.
    #[allow(dead_code)] // exercised via the module-level `to_screen` this delegates to; kept as
    // the documented `&self` entry point future callers (T6/T7) should prefer.
    fn world_to_screen(&self, origin: Point<Pixels>, world: (f32, f32)) -> Point<Pixels> {
        to_screen(origin, world, self.pan, self.zoom)
    }
}

impl EventEmitter<TreeEvent> for ErDiagramView {}

impl Render for ErDiagramView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout_for_prepaint = self.layout.clone();
        let layout_for_paint = self.layout.clone();
        let tables = self.tables.clone();
        let pan = self.pan;
        let zoom = self.zoom;
        let selected = self.selected.clone();
        let dragging = self.drag_state.is_some();
        let entity = cx.entity();
        let schema_label = self.schema_label.clone();
        let truncated_notice = self.truncated_notice.clone();

        let mut canvas_area = div()
            .id("er-diagram-root")
            .flex_1()
            .bg(rgb(0x1e1e2e))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, e: &gpui::MouseDownEvent, _window, cx| {
                    if let Some(key) = hit_test(&this.hit_boxes, e.position) {
                        this.selected = Some(key.clone());
                        this.drag_state = None;
                        if let Some(t) =
                            this.tables.iter().find(|t| t.schema == key.schema && t.name == key.name)
                        {
                            let ddl = t.ddl.clone().unwrap_or_else(|| dbc_core::synthesize_create_table(t));
                            cx.emit(TreeEvent::OpenDdl { title: t.name.clone(), ddl });
                        }
                    } else {
                        this.selected = None;
                        this.drag_state = Some((e.position, this.pan));
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, e: &gpui::ScrollWheelEvent, _window, cx| {
                let delta_lines = match e.delta {
                    ScrollDelta::Lines(p) => p.y,
                    ScrollDelta::Pixels(p) => p.y.as_f32() / 20.0,
                };
                let factor = 1.1f32.powf(delta_lines);
                let mouse_local = (
                    f32::from(e.position.x) - f32::from(this.canvas_origin.x),
                    f32::from(e.position.y) - f32::from(this.canvas_origin.y),
                );
                let (new_pan, new_zoom) = zoom_at((this.pan.x, this.pan.y), this.zoom, mouse_local, factor);
                this.pan = point(new_pan.0, new_pan.1);
                this.zoom = new_zoom;
                cx.notify();
            }))
            .child(
                canvas(
                    move |bounds, _window, app| {
                        let boxes = compute_hit_boxes(bounds.origin, &layout_for_prepaint, pan, zoom);
                        entity.update(app, |view, _cx| {
                            view.hit_boxes = boxes;
                            view.canvas_origin = bounds.origin;
                        });
                        bounds
                    },
                    move |_bounds, canvas_bounds: Bounds<Pixels>, window, app| {
                        paint_diagram(
                            canvas_bounds,
                            &layout_for_paint,
                            &tables,
                            pan,
                            zoom,
                            selected.as_ref(),
                            window,
                            app,
                        );
                    },
                )
                .size_full(),
            );

        if dragging {
            canvas_area = canvas_area
                .on_mouse_move(cx.listener(|this, e: &gpui::MouseMoveEvent, _window, cx| {
                    if let Some((start_pos, start_pan)) = this.drag_state {
                        let zoom = this.zoom.max(0.0001);
                        let dx = (f32::from(e.position.x) - f32::from(start_pos.x)) / zoom;
                        let dy = (f32::from(e.position.y) - f32::from(start_pos.y)) / zoom;
                        this.pan = point(start_pan.x + dx, start_pan.y + dy);
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _e, _window, cx| {
                        this.drag_state = None;
                        cx.notify();
                    }),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _e, _window, cx| {
                        this.drag_state = None;
                        cx.notify();
                    }),
                );
        }

        // G8 T6/T7: header bar (schema label + "Export…" button) and, when
        // T7's cap truncated the scoped table list, a one-line warning
        // banner — both above the canvas, which keeps `flex_1()` and fills
        // the rest of the tab.
        let header = div()
            .id("er-diagram-header")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .bg(rgb(0x181825))
            .text_color(rgb(0xcdd6f4))
            .child(format!("ER: {schema_label}"))
            .child(
                div()
                    .id("er-diagram-export")
                    .cursor_pointer()
                    .bg(rgb(0x313244))
                    .text_color(rgb(0xcdd6f4))
                    .px_2()
                    .rounded_md()
                    .child("Export…")
                    .on_click(cx.listener(|v, _, _window, cx| v.start_export_svg(cx))),
            );

        let banner = truncated_notice.map(|msg| {
            div()
                .id("er-diagram-truncated-notice")
                .w_full()
                .px_2()
                .py_1()
                .bg(rgb(0x45475a))
                .text_color(rgb(0xf9e2af))
                .child(msg)
        });

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .child(header)
            .children(banner)
            .child(canvas_area)
    }
}

/// G8 T7: pure truncation predicate — extracted so it doesn't need a live
/// `SchemaSnapshot`/GPUI to test. `AppView::open_er_diagram` calls this on
/// the schema-scoped table slice BEFORE `build_graph`/`compute_layout` ever
/// run (`dbc-core`'s layout stays scale-agnostic; the cap is this crate's
/// rendering/UX-scale concern, not a graph-correctness one). Returns the
/// (possibly truncated) table list and `Some(hidden_count)` when truncation
/// happened, `None` when `tables.len() <= cap`.
pub fn cap_tables(mut tables: Vec<TableInfo>, cap: usize) -> (Vec<TableInfo>, Option<usize>) {
    if tables.len() <= cap {
        return (tables, None);
    }
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    let hidden = tables.len() - cap;
    tables.truncate(cap);
    (tables, Some(hidden))
}

/// Pure hit-test: walks `hit_boxes` in reverse paint order (topmost node
/// wins on overlap — the layout algorithm guarantees no overlap by
/// construction, but reverse order is the correct convention regardless) and
/// returns the first box containing `point`. `Bounds::contains` is the same
/// primitive used throughout `grid.rs`. O(n) linear scan in node count —
/// documented, not optimized (T7's `DIAGRAM_TABLE_CAP` bounds n further).
fn hit_test(hit_boxes: &[(TableKey, Bounds<Pixels>)], point: Point<Pixels>) -> Option<TableKey> {
    hit_boxes.iter().rev().find(|(_, b)| b.contains(&point)).map(|(k, _)| k.clone())
}

/// Screen-space (window-absolute) node boxes for the current pan/zoom —
/// shares `to_screen`/`fmt_coord` with `paint_node` so hit-testing and
/// painting can never drift apart (same node box math, same non-finite
/// guard).
fn compute_hit_boxes(
    origin: Point<Pixels>,
    layout: &DiagramLayout,
    pan: Point<f32>,
    zoom: f32,
) -> Vec<(TableKey, Bounds<Pixels>)> {
    layout
        .nodes
        .iter()
        .map(|n| {
            let top_left = to_screen(origin, (n.x, n.y), pan, zoom);
            let sz = size(px(fmt_coord(n.w * zoom)), px(fmt_coord(n.h * zoom)));
            (n.key.clone(), Bounds::new(top_left, sz))
        })
        .collect()
}

/// Anchored zoom (T5 grounding): given the current `pan`/`zoom` and the
/// cursor's canvas-local position, returns a new `(pan, zoom)` such that the
/// world point under the cursor stays under the cursor. Pure, no GPUI types
/// — plain `f32` tuples, unit-tested standalone (`zoom_math_tests` below).
/// `new_zoom` is clamped to `[ZOOM_MIN, ZOOM_MAX]` so a runaway scroll delta
/// (or a `factor` of `0.0`/`inf` from a malformed event) can never collapse
/// zoom to `0.0` or blow it up to infinity — both of which would feed a
/// non-finite value into `to_screen` (already guarded by `fmt_coord`, but
/// the clamp here stops it at the source instead of relying only on that
/// backstop).
pub fn zoom_at(pan: (f32, f32), zoom: f32, mouse_local: (f32, f32), factor: f32) -> ((f32, f32), f32) {
    let safe_zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
    let safe_factor = if factor.is_finite() && factor > 0.0 { factor } else { 1.0 };
    let new_zoom = (safe_zoom * safe_factor).clamp(ZOOM_MIN, ZOOM_MAX);
    let world_under = (mouse_local.0 / safe_zoom - pan.0, mouse_local.1 / safe_zoom - pan.1);
    let new_pan = (mouse_local.0 / new_zoom - world_under.0, mouse_local.1 / new_zoom - world_under.1);
    (new_pan, new_zoom)
}

/// Non-finite coordinates (NaN/inf) must never reach GPUI's path/quad
/// builders: `PathBuilder::move_to`/`line_to`/`curve_to` debug-assert
/// finiteness (lyon_path's `nan_check`) INSIDE the call, before
/// `.build()`'s own `Result` gets any chance to catch it. Mirrors
/// `dbc_core::erd::svg::fmt_coord`'s "clamp to 0.0" posture — a
/// malformed/adversarial `DiagramLayout` (hand-built, or a future layout
/// bug) is already a display-only edge case; never letting it panic the
/// paint pass is the load-bearing part, not the exact fallback value.
fn fmt_coord(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn to_screen(origin: Point<Pixels>, world: (f32, f32), pan: Point<f32>, zoom: f32) -> Point<Pixels> {
    let x = fmt_coord((world.0 + pan.x) * zoom);
    let y = fmt_coord((world.1 + pan.y) * zoom);
    point(origin.x + px(x), origin.y + px(y))
}

/// `shape_line` debug-asserts the text contains no `\n`
/// (`text_system.rs:404`) — a catalog identifier can legally carry an
/// embedded newline or other C0 control character (SQLite quoted names,
/// same hostile-input class `dbc_core::erd::svg::escape_xml` already
/// treats as expected, not exceptional). Every char below 0x20 collapses
/// to a single space so multi-line/control-char garbage renders as one
/// (still legible, still same-length) line instead of panicking the
/// paint pass.
fn sanitize_for_display(s: &str) -> String {
    s.chars().map(|c| if (c as u32) < 0x20 { ' ' } else { c }).collect()
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
    selected: Option<&TableKey>,
    window: &mut Window,
    app: &mut App,
) {
    for e in &layout.edges {
        paint_edge(bounds.origin, e, pan, zoom, EDGE_COLOR, window);
    }
    for n in &layout.nodes {
        if let Some(t) = tables.iter().find(|t| t.schema == n.key.schema && t.name == n.key.name) {
            let is_selected = selected == Some(&n.key);
            paint_node(bounds.origin, n, t, pan, zoom, is_selected, window, app);
        }
    }
    // T5 selection highlight (grounding): every edge touching the selected
    // node is repainted a second time, last, in the accent color — cheap
    // z-order trick, no new state beyond `selected` itself.
    if let Some(key) = selected {
        for e in layout.edges.iter().filter(|e| &e.from == key || &e.to == key) {
            paint_edge(bounds.origin, e, pan, zoom, ACCENT_COLOR, window);
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
    // Every control char (including the '\n' shape_line debug-asserts
    // against) is replaced 1:1 with a space before it ever reaches GPUI's
    // text system — see `sanitize_for_display`.
    let sanitized = sanitize_for_display(text);
    let mut font = window.text_style().font();
    if bold {
        font.weight = gpui::FontWeight::BOLD;
    }
    let run = TextRun {
        len: sanitized.len(),
        font,
        color: rgb(color).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window.text_system().shape_line(sanitized.into(), font_size, &[run], None);
    // Defensive: any remaining shaping failure must never panic the paint
    // pass — worst case, that one line is silently skipped, the rest of
    // the diagram still renders.
    let _ = shaped.paint(origin, line_height, gpui::TextAlign::Left, None, window, app);
}

fn paint_node(
    origin: Point<Pixels>,
    n: &PositionedNode,
    t: &TableInfo,
    pan: Point<f32>,
    zoom: f32,
    is_selected: bool,
    window: &mut Window,
    app: &mut App,
) {
    let top_left = to_screen(origin, (n.x, n.y), pan, zoom);
    let sz = size(px(fmt_coord(n.w * zoom)), px(fmt_coord(n.h * zoom)));
    let (border_color, border_w) =
        if is_selected { (ACCENT_COLOR, px(2.0)) } else { (NODE_BORDER, px(1.0)) };
    window.paint_quad(
        gpui::fill(Bounds::new(top_left, sz), rgb(NODE_FILL))
            .corner_radii(px(4.))
            .border_widths(border_w)
            .border_color(rgb(border_color)),
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

fn paint_edge(origin: Point<Pixels>, e: &RoutedEdge, pan: Point<f32>, zoom: f32, color: u32, window: &mut Window) {
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
        window.paint_path(path, rgb(color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `to_screen`/`fmt_coord` are the single choke point every world
    // coordinate passes through before becoming a `Point<Pixels>` fed to
    // `PathBuilder`/`paint_quad` — GPUI's `Window`/`App` can't be
    // constructed in a plain unit test (no such harness exists anywhere
    // in this codebase), so these pure helpers are what stand in for
    // "a `DiagramLayout` with a NaN/inf coordinate paints without panic":
    // every path a non-finite coordinate could take into GPUI funnels
    // through one of them.

    #[test]
    fn fmt_coord_clamps_non_finite_to_zero() {
        assert_eq!(fmt_coord(f32::NAN), 0.0);
        assert_eq!(fmt_coord(f32::INFINITY), 0.0);
        assert_eq!(fmt_coord(f32::NEG_INFINITY), 0.0);
        assert_eq!(fmt_coord(42.5), 42.5);
    }

    #[test]
    fn to_screen_never_produces_non_finite_pixels_from_nan_or_inf_world_coords() {
        let origin = point(px(0.0), px(0.0));
        let pan = point(0.0, 0.0);
        for world in [
            (f32::NAN, 10.0),
            (10.0, f32::NAN),
            (f32::INFINITY, 10.0),
            (10.0, f32::NEG_INFINITY),
            (f32::NAN, f32::INFINITY),
        ] {
            let p = to_screen(origin, world, pan, 1.0);
            assert!(f32::from(p.x).is_finite(), "x from world {world:?} must be finite");
            assert!(f32::from(p.y).is_finite(), "y from world {world:?} must be finite");
        }
    }

    #[test]
    fn to_screen_non_finite_zoom_or_pan_also_clamps() {
        let origin = point(px(0.0), px(0.0));
        let p = to_screen(origin, (10.0, 10.0), point(f32::NAN, 0.0), f32::INFINITY);
        assert!(f32::from(p.x).is_finite());
        assert!(f32::from(p.y).is_finite());
    }

    #[test]
    fn to_screen_finite_input_is_unaffected() {
        let origin = point(px(5.0), px(5.0));
        let p = to_screen(origin, (10.0, 20.0), point(1.0, 2.0), 2.0);
        // (10 + 1) * 2 = 22, (20 + 2) * 2 = 44, offset by origin.
        assert_eq!(f32::from(p.x), 5.0 + 22.0);
        assert_eq!(f32::from(p.y), 5.0 + 44.0);
    }

    #[test]
    fn sanitize_for_display_replaces_newlines_and_control_chars_with_space() {
        let s = sanitize_for_display("we\"ird\nname\rwith\ttabs\x01ctrl");
        assert!(!s.contains('\n'));
        assert!(!s.contains('\r'));
        assert!(!s.contains('\t'));
        assert!(!s.contains('\x01'));
        assert_eq!(s, "we\"ird name with tabs ctrl");
    }

    #[test]
    fn sanitize_for_display_preserves_byte_length_and_normal_text() {
        let s = "plain_table_name";
        assert_eq!(sanitize_for_display(s), s);
        let with_newline = "a\nb";
        assert_eq!(sanitize_for_display(with_newline).len(), with_newline.len());
    }

    #[test]
    fn sanitize_for_display_leaves_non_control_unicode_untouched() {
        let s = sanitize_for_display("tábulka_ěščř");
        assert_eq!(s, "tábulka_ěščř");
    }

    // --- T5: hit-testing (pure — `Bounds<Pixels>`/`Point<Pixels>` are
    // plain value types, no live `Window` needed) ---

    fn key(name: &str) -> TableKey {
        TableKey { schema: None, name: name.into() }
    }

    fn box_at(name: &str, x: f32, y: f32, w: f32, h: f32) -> (TableKey, Bounds<Pixels>) {
        (key(name), Bounds::new(point(px(x), px(y)), size(px(w), px(h))))
    }

    #[test]
    fn hit_test_finds_containing_box() {
        let boxes = vec![box_at("a", 0.0, 0.0, 100.0, 50.0), box_at("b", 200.0, 0.0, 100.0, 50.0)];
        assert_eq!(hit_test(&boxes, point(px(10.0), px(10.0))), Some(key("a")));
        assert_eq!(hit_test(&boxes, point(px(210.0), px(10.0))), Some(key("b")));
    }

    #[test]
    fn hit_test_miss_returns_none() {
        let boxes = vec![box_at("a", 0.0, 0.0, 100.0, 50.0)];
        assert_eq!(hit_test(&boxes, point(px(500.0), px(500.0))), None);
    }

    #[test]
    fn hit_test_overlap_prefers_topmost_last_painted() {
        // Reverse-paint-order convention: later entries (painted on top)
        // win on overlap.
        let boxes = vec![box_at("under", 0.0, 0.0, 100.0, 100.0), box_at("over", 0.0, 0.0, 100.0, 100.0)];
        assert_eq!(hit_test(&boxes, point(px(10.0), px(10.0))), Some(key("over")));
    }

    #[test]
    fn compute_hit_boxes_matches_to_screen_node_geometry() {
        let layout = DiagramLayout {
            nodes: vec![PositionedNode { key: key("t"), x: 10.0, y: 20.0, w: 220.0, h: 60.0 }],
            edges: vec![],
        };
        let origin = point(px(5.0), px(5.0));
        let pan = point(1.0, 2.0);
        let zoom = 2.0;
        let boxes = compute_hit_boxes(origin, &layout, pan, zoom);
        assert_eq!(boxes.len(), 1);
        let (k, b) = &boxes[0];
        assert_eq!(*k, key("t"));
        let expect_top_left = to_screen(origin, (10.0, 20.0), pan, zoom);
        assert_eq!(b.origin, expect_top_left);
        assert_eq!(f32::from(b.size.width), 220.0 * zoom);
        assert_eq!(f32::from(b.size.height), 60.0 * zoom);
    }
}

#[cfg(test)]
mod zoom_math_tests {
    use super::*;

    #[test]
    fn zoom_in_keeps_cursor_world_point_fixed() {
        let (new_pan, new_zoom) = zoom_at((0.0, 0.0), 1.0, (100.0, 50.0), 1.1);
        assert!((new_zoom - 1.1).abs() < 1e-6);
        // Re-derive the world point under the cursor at the NEW pan/zoom
        // and confirm it matches what it was before the zoom.
        let world_before = (100.0 / 1.0 - 0.0, 50.0 / 1.0 - 0.0);
        let world_after = (100.0 / new_zoom - new_pan.0, 50.0 / new_zoom - new_pan.1);
        assert!((world_before.0 - world_after.0).abs() < 1e-4);
        assert!((world_before.1 - world_after.1).abs() < 1e-4);
    }

    #[test]
    fn zoom_out_also_keeps_cursor_world_point_fixed() {
        let (new_pan, new_zoom) = zoom_at((3.0, -2.0), 1.5, (40.0, 80.0), 0.9);
        let world_before = (40.0 / 1.5 - 3.0, 80.0 / 1.5 - (-2.0));
        let world_after = (40.0 / new_zoom - new_pan.0, 80.0 / new_zoom - new_pan.1);
        assert!((world_before.0 - world_after.0).abs() < 1e-4);
        assert!((world_before.1 - world_after.1).abs() < 1e-4);
    }

    #[test]
    fn zoom_clamps_to_bounds() {
        let (_, z_min) = zoom_at((0.0, 0.0), 0.21, (0.0, 0.0), 0.5);
        assert!((z_min - ZOOM_MIN).abs() < 1e-6);
        let (_, z_max) = zoom_at((0.0, 0.0), 2.9, (0.0, 0.0), 2.0);
        assert!((z_max - ZOOM_MAX).abs() < 1e-6);
    }

    #[test]
    fn zoom_never_produces_non_finite_output_from_degenerate_input() {
        for (zoom, factor) in [
            (0.0, 1.1),
            (f32::NAN, 1.1),
            (f32::INFINITY, 1.1),
            (1.0, 0.0),
            (1.0, f32::NAN),
            (1.0, f32::INFINITY),
            (f32::NEG_INFINITY, f32::NAN),
        ] {
            let (pan, z) = zoom_at((0.0, 0.0), zoom, (10.0, 10.0), factor);
            assert!(z.is_finite(), "zoom {zoom} factor {factor} produced non-finite z={z}");
            assert!(z >= ZOOM_MIN && z <= ZOOM_MAX, "z={z} out of clamp range");
            assert!(pan.0.is_finite() && pan.1.is_finite(), "pan {pan:?} not finite");
        }
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    fn t(name: &str) -> TableInfo {
        TableInfo { name: name.into(), ..Default::default() }
    }

    #[test]
    fn under_cap_is_untouched_and_unsorted() {
        let (out, hidden) = cap_tables(vec![t("z"), t("a")], 150);
        assert_eq!(hidden, None);
        assert_eq!(out.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["z", "a"]);
    }

    #[test]
    fn over_cap_truncates_alphabetically_and_reports_hidden_count() {
        let tables: Vec<TableInfo> = (0..5).map(|i| t(&format!("t{i}"))).collect();
        let (out, hidden) = cap_tables(tables, 3);
        assert_eq!(hidden, Some(2));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].name, "t0"); // already alphabetical in this fixture
    }

    #[test]
    fn over_cap_sorts_before_truncating() {
        let tables = vec![t("zeta"), t("alpha"), t("mid")];
        let (out, hidden) = cap_tables(tables, 2);
        assert_eq!(hidden, Some(1));
        assert_eq!(out.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["alpha", "mid"]);
    }
}
