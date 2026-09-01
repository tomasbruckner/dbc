//! „Which server am I actually talking to?" — the version, shown on the
//! connection row in the sidebar (user request, 2026-09-01: „u connection
//! bych nekde chtel videt verzi toho serveru (napr. pg 18)").
//!
//! Everything decidable without a server lives here: the SQL each engine
//! answers with, and the rule for turning what comes back into something
//! short enough to sit inside `prodej (pg 18)`. `runner::fetch_server_version`
//! is the thin part that opens a connection and reads one cell.
//!
//! ## Why the display rule is per-engine rather than „the major number"
//!
//! „pg 18" is the right shape for PostgreSQL and for SQL Server, where the
//! major number IS how people name the release and the rest is a patch
//! level nobody quotes. It is useless for SQLite and DuckDB, where the
//! major number has been `3` since 2004 and `1` respectively — „sqlite 3"
//! says nothing that „sqlite" did not. So server engines get one
//! component and file engines get two. See [`version_components`].

use dbc_state::Engine;

/// The statement that asks each engine for its own version.
///
/// All four are reads and none needs a privilege the connection does not
/// already have to be useful at all — a connection that cannot run these
/// cannot list its databases either, and that is the step this rides along
/// with.
///
/// `SHOW server_version` rather than `version()` on PostgreSQL: `version()`
/// returns a sentence („PostgreSQL 18.1 on x86_64-pc-linux-gnu, compiled
/// by gcc…") and `SHOW` returns the number, so the parsing below has less
/// to be wrong about. `SERVERPROPERTY('ProductVersion')` on SQL Server for
/// the same reason — `@@VERSION` is four lines of prose.
pub(crate) fn version_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "SHOW server_version",
        Engine::Mssql => "SELECT SERVERPROPERTY('ProductVersion')",
        Engine::Sqlite => "SELECT sqlite_version()",
        Engine::Duckdb => "SELECT version()",
    }
}

/// How many dot-separated components of the version to show.
///
/// One for the engines whose major number is the release name (PostgreSQL
/// 18, SQL Server 16); two for the engines where it never changes and the
/// minor is the informative part (SQLite 3.45, DuckDB 1.1).
fn version_components(engine: Engine) -> usize {
    match engine {
        Engine::Postgres | Engine::Mssql => 1,
        Engine::Sqlite | Engine::Duckdb => 2,
    }
}

/// Turn whatever the server said into the short form shown on the row.
///
/// Deliberately forgiving about the input and strict about the output: it
/// takes the first run of digits-and-dots it can find, ignoring a leading
/// `v` (DuckDB answers „v1.1.3") and any prose around it (a PostgreSQL
/// build answering „18.1 (Debian 18.1-1)" still yields `18`). Anything it
/// cannot read that way is `None` rather than a guess — a wrong version
/// number on a connection row is worse than no version number, because it
/// is the kind of thing someone quotes in a bug report.
pub(crate) fn short_version(engine: Engine, raw: &str) -> Option<String> {
    let mut chars = raw.trim().chars().peekable();
    // Skip a leading `v`/`V`, then anything up to the first digit.
    let mut digits = String::new();
    let mut started = false;
    for c in chars.by_ref() {
        if c.is_ascii_digit() {
            started = true;
            digits.push(c);
        } else if started {
            if c == '.' && !digits.ends_with('.') {
                digits.push(c);
            } else {
                break;
            }
        }
    }
    let digits = digits.trim_end_matches('.');
    if digits.is_empty() {
        return None;
    }
    let short: Vec<&str> =
        digits.split('.').take(version_components(engine)).collect();
    // `split` on a non-empty string always yields at least one piece, and
    // that piece is digits by construction — but an empty result would
    // render as `prodej (pg )`, so it is refused rather than trusted.
    if short.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(short.join("."))
}

/// The engine segment of a connection row: `pg` alone, or `pg 18` once the
/// server has said which it is.
///
/// One function so the sidebar and any future caller cannot drift apart on
/// the spacing, and so „no version yet" renders as exactly what the row
/// looked like before this feature existed.
pub(crate) fn engine_segment(engine: Engine, version: Option<&str>) -> String {
    let label = crate::connections_ui::engine_label(engine);
    match version {
        Some(v) if !v.is_empty() => format!("{label} {v}"),
        _ => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every engine must be askable, and no two engines may share a
    /// statement — a copy-paste that pointed SQL Server at
    /// `sqlite_version()` would fail at runtime on a live server only.
    #[test]
    fn every_engine_has_its_own_version_statement() {
        let all = [Engine::Postgres, Engine::Mssql, Engine::Sqlite, Engine::Duckdb];
        for e in all {
            assert!(!version_sql(e).is_empty(), "{e:?}");
        }
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(version_sql(*a), version_sql(*b), "{a:?} and {b:?} share SQL");
            }
        }
    }

    /// The shapes these servers really answer with, including the two that
    /// motivated the parser: DuckDB's leading `v`, and a PostgreSQL build
    /// that appends its packaging.
    #[test]
    fn real_server_answers_become_the_short_form() {
        for (engine, raw, want) in [
            (Engine::Postgres, "18.1", "18"),
            (Engine::Postgres, "16.2 (Debian 16.2-1.pgdg120+2)", "16"),
            (Engine::Postgres, "18beta1", "18"),
            (Engine::Mssql, "16.0.4125.3", "16"),
            (Engine::Mssql, "15.0.4335.1", "15"),
            (Engine::Sqlite, "3.45.1", "3.45"),
            (Engine::Duckdb, "v1.1.3", "1.1"),
            (Engine::Duckdb, "1.0.0", "1.0"),
        ] {
            assert_eq!(
                short_version(engine, raw).as_deref(),
                Some(want),
                "{engine:?} {raw:?}"
            );
        }
    }

    /// Nothing readable ⇒ no version, never a guess: the row simply looks
    /// the way it did before this feature.
    #[test]
    fn unreadable_answers_yield_nothing_rather_than_a_wrong_number() {
        for raw in ["", "   ", "unknown", "NULL", "-", "v", "..."] {
            assert_eq!(short_version(Engine::Postgres, raw), None, "{raw:?}");
        }
    }

    /// Whitespace and surrounding prose must not change the answer — this
    /// is a cell read straight out of a result set.
    #[test]
    fn surrounding_noise_is_ignored() {
        assert_eq!(short_version(Engine::Postgres, "  18.1  ").as_deref(), Some("18"));
        assert_eq!(
            short_version(Engine::Postgres, "PostgreSQL 17.2 on x86_64").as_deref(),
            Some("17")
        );
    }

    /// The distinction the whole per-engine rule exists for.
    #[test]
    fn file_engines_keep_the_minor_because_their_major_never_moves() {
        assert_eq!(short_version(Engine::Sqlite, "3.45.1").as_deref(), Some("3.45"));
        assert_eq!(short_version(Engine::Postgres, "18.1.0").as_deref(), Some("18"));
        assert_eq!(version_components(Engine::Sqlite), 2);
        assert_eq!(version_components(Engine::Postgres), 1);
    }

    /// A version with fewer components than we ask for is fine — SQLite
    /// answering „3" gives „3", not „3." and not `None`.
    #[test]
    fn a_short_answer_is_used_as_is() {
        assert_eq!(short_version(Engine::Sqlite, "3").as_deref(), Some("3"));
        assert_eq!(short_version(Engine::Duckdb, "v2").as_deref(), Some("2"));
    }

    /// Before the server has answered, the row must read exactly as it did
    /// before this feature existed.
    #[test]
    fn the_row_is_unchanged_until_a_version_is_known() {
        assert_eq!(engine_segment(Engine::Postgres, None), "pg");
        assert_eq!(engine_segment(Engine::Postgres, Some("")), "pg");
        assert_eq!(engine_segment(Engine::Postgres, Some("18")), "pg 18");
        assert_eq!(engine_segment(Engine::Mssql, Some("16")), "mssql 16");
        assert_eq!(engine_segment(Engine::Sqlite, Some("3.45")), "sqlite 3.45");
    }
}
