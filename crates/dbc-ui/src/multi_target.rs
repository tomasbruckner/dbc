//! State behind `TabContent::MultiTarget` (spec §3): one slot per target,
//! each with its own result grids, plus the text-only merged view. Plain
//! data behind `Rc<RefCell<_>>` like `ScriptRunState` — the spawned event
//! loop in `main.rs` mutates it and calls `cx.notify()`; rendering lives
//! on `AppView`. GPUI appears only as the `Entity<ResultGrid>` handle
//! type, the same allowance `tabs.rs` makes.
//!
//! Merged view: `ResultBuffer` exposes cells only as text
//! (`cell_text`/`cell_is_null`), so the merged grid is Utf8 throughout —
//! a documented deviation from the spec's „typ zachován" (see the
//! spec's §8 note). Numbers still sort/copy as their text.
//!
//! Task 6/7 wire this module into `AppView`/the event loop; until then
//! nothing in the binary constructs a `MultiTargetState`, hence the
//! blanket `#[allow(dead_code)]`.

#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dbc_buffer::ResultBuffer;
use dbc_core::arrow::array::{ArrayRef, RecordBatch, StringArray};
use dbc_core::arrow::datatypes::{DataType, Field, Schema};
use gpui::Entity;

use crate::grid::ResultGrid;
use crate::tabs::collapse_title;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One row-producing statement's grid inside a target.
pub struct ResultSlot {
    pub grid: Entity<ResultGrid>,
    pub buffer: Rc<RefCell<ResultBuffer>>,
    pub sql: String,
}

pub struct TargetSlot {
    pub conn_name: String,
    pub database: String,
    pub status: TargetStatus,
    pub results: Vec<ResultSlot>,
    pub active_result: usize,
    pub rows_returned: u64,
    pub affected: u64,
    pub writes: usize,
    pub statements_total: usize,
    pub error: Option<String>,
    pub elapsed: Option<Duration>,
}

impl TargetSlot {
    pub fn new(conn_name: &str, database: &str) -> Self {
        Self {
            conn_name: conn_name.to_string(),
            database: database.to_string(),
            status: TargetStatus::Pending,
            results: Vec::new(),
            active_result: 0,
            rows_returned: 0,
            affected: 0,
            writes: 0,
            statements_total: 0,
            error: None,
            elapsed: None,
        }
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.conn_name, self.database)
    }
}

pub struct MultiTargetState {
    pub sql: String,
    pub targets: Vec<TargetSlot>,
    pub active: usize,
    pub merged: Option<ResultSlot>,
    pub show_merged: bool,
    pub started_at: Instant,
    pub finished: bool,
}

impl MultiTargetState {
    pub fn new(sql: &str, targets: Vec<(String, String)>) -> Self {
        Self {
            sql: sql.to_string(),
            targets: targets.iter().map(|(c, d)| TargetSlot::new(c, d)).collect(),
            active: 0,
            merged: None,
            show_merged: false,
            started_at: Instant::now(),
            finished: false,
        }
    }

    pub fn all_finished(&self) -> bool {
        self.targets.iter().all(|t| !matches!(t.status, TargetStatus::Pending | TargetStatus::Running))
    }

    pub fn any_rows(&self) -> bool {
        self.targets.iter().any(|t| t.rows_returned > 0)
    }

    fn count(&self, s: TargetStatus) -> usize {
        self.targets.iter().filter(|t| t.status == s).count()
    }
}

/// `1 204` — a narrow no-break space every three digits, the Czech way.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\u{202F}');
        }
        out.push(ch);
    }
    out
}

pub fn chip_text(slot: &TargetSlot) -> String {
    let label = slot.label();
    match slot.status {
        TargetStatus::Pending => format!("{label} · čeká"),
        TargetStatus::Running => format!("{label} · běží"),
        TargetStatus::Failed => format!("✗ {label} · chyba"),
        TargetStatus::Cancelled => format!("{label} · zrušeno"),
        TargetStatus::Done => {
            if slot.rows_returned == 0 && slot.writes > 0 {
                format!("✓ {label} · {} ovlivněno", group_thousands(slot.affected))
            } else {
                format!("✓ {label} · {} ř.", group_thousands(slot.rows_returned))
            }
        }
    }
}

pub fn tab_title(n: usize, sql: &str) -> String {
    format!("{n}× {}", collapse_title(sql))
}

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64()).replace('.', ",")
}

pub fn status_line(state: &MultiTargetState) -> String {
    let mut parts = vec![
        format!("{} cílů", state.targets.len()),
        format!("{} hotovo", state.count(TargetStatus::Done)),
        format!("{} chyba", state.count(TargetStatus::Failed)),
        format!("{} běží", state.count(TargetStatus::Running)),
    ];
    if state.show_merged {
        parts.push("sloučeno".to_string());
        let failed = state.count(TargetStatus::Failed);
        if failed > 0 {
            let noun = match failed {
                1 => "cíl s chybou není",
                2..=4 => "cíle s chybou nejsou",
                _ => "cílů s chybou není",
            };
            parts.push(format!("{failed} {noun} ve sloučení"));
        }
        return parts.join(" · ");
    }
    if let Some(t) = state.targets.get(state.active) {
        parts.push(t.label());
        match t.status {
            TargetStatus::Failed => parts.push("chyba".to_string()),
            TargetStatus::Done => {
                if t.rows_returned == 0 && t.writes > 0 {
                    parts.push(format!("{} ovlivněno", group_thousands(t.affected)));
                } else {
                    parts.push(format!("{} ř.", group_thousands(t.rows_returned)));
                }
                if let Some(e) = t.elapsed {
                    parts.push(secs(e));
                }
            }
            TargetStatus::Running => parts.push("běží".to_string()),
            TargetStatus::Pending => parts.push("čeká".to_string()),
            TargetStatus::Cancelled => parts.push("zrušeno".to_string()),
        }
    }
    parts.join(" · ")
}

/// Per-target inputs to the merged view: `(target_ix, label, buffer)` for
/// each target with rows, using the LAST result of a target with several,
/// already filtered to non-empty buffers via `nonempty`. This is the
/// single source of truth `merged_plan` and `build_merged_buffer` both
/// build from — `merged_plan` takes its `Vec<usize>` from the same
/// filtered list that produces the column plan, so the two can never
/// drift out of step (a target whose first statement had rows but whose
/// LAST result is empty must vanish from both together, not from one and
/// not the other).
///
/// This is also the GPUI-free seam `merged_plan_from`/`build_merged_from`
/// are tested through — a real `ResultSlot` needs an `Entity<ResultGrid>`,
/// which needs a window, so tests build `(usize, String,
/// Rc<RefCell<ResultBuffer>>)` triples directly instead.
fn merged_inputs(state: &MultiTargetState) -> Vec<(usize, String, Rc<RefCell<ResultBuffer>>)> {
    let raw: Vec<(usize, String, Rc<RefCell<ResultBuffer>>)> = state
        .targets
        .iter()
        .enumerate()
        .filter(|(_, t)| t.rows_returned > 0)
        .filter_map(|(ix, t)| t.results.last().map(|last| (ix, t.label(), last.buffer.clone())))
        .collect();
    nonempty(&raw)
}

/// Drop any input whose buffer has no rows — a defensive filter so an
/// empty last result (nothing pushed, or a statement that genuinely
/// returned zero rows) does not pollute the union with its columns.
/// `merged_inputs`, `merged_plan_from`, and `build_merged_from` all go
/// through this (the latter two redundantly, if fed already-filtered
/// input — harmless) so `plan.mapping` and any index derived from the
/// same filtered list stay aligned.
fn nonempty(
    inputs: &[(usize, String, Rc<RefCell<ResultBuffer>>)],
) -> Vec<(usize, String, Rc<RefCell<ResultBuffer>>)> {
    inputs.iter().filter(|(_, _, buf)| buf.borrow().row_count() > 0).cloned().collect()
}

/// The column plan for a set of `(target_ix, label, buffer)` inputs, in
/// the order given.
fn merged_plan_from(inputs: &[(usize, String, Rc<RefCell<ResultBuffer>>)]) -> dbc_connect::targets::ColumnPlan {
    let schemas: Vec<Vec<String>> = nonempty(inputs)
        .iter()
        .map(|(_, _, buf)| buf.borrow().schema().fields().iter().map(|f| f.name().to_string()).collect())
        .collect();
    dbc_connect::targets::union_columns("zdroj", &schemas)
}

/// Which targets take part in the merged view (the ones with rows; the
/// LAST result of a target with several) and how their columns line up.
/// `ixs` and the plan are derived from the SAME `merged_inputs(state)`
/// call, so `ixs.len() == plan.mapping.len()` always holds.
pub fn merged_plan(state: &MultiTargetState) -> (dbc_connect::targets::ColumnPlan, Vec<usize>) {
    let inputs = merged_inputs(state);
    let ixs = inputs.iter().map(|(ix, _, _)| *ix).collect();
    (merged_plan_from(&inputs), ixs)
}

/// The merged buffer for a set of `(target_ix, label, buffer)` inputs:
/// every column Utf8 (see module doc), `zdroj` first. Built in 4096-row
/// batches so a large union does not allocate one giant array set. A
/// spill I/O failure (`RecordBatch::try_new` or `ResultBuffer::push`) is
/// surfaced rather than swallowed — silently dropping up to 4096 rows
/// from the merged view is worse than reporting the error.
fn build_merged_from(inputs: &[(usize, String, Rc<RefCell<ResultBuffer>>)]) -> Result<ResultBuffer, String> {
    let inputs = nonempty(inputs);
    let plan = merged_plan_from(&inputs);
    let schema = std::sync::Arc::new(Schema::new(
        plan.columns.iter().map(|c| Field::new(c, DataType::Utf8, true)).collect::<Vec<_>>(),
    ));
    let mut out = ResultBuffer::new(schema.clone());
    let ncols = plan.columns.len();
    let mut cols: Vec<Vec<Option<String>>> = vec![Vec::new(); ncols];
    let flush = |cols: &mut Vec<Vec<Option<String>>>, out: &mut ResultBuffer| -> Result<(), String> {
        if cols[0].is_empty() {
            return Ok(());
        }
        let arrays: Vec<ArrayRef> = cols
            .iter_mut()
            .map(|c| std::sync::Arc::new(StringArray::from(std::mem::take(c))) as ArrayRef)
            .collect();
        let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|e| e.to_string())?;
        out.push(batch).map_err(|e| e.to_string())
    };
    for (src_ix, (_, label, buffer)) in inputs.iter().enumerate() {
        let mut buf = buffer.borrow_mut();
        for r in 0..buf.row_count() {
            cols[0].push(Some(label.clone()));
            for (j, m) in plan.mapping[src_ix].iter().enumerate() {
                let cell = match m {
                    Some(c) if !buf.cell_is_null(r, *c) => Some(buf.cell_text(r, *c)),
                    _ => None,
                };
                cols[j + 1].push(cell);
            }
            if cols[0].len() >= 4096 {
                flush(&mut cols, &mut out)?;
            }
        }
    }
    flush(&mut cols, &mut out)?;
    Ok(out)
}

/// The merged buffer: every column Utf8 (see module doc), `zdroj` first.
/// Built in 4096-row batches so a large union does not allocate one
/// giant array set. Propagates the first spill I/O failure instead of
/// silently dropping rows.
pub fn build_merged_buffer(state: &MultiTargetState) -> Result<ResultBuffer, String> {
    build_merged_from(&merged_inputs(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::arrow::array::Int64Array;
    use std::sync::Arc;

    fn slot(label: &str, status: TargetStatus) -> TargetSlot {
        let mut s = TargetSlot::new(label.split('/').next().unwrap(), label.split('/').nth(1).unwrap());
        s.status = status;
        s
    }

    #[test]
    fn chip_text_per_status() {
        let mut s = slot("prod/a", TargetStatus::Pending);
        assert_eq!(chip_text(&s), "prod/a · čeká");
        s.status = TargetStatus::Running;
        assert_eq!(chip_text(&s), "prod/a · běží");
        s.status = TargetStatus::Failed;
        assert_eq!(chip_text(&s), "✗ prod/a · chyba");
        s.status = TargetStatus::Cancelled;
        assert_eq!(chip_text(&s), "prod/a · zrušeno");
    }

    #[test]
    fn chip_text_done_reads_rows_or_affected() {
        let mut s = slot("prod/a", TargetStatus::Done);
        s.rows_returned = 1204;
        assert_eq!(chip_text(&s), "✓ prod/a · 1\u{202F}204 ř.");
        s.rows_returned = 0;
        s.writes = 1;
        s.affected = 3;
        assert_eq!(chip_text(&s), "✓ prod/a · 3 ovlivněno");
        s.writes = 0;
        assert_eq!(chip_text(&s), "✓ prod/a · 0 ř.");
    }

    #[test]
    fn tab_title_prefixes_count() {
        assert_eq!(tab_title(5, "select  count(*)\nfrom t"), "5× select count(*) from t");
    }

    #[test]
    fn status_line_counts_and_names_active() {
        let mut st = MultiTargetState::new("select 1", vec![("prod".into(), "a".into()), ("prod".into(), "b".into()), ("x".into(), "c".into())]);
        st.targets[0].status = TargetStatus::Done;
        st.targets[0].rows_returned = 2;
        st.targets[0].elapsed = Some(Duration::from_millis(40));
        st.targets[1].status = TargetStatus::Failed;
        st.targets[2].status = TargetStatus::Running;
        assert_eq!(status_line(&st), "3 cílů · 1 hotovo · 1 chyba · 1 běží · prod/a · 2 ř. · 0,04 s");
        st.active = 1;
        assert_eq!(status_line(&st), "3 cílů · 1 hotovo · 1 chyba · 1 běží · prod/b · chyba");
        st.show_merged = true;
        assert_eq!(status_line(&st), "3 cílů · 1 hotovo · 1 chyba · 1 běží · sloučeno · 1 cíl s chybou není ve sloučení");
    }

    #[test]
    fn thousands_grouping_uses_narrow_space() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1204), "1\u{202F}204");
        assert_eq!(group_thousands(1234567), "1\u{202F}234\u{202F}567");
    }

    #[test]
    fn all_finished_and_any_rows() {
        let mut st = MultiTargetState::new("select 1", vec![("p".into(), "a".into()), ("p".into(), "b".into())]);
        assert!(!st.all_finished());
        st.targets[0].status = TargetStatus::Done;
        st.targets[1].status = TargetStatus::Cancelled;
        assert!(st.all_finished());
        assert!(!st.any_rows());
        st.targets[0].rows_returned = 1;
        assert!(st.any_rows());
    }

    /// `id:Int64, name:Utf8` with one row `(1, "x")`.
    fn buffer_a() -> ResultBuffer {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let mut buf = ResultBuffer::new(schema.clone());
        buf.push(
            RecordBatch::try_new(
                schema,
                vec![
                    Arc::new(Int64Array::from(vec![1])) as ArrayRef,
                    Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
                ],
            )
            .unwrap(),
        )
        .unwrap();
        buf
    }

    /// `name:Utf8, extra:Int64` with one row `("y", 2)`.
    fn buffer_b() -> ResultBuffer {
        let schema = Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("extra", DataType::Int64, true),
        ]));
        let mut buf = ResultBuffer::new(schema.clone());
        buf.push(
            RecordBatch::try_new(
                schema,
                vec![
                    Arc::new(StringArray::from(vec!["y"])) as ArrayRef,
                    Arc::new(Int64Array::from(vec![2])) as ArrayRef,
                ],
            )
            .unwrap(),
        )
        .unwrap();
        buf
    }

    #[test]
    fn merged_plan_skips_targets_without_rows_and_uses_last_result() {
        let empty_schema = Arc::new(Schema::new(vec![Field::new("whatever", DataType::Utf8, true)]));
        let inputs = vec![
            (0usize, "prod/a".to_string(), Rc::new(RefCell::new(buffer_a()))),
            (1usize, "prod/b".to_string(), Rc::new(RefCell::new(buffer_b()))),
            (2usize, "x/c".to_string(), Rc::new(RefCell::new(ResultBuffer::new(empty_schema)))),
        ];

        let plan = merged_plan_from(&inputs);
        assert_eq!(plan.columns, vec!["zdroj", "id", "name", "extra"]);
    }

    #[test]
    fn build_merged_buffer_is_all_text_with_source_first() {
        let inputs = vec![
            (0usize, "prod/a".to_string(), Rc::new(RefCell::new(buffer_a()))),
            (1usize, "prod/b".to_string(), Rc::new(RefCell::new(buffer_b()))),
        ];

        let mut merged = build_merged_from(&inputs).expect("merge should succeed");
        assert_eq!(merged.row_count(), 2);
        for f in merged.schema().fields() {
            assert_eq!(*f.data_type(), DataType::Utf8);
        }
        // columns: zdroj, id, name, extra
        assert_eq!(merged.cell_text(0, 0), "prod/a");
        assert_eq!(merged.cell_text(0, 1), "1");
        assert_eq!(merged.cell_text(0, 2), "x");
        assert!(merged.cell_is_null(0, 3));
        assert_eq!(merged.cell_text(1, 0), "prod/b");
        assert!(merged.cell_is_null(1, 1));
        assert_eq!(merged.cell_text(1, 2), "y");
        assert_eq!(merged.cell_text(1, 3), "2");
    }

    #[test]
    fn build_merged_buffer_batches_beyond_4096_rows() {
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
        let mut buf = ResultBuffer::new(schema.clone());
        let values: Vec<i64> = (0..5000).collect();
        buf.push(RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(values)) as ArrayRef]).unwrap())
            .unwrap();

        let inputs = vec![(0usize, "prod/a".to_string(), Rc::new(RefCell::new(buf)))];
        let mut merged = build_merged_from(&inputs).expect("merge should succeed");
        assert_eq!(merged.row_count(), 5000);
        assert_eq!(merged.cell_text(4999, 1), "4999");
    }

    /// Reproduces the desync the controller flagged: target index 1 has
    /// rows overall (a prior statement returned some) but its LAST result
    /// is an empty buffer. It must vanish from `ixs` and `plan.mapping`
    /// together — a positional zip between them must never see one
    /// without the other.
    #[test]
    fn merged_plan_indexes_match_its_mapping_when_last_result_is_empty() {
        let empty_schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let raw = vec![
            (0usize, "prod/a".to_string(), Rc::new(RefCell::new(buffer_a()))),
            (1usize, "prod/b".to_string(), Rc::new(RefCell::new(ResultBuffer::new(empty_schema)))),
            (2usize, "x/c".to_string(), Rc::new(RefCell::new(buffer_b()))),
        ];

        let filtered = nonempty(&raw);
        let ixs: Vec<usize> = filtered.iter().map(|(ix, _, _)| *ix).collect();
        let plan = merged_plan_from(&filtered);

        assert_eq!(ixs, vec![0, 2]);
        assert!(!ixs.contains(&1));
        assert_eq!(ixs.len(), plan.mapping.len());
    }
}
