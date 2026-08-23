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

/// Maps an `odbc_api::Error` into this driver's `QueryError`. The full
/// `Display` text (which folds in SQLSTATE, native error code, and vendor
/// message) is kept as `message`; `code` is populated with the bare
/// SQLSTATE when the error carries a diagnostic record
/// (`Error::Diagnostics { record, .. }` — `record.state.as_str()`, per
/// `odbc_api::handles::diagnostics::Record`/`State`), mirroring
/// `dbc-driver-postgres`'s `pg_err`. `ODBC` standard SQLSTATE `HY008`
/// ("Operation canceled") is normalized to this driver's `"cancelled"`
/// sentinel, matching `pg_err`'s `57014` special case, so UI-level cancel
/// handling doesn't need to special-case this driver even when a
/// server/driver-side cancel (rather than this driver's own cooperative
/// check) is what produced the error. Variants without a diagnostic record
/// (`NoDiagnostics`, `FailedAllocatingEnvironment`, ...) get `code: None`.
pub fn odbc_err(e: odbc_api::Error) -> QueryError {
    let code = match &e {
        odbc_api::Error::Diagnostics { record, .. } => {
            let state = record.state.as_str();
            Some(if state == "HY008" { "cancelled".to_string() } else { state.to_string() })
        }
        _ => None,
    };
    QueryError { code, message: e.to_string(), position: None }
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
    use odbc_api::handles::{Record, State};

    fn diagnostics_error(state: &[u8; 5]) -> odbc_api::Error {
        odbc_api::Error::Diagnostics {
            record: Record { state: State(*state), native_error: 207, message: Vec::new() },
            function: "SQLExecDirect",
        }
    }

    #[test]
    fn odbc_err_extracts_sqlstate_as_code() {
        let mapped = odbc_err(diagnostics_error(b"42S22"));
        assert_eq!(mapped.code.as_deref(), Some("42S22"));
    }

    #[test]
    fn odbc_err_normalizes_hy008_to_cancelled() {
        let mapped = odbc_err(diagnostics_error(b"HY008"));
        assert_eq!(mapped.code.as_deref(), Some("cancelled"));
    }

    #[test]
    fn odbc_err_no_code_without_a_diagnostic_record() {
        let mapped = odbc_err(odbc_api::Error::NoDiagnostics { function: "SQLExecDirect" });
        assert_eq!(mapped.code, None);
    }

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
