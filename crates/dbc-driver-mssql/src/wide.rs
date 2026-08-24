//! Wide-character (`SQL_C_WCHAR` / UTF-16) result-set buffer, shared by
//! `query()` and `schema.rs`'s catalog fetches.
//!
//! # Why not narrow (`SQL_C_CHAR`) binding
//!
//! `odbc_api::buffers::TextRowSet` binds `SQL_C_CHAR` (`TextColumn<u8>`).
//! For an `nvarchar`/`nchar` column (SQL Server's native UTF-16 text
//! types), the driver has to transcode UTF-16 -> the *process's ANSI
//! codepage* -> the bytes we receive, not UTF-16 -> UTF-8. On a Czech
//! Windows install (`cp1250`), and Czech text is a primary use case for
//! this driver, every diacritic (á, č, ř, š, ž, ...) round-trips through a
//! codepage that cannot represent it losslessly. The bytes we get back
//! still happen to decode as *valid* UTF-8 (single-byte codepage output),
//! so the old code's `from_utf8`-based decode never errored — it just
//! silently produced the wrong text. Binding `SQL_C_WCHAR`
//! (`WCharColumn` / `TextColumn<u16>`) instead gets the driver's native
//! UTF-16 transcoding, which has no codepage dependency.
//!
//! # Why a hand-built buffer instead of a `WTextRowSet` convenience type
//!
//! odbc-api does not ship a wide equivalent of `TextRowSet` (no
//! `WTextRowSet` type or `ColumnarBuffer<WCharColumn>::for_cursor`
//! constructor exists in 29.0.0). [`build`] replicates
//! `TextRowSet::for_cursor`'s sizing logic by hand: per-column buffer size
//! from the driver-reported display size (`DataType::utf16_len`, falling
//! back to `col_display_size`), capped at `max_str_limit` UTF-16 code
//! units. This keeps the same "size to the actual column, don't just
//! allocate every column at the cap" behavior `TextRowSet::for_cursor` has
//! — allocating every column at a large uniform cap regardless of its real
//! width would multiply memory use by however many columns a query
//! returns.
//!
//! # Truncation is a known, explicit limitation of this approach
//!
//! See [`cell_text`]'s doc comment for why a value at exactly the
//! column's cap is reported as "possibly truncated" even on the rare
//! occasion it was not: the safe `ColumnarBuffer`/`Slice` API exposed for
//! a hand-rolled column type (anything other than the crate's own
//! `TextRowSet` u8 specialization) does not surface the raw per-cell
//! `Indicator` — only `TextColumnSlice::content_length_at`, which is
//! already capped at the column's `max_len()` and cannot distinguish "the
//! value is exactly this long" from "the value was longer and got cut
//! off here". Getting the *exact* original length back would require
//! implementing `RowSetBuffer` by hand (the crate's own escape hatch for
//! this — see its module doc — "requires unsafe code"), which is more
//! unsafe surface than this driver was asked to carry for a formatting
//! nicety. The chosen tradeoff is deliberately fail-safe in the opposite
//! direction of the bug being fixed: an exact-length false positive prints
//! an (unnecessary) truncation marker; it never prints a silently
//! shortened value as if it were complete.

use std::num::NonZeroUsize;

use dbc_core::QueryError;
use odbc_api::buffers::{ColumnarBuffer, TextColumnSlice, WCharColumn};
use odbc_api::ResultSetMetadata;

use crate::types::odbc_err;

/// A wide-character columnar row-set buffer: one [`WCharColumn`] per result
/// column, bound via [`odbc_api::Cursor::bind_buffer`].
pub type WRowSet = ColumnarBuffer<WCharColumn>;

/// Read-only view of one column's fetched rows within a [`WRowSet`] batch,
/// as returned by `WRowSet::column`.
pub type WSlice<'a> = TextColumnSlice<'a, u16>;

/// Builds a [`WRowSet`] sized to `cursor`'s columns (one [`WCharColumn`]
/// per column, 1-indexed to match ODBC column numbering), each capped at
/// `max_str_limit` UTF-16 code units. Returns the buffer along with the
/// column count, since both are needed by every caller and computing the
/// count is otherwise a second `num_result_cols` round trip.
pub fn build(
    cursor: &mut impl ResultSetMetadata,
    batch_size: usize,
    max_str_limit: usize,
) -> Result<(WRowSet, usize), QueryError> {
    let num_cols: u16 = cursor.num_result_cols().map_err(odbc_err)?.try_into().map_err(|_| {
        QueryError::msg("driver reported a negative column count")
    })?;

    let mut columns: Vec<(u16, WCharColumn)> = Vec::with_capacity(num_cols as usize);
    for col_number in 1..=num_cols {
        // Mirrors `odbc_api::result_set_metadata::utf8_display_sizes`
        // (used internally by `TextRowSet::for_cursor`), swapped to the
        // UTF-16 sizing (`DataType::utf16_len`, 2 code units per char
        // worst case) since we bind wide, not narrow.
        let reported: Option<NonZeroUsize> =
            match cursor.col_data_type(col_number).map_err(odbc_err)?.utf16_len() {
                Some(len) => Some(len),
                None => cursor.col_display_size(col_number).map_err(odbc_err)?,
            };
        // G15 T8 live-verified fix (HARD GATE ITEM 4 — schema.rs view-kind
        // misclassification, found via `mssql_integration.rs::schema_snapshot_smoke`
        // failing live): when the buffer is sized from the DRIVER-REPORTED
        // display size (the `Some(_)` arm — not the `max_str_limit` fallback
        // for a size-less type), add a ONE-code-unit safety margin before
        // capping. Without it, a column whose reported size EXACTLY equals
        // its real maximum content width (the common case for fixed-shape
        // types, not an edge case) triggers `cell_text`'s "len >= max_len"
        // truncation heuristic on every single value, 100% of the time —
        // confirmed live for `DataType::Bit` (`display_size() == 1`, and a
        // bit value is always exactly the 1-character text "0"/"1"): every
        // `bit` column read through this driver's catalog queries
        // (`sys.tables`/`sys.views`'s synthesized `is_view`, `sys.columns
        // .is_nullable`, ...) was silently replaced by the truncation
        // marker string, which `cell_bool`'s `== "1"` check then always
        // reads as `false` — `schema.rs`'s view/table classification (and
        // every other live bit-flag read) was wrong for EVERY row, not a
        // rare boundary case. The margin costs one extra `u16` per
        // buffered row for reported-size columns (negligible) and cannot
        // cause a genuine over-length value to be missed: content beyond
        // `reported + 1` still exceeds the buffer and is still correctly
        // flagged truncated (see `cell_text`'s doc comment — this module's
        // "over-flag rather than silently truncate" posture is preserved,
        // just no longer OFF BY ONE for the exact-width case). The
        // `max_str_limit` fallback path (`None` arm, used when the driver
        // reports no size at all, e.g. `nvarchar(max)`) is UNCHANGED — that
        // cap is a deliberate hard ceiling, not a display-size estimate,
        // and `query_reports_truncation_marker_for_oversized_nvarchar_max`
        // already proves genuine over-cap truncation still triggers there.
        let max_str_len = match reported {
            Some(len) => (len.get() + 1).min(max_str_limit),
            None => max_str_limit,
        };
        columns.push((col_number, WCharColumn::new(batch_size, max_str_len)));
    }

    Ok((ColumnarBuffer::new(columns), num_cols as usize))
}

/// Decodes one cell: `None` for SQL `NULL`; otherwise `Some` of either the
/// decoded text or one of this driver's `<...>` placeholder markers
/// (truncation / invalid UTF-16), consistent with the sqlite driver's
/// `<blob N B>` placeholder convention.
///
/// Truncation detection: see the module doc's "Truncation is a known,
/// explicit limitation" section. In short, a cell whose reported length
/// equals the column's cap is treated as truncated even though, rarely,
/// it may just happen to be exactly that long — the safe API available
/// here cannot tell the two apart, and this driver would rather over-flag
/// than silently show a partial value.
pub fn cell_text(slice: WSlice<'_>, row_index: usize) -> Option<String> {
    let max_len = slice.max_len();
    match slice.content_length_at(row_index) {
        None => None, // SQL NULL
        Some(len) if len >= max_len => Some(truncated_marker(max_len)),
        Some(_) => match slice.get(row_index) {
            None => None, // defensive: content_length_at said non-null, so this shouldn't happen
            Some(units) => match String::from_utf16(units) {
                Ok(s) => Some(s),
                Err(_) => Some("<decode error: invalid utf-16>".to_string()),
            },
        },
    }
}

/// Explicit "this value may have been cut off" marker — mirrors the
/// sqlite driver's `<blob N B>` placeholder convention. `max_len_chars` is
/// the column's UTF-16 buffer cap, the only bound on the true length this
/// driver can report (see module doc).
fn truncated_marker(max_len_chars: usize) -> String {
    format!("<zkráceno: >= {max_len_chars} znaků>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_marker_reports_the_cap() {
        assert_eq!(truncated_marker(4096), "<zkráceno: >= 4096 znaků>");
    }
}
