//! G14 T8: `ChartView` — GPUI canvas rendering of `chart_data::ChartData`.
//!
//! Read-only, pure paint over an already-materialized `ResultBuffer`
//! snapshot: draws bars (`paint_quad`) or lines (`PathBuilder`/`paint_path`)
//! plus axis ticks, exactly the same `canvas()` idiom `er_diagram_view.rs`
//! already uses (canvas at er_diagram_view.rs:214-236, text at :453-487,
//! stroked paths at :555-569). No interaction beyond the header's
//! "Upravit…" button — re-picking axes is Task 11's job (it owns
//! `ModalState::ChartPicker`); this view only emits the request.
//!
//! Gap semantics (design §2.2, `chart_data`'s contract): a `None` point is a
//! NULL/unparsable cell — bars simply skip it, lines break the stroked run
//! (never interpolate across a gap, never draw 0).
// consumed by main.rs (G14 Task 11); allow removed there
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    canvas, div, point, prelude::*, px, size, App, Bounds, Context, EventEmitter, Hsla,
    PathBuilder, Pixels, Point, Render, TextRun, Window,
};

use dbc_buffer::ResultBuffer;

use crate::chart_data::{self, ChartData, ChartKind};
use crate::theme::{ActiveTheme, Theme};

const PAD_LEFT: f32 = 48.0; // y-axis label gutter
const PAD_RIGHT: f32 = 8.0;
const PAD_TOP: f32 = 8.0;
const PAD_BOTTOM: f32 = 22.0; // x tick labels
const LABEL_MIN_PX: f32 = 60.0; // label every Nth tick so labels don't collide

/// Review hardening (M1): X columns are unfiltered — any type, including
/// TEXT/VARCHAR(MAX) — so an x-label must be capped BEFORE it's stored in
/// `ChartData`, not just sanitized at paint time. Without this, a wide text
/// column would fully text-shape a multi-KB string on every visible tick,
/// every repaint. Char-count cap (not byte-slicing — must stay UTF-8 safe).
const X_LABEL_CHAR_CAP: usize = 40;

/// Review hardening (M3): belt only — the real UX bound is the picker's
/// checkbox list (Task 11), which offers at most `column_count()` numeric
/// columns. This stops an unbounded/malformed `y_cols` list (e.g. a future
/// non-picker caller) from painting an unbounded number of series, and thus
/// doing unbounded work, on every repaint.
const MAX_SERIES: usize = 8;

pub enum ChartViewEvent {
    /// "Upravit…" clicked — main.rs reopens `ModalState::ChartPicker`
    /// seeded from `picker_seed()`, edit-in-place (design §2.4's only
    /// interaction; Task 11 wires the actual modal).
    ReopenPicker,
}

impl EventEmitter<ChartViewEvent> for ChartView {}

pub struct ChartView {
    buffer: Rc<RefCell<ResultBuffer>>,
    kind: ChartKind,
    x_col: usize,
    y_cols: Vec<usize>,
    source_title: String,
    data: ChartData,
}

impl ChartView {
    pub fn new(
        buffer: Rc<RefCell<ResultBuffer>>,
        kind: ChartKind,
        x_col: usize,
        y_cols: Vec<usize>,
        source_title: String,
    ) -> Self {
        let data = Self::compute(&buffer, x_col, &y_cols);
        Self { buffer, kind, x_col, y_cols, source_title, data }
    }

    /// Associated fn (not method) so the unit test can call it without a
    /// GPUI Entity. Reads at most `CHART_ROW_HARD_CAP` rows; NULL → `None`.
    fn compute(buffer: &Rc<RefCell<ResultBuffer>>, x_col: usize, y_cols: &[usize]) -> ChartData {
        let mut buf = buffer.borrow_mut();
        let total = buf.row_count();
        let rows = total.min(chart_data::CHART_ROW_HARD_CAP);
        // Belt: the picker only offers real columns, but never panic on an
        // out-of-range index — silently drop it (tested). Also belt-caps
        // the series count (M3) — see MAX_SERIES doc comment.
        let y_cols: Vec<usize> = y_cols
            .iter()
            .copied()
            .filter(|&c| c < buf.column_count())
            .take(MAX_SERIES)
            .collect();
        // names first — schema() borrows buf, cell_text needs &mut:
        let names: Vec<String> =
            y_cols.iter().map(|&c| buf.schema().field(c).name().clone()).collect();
        // (M1) cap each x-label's char count BEFORE it's stored — see
        // X_LABEL_CHAR_CAP doc comment.
        let x_labels: Vec<String> =
            (0..rows).map(|r| cap_x_label(buf.cell_text(r, x_col))).collect();
        let y_columns: Vec<(String, Vec<Option<String>>)> = y_cols
            .iter()
            .zip(names)
            .map(|(&c, name)| {
                let cells = (0..rows)
                    .map(|r| (!buf.cell_is_null(r, c)).then(|| buf.cell_text(r, c)))
                    .collect();
                (name, cells)
            })
            .collect();
        chart_data::prepare(x_labels, &y_columns, chart_data::CHART_ROW_HARD_CAP, total)
    }

    /// Re-pick path: recompute `ChartData` in place (design §2.4 — edits
    /// the tab, doesn't spawn a new one).
    pub fn reconfigure(
        &mut self,
        kind: ChartKind,
        x_col: usize,
        y_cols: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        self.kind = kind;
        self.x_col = x_col;
        self.y_cols = y_cols;
        self.data = Self::compute(&self.buffer, self.x_col, &self.y_cols);
        cx.notify();
    }

    /// (kind, x_col, y_cols) — prefills the reopened picker.
    pub fn picker_seed(&self) -> (ChartKind, usize, Vec<usize>) {
        (self.kind, self.x_col, self.y_cols.clone())
    }

    pub fn source_title(&self) -> &str {
        &self.source_title
    }

    /// Rc clone of the snapshot buffer — Task 11's picker-reopen path
    /// rebuilds the column list from it.
    pub fn buffer_handle(&self) -> Rc<RefCell<ResultBuffer>> {
        self.buffer.clone()
    }
}

impl Render for ChartView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        let data = self.data.clone();
        let kind = self.kind;
        let source_title = self.source_title.clone();
        let kind_label = match kind {
            ChartKind::Bar => "sloupcový",
            ChartKind::Line => "spojnicový",
        };

        let header = div()
            .id("chart-view-header")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .bg(theme.bg_app)
            .text_color(theme.text_muted)
            .child(format!("Graf: {source_title} ({kind_label})"))
            .child(
                div()
                    .id("chart-view-edit")
                    .cursor_pointer()
                    .bg(theme.bg_hover)
                    .text_color(theme.text_primary)
                    .px_2()
                    .rounded_md()
                    .child("Upravit…")
                    .on_click(cx.listener(|_this, _, _window, cx| {
                        cx.emit(ChartViewEvent::ReopenPicker);
                    })),
            );

        let plot = canvas(
            move |bounds, _window, _app| bounds,
            move |_bounds, canvas_bounds: Bounds<Pixels>, window, app| {
                paint_chart(canvas_bounds, &data, kind, window, app);
            },
        )
        .flex_1()
        .w_full();

        div()
            .id("chart-view-root")
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.bg_deep)
            .child(header)
            .child(plot)
    }
}

/// Char-count cap (UTF-8 safe — never slices mid-codepoint), not a byte cap.
/// See `X_LABEL_CHAR_CAP` doc comment (M1).
fn cap_x_label(s: String) -> String {
    if s.chars().count() <= X_LABEL_CHAR_CAP {
        s
    } else {
        let mut truncated: String = s.chars().take(X_LABEL_CHAR_CAP).collect();
        truncated.push('…');
        truncated
    }
}

fn series_color(theme: &Theme, i: usize) -> Hsla {
    // design §2.1: fixed 4-color rotation, wrap-around accepted for v1.
    [theme.accent, theme.success, theme.warn, theme.danger][i % 4]
}

fn paint_chart(
    bounds: Bounds<Pixels>,
    data: &ChartData,
    kind: ChartKind,
    window: &mut Window,
    app: &mut App,
) {
    let theme = *app.theme(); // Theme is Copy — one read, then paint freely
    // (M2) clamp to non-negative — when the pane is smaller than the fixed
    // padding (a narrow/short tab, a live resize mid-drag), an unclamped
    // subtraction here would give paint_quad negative-size Bounds for the
    // axis lines below (a visual glitch, not a panic, but still wrong).
    let plot_w = (f32::from(bounds.size.width) - (PAD_LEFT + PAD_RIGHT)).max(0.0);
    let plot_h = (f32::from(bounds.size.height) - (PAD_TOP + PAD_BOTTOM)).max(0.0);
    let plot =
        Bounds::new(bounds.origin + point(px(PAD_LEFT), px(PAD_TOP)), size(px(plot_w), px(plot_h)));
    // axes: 1px quads (design §2.3)
    window.paint_quad(gpui::fill(
        Bounds::new(point(plot.left(), plot.bottom()), size(plot.size.width, px(1.))),
        theme.border,
    ));
    window.paint_quad(gpui::fill(
        Bounds::new(point(plot.left(), plot.top()), size(px(1.), plot.size.height)),
        theme.border,
    ));

    let Some(raw_range) = chart_data::value_range(&data.series) else {
        paint_label(plot.origin, "žádná číselná data k vykreslení", theme.text_muted, window, app);
        return;
    };
    let range = match kind {
        ChartKind::Bar => chart_data::bar_range(raw_range),
        ChartKind::Line => raw_range,
    };
    let shown = chart_data::visible_ticks(data.x_labels.len(), f32::from(plot.size.width));
    let h = f32::from(plot.size.height);
    let w = f32::from(plot.size.width);
    let tick_w = w / shown.max(1) as f32;

    match kind {
        ChartKind::Bar => {
            let group_w = tick_w * 0.8;
            let bar_w = (group_w / data.series.len().max(1) as f32).max(1.0);
            let y0 = chart_data::scale_to(range, 0.0, h);
            for t in 0..shown {
                for (si, s) in data.series.iter().enumerate() {
                    let Some(v) = s.points[t] else { continue }; // gap: no bar
                    let y = chart_data::scale_to(range, v, h);
                    let (top, bottom) = if y < y0 { (y, y0) } else { (y0, y) };
                    let x = t as f32 * tick_w + tick_w * 0.1 + si as f32 * bar_w;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(plot.left() + px(x), plot.top() + px(top)),
                            size(px(bar_w.max(1.0)), px((bottom - top).max(1.0))),
                        ),
                        series_color(&theme, si),
                    ));
                }
            }
        }
        ChartKind::Line => {
            for (si, s) in data.series.iter().enumerate() {
                // one stroked path per maximal run of consecutive points; a
                // gap (None) breaks the run (design §2.2: skip the segment)
                let color = series_color(&theme, si);
                let mut run: Vec<Point<Pixels>> = Vec::new();
                for t in 0..shown {
                    match s.points[t] {
                        Some(v) => run.push(point(
                            plot.left() + px(t as f32 * tick_w + tick_w / 2.0),
                            plot.top() + px(chart_data::scale_to(range, v, h)),
                        )),
                        None => flush_run(&mut run, color, window),
                    }
                }
                flush_run(&mut run, color, window);
            }
        }
    }

    // x tick labels: every Nth so they don't collide (ordinary GPUI text
    // shaping, same shape_line+paint idiom as er_diagram's paint_text_line)
    let label_every = ((LABEL_MIN_PX / tick_w).ceil() as usize).max(1);
    for t in (0..shown).step_by(label_every) {
        paint_label(
            point(plot.left() + px(t as f32 * tick_w), plot.bottom() + px(4.)),
            &data.x_labels[t],
            theme.text_muted,
            window,
            app,
        );
    }
    // y axis: min + max
    paint_label(
        point(bounds.left() + px(2.), plot.top()),
        &chart_data::format_axis(range.1),
        theme.text_muted,
        window,
        app,
    );
    paint_label(
        point(bounds.left() + px(2.), plot.bottom() - px(14.)),
        &chart_data::format_axis(range.0),
        theme.text_muted,
        window,
        app,
    );
    // honest truncation note (design §2.1 / curation 3)
    if shown < data.total_rows {
        paint_label(
            point(plot.left(), bounds.top()),
            &format!("zobrazeno prvních {shown} z {} řádků", data.total_rows),
            theme.warn,
            window,
            app,
        );
    }
}

/// ≥2 points: `PathBuilder::stroke(px(1.5))` + move_to/line_to/build/
/// paint_path (er_diagram_view.rs:559-567 verbatim idiom). Exactly 1 point:
/// a 3×3 dot quad so an isolated value between gaps is still visible.
/// Clears the run either way.
fn flush_run(run: &mut Vec<Point<Pixels>>, color: Hsla, window: &mut Window) {
    match run.len() {
        0 => {}
        1 => {
            let p = run[0];
            window.paint_quad(gpui::fill(
                Bounds::new(point(p.x - px(1.5), p.y - px(1.5)), size(px(3.0), px(3.0))),
                color,
            ));
        }
        _ => {
            let mut builder = PathBuilder::stroke(px(1.5));
            builder.move_to(run[0]);
            for p in &run[1..] {
                builder.line_to(*p);
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
    }
    run.clear();
}

/// Single-run shape_line + paint, control chars sanitized to spaces first —
/// small deliberate copy of `er_diagram_view::paint_text_line` (that fn is
/// private and takes a bold flag/explicit sizes this call site doesn't
/// need; house precedent for small copies: `collapse_sql`).
fn paint_label(origin: Point<Pixels>, text: &str, color: Hsla, window: &mut Window, app: &mut App) {
    if text.is_empty() {
        return;
    }
    // Every control char (including '\n', which shape_line debug-asserts
    // against) collapses to a space so hostile/garbage cell text can never
    // panic the paint pass.
    let sanitized: String = text.chars().map(|c| if (c as u32) < 0x20 { ' ' } else { c }).collect();
    let font = window.text_style().font();
    let run = TextRun {
        len: sanitized.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let font_size = window.text_style().font_size.to_pixels(window.rem_size());
    let shaped = window.text_system().shape_line(sanitized.into(), font_size, &[run], None);
    // Defensive: any remaining shaping failure must never panic the paint
    // pass — worst case, that one label is silently skipped.
    let _ = shaped.paint(origin, font_size, gpui::TextAlign::Left, None, window, app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::arrow::array::{Int64Array, RecordBatch, StringArray};
    use dbc_core::arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    /// `ResultBuffer` over a tiny in-memory Arrow batch — exact fixture
    /// idiom of dbc-buffer's own `batch()` test helper
    /// (dbc-buffer/src/lib.rs:224), with a NULLABLE Int64 y-column: rows
    /// 0..4, y NULL at row 2.
    fn test_buffer() -> Rc<RefCell<ResultBuffer>> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("label", DataType::Utf8, false),
            Field::new("y", DataType::Int64, true),
        ]));
        let labels = StringArray::from_iter_values((0..4).map(|i| format!("r{i}")));
        let ys = Int64Array::from_iter([Some(10), Some(20), None, Some(40)]);
        let b = RecordBatch::try_new(schema, vec![Arc::new(labels), Arc::new(ys)]).unwrap();
        let mut buf = ResultBuffer::new(b.schema());
        buf.push(b).unwrap();
        Rc::new(RefCell::new(buf))
    }

    #[test]
    fn compute_reads_nulls_as_gaps_and_respects_the_hard_cap() {
        let buffer = test_buffer();
        let data = ChartView::compute(&buffer, /*x_col*/ 0, &[1]);
        assert_eq!(data.total_rows, 4);
        assert_eq!(data.x_labels, vec!["r0", "r1", "r2", "r3"]);
        // the NULL cell surfaced as a gap, never 0 (design §2.2):
        assert_eq!(data.series[0].points, vec![Some(10.0), Some(20.0), None, Some(40.0)]);
        assert!(data.x_labels.len() <= chart_data::CHART_ROW_HARD_CAP);
    }

    #[test]
    fn compute_skips_out_of_range_y_columns_without_panicking() {
        let buffer = test_buffer();
        let data = ChartView::compute(&buffer, 0, &[1, 99]); // 99: belt only
        assert_eq!(data.series.len(), 1);
    }

    /// M1: a pathologically wide x-column cell (e.g. TEXT/VARCHAR(MAX) —
    /// x-columns are unfiltered by type) must be capped BEFORE it's stored,
    /// not just sanitized at paint time.
    #[test]
    fn compute_caps_x_label_length_and_marks_truncation() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("label", DataType::Utf8, false),
            Field::new("y", DataType::Int64, true),
        ]));
        let long = "x".repeat(10_000);
        let labels = StringArray::from_iter_values([long]);
        let ys = Int64Array::from_iter([Some(1)]);
        let b = RecordBatch::try_new(schema, vec![Arc::new(labels), Arc::new(ys)]).unwrap();
        let mut buf = ResultBuffer::new(b.schema());
        buf.push(b).unwrap();
        let buffer = Rc::new(RefCell::new(buf));

        let data = ChartView::compute(&buffer, 0, &[1]);
        assert_eq!(data.x_labels.len(), 1);
        let capped = &data.x_labels[0];
        assert!(capped.chars().count() <= X_LABEL_CHAR_CAP + 1, "got {} chars", capped.chars().count());
        assert!(capped.ends_with('…'));
    }

    /// M3: belt cap on series count — the picker (Task 11) is the real UX
    /// bound, this only stops a runaway y_cols list from painting an
    /// unbounded number of series.
    #[test]
    fn compute_caps_series_count_at_max_series() {
        let n = MAX_SERIES + 5;
        let mut fields = vec![Field::new("label", DataType::Utf8, false)];
        let mut arrays: Vec<Arc<dyn dbc_core::arrow::array::Array>> = vec![Arc::new(
            StringArray::from_iter_values((0..1).map(|i| format!("r{i}"))),
        )];
        for i in 0..n {
            fields.push(Field::new(format!("y{i}"), DataType::Int64, true));
            arrays.push(Arc::new(Int64Array::from_iter([Some(i as i64)])));
        }
        let schema = Arc::new(Schema::new(fields));
        let b = RecordBatch::try_new(schema, arrays).unwrap();
        let mut buf = ResultBuffer::new(b.schema());
        buf.push(b).unwrap();
        let buffer = Rc::new(RefCell::new(buf));

        let y_cols: Vec<usize> = (1..=n).collect(); // all n y-columns, 1-indexed past label
        let data = ChartView::compute(&buffer, 0, &y_cols);
        assert_eq!(data.series.len(), MAX_SERIES);
    }
}
