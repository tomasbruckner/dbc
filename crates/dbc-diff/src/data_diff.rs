//! G7: PK-based data diff over two already-fetched `ResultBuffer`s (design
//! §4). Pure computation over already-materialized cell data — no I/O, no
//! SQL, no GPUI. `dbc-ui`'s `fetch_diff_side` (T5) is what fills the two
//! buffers this module reads.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbc_buffer::ResultBuffer;
use dbc_core::arrow::array::{Array, RecordBatch, StringArray};
use dbc_core::arrow::datatypes::{DataType, Field, Schema};

/// design §4: double `ResultBuffer`'s own in-memory row cap (spill absorbs
/// the rest) — a hard ceiling on data-diff scale, not a memory tuning knob.
pub const DIFF_ROW_CAP: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum RowDiff {
    Added { right_row: usize },
    Removed { left_row: usize },
    /// `changed_cols` indexes into `DataDiffOutcome::intersection_columns`.
    Changed { left_row: usize, right_row: usize, changed_cols: Vec<usize> },
    Unchanged { left_row: usize, right_row: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataDiffOutcome {
    pub rows: Vec<RowDiff>,
    pub intersection_columns: Vec<String>,
    pub left_only_columns: Vec<String>,
    pub right_only_columns: Vec<String>,
}

fn exceeds_row_cap(row_count: usize, cap: usize) -> bool {
    row_count > cap
}

fn over_cap_error() -> String {
    format!(
        "tabulka má víc než {DIFF_ROW_CAP} řádků — porovnání dat na tak velké tabulce zatím není podporováno; zúžete výběr přes WHERE"
    )
}

fn pk_key(buf: &mut ResultBuffer, row: usize, pk_cols: &[usize]) -> Vec<Option<String>> {
    pk_cols
        .iter()
        .map(|&c| if buf.cell_is_null(row, c) { None } else { Some(buf.cell_text(row, c)) })
        .collect()
}

fn build_pk_index(buf: &mut ResultBuffer, pk_cols: &[usize]) -> HashMap<Vec<Option<String>>, usize> {
    let mut index = HashMap::with_capacity(buf.row_count());
    for row in 0..buf.row_count() {
        index.insert(pk_key(buf, row, pk_cols), row);
    }
    index
}

/// `(intersection in LEFT order, left_only, right_only)`.
pub fn intersect_columns(left_names: &[String], right_names: &[String]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let right_set: HashSet<&str> = right_names.iter().map(String::as_str).collect();
    let left_set: HashSet<&str> = left_names.iter().map(String::as_str).collect();
    let intersection: Vec<String> = left_names.iter().filter(|n| right_set.contains(n.as_str())).cloned().collect();
    let left_only: Vec<String> = left_names.iter().filter(|n| !right_set.contains(n.as_str())).cloned().collect();
    let right_only: Vec<String> = right_names.iter().filter(|n| !left_set.contains(n.as_str())).cloned().collect();
    (intersection, left_only, right_only)
}

fn intersection_col_pairs(intersection: &[String], left_names: &[String], right_names: &[String]) -> Vec<(usize, usize)> {
    intersection
        .iter()
        .map(|name| {
            let li = left_names.iter().position(|n| n == name).expect("name came from the intersection");
            let ri = right_names.iter().position(|n| n == name).expect("name came from the intersection");
            (li, ri)
        })
        .collect()
}

/// design §4: NULL-vs-NULL equal, NULL-vs-value always different (checked
/// first). Numeric family -> parse both as f64, fallback to trimmed string
/// on parse failure (never panics). Boolean family -> parse both as bool
/// (case-insensitive), same fallback. Everything else -> trimmed string.
///
/// Deviation from the plan's literal draft (justified, see task brief):
/// the numeric branch additionally falls back to trimmed-string compare
/// when EITHER parsed value is non-finite (`NaN`). IEEE-754 `NaN != NaN`
/// would otherwise mark two cells that are textually identical (e.g. both
/// "NaN") as Changed — a "NaN-poisoned equality surprise" the task brief
/// explicitly calls out to avoid. `Infinity`/`-Infinity` need no such
/// fallback: `f64::INFINITY == f64::INFINITY` is `true` and already
/// deterministic, so only the NaN case is special-cased.
fn cells_equal(left_type: &DataType, right_type: &DataType, left_null: bool, right_null: bool, left_text: &str, right_text: &str) -> bool {
    if left_null || right_null {
        return left_null == right_null;
    }
    if left_type.is_numeric() && right_type.is_numeric() {
        return match (left_text.trim().parse::<f64>(), right_text.trim().parse::<f64>()) {
            (Ok(l), Ok(r)) if l.is_nan() || r.is_nan() => left_text.trim() == right_text.trim(),
            (Ok(l), Ok(r)) => l == r,
            _ => left_text.trim() == right_text.trim(),
        };
    }
    if matches!(left_type, DataType::Boolean) && matches!(right_type, DataType::Boolean) {
        let lb = left_text.trim().to_ascii_lowercase().parse::<bool>();
        let rb = right_text.trim().to_ascii_lowercase().parse::<bool>();
        return match (lb, rb) {
            (Ok(l), Ok(r)) => l == r,
            _ => left_text.trim() == right_text.trim(),
        };
    }
    left_text.trim() == right_text.trim()
}

pub fn diff_data(
    left: &mut ResultBuffer, left_names: &[String], left_pk_cols: &[usize],
    right: &mut ResultBuffer, right_names: &[String], right_pk_cols: &[usize],
) -> Result<DataDiffOutcome, String> {
    if exceeds_row_cap(left.row_count(), DIFF_ROW_CAP) || exceeds_row_cap(right.row_count(), DIFF_ROW_CAP) {
        return Err(over_cap_error());
    }
    let (intersection, left_only, right_only) = intersect_columns(left_names, right_names);
    let inter_cols = intersection_col_pairs(&intersection, left_names, right_names);
    let left_types: Vec<DataType> = left.schema().fields().iter().map(|f| f.data_type().clone()).collect();
    let right_types: Vec<DataType> = right.schema().fields().iter().map(|f| f.data_type().clone()).collect();

    let right_index = build_pk_index(right, right_pk_cols);
    let mut matched_right: HashSet<usize> = HashSet::new();
    let mut rows = Vec::with_capacity(left.row_count().max(right.row_count()));

    for lrow in 0..left.row_count() {
        let key = pk_key(left, lrow, left_pk_cols);
        match right_index.get(&key) {
            None => rows.push(RowDiff::Removed { left_row: lrow }),
            Some(&rrow) => {
                matched_right.insert(rrow);
                let mut changed_cols = Vec::new();
                for (ix, &(lc, rc)) in inter_cols.iter().enumerate() {
                    let ln = left.cell_is_null(lrow, lc);
                    let rn = right.cell_is_null(rrow, rc);
                    let lt = left.cell_text(lrow, lc);
                    let rt = right.cell_text(rrow, rc);
                    if !cells_equal(&left_types[lc], &right_types[rc], ln, rn, &lt, &rt) {
                        changed_cols.push(ix);
                    }
                }
                rows.push(if changed_cols.is_empty() {
                    RowDiff::Unchanged { left_row: lrow, right_row: rrow }
                } else {
                    RowDiff::Changed { left_row: lrow, right_row: rrow, changed_cols }
                });
            }
        }
    }
    for rrow in 0..right.row_count() {
        if !matched_right.contains(&rrow) {
            rows.push(RowDiff::Added { right_row: rrow });
        }
    }

    Ok(DataDiffOutcome { rows, intersection_columns: intersection, left_only_columns: left_only, right_only_columns: right_only })
}

/// Synthetic all-Utf8 "old → new" batch for the "Změněné řádky" grid
/// section (design §4), plus the exact `(row, col)` set that changed — the
/// grid's tint side-channel.
pub fn build_changed_batch(
    left: &mut ResultBuffer, right: &mut ResultBuffer,
    intersection_columns: &[String], left_names: &[String], right_names: &[String],
    rows: &[RowDiff],
) -> (RecordBatch, HashSet<(usize, usize)>) {
    let inter_cols = intersection_col_pairs(intersection_columns, left_names, right_names);
    let mut tinted: HashSet<(usize, usize)> = HashSet::new();
    let mut columns: Vec<Vec<Option<String>>> = vec![Vec::new(); intersection_columns.len()];
    let mut out_row = 0usize;

    for rd in rows {
        let RowDiff::Changed { left_row, right_row, changed_cols } = rd else { continue };
        let changed_set: HashSet<usize> = changed_cols.iter().copied().collect();
        for (ix, &(lc, rc)) in inter_cols.iter().enumerate() {
            let text = if changed_set.contains(&ix) {
                tinted.insert((out_row, ix));
                let lt = if left.cell_is_null(*left_row, lc) { "NULL".to_string() } else { left.cell_text(*left_row, lc) };
                let rt = if right.cell_is_null(*right_row, rc) { "NULL".to_string() } else { right.cell_text(*right_row, rc) };
                format!("{lt} → {rt}")
            } else if left.cell_is_null(*left_row, lc) {
                String::new()
            } else {
                left.cell_text(*left_row, lc)
            };
            columns[ix].push(Some(text));
        }
        out_row += 1;
    }

    let fields: Vec<Field> = intersection_columns.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect();
    let schema = Arc::new(Schema::new(fields));
    let arrays: Vec<Arc<dyn Array>> = columns.into_iter().map(|c| Arc::new(StringArray::from(c)) as Arc<dyn Array>).collect();
    let batch = RecordBatch::try_new(schema, arrays).expect("well-formed synthetic diff batch — column count matches schema by construction");
    (batch, tinted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::arrow::array::StringArray;

    fn buf(names: &[&str], rows: Vec<Vec<Option<&str>>>) -> (ResultBuffer, Vec<String>) {
        let fields: Vec<Field> = names.iter().map(|n| Field::new(*n, DataType::Utf8, true)).collect();
        let schema = Arc::new(Schema::new(fields));
        let ncols = names.len();
        let mut arrays: Vec<Arc<dyn Array>> = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let col: Vec<Option<&str>> = rows.iter().map(|r| r[c]).collect();
            arrays.push(Arc::new(StringArray::from(col)));
        }
        let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let mut rb = ResultBuffer::new(schema);
        rb.push(batch).unwrap();
        (rb, names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn classifies_added_removed_changed_unchanged() {
        let (mut left, ln) = buf(&["id", "n"], vec![
            vec![Some("1"), Some("a")], vec![Some("2"), Some("b")], vec![Some("3"), Some("c")],
        ]);
        let (mut right, rn) = buf(&["id", "n"], vec![
            vec![Some("1"), Some("a")], vec![Some("2"), Some("B")], vec![Some("4"), Some("d")],
        ]);
        let outcome = diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
        let added = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Added { .. })).count();
        let removed = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Removed { .. })).count();
        let changed = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Changed { .. })).count();
        let unchanged = outcome.rows.iter().filter(|r| matches!(r, RowDiff::Unchanged { .. })).count();
        assert_eq!((added, removed, changed, unchanged), (1, 1, 1, 1));
    }

    #[test]
    fn column_set_intersection_when_sides_differ() {
        let (mut left, ln) = buf(&["id", "a", "only_left"], vec![vec![Some("1"), Some("x"), Some("z")]]);
        let (mut right, rn) = buf(&["id", "a", "only_right"], vec![vec![Some("1"), Some("x"), Some("w")]]);
        let outcome = diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
        assert_eq!(outcome.intersection_columns, vec!["id".to_string(), "a".to_string()]);
        assert_eq!(outcome.left_only_columns, vec!["only_left".to_string()]);
        assert_eq!(outcome.right_only_columns, vec!["only_right".to_string()]);
        assert!(matches!(outcome.rows[0], RowDiff::Unchanged { .. }));
    }

    #[test]
    fn build_changed_batch_marks_only_the_differing_cells() {
        let (mut left, ln) = buf(&["id", "n"], vec![vec![Some("1"), Some("a")]]);
        let (mut right, rn) = buf(&["id", "n"], vec![vec![Some("1"), Some("b")]]);
        let outcome = diff_data(&mut left, &ln, &[0], &mut right, &rn, &[0]).unwrap();
        let (batch, tinted) = build_changed_batch(&mut left, &mut right, &outcome.intersection_columns, &ln, &rn, &outcome.rows);
        assert_eq!(batch.num_rows(), 1);
        assert!(tinted.contains(&(0, 1)));
        assert!(!tinted.contains(&(0, 0)));
    }

    // --- typed value comparison, unit-tested directly (avoids arrow's own
    // numeric-to-text formatting quirks muddying the intent) ---

    #[test]
    fn numeric_text_variants_compare_equal() {
        assert!(cells_equal(&DataType::Int64, &DataType::Float64, false, false, "1", "1.0"));
        assert!(!cells_equal(&DataType::Int64, &DataType::Float64, false, false, "1", "2"));
    }

    #[test]
    fn null_vs_null_is_equal_null_vs_value_is_changed() {
        assert!(cells_equal(&DataType::Utf8, &DataType::Utf8, true, true, "", ""));
        assert!(!cells_equal(&DataType::Utf8, &DataType::Utf8, true, false, "", "x"));
    }

    #[test]
    fn non_numeric_non_bool_uses_trimmed_string_compare() {
        assert!(cells_equal(&DataType::Utf8, &DataType::Utf8, false, false, " a ", "a"));
        assert!(!cells_equal(&DataType::Utf8, &DataType::Utf8, false, false, "a", "b"));
    }

    #[test]
    fn boolean_family_compares_case_insensitively() {
        assert!(cells_equal(&DataType::Boolean, &DataType::Boolean, false, false, "true", "TRUE"));
        assert!(!cells_equal(&DataType::Boolean, &DataType::Boolean, false, false, "true", "false"));
    }

    // --- non-finite floats: deterministic, never panics, never NaN-poisoned
    // (task brief requirement — deviation from the plan's literal draft) ---

    #[test]
    fn nan_text_on_both_sides_compares_equal_via_string_fallback() {
        // IEEE-754 NaN != NaN would otherwise make two textually-identical
        // "NaN" cells show as Changed. That's a surprise, not a diff. The
        // fallback is plain trimmed-string compare (case-sensitive, same as
        // every other non-numeric-parseable comparison — no engine-driven
        // casing normalization, matching the design's stated philosophy).
        assert!(cells_equal(&DataType::Float64, &DataType::Float64, false, false, "NaN", "NaN"));
        assert!(!cells_equal(&DataType::Float64, &DataType::Float64, false, false, "nan", "NaN"));
    }

    #[test]
    fn nan_vs_a_real_number_is_changed() {
        assert!(!cells_equal(&DataType::Float64, &DataType::Float64, false, false, "NaN", "1.0"));
    }

    #[test]
    fn infinity_compares_equal_without_special_casing() {
        assert!(cells_equal(&DataType::Float64, &DataType::Float64, false, false, "inf", "infinity"));
        assert!(!cells_equal(&DataType::Float64, &DataType::Float64, false, false, "inf", "-inf"));
    }

    // --- row cap ---

    #[test]
    fn exceeds_row_cap_boundary() {
        assert!(!exceeds_row_cap(DIFF_ROW_CAP, DIFF_ROW_CAP));
        assert!(exceeds_row_cap(DIFF_ROW_CAP + 1, DIFF_ROW_CAP));
    }

    #[test]
    fn over_cap_error_is_explicit_not_silent() {
        let msg = over_cap_error();
        assert!(msg.contains(&DIFF_ROW_CAP.to_string()));
        assert!(msg.to_uppercase().contains("WHERE"));
    }
}
