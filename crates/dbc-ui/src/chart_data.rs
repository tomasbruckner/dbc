//! G14 charts — pure data prep + scale math (design §2.2). GPUI-free like
//! tabs.rs/sandbox.rs; chart_view.rs only paints this module's output.
// consumed by chart_view.rs (G14 Task 8); allow removed there
#![allow(dead_code)]

pub const CHART_ROW_HARD_CAP: usize = 500;
pub const MIN_PX_PER_TICK: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub label: String,
    pub points: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub x_labels: Vec<String>,
    pub series: Vec<ChartSeries>,
    /// Buffer row count BEFORE capping — drives the honest truncation note.
    pub total_rows: usize,
}

/// Strict parse (mirrors sandbox::sql_value's posture): trimmed
/// `f64::from_str`, non-finite refused. A failure is a GAP (None), never a
/// silent 0 — 0 is a real, different value (design §2.2).
pub fn parse_y(cell: &str) -> Option<f64> {
    cell.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

pub fn prepare(
    x_labels: Vec<String>,
    y_columns: &[(String, Vec<Option<String>>)],
    row_cap: usize,
    total_rows: usize,
) -> ChartData {
    let cap = row_cap.min(x_labels.len());
    let x: Vec<String> = x_labels.into_iter().take(cap).collect();
    let series = y_columns
        .iter()
        .map(|(name, cells)| ChartSeries {
            label: name.clone(),
            points: cells.iter().take(cap).map(|c| c.as_deref().and_then(parse_y)).collect(),
        })
        .collect();
    ChartData { x_labels: x, series, total_rows }
}

pub fn value_range(series: &[ChartSeries]) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for s in series {
        for v in s.points.iter().flatten() {
            min = min.min(*v);
            max = max.max(*v);
        }
    }
    (min <= max).then_some((min, max))
}

/// Bars are drawn FROM zero — a bar chart whose axis starts at the data
/// minimum lies about magnitude.
pub fn bar_range(range: (f64, f64)) -> (f64, f64) {
    (range.0.min(0.0), range.1.max(0.0))
}

/// Pixel distance from the plot TOP for `value` (GPUI y grows downward).
/// Degenerate range (constant column) → midline, never a division by zero.
pub fn scale_to(range: (f64, f64), value: f64, pixel_height: f32) -> f32 {
    let span = range.1 - range.0;
    if !(span > 0.0) || !span.is_finite() {
        return pixel_height / 2.0;
    }
    let frac = ((value - range.0) / span).clamp(0.0, 1.0) as f32;
    pixel_height - frac * pixel_height
}

/// Curation item 3: width-derived cap — floor(w / 3px), 500 hard bound.
pub fn visible_ticks(total_ticks: usize, plot_width_px: f32) -> usize {
    if total_ticks == 0 {
        return 0;
    }
    let by_width = (plot_width_px / MIN_PX_PER_TICK).floor().max(1.0) as usize;
    total_ticks.min(by_width).min(CHART_ROW_HARD_CAP)
}

/// Axis label: integers without a decimal tail, everything else trimmed of
/// trailing zeros ("{v}" via Rust's shortest-roundtrip float Display).
pub fn format_axis(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_y_strict() {
        assert_eq!(parse_y(" 42 "), Some(42.0));
        assert_eq!(parse_y("3.14"), Some(3.14));
        assert_eq!(parse_y("-1.5e3"), Some(-1500.0));
        assert_eq!(parse_y(""), None);
        assert_eq!(parse_y("abc"), None);
        assert_eq!(parse_y("1,5"), None); // locale comma is NOT a number
        assert_eq!(parse_y("NaN"), None); // non-finite is a gap, not a point
        assert_eq!(parse_y("inf"), None);
    }

    #[test]
    fn prepare_null_and_garbage_become_gaps_never_zero() {
        let data = prepare(
            vec!["a".into(), "b".into(), "c".into()],
            &[("y".into(), vec![Some("1".into()), None, Some("x".into())])],
            500,
            3,
        );
        assert_eq!(data.series[0].points, vec![Some(1.0), None, None]);
    }

    #[test]
    fn prepare_caps_rows_and_keeps_total() {
        let x: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        let cells: Vec<Option<String>> = (0..10).map(|i| Some(i.to_string())).collect();
        let data = prepare(x, &[("y".into(), cells)], 4, 10);
        assert_eq!(data.x_labels.len(), 4);
        assert_eq!(data.series[0].points.len(), 4);
        assert_eq!(data.total_rows, 10);
    }

    #[test]
    fn prepare_multiple_y_columns_in_input_order() {
        let data = prepare(
            vec!["a".into()],
            &[("y1".into(), vec![Some("1".into())]), ("y2".into(), vec![Some("2".into())])],
            500,
            1,
        );
        assert_eq!(data.series.len(), 2);
        assert_eq!(data.series[0].label, "y1");
        assert_eq!(data.series[1].label, "y2");
    }

    #[test]
    fn value_range_ignores_gaps_and_is_none_when_empty() {
        let s = vec![ChartSeries { label: "y".into(), points: vec![None, Some(-2.0), Some(5.0)] }];
        assert_eq!(value_range(&s), Some((-2.0, 5.0)));
        let empty = vec![ChartSeries { label: "y".into(), points: vec![None, None] }];
        assert_eq!(value_range(&empty), None);
    }

    #[test]
    fn bar_range_always_includes_zero() {
        assert_eq!(bar_range((2.0, 5.0)), (0.0, 5.0));
        assert_eq!(bar_range((-5.0, -2.0)), (-5.0, 0.0));
        assert_eq!(bar_range((-1.0, 1.0)), (-1.0, 1.0));
    }

    #[test]
    fn scale_to_min_max_mid_and_degenerate() {
        // px from the plot TOP: max → 0.0, min → full height.
        assert_eq!(scale_to((0.0, 10.0), 10.0, 100.0), 0.0);
        assert_eq!(scale_to((0.0, 10.0), 0.0, 100.0), 100.0);
        assert_eq!(scale_to((0.0, 10.0), 5.0, 100.0), 50.0);
        // Constant column (min == max) must not divide by zero — midline.
        assert_eq!(scale_to((7.0, 7.0), 7.0, 100.0), 50.0);
    }

    #[test]
    fn visible_ticks_width_derived_with_hard_cap() {
        // curation item 3: max_bars = plot_width_px / 3, hard cap 500.
        assert_eq!(visible_ticks(1000, 300.0), 100);
        assert_eq!(visible_ticks(50, 300.0), 50); // fewer rows than room
        assert_eq!(visible_ticks(10_000, 9000.0), 500); // hard cap
        assert_eq!(visible_ticks(10, 1.0), 1); // degenerate width
        assert_eq!(visible_ticks(0, 300.0), 0);
    }

    #[test]
    fn format_axis_trims_noise() {
        assert_eq!(format_axis(1500.0), "1500");
        assert_eq!(format_axis(3.14), "3.14");
        assert_eq!(format_axis(0.5), "0.5");
        assert_eq!(format_axis(-2.0), "-2");
    }
}
