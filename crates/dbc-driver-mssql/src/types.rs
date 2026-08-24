//! Small pure helpers factored out for unit testing without a live server:
//! the row-count sentinel mapping and error conversion.

use dbc_core::QueryError;

/// Maps `odbc_api::Preallocated::row_count()`'s `Result<Option<usize>, _>`
/// to the `u64` this driver's `execute()` promises.
///
/// odbc-api's `row_count()` already folds the raw ODBC `SQLRowCount` "-1 =
/// unknown" sentinel into `None` (see its doc: "May return `None` if row
/// count is not available") — so by the time a value reaches here, `-1` can
/// no longer surface as a bogus `usize`/`u64`.
///
/// `None` maps to `Ok(0)` — the "0 affected" convention the pg/sqlite
/// drivers already use for statements that don't count rows — rather than
/// an error. **G15 §3c Appendix F2 grounding find:** every T-SQL
/// transaction-control batch this driver's write path sends (`SET
/// XACT_ABORT ON; BEGIN TRANSACTION`, bare `COMMIT`/`ROLLBACK`, plain `SET
/// ...` statements) reports `SQL_NO_ROW_COUNT` — they are not DML, so there
/// is nothing to count. Mapping that to an error (this function's behavior
/// before G15) would fail every sanctioned write sequence's very first
/// statement on MSSQL (`tests/mssql_tx_matrix.rs` case 0 characterizes this
/// live). Genuine DML (`INSERT`/`UPDATE`/`DELETE`) always reports a real
/// count on SQL Server, so `drive_write_sequence`'s affected-row-mismatch
/// check keeps its meaning — this relaxation only affects statements that
/// were never counting rows to begin with.
pub fn map_row_count(row_count: Option<usize>) -> Result<u64, QueryError> {
    match row_count {
        Some(n) => Ok(n as u64),
        None => Ok(0),
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
    fn unknown_row_count_maps_to_zero_not_an_error() {
        // G15 §3c Appendix F2: SET/BEGIN/COMMIT/ROLLBACK batches report
        // SQL_NO_ROW_COUNT — this is the "0 affected" convention, not a
        // failure (see the doc comment on `map_row_count`).
        assert_eq!(map_row_count(None).unwrap(), 0);
    }

    #[test]
    fn cancelled_err_has_cancelled_code() {
        let e = cancelled_err();
        assert_eq!(e.code.as_deref(), Some("cancelled"));
    }
}
