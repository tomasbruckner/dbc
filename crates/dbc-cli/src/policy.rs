//! What this invocation is allowed to run — pure.
//!
//! The rule this codebase applies to every write path is that nothing
//! writes without an explicit act of consent naming what will run. In the
//! GUI that act is a confirm dialog showing the exact SQL. A CLI has no
//! dialog, so the act is `--write`, typed by a person, for one invocation.
//!
//! Two properties matter more than the flag itself:
//!
//! * **The connection's `read_only` flag is not overridable.** `--write` is
//!   consent for THIS run; `read_only` is a standing decision about the
//!   connection, made in the app, and a command-line flag must not be able
//!   to undo it.
//! * **A batch is decided before any of it runs.** A `.sql` file is
//!   checked statement by statement first, and refused as a whole — the
//!   alternative is a file that half-applies and leaves the database in a
//!   state nobody described.

use dbc_core::format::statement_kind;
use dbc_core::{is_read_statement_d, split_sql, Dialect};

/// One statement, with the decision already made about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub sql: String,
    /// `false` means it goes through the driver's write path.
    pub is_read: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The text could not be split into statements at all (an unterminated
    /// string or comment). Nothing is run: the guard is fail-closed, and
    /// text this layer cannot read is text it cannot vouch for.
    Unparsable(String),
    /// No statements in it.
    Empty,
    /// The connection is marked read-only in the app.
    ConnectionIsReadOnly { kind: &'static str },
    /// A write, without `--write`.
    NeedsWriteFlag { kind: &'static str },
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Unparsable(why) => {
                format!("SQL se nepodařilo rozdělit na příkazy ({why}) — nic se nespustilo")
            }
            Refusal::Empty => "žádný SQL příkaz ke spuštění".to_string(),
            Refusal::ConnectionIsReadOnly { kind } => format!(
                "připojení je označené jen pro čtení, takže {kind} neprojde — \
                 ani s --write; ten příznak se mění v aplikaci"
            ),
            Refusal::NeedsWriteFlag { kind } => format!(
                "{kind} zapisuje — spusť to znovu s --write, pokud to tak opravdu chceš; \
                 nic se zatím nespustilo"
            ),
        }
    }
}

/// Decide the whole batch up front.
///
/// Every statement is classified, and the FIRST one that may not run
/// refuses the entire batch. The order matters for the message a user
/// gets — being told about statement 7 while statements 1..6 already ran
/// would be a report about a database that has already changed.
pub fn plan(
    sql: &str,
    dialect: Dialect,
    connection_is_read_only: bool,
    write_flag: bool,
) -> Result<Vec<Stmt>, Refusal> {
    let statements =
        split_sql(sql, dialect).map_err(|e| Refusal::Unparsable(format!("{e:?}")))?;
    let statements: Vec<String> =
        statements.into_iter().filter(|s| !s.trim().is_empty()).collect();
    if statements.is_empty() {
        return Err(Refusal::Empty);
    }

    let mut out = Vec::with_capacity(statements.len());
    for s in statements {
        let is_read = is_read_statement_d(&s, dialect);
        if !is_read {
            let kind = statement_kind(&s);
            if connection_is_read_only {
                return Err(Refusal::ConnectionIsReadOnly { kind });
            }
            if !write_flag {
                return Err(Refusal::NeedsWriteFlag { kind });
            }
        }
        out.push(Stmt { sql: s, is_read });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Dialect = Dialect::Postgres;

    #[test]
    fn a_plain_select_needs_no_flag() {
        let plan = plan("select 1", PG, false, false).unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].is_read);
    }

    #[test]
    fn a_write_without_the_flag_is_refused_and_names_the_flag() {
        let e = plan("delete from t", PG, false, false).unwrap_err();
        assert!(matches!(e, Refusal::NeedsWriteFlag { .. }));
        assert!(e.message().contains("--write"), "{}", e.message());
        assert!(e.message().contains("nic se zatím nespustilo"), "{}", e.message());
    }

    #[test]
    fn a_write_with_the_flag_is_allowed() {
        let plan = plan("delete from t", PG, false, true).unwrap();
        assert!(!plan[0].is_read);
    }

    /// The whole point of the standing flag: consent for one run must not
    /// be able to override a decision made about the connection.
    #[test]
    fn write_flag_cannot_override_a_read_only_connection() {
        for flag in [false, true] {
            let e = plan("delete from t", PG, true, flag).unwrap_err();
            assert!(matches!(e, Refusal::ConnectionIsReadOnly { .. }), "flag={flag}");
            assert!(e.message().contains("ani s --write"), "{}", e.message());
        }
    }

    /// A read is still a read on a read-only connection — the flag bars
    /// writes, it does not bar the connection.
    #[test]
    fn a_read_only_connection_still_reads() {
        assert!(plan("select 1", PG, true, false).unwrap()[0].is_read);
    }

    /// The property that makes a `.sql` file safe to hand over: the batch
    /// is refused BEFORE the harmless statements in front of the write
    /// have run.
    #[test]
    fn one_write_late_in_a_batch_refuses_the_whole_batch() {
        let e = plan("select 1; select 2; drop table t", PG, false, false).unwrap_err();
        assert!(matches!(e, Refusal::NeedsWriteFlag { .. }));
    }

    #[test]
    fn a_mixed_batch_with_the_flag_keeps_each_statements_own_verdict() {
        let plan = plan("select 1; delete from t", PG, false, true).unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan[0].is_read);
        assert!(!plan[1].is_read);
    }

    #[test]
    fn blank_input_and_a_lone_semicolon_are_refused_as_empty() {
        assert_eq!(plan("", PG, false, false).unwrap_err(), Refusal::Empty);
        assert_eq!(plan("   \n  ", PG, false, false).unwrap_err(), Refusal::Empty);
        assert_eq!(plan(";", PG, false, false).unwrap_err(), Refusal::Empty);
    }

    /// Fail closed: text the splitter cannot read is text nothing may run.
    #[test]
    fn unsplittable_text_runs_nothing() {
        let e = plan("select 'unterminated", PG, false, true).unwrap_err();
        assert!(matches!(e, Refusal::Unparsable(_)), "{e:?}");
        assert!(e.message().contains("nic se nespustilo"), "{}", e.message());
    }

    /// Comments must not smuggle a write past the classifier, and the
    /// bracket-quoted T-SQL identifier must not be mistaken for one.
    #[test]
    fn the_dialect_is_threaded_through_to_the_classifier() {
        // `[Delete]` is a column name in T-SQL, not the DELETE keyword.
        let plan = plan("SELECT [Delete] FROM AuditLog", Dialect::Mssql, false, false);
        assert!(plan.is_ok(), "{plan:?}");
    }
}
