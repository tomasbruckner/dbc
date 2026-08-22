/// SQL execution guards: auto-LIMIT and read-only statement detection.
///
/// Fail-closed is the guiding principle throughout this module: whenever the
/// scanner cannot be sure a statement is safe (unterminated comment/string,
/// ambiguous PRAGMA form, an unrecognized leading keyword, a write keyword
/// appearing anywhere in the text, ...), `is_read_statement` returns `false`
/// and `apply_auto_limit` leaves the SQL untouched.

/// One significant lexical item, produced by [`tokenize`]. Everything inside
/// string literals, quoted identifiers, and comments is discarded; only bare
/// words, `;`, and `=` survive, which is exactly what the guards below need.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// An alphanumeric/underscore token, upper-cased.
    Word(String),
    /// A top-level `;` statement separator.
    Semi,
    /// A top-level `=` character (used for the PRAGMA setter check).
    Eq,
}

/// Write-statement keywords rejected anywhere they appear as a bare token
/// (outside strings/quoted identifiers/comments), regardless of leading
/// keyword. This closes both the data-modifying-CTE bypass
/// (`WITH x AS (UPDATE ...) SELECT * FROM x`) and the `EXPLAIN ANALYZE
/// UPDATE ...` bypass (Postgres's `EXPLAIN ANALYZE` actually executes the
/// statement).
///
/// This is deliberately conservative: a `SELECT` over an unquoted column or
/// alias literally named e.g. `update` would also be rejected. That's an
/// acceptable false positive for a read-only guard -- fail closed.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "DROP", "ALTER", "CREATE", "TRUNCATE", "GRANT",
    "REVOKE", "COPY", "CALL", "DO", "VACUUM", "REINDEX", "ATTACH", "DETACH", "REPLACE", "UPSERT",
    "EXEC", "EXECUTE",
];

/// Leading keywords that may start a read-only statement.
const READ_LEADING_KEYWORDS: &[&str] = &["SELECT", "WITH", "EXPLAIN", "SHOW", "VALUES", "PRAGMA"];

/// SQLite pragmas that are pure getters. `PRAGMA name = value` and
/// `PRAGMA name(value)` are *both* setter syntaxes ("yield identical
/// results" per the SQLite docs), and once the tokenizer drops punctuation
/// `PRAGMA name(arg)` is indistinguishable from `PRAGMA schema.name` -- so
/// instead of guessing getter vs. setter from shape, only pragmas on this
/// allowlist count as reads. Schema-qualified pragmas are rejected
/// (fail closed; rare in interactive use).
const READ_PRAGMAS: &[&str] = &[
    "TABLE_INFO",
    "TABLE_XINFO",
    "INDEX_LIST",
    "INDEX_INFO",
    "INDEX_XINFO",
    "FOREIGN_KEY_LIST",
    "DATABASE_LIST",
    "COLLATION_LIST",
    "FUNCTION_LIST",
    "MODULE_LIST",
    "PRAGMA_LIST",
    "COMPILE_OPTIONS",
    "INTEGRITY_CHECK",
    "QUICK_CHECK",
    "FREELIST_COUNT",
    "PAGE_COUNT",
];

/// Tokenizes `sql` into a flat sequence of [`Item`]s, tracking:
/// - single-quoted string literals (`''` is an escaped quote),
/// - double-quoted identifiers (`""` is an escaped quote),
/// - `--` line comments (run to end of line),
/// - `/* ... */` block comments, which **nest** (matches PostgreSQL's actual
///   nesting semantics -- tracked via a depth counter, not a bool, which is
///   the root cause fix for the `/* /* */ SELECT 1 */ UPDATE ...` bypass).
///
/// Returns `None` if the input ends inside any of the above open constructs
/// (unterminated string/quoted-ident/comment). Callers must treat `None` as
/// "cannot determine safety" and fail closed.
///
/// Known limitation (non-blocking, see Task 6 security review Issue 5):
/// PostgreSQL dollar-quoted strings (`$$...$$`) are not recognized. This can
/// only cause `apply_auto_limit` to *skip* appending a LIMIT (a limiting
/// keyword inside a `$$...$$` body false-positives as top-level) or
/// `is_read_statement` to *reject* a statement it could have allowed (a
/// write keyword inside a `$$...$$` body false-positives as a bare token) --
/// both fail in the safe direction.
fn tokenize(sql: &str) -> Option<Vec<Item>> {
    let mut items = Vec::new();
    let mut chars = sql.chars().peekable();
    let mut in_single_string = false;
    let mut in_double_ident = false;
    let mut in_line_comment = false;
    let mut block_comment_depth: u32 = 0;

    while let Some(c) = chars.next() {
        if in_single_string {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_single_string = false;
                }
            }
            continue;
        }

        if in_double_ident {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                } else {
                    in_double_ident = false;
                }
            }
            continue;
        }

        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        if block_comment_depth > 0 {
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                block_comment_depth += 1;
            } else if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                block_comment_depth -= 1;
            }
            continue;
        }

        if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            in_line_comment = true;
            continue;
        }

        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            block_comment_depth = 1;
            continue;
        }

        if c == '\'' {
            in_single_string = true;
            continue;
        }

        if c == '"' {
            in_double_ident = true;
            continue;
        }

        if c == ';' {
            items.push(Item::Semi);
            continue;
        }

        if c == '=' {
            items.push(Item::Eq);
            continue;
        }

        if c.is_alphanumeric() || c == '_' {
            let mut token = String::new();
            token.push(c);
            while let Some(&ch) = chars.peek() {
                if ch.is_alphanumeric() || ch == '_' {
                    token.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            items.push(Item::Word(token.to_uppercase()));
            continue;
        }

        // Other punctuation (parens, commas, operators, ...) is not
        // significant to any guard here and is dropped.
    }

    if in_single_string || in_double_ident || in_line_comment || block_comment_depth > 0 {
        None
    } else {
        Some(items)
    }
}

/// The very first item, if it's a bare word (matches the old
/// "first significant token" behavior: statements that don't start with a
/// recognizable word -- e.g. start with `;` or `=` -- have no first token).
fn first_word(items: &[Item]) -> Option<&str> {
    match items.first() {
        Some(Item::Word(w)) => Some(w.as_str()),
        _ => None,
    }
}

/// Splits a flat token stream into top-level statements on [`Item::Semi`].
/// Empty segments (consecutive `;`, or a trailing `;` with nothing but
/// whitespace/comments after it) are dropped -- they contribute nothing to
/// execute and aren't statements to validate.
fn split_statements(items: &[Item]) -> Vec<&[Item]> {
    let mut statements = Vec::new();
    let mut start = 0;
    for (i, item) in items.iter().enumerate() {
        if matches!(item, Item::Semi) {
            if start < i {
                statements.push(&items[start..i]);
            }
            start = i + 1;
        }
    }
    if start < items.len() {
        statements.push(&items[start..]);
    }
    statements
}

/// Checks if a single top-level statement (already split out of any `;`
/// batch) is read-only: its leading keyword is on the read allowlist, no
/// write keyword appears anywhere in it, and -- for `PRAGMA` -- it contains
/// no `=` (setter form).
fn is_single_statement_read(stmt: &[Item]) -> bool {
    let first = match first_word(stmt) {
        Some(w) => w,
        None => return false,
    };

    if !READ_LEADING_KEYWORDS.contains(&first) {
        return false;
    }

    // Write-keyword blacklist scan: every token in the statement, not just
    // the leading one. Closes the data-modifying-CTE and `EXPLAIN ANALYZE
    // UPDATE` bypasses (see WRITE_KEYWORDS doc comment).
    for item in stmt {
        if let Item::Word(w) = item {
            if WRITE_KEYWORDS.contains(&w.as_str()) {
                return false;
            }
        }
    }

    // PRAGMA: only pragmas on the READ_PRAGMAS allowlist, and never with a
    // top-level `=`, count as reads. Both `PRAGMA name = value` and
    // `PRAGMA name(value)` are setter syntaxes in SQLite, so shape alone
    // cannot prove a pragma is a getter.
    if first == "PRAGMA" {
        if stmt.iter().any(|i| matches!(i, Item::Eq)) {
            return false;
        }
        let name = stmt.iter().filter_map(|i| match i {
            Item::Word(w) => Some(w.as_str()),
            _ => None,
        }).nth(1);
        match name {
            Some(n) if READ_PRAGMAS.contains(&n) => {}
            _ => return false,
        }
    }

    true
}

/// Checks if the SQL statement is a read-only query.
///
/// Fail-closed: an unterminated string/comment, an unrecognized leading
/// keyword, a write keyword anywhere in the text, a PRAGMA setter, or any
/// sub-statement of a `;`-separated batch failing the above all cause this
/// to return `false`. See the module doc comment and [`WRITE_KEYWORDS`] for
/// the specific bypasses this closes.
///
/// The text is split on top-level `;` and *every* non-empty statement must
/// independently pass, so `SELECT 1; DROP TABLE t` is rejected. This isn't
/// currently exploitable via the SQLite/Postgres drivers in this repo (both
/// fail closed on multi-statement text at the protocol layer), but
/// future-proofs the guard for drivers -- e.g. MSSQL/odbc-api -- that may
/// execute semicolon-separated batches.
pub fn is_read_statement(sql: &str) -> bool {
    let items = match tokenize(sql) {
        Some(items) => items,
        None => return false,
    };

    let statements = split_statements(&items);
    if statements.is_empty() {
        return false;
    }

    statements.iter().all(|stmt| is_single_statement_read(stmt))
}

/// Applies an automatic LIMIT clause to SELECT statements if safe.
///
/// Returns a tuple of (possibly rewritten SQL, whether it changed).
///
/// This is a heuristic that:
/// - Only applies to statements starting with SELECT (not WITH)
/// - Does not apply if the statement contains a LIMIT, OFFSET, FETCH, or INTO
///   token (flat scan, not paren/subquery-depth aware -- see Task 6 security
///   review Issue 5: this can only under-apply the limit, e.g. a subquery's
///   own LIMIT suppresses the outer auto-limit, which is a missed
///   optimization, not a safety issue)
/// - Does not apply if the statement ends in an open string literal or
///   comment (including unterminated nested block comments)
/// - Appends " LIMIT {n}" before any trailing semicolon
pub fn apply_auto_limit(sql: &str, limit: u64) -> (String, bool) {
    let items = match tokenize(sql) {
        Some(items) => items,
        None => return (sql.to_string(), false),
    };

    // Only apply to SELECT statements (not WITH)
    if first_word(&items) != Some("SELECT") {
        return (sql.to_string(), false);
    }

    // Check if statement already has a limiting clause
    let has_limiting_clause = items.iter().any(|i| {
        matches!(i, Item::Word(w) if matches!(w.as_str(), "LIMIT" | "OFFSET" | "FETCH" | "INTO"))
    });
    if has_limiting_clause {
        return (sql.to_string(), false);
    }

    // Apply the LIMIT clause
    let trimmed = sql.trim_end();
    let limit_str = format!(" LIMIT {}", limit);

    let result = if trimmed.ends_with(';') {
        let without_semicolon = &trimmed[..trimmed.len() - 1];
        format!("{}{};", without_semicolon, limit_str)
    } else {
        format!("{}{}", trimmed, limit_str)
    };

    (result, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_allowlist() {
        assert!(is_read_statement("  SELECT 1"));
        assert!(is_read_statement("-- note\nwith x as (select 1) select * from x"));
        assert!(is_read_statement("EXPLAIN ANALYZE select 1"));
        assert!(!is_read_statement("UPDATE t SET a=1"));
        assert!(!is_read_statement("/* c */ delete from t"));
        assert!(!is_read_statement("insert into t values (1)"));
    }

    #[test]
    fn auto_limit_appends() {
        let (sql, changed) = apply_auto_limit("select * from big", 1000);
        assert!(changed);
        assert_eq!(sql, "select * from big LIMIT 1000");
        let (sql2, changed2) = apply_auto_limit("select * from big;", 1000);
        assert!(changed2);
        assert_eq!(sql2, "select * from big LIMIT 1000;");
    }

    #[test]
    fn auto_limit_leaves_limited_and_nonselect_alone() {
        assert!(!apply_auto_limit("select * from t limit 5", 1000).1);
        assert!(!apply_auto_limit("select * from t OFFSET 2", 1000).1);
        assert!(!apply_auto_limit("update t set a=1", 1000).1);
        assert!(!apply_auto_limit("with x as (select 1) select * from x", 1000).1);
        // LIMIT inside a string literal must not count as a LIMIT token:
        let (s, ch) = apply_auto_limit("select 'no limit here' from t", 1000);
        assert!(ch);
        assert_eq!(s, "select 'no limit here' from t LIMIT 1000");
    }

    // --- Task 6 security review regression tests ---

    #[test]
    fn nested_block_comment_bypass_fails_closed() {
        // PostgreSQL nests /* */: the real leading statement here is
        // `UPDATE t SET a=1`, not `SELECT 1`.
        assert!(!is_read_statement(
            "/* /* */ SELECT 1 */ UPDATE t SET a=1"
        ));
    }

    #[test]
    fn data_modifying_cte_fails_closed() {
        assert!(!is_read_statement(
            "WITH x AS (UPDATE t SET a=1 RETURNING *) SELECT * FROM x"
        ));
    }

    #[test]
    fn explain_analyze_write_fails_closed() {
        // Postgres's EXPLAIN ANALYZE actually executes the statement.
        assert!(!is_read_statement("EXPLAIN ANALYZE UPDATE t SET a=1"));
    }

    #[test]
    fn pragma_setter_vs_getter() {
        assert!(!is_read_statement("PRAGMA journal_mode=DELETE"));
        assert!(is_read_statement("PRAGMA table_info(t)"));
        // Paren-form setters ("PRAGMA name(value)" == "PRAGMA name = value"):
        assert!(!is_read_statement("PRAGMA journal_mode(WAL)"));
        assert!(!is_read_statement("PRAGMA writable_schema(1)"));
        assert!(!is_read_statement("PRAGMA foreign_keys(0)"));
        // Allowlisted getters without arguments:
        assert!(is_read_statement("PRAGMA integrity_check"));
        assert!(is_read_statement("PRAGMA database_list"));
        // Non-allowlisted getter → fail closed (accepted cost):
        assert!(!is_read_statement("PRAGMA journal_mode"));
    }

    #[test]
    fn multi_statement_batch_requires_every_statement_to_be_read() {
        assert!(!is_read_statement("SELECT 1; DROP TABLE t"));
        assert!(is_read_statement("SELECT 1; SELECT 2"));
    }

    #[test]
    fn unterminated_block_comment_fails_closed() {
        assert!(!is_read_statement("SELECT 1 /* unterminated"));
        assert!(!apply_auto_limit("SELECT 1 /* unterminated", 1000).1);
    }

    #[test]
    fn string_literal_write_keyword_is_not_a_bypass_trigger() {
        // A string literal that happens to contain a write keyword must not
        // trip the blacklist scan.
        assert!(is_read_statement("select 'update' from t"));
    }
}
