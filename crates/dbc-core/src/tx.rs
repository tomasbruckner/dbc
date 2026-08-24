//! G15 §3a: dialect-correct transaction-control text for every sanctioned
//! write sequence (fixes G12's bare-`BEGIN` bug -- `BEGIN` alone is invalid
//! T-SQL). Postgres/Sqlite strings are byte-identical to the literals the
//! runner used before G15: zero behavior change for those engines.

use crate::split::Dialect;

/// Mssql: `SET XACT_ABORT ON` is FUSED to `BEGIN TRANSACTION` in one batch
/// (it has no only-statement restriction) so no sequence anywhere can open
/// an MSSQL transaction without it. §3b: under `XACT_ABORT OFF`, T-SQL's
/// per-error-class batch-vs-statement abort behavior makes "stop at first
/// error, roll back everything" untestable; `ON` collapses it to the
/// pg-like contract (any runtime error dooms and rolls back the whole
/// transaction). The subsequent explicit ROLLBACK then failing with "no
/// corresponding BEGIN TRANSACTION" is swallowed by the sequences'
/// existing `let _ =` discard posture. Verified empirically by the §3c
/// matrix (dbc-driver-mssql/tests/mssql_tx_matrix.rs) before any
/// feature-ON flip merges.
pub fn tx_begin_sql(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => "BEGIN",
        Dialect::Mssql => "SET XACT_ABORT ON; BEGIN TRANSACTION",
    }
}

/// `COMMIT` is valid T-SQL as-is; the dialect parameter is kept so every
/// call site reads uniformly and a future divergence has a seam.
pub fn tx_commit_sql(_dialect: Dialect) -> &'static str {
    "COMMIT"
}

pub fn tx_rollback_sql(_dialect: Dialect) -> &'static str {
    "ROLLBACK"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_helpers_pg_sqlite_are_the_historic_literals() {
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(tx_begin_sql(d), "BEGIN");
            assert_eq!(tx_commit_sql(d), "COMMIT");
            assert_eq!(tx_rollback_sql(d), "ROLLBACK");
        }
    }

    #[test]
    fn tx_begin_mssql_is_fused_xact_abort() {
        assert_eq!(tx_begin_sql(Dialect::Mssql), "SET XACT_ABORT ON; BEGIN TRANSACTION");
        assert_eq!(tx_commit_sql(Dialect::Mssql), "COMMIT");
        assert_eq!(tx_rollback_sql(Dialect::Mssql), "ROLLBACK");
    }
}
