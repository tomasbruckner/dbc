//! Recording a CLI run in the app's query history.
//!
//! The same `history.sqlite` the GUI writes, with `kind = "cli"` — which
//! is what puts these runs in their own section in the app's history
//! panel instead of mixed among clicks. Machine-local in both profile and
//! workspace mode (design §W5), so a history row never travels with a
//! shared workspace folder.
//!
//! **What is stored, and what is not.** SQL text, the connection's NAME,
//! when it started, how long it took, how many rows, and the error text if
//! it failed. Never a result value, never a host, never a credential — the
//! same contract the GUI recorder keeps, and the reason a history file is
//! not a place secrets can leak to.
//!
//! **Never fatal.** Every failure here is swallowed. By the time a run is
//! recorded it has already happened; refusing to print its result because
//! a convenience log could not be opened would punish the user for the
//! wrong thing.

use std::path::Path;

use dbc_state::{HistoryDb, KIND_CLI};

pub struct Recorder {
    db: Option<HistoryDb>,
}

impl Recorder {
    /// A history file that cannot be opened yields a recorder that records
    /// nothing — deliberately indistinguishable, from the caller's side,
    /// from one that works.
    pub fn open(path: &Path) -> Recorder {
        Recorder { db: HistoryDb::open(path).ok() }
    }

    /// Seconds since the epoch, the unit `HistoryDb` stores. A clock
    /// before 1970 yields 0 rather than failing the run.
    pub fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    pub fn record(
        &mut self,
        sql: &str,
        connection: &str,
        started_at: i64,
        duration_ms: Option<i64>,
        row_count: Option<i64>,
        error: Option<&str>,
    ) {
        if let Some(db) = self.db.as_mut() {
            let _ = db.add_with_kind(
                sql,
                connection,
                started_at,
                duration_ms,
                row_count,
                error,
                KIND_CLI,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip that matters: what `dbc` writes is what the panel
    /// groups on. A literal typed twice would pass a test like this on one
    /// side and fail silently on the other, which is why `KIND_CLI` is a
    /// shared constant rather than a string in two crates.
    #[test]
    fn a_recorded_run_lands_under_the_cli_kind_with_no_secret_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let mut rec = Recorder::open(&path);
        rec.record("select 1", "produkce/sklad", 1_700_000_000, Some(12), Some(3), None);

        let db = HistoryDb::open(&path).unwrap();
        let found = db.search("", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, KIND_CLI);
        assert_eq!(found[0].sql, "select 1");
        assert_eq!(found[0].connection, "produkce/sklad");
        assert_eq!(found[0].row_count, Some(3));
        assert_eq!(found[0].duration_ms, Some(12));
        assert!(found[0].error.is_none());
    }

    #[test]
    fn a_failed_run_records_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let mut rec = Recorder::open(&path);
        rec.record("select nope", "produkce", 1_700_000_000, None, None, Some("no such column"));

        let db = HistoryDb::open(&path).unwrap();
        let found = db.search("", 10).unwrap();
        assert_eq!(found[0].error.as_deref(), Some("no such column"));
        assert_eq!(found[0].kind, KIND_CLI);
    }

    /// The whole point of swallowing failures: an unopenable history must
    /// not be able to take a command down with it.
    #[test]
    fn an_unopenable_history_records_nothing_and_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        // A DIRECTORY where the file should be — open must fail.
        let path = dir.path().join("blocked");
        std::fs::create_dir(&path).unwrap();
        let mut rec = Recorder::open(&path);
        rec.record("select 1", "c", 0, None, None, None);
    }

    #[test]
    fn the_timestamp_is_a_plausible_epoch_second() {
        // Any clock this side of 2020 — the assertion is that it is
        // SECONDS, not millis, which is what `HistoryDb` stores.
        let now = Recorder::now_secs();
        assert!(now > 1_577_836_800, "{now} is not seconds since the epoch");
        assert!(now < 100_000_000_000, "{now} looks like milliseconds");
    }
}
