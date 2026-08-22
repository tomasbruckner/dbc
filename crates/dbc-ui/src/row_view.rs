//! Row-view layer for `ResultGrid`: local sort (G4 Task 2) and column
//! filters (Task 3, scaffolded here but always a no-op until `filters` is
//! populated) over the underlying `ResultBuffer`, without ever touching
//! GPUI or `ResultBuffer` directly — cell access comes through a
//! caller-supplied closure (`FnMut(row, col) -> String`) so tests here run
//! against a plain `Vec<Vec<String>>` fixture instead of a real buffer, and
//! `grid.rs` supplies a closure over `ResultBuffer::cell_text`.

/// Display-order → source-row mapping. `Identity` is the common case (no
/// sort/filter active): `source_row` for it is just the identity function,
/// and it costs nothing to keep fresh as the buffer grows during streaming
/// (`rebuild`'s no-sort/no-filter path is an O(1) early return, not an
/// O(n) rebuild) — see `ResultGrid::on_batch_grown`. `Mapped` is only
/// materialized once a sort and/or filter is actually active.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowOrder {
    Identity(usize),
    Mapped(Vec<u32>),
}

pub struct RowView {
    order: RowOrder,
    /// (source col ix, ascending). `None` = unsorted (insertion order).
    pub sort: Option<(usize, bool)>,
    /// (source col ix, needle), AND'd together. Empty until Task 3 wires a
    /// filter UI; `rebuild` already applies it so Task 3 only needs to
    /// populate this field.
    pub filters: Vec<(usize, String)>,
}

impl RowView {
    pub fn identity(rows: usize) -> Self {
        Self { order: RowOrder::Identity(rows), sort: None, filters: Vec::new() }
    }

    pub fn len(&self) -> usize {
        match &self.order {
            RowOrder::Identity(n) => *n,
            RowOrder::Mapped(v) => v.len(),
        }
    }

    /// Companion to `len` (clippy convention). Not called anywhere in
    /// `dbc-ui` yet — kept as part of the pure/tested public surface.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maps a display-order index to the underlying buffer's row index.
    /// Panics on an out-of-range `display_ix`, same as `Vec` indexing —
    /// callers (uniform_list's row range) never pass one past `len()`.
    pub fn source_row(&self, display_ix: usize) -> usize {
        match &self.order {
            RowOrder::Identity(_) => display_ix,
            RowOrder::Mapped(v) => v[display_ix] as usize,
        }
    }

    /// Rebuilds the display order from scratch against `rows` source rows,
    /// via `cell(row, col) -> String`. Filters (AND, case-insensitive
    /// substring) are applied first, then a stable sort.
    ///
    /// Sort compare: if both cell values parse as `f64`, compare
    /// numerically; otherwise fall back to a case-insensitive string
    /// compare. `sort_by` is stable, so equal keys keep their relative
    /// (post-filter) order.
    ///
    /// When neither a sort nor a filter is active this is an O(1) reset to
    /// `Identity(rows)` — cheap enough to call on every streamed batch (see
    /// `ResultGrid::on_batch_grown`), unlike the O(n) or O(n log n) work a
    /// filter/sort pass would cost.
    pub fn rebuild(&mut self, rows: usize, cell: &mut dyn FnMut(usize, usize) -> String) {
        if self.filters.is_empty() && self.sort.is_none() {
            self.order = RowOrder::Identity(rows);
            return;
        }

        let mut order: Vec<u32> = if self.filters.is_empty() {
            (0..rows as u32).collect()
        } else {
            (0..rows as u32)
                .filter(|&r| {
                    self.filters.iter().all(|(col, needle)| {
                        needle.is_empty()
                            || cell(r as usize, *col).to_lowercase().contains(&needle.to_lowercase())
                    })
                })
                .collect()
        };

        if let Some((col, asc)) = self.sort {
            order.sort_by(|&a, &b| {
                let av = cell(a as usize, col);
                let bv = cell(b as usize, col);
                let ord = match (av.parse::<f64>(), bv.parse::<f64>()) {
                    (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                    _ => av.to_lowercase().cmp(&bv.to_lowercase()),
                };
                if asc {
                    ord
                } else {
                    ord.reverse()
                }
            });
        }

        self.order = RowOrder::Mapped(order);
    }
}

/// G4 Task 3: pure match-finding for the Ctrl+F in-result search — given
/// `rows` DISPLAY-order rows and an explicit list of (visible) SOURCE
/// column indices, returns every `(display_row, source_col)` whose cell
/// text contains `needle` (case-insensitive), in row-major order. `cell`
/// is called with the SAME coordinates that end up in the result, so
/// `grid.rs`'s caller closure is responsible for mapping `display_row`
/// through `RowView::source_row` before reading the buffer — this function
/// itself has no notion of a buffer or a `RowView`, only cell text, same
/// "closure-supplied cell access" shape as `RowView::rebuild`. An empty
/// `needle` yields no matches (nothing to jump to, rather than "everything
/// matches" like an empty filter) since a find-bar's whole purpose is
/// jumping between specific matches.
pub fn find_matches(
    rows: usize,
    cols: &[usize],
    needle: &str,
    cell: &mut dyn FnMut(usize, usize) -> String,
) -> Vec<(usize, usize)> {
    find_matches_capped(rows, cols, needle, usize::MAX, usize::MAX, cell).0
}

/// Capped variant (Task 3 review issue 2): the scan runs synchronously on
/// the UI thread per keystroke, so both the scanned row count and the match
/// count are bounded. Returns `(matches, capped)` — `capped == true` when
/// the scan stopped early (rows beyond `max_rows` unscanned, or
/// `max_matches` reached), so the UI can show "1000+" instead of a wrong
/// exact total.
pub fn find_matches_capped(
    rows: usize,
    cols: &[usize],
    needle: &str,
    max_rows: usize,
    max_matches: usize,
    cell: &mut dyn FnMut(usize, usize) -> String,
) -> (Vec<(usize, usize)>, bool) {
    if needle.is_empty() {
        return (Vec::new(), false);
    }
    let needle_lower = needle.to_lowercase();
    let mut out = Vec::new();
    let scanned_rows = rows.min(max_rows);
    for r in 0..scanned_rows {
        for &c in cols {
            if cell(r, c).to_lowercase().contains(&needle_lower) {
                out.push((r, c));
                if out.len() >= max_matches {
                    return (out, true);
                }
            }
        }
    }
    (out, scanned_rows < rows)
}

/// Wrap-around next/prev index into a match list of length `len`. `None`
/// when `len == 0` (nothing to jump to). `current == None` (no selection
/// yet) lands on the first match going forward, or the last one going
/// backward — so a fresh Ctrl+F followed immediately by Shift+Enter still
/// does something sensible instead of no-op'ing.
pub fn wrapped_index(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => {
            if forward {
                0
            } else {
                len - 1
            }
        }
        Some(cur) => {
            if forward {
                (cur + 1) % len
            } else {
                (cur + len - 1) % len
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `cell` closure over a fixture grid (`rows[row][col]`).
    fn accessor(rows: Vec<Vec<&'static str>>) -> impl FnMut(usize, usize) -> String {
        move |r, c| rows[r][c].to_string()
    }

    #[test]
    fn identity_is_the_default_and_maps_1_to_1() {
        let v = RowView::identity(5);
        assert_eq!(v.len(), 5);
        assert!(!v.is_empty());
        for i in 0..5 {
            assert_eq!(v.source_row(i), i);
        }
    }

    #[test]
    fn empty_identity_reports_empty() {
        let v = RowView::identity(0);
        assert_eq!(v.len(), 0);
        assert!(v.is_empty());
    }

    #[test]
    fn rebuild_with_no_sort_or_filter_stays_cheap_identity() {
        let mut v = RowView::identity(0);
        let mut cell = accessor(vec![vec!["a"], vec!["b"], vec!["c"]]);
        v.rebuild(3, &mut cell);
        assert_eq!(v.len(), 3);
        assert_eq!(v.source_row(0), 0);
        assert_eq!(v.source_row(2), 2);
    }

    #[test]
    fn numeric_sort_compares_as_numbers_not_strings() {
        // Lexicographically "10" < "2" < "9", but numerically 2 < 9 < 10 —
        // this is the whole point of the numeric-parse fast path.
        let rows = vec![vec!["9"], vec!["10"], vec!["2"]];
        let mut v = RowView::identity(rows.len());
        v.sort = Some((0, true));
        let mut cell = accessor(rows);
        v.rebuild(3, &mut cell);
        assert_eq!(v.len(), 3);
        // Source rows in display order: 2 (src 2), 9 (src 0), 10 (src 1).
        assert_eq!(v.source_row(0), 2);
        assert_eq!(v.source_row(1), 0);
        assert_eq!(v.source_row(2), 1);
    }

    #[test]
    fn descending_numeric_sort_reverses_order() {
        let rows = vec![vec!["9"], vec!["10"], vec!["2"]];
        let mut v = RowView::identity(rows.len());
        v.sort = Some((0, false));
        let mut cell = accessor(rows);
        v.rebuild(3, &mut cell);
        assert_eq!(v.source_row(0), 1); // 10
        assert_eq!(v.source_row(1), 0); // 9
        assert_eq!(v.source_row(2), 2); // 2
    }

    #[test]
    fn non_numeric_values_fall_back_to_case_insensitive_string_compare() {
        let rows = vec![vec!["Banana"], vec!["apple"], vec!["Cherry"]];
        let mut v = RowView::identity(rows.len());
        v.sort = Some((0, true));
        let mut cell = accessor(rows);
        v.rebuild(3, &mut cell);
        assert_eq!(v.source_row(0), 1); // apple
        assert_eq!(v.source_row(1), 0); // Banana
        assert_eq!(v.source_row(2), 2); // Cherry
    }

    #[test]
    fn sort_is_stable_for_equal_keys() {
        // Two rows share sort key "1"; their relative (original) order must
        // survive the sort.
        let rows = vec![
            vec!["b", "1"], // src 0
            vec!["a", "1"], // src 1
            vec!["c", "0"], // src 2
        ];
        let mut v = RowView::identity(rows.len());
        v.sort = Some((1, true));
        let mut cell = accessor(rows);
        v.rebuild(3, &mut cell);
        // "0" < "1" numerically, so src 2 first; among the two "1"s, src 0
        // (originally first) must stay before src 1.
        assert_eq!(v.source_row(0), 2);
        assert_eq!(v.source_row(1), 0);
        assert_eq!(v.source_row(2), 1);
    }

    #[test]
    fn filters_are_applied_before_sort_and_are_case_insensitive_substrings() {
        let rows = vec![
            vec!["apple", "3"],   // src 0
            vec!["melon", "1"],   // src 1 — does not contain "ap"
            vec!["apricot", "2"], // src 2
        ];
        let mut v = RowView::identity(rows.len());
        v.filters = vec![(0, "ap".to_string())]; // matches apple, apricot
        v.sort = Some((1, true));
        let mut cell = accessor(rows);
        v.rebuild(3, &mut cell);
        assert_eq!(v.len(), 2);
        assert_eq!(v.source_row(0), 2); // apricot, key "2"
        assert_eq!(v.source_row(1), 0); // apple, key "3"
    }

    #[test]
    fn empty_needle_filter_matches_everything() {
        let rows = vec![vec!["x"], vec!["y"]];
        let mut v = RowView::identity(rows.len());
        v.filters = vec![(0, String::new())];
        let mut cell = accessor(rows);
        v.rebuild(2, &mut cell);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn rebuild_reflects_growth_between_calls_in_identity_mode() {
        // Simulates streaming: rebuild called again with a larger `rows`
        // once more batches have arrived, no sort/filter active.
        let mut v = RowView::identity(2);
        let mut cell = accessor(vec![vec!["a"], vec!["b"], vec!["c"], vec!["d"]]);
        v.rebuild(4, &mut cell);
        assert_eq!(v.len(), 4);
        assert_eq!(v.source_row(3), 3);
    }

    #[test]
    fn combined_sort_and_filter_reflects_both() {
        // Filter keeps rows containing "a" in col 0, then sorts by the
        // numeric col 1 descending — exercises both RowView knobs active at
        // once, on top of the existing filter-then-sort test above.
        let rows = vec![
            vec!["banana", "2"], // src 0 — no "a"? actually has "a"
            vec!["kiwi", "5"],   // src 1 — filtered out (no "a")
            vec!["papaya", "9"], // src 2
            vec!["grape", "1"],  // src 3
        ];
        let mut v = RowView::identity(rows.len());
        v.filters = vec![(0, "a".to_string())];
        v.sort = Some((1, false)); // descending numeric
        let mut cell = accessor(rows);
        v.rebuild(4, &mut cell);
        // Survivors of the filter: banana(2), papaya(9), grape(1) — src 1
        // (kiwi) has no "a" and is dropped. Descending by col 1: papaya(9),
        // banana(2), grape(1).
        assert_eq!(v.len(), 3);
        assert_eq!(v.source_row(0), 2); // papaya
        assert_eq!(v.source_row(1), 0); // banana
        assert_eq!(v.source_row(2), 3); // grape
    }

    // --- find_matches / wrapped_index (G4 Task 3) ---

    #[test]
    fn find_matches_is_case_insensitive_and_row_major() {
        let rows = vec![
            vec!["Apple", "melon"],  // row 0
            vec!["kiwi", "APPLE"],   // row 1
            vec!["pear", "grape"],   // row 2
        ];
        let mut cell = accessor(rows);
        let matches = find_matches(3, &[0, 1], "app", &mut cell);
        assert_eq!(matches, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn find_matches_only_searches_given_columns() {
        // Column 1 ("hidden") is deliberately excluded from `cols` — a
        // match there must not appear, same as grid.rs excluding hidden
        // columns from the search.
        let rows = vec![vec!["cat", "apple"]];
        let mut cell = accessor(rows);
        let matches = find_matches(1, &[0], "apple", &mut cell);
        assert!(matches.is_empty());
    }

    #[test]
    fn find_matches_empty_needle_yields_no_matches() {
        let rows = vec![vec!["a"], vec!["b"]];
        let mut cell = accessor(rows);
        assert!(find_matches(2, &[0], "", &mut cell).is_empty());
    }

    #[test]
    fn wrapped_index_forward_wraps_past_the_end() {
        assert_eq!(wrapped_index(None, 3, true), Some(0));
        assert_eq!(wrapped_index(Some(0), 3, true), Some(1));
        assert_eq!(wrapped_index(Some(2), 3, true), Some(0));
    }

    #[test]
    fn wrapped_index_backward_wraps_past_the_start() {
        assert_eq!(wrapped_index(None, 3, false), Some(2));
        assert_eq!(wrapped_index(Some(0), 3, false), Some(2));
        assert_eq!(wrapped_index(Some(2), 3, false), Some(1));
    }

    #[test]
    fn wrapped_index_empty_list_is_always_none() {
        assert_eq!(wrapped_index(None, 0, true), None);
        assert_eq!(wrapped_index(Some(0), 0, false), None);
    }
}
