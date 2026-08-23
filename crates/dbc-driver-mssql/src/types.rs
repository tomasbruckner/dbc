//! Small pure helpers factored out for unit testing without a live server:
//! the row-count sentinel mapping and error conversion.

use dbc_core::QueryError;

/// Maps `odbc_api::Preallocated::row_count()`'s `Result<Option<usize>, _>`
/// to the `u64` this driver's `execute()` promises.
///
/// odbc-api's `row_count()` already folds the raw ODBC `SQLRowCount` "-1 =
/// unknown" sentinel into `None` (see its doc: "May return `None` if row
/// count is not available") — so by the time a value reaches here, `-1` can
/// no longer surface as a bogus `usize`/`u64`. This function's job is to
/// refuse to paper over that `None` with a silent `0`: an unknown row count
/// is surfaced as an error, never blindly cast, per the "errors are values"
/// contract on `Connection::execute`.
pub fn map_row_count(row_count: Option<usize>) -> Result<u64, QueryError> {
    match row_count {
        Some(n) => Ok(n as u64),
        None => Err(QueryError::msg(
            "row count not available for this statement (driver reported SQL_NO_ROW_COUNT / -1)",
        )),
    }
}

/// Maps an `odbc_api::Error` into this driver's `QueryError`. odbc-api
/// formats its `Display` output from the ODBC diagnostic record chain
/// (SQLSTATE, native error code, and vendor message all folded into the
/// text), so the full text is kept as `message`; there is no stable,
/// version-independent accessor for the bare SQLSTATE on the public `Error`
/// type to split out into `code` (see `lib.rs` module doc for why `code` is
/// `None` here rather than best-effort string-scraped).
pub fn odbc_err(e: odbc_api::Error) -> QueryError {
    QueryError { code: None, message: e.to_string(), position: None }
}

/// Standard "operation was cooperatively cancelled" error, matching the
/// `code: Some("cancelled")` convention the sqlite/postgres drivers use so
/// UI-level cancel handling doesn't need to special-case this driver.
pub fn cancelled_err() -> QueryError {
    QueryError { code: Some("cancelled".into()), message: "query cancelled".into(), position: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_row_count_maps_through() {
        assert_eq!(map_row_count(Some(0)).unwrap(), 0);
        assert_eq!(map_row_count(Some(42)).unwrap(), 42);
    }

    #[test]
    fn unknown_row_count_is_an_error_not_a_silent_zero() {
        let err = map_row_count(None).unwrap_err();
        assert!(err.message.contains("not available"));
    }

    #[test]
    fn cancelled_err_has_cancelled_code() {
        let e = cancelled_err();
        assert_eq!(e.code.as_deref(), Some("cancelled"));
    }
}
