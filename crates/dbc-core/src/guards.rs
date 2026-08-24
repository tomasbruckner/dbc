/// SQL execution guards: auto-LIMIT and read-only statement detection.
///
/// Fail-closed is the guiding principle throughout this module: whenever the
/// scanner cannot be sure a statement is safe (unterminated comment/string,
/// ambiguous PRAGMA form, an unrecognized leading keyword, a write keyword
/// appearing anywhere in the text, ...), `is_read_statement` returns `false`
/// and `apply_auto_limit` leaves the SQL untouched.

use crate::split::Dialect;

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
/// `INTO` (review round 1 finding #2) closes the `SELECT ... INTO new_tbl
/// FROM t` bypass -- PostgreSQL's legacy `CREATE TABLE AS` spelling, which
/// otherwise lexically starts with the allowlisted `SELECT` keyword and
/// contains no other write keyword. Layer 2 (forced
/// `default_transaction_read_only=on`) already catches this on Postgres,
/// but layer 1 (this lexical gate) should too -- defense in depth means
/// each layer holds on its own, not just in combination. `apply_auto_limit`
/// already treats a bare `INTO` token as a "has a limiting/target clause,
/// don't touch it" signal (see its own doc comment), so this doesn't change
/// that function's behavior at all -- it only widens what this module's
/// *other* guard, `is_read_statement`, rejects.
const WRITE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "DROP", "ALTER", "CREATE", "TRUNCATE", "GRANT",
    "REVOKE", "COPY", "CALL", "DO", "VACUUM", "REINDEX", "ATTACH", "DETACH", "REPLACE", "UPSERT",
    "EXEC", "EXECUTE", "INTO",
];

/// Leading keywords that may start a read-only statement — the base list,
/// valid for EVERY dialect (pre-G16 behavior, unchanged).
const READ_LEADING_KEYWORDS: &[&str] = &["SELECT", "WITH", "EXPLAIN", "SHOW", "VALUES", "PRAGMA"];

/// G16 §5: DuckDB's idiomatic read forms (`FROM t`, `DESCRIBE t`,
/// `SUMMARIZE t`, `PIVOT`/`UNPIVOT`), allow-listed for every dialect
/// EXCEPT `Dialect::Mssql` (see [`is_single_statement_read`]'s gate).
/// Without them these statements were not "provably read", so the G12
/// dispatch matrix routed them to `execute()` — a row-less run and an
/// empty grid, a wrong-result bug for DuckDB's most idiomatic query form.
///
/// Why the widening MUST be dialect-gated (T2 review round 1, BLOCKER):
/// T-SQL executes the first statement of a batch as an implicit
/// stored-procedure call with no EXEC keyword (the `sp_help t`
/// convention), and DESCRIBE/SUMMARIZE are NOT T-SQL reserved words —
/// `CREATE PROCEDURE DESCRIBE ...` is legal, so the batch `DESCRIBE t`
/// would EXECUTE procedure [DESCRIBE] with 't' as an argument. On MSSQL
/// this client-side guard is the ONLY read-only enforcement (no
/// server-side read-only session backstop exists there), so a dialect-
/// blind widening would have weakened the MSSQL read-only guard for
/// existing connections. FROM/PIVOT/UNPIVOT ARE T-SQL reserved words
/// (cannot lead a batch or name an unbracketed procedure) and would be
/// harmless, but the gate excludes ALL FIVE for Mssql — byte-identical
/// pre-G16 classification, no per-keyword reserved-word reasoning to
/// maintain.
///
/// Why pg/sqlite intentionally share DuckDB's widened list: neither has
/// any statement (read OR write) that starts with one of the five words —
/// a leading FROM/DESCRIBE/... is a syntax error the server refuses, so a
/// fail-closed guard passing it is harmless — and Postgres additionally
/// has the server-side `default_transaction_read_only=on` backstop. The
/// every-token WRITE_KEYWORDS scan above is the unchanged second layer
/// for all dialects.
///
/// UNPIVOT caveat: DuckDB's simplified `UNPIVOT … INTO NAME … VALUE …`
/// form still fails closed on the `INTO` blacklist entry (see
/// `duckdb_unpivot_into_form_still_fails_closed_on_the_into_blacklist` —
/// deliberate; the SQL-standard `FROM t UNPIVOT (…)` form is the
/// read-classified spelling).
const DUCKDB_READ_LEADING_KEYWORDS: &[&str] =
    &["FROM", "DESCRIBE", "SUMMARIZE", "PIVOT", "UNPIVOT"];

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

/// Pg/sqlite-convention tokenizer -- thin wrapper over [`tokenize_d`]
/// (G15 T1 review fix), byte-identical to the pre-G15 behavior: `[`/`]`
/// stay ordinary dropped punctuation, so e.g. an array subscript
/// `arr[1]` tokenizes exactly as before (`1` still surfaces as
/// `Item::Word("1")`).
fn tokenize(sql: &str) -> Option<Vec<Item>> {
    tokenize_d(sql, Dialect::Postgres)
}

/// Tokenizes `sql` into a flat sequence of [`Item`]s, tracking:
/// - single-quoted string literals (`''` is an escaped quote),
/// - double-quoted identifiers (`""` is an escaped quote),
/// - Mssql only: `[...]` bracket-quoted identifiers (`]]` is an escaped
///   `]`, mirroring `split.rs`'s `InBracketIdent`/`BracketIdentMaybeEnd`
///   states) -- G15 T1 review fix: a bracket-quoted reserved word (e.g.
///   `[Delete]`, `[Top]`, `[Order]`) must NEVER surface as a bare
///   `Item::Word` and match a keyword, closing both the false-reject-as-
///   write bug in `is_read_statement_d` and the missed-`TOP`-insertion bug
///   in `apply_auto_limit_d`. Postgres/Sqlite: `[`/`]` are never
///   bracket-quoting syntax (Postgres uses them for array subscripts, e.g.
///   `arr[1]`) -- dialect-gated exactly like `split.rs`'s own `'['`
///   handling, so `tokenize(sql)` (== `tokenize_d(sql, Dialect::Postgres)`)
///   stays byte-identical to the pre-fix behavior.
/// - `--` line comments (run to end of line),
/// - `/* ... */` block comments, which **nest** (matches PostgreSQL's actual
///   nesting semantics -- tracked via a depth counter, not a bool, which is
///   the root cause fix for the `/* /* */ SELECT 1 */ UPDATE ...` bypass).
///
/// Returns `None` if the input ends inside any of the above open constructs
/// (unterminated string/quoted-ident/bracket-ident/comment). Callers must
/// treat `None` as "cannot determine safety" and fail closed.
///
/// Known limitation (non-blocking, see Task 6 security review Issue 5):
/// PostgreSQL dollar-quoted strings (`$$...$$`) are not recognized. This can
/// only cause `apply_auto_limit` to *skip* appending a LIMIT (a limiting
/// keyword inside a `$$...$$` body false-positives as top-level) or
/// `is_read_statement` to *reject* a statement it could have allowed (a
/// write keyword inside a `$$...$$` body false-positives as a bare token) --
/// both fail in the safe direction.
fn tokenize_d(sql: &str, dialect: Dialect) -> Option<Vec<Item>> {
    let mut items = Vec::new();
    let mut chars = sql.chars().peekable();
    let mut in_single_string = false;
    let mut in_double_ident = false;
    let mut in_bracket_ident = false;
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

        if in_bracket_ident {
            if c == ']' {
                if chars.peek() == Some(&']') {
                    chars.next();
                } else {
                    in_bracket_ident = false;
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

        if c == '[' && dialect == Dialect::Mssql {
            in_bracket_ident = true;
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

    if in_single_string
        || in_double_ident
        || in_bracket_ident
        || in_line_comment
        || block_comment_depth > 0
    {
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
///
/// The allowlist is dialect-aware (G16 §5 / T2 review): the base
/// [`READ_LEADING_KEYWORDS`] applies everywhere; the DuckDB widening
/// ([`DUCKDB_READ_LEADING_KEYWORDS`]) is excluded under `Dialect::Mssql`
/// — see that const's doc comment for the implicit-proc-call hazard.
fn is_single_statement_read(stmt: &[Item], dialect: Dialect) -> bool {
    let first = match first_word(stmt) {
        Some(w) => w,
        None => return false,
    };

    let leading_ok = READ_LEADING_KEYWORDS.contains(&first)
        || (dialect != Dialect::Mssql && DUCKDB_READ_LEADING_KEYWORDS.contains(&first));
    if !leading_ok {
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
///
/// Thin pg-convention wrapper over [`is_read_statement_d`] (G15 T1 review
/// fix) -- byte-identical to the pre-G15 behavior for every existing
/// call site (none of them thread a dialect through yet; that's later
/// tasks' job).
pub fn is_read_statement(sql: &str) -> bool {
    is_read_statement_d(sql, Dialect::Postgres)
}

/// Dialect-aware sibling of [`is_read_statement`] (G15 T1 review fix).
/// Not wired into any call site yet -- added now so the Layer-1 read guard
/// is bracket-correct in `dbc-core` *before* MSSQL SQL text can ever reach
/// it (a bracket-quoted reserved word like `[Delete]`, `[Order]`, `[Top]`
/// must never be mistaken for the bare keyword and false-reject a genuine
/// read, e.g. `SELECT [Delete], [Update] FROM AuditLog`). Postgres/Sqlite
/// callers are unaffected (`[`/`]` aren't quoting syntax for those
/// dialects -- see [`tokenize_d`]).
pub fn is_read_statement_d(sql: &str, dialect: Dialect) -> bool {
    let items = match tokenize_d(sql, dialect) {
        Some(items) => items,
        None => return false,
    };

    let statements = split_statements(&items);
    if statements.is_empty() {
        return false;
    }

    statements.iter().all(|stmt| is_single_statement_read(stmt, dialect))
}

/// Applies an automatic LIMIT clause to SELECT statements if safe.
///
/// Returns a tuple of (possibly rewritten SQL, whether it changed).
///
/// Thin pg-convention wrapper over [`apply_auto_limit_d`] -- byte-identical
/// pg/sqlite behavior, unchanged by G15.
pub fn apply_auto_limit(sql: &str, limit: u64) -> (String, bool) {
    apply_auto_limit_d(sql, limit, Dialect::Postgres)
}

/// Dialect-aware sibling of [`apply_auto_limit`] (G15 §2d). Postgres/Sqlite
/// keep the historic `LIMIT {n}` suffix behavior (`apply_auto_limit_pg`,
/// renamed from the pre-G15 `apply_auto_limit` body, byte-identical).
/// Mssql inserts `TOP {n}` immediately after the leading `SELECT [ALL |
/// DISTINCT]` head instead -- T-SQL has no trailing `LIMIT`.
///
/// **Documented decision (G15 T1 review, MINOR finding): `WITH cte AS
/// (...) SELECT ...` is intentionally left a no-op (under-applies, never
/// over-applies -- same posture as every other gap this function
/// documents).** Correctly finding the top-level `SELECT` after a
/// (possibly multi-CTE, possibly nested-parens-in-string-literals) `WITH`
/// clause needs depth-aware scanning that also respects string/comment/
/// bracket-ident boundaries -- effectively a second tokenizer -- which
/// isn't worth the risk of a subtly wrong paren-matcher (e.g. an unbalanced
/// `(` inside a CTE body's string literal) for what is purely a query-size
/// optimization: the runner's row-cap still bounds result-set size even
/// when the auto-`TOP` doesn't fire. Revisit only if this proves a real
/// pain point in practice (T8-noted limitation).
pub fn apply_auto_limit_d(sql: &str, limit: u64, dialect: Dialect) -> (String, bool) {
    match dialect {
        Dialect::Postgres | Dialect::Sqlite => apply_auto_limit_pg(sql, limit),
        Dialect::Mssql => {
            let items = match tokenize_d(sql, Dialect::Mssql) {
                Some(items) => items,
                None => return (sql.to_string(), false),
            };
            // Only a bare leading SELECT is handled -- see the doc comment
            // above for the WITH/CTE no-op decision.
            if first_word(&items) != Some("SELECT") {
                return (sql.to_string(), false);
            }
            // T-SQL blocking tokens (design §2d list): flat scan, depth-
            // unaware -- can only under-apply, never over-apply.
            let has_limiting_clause = items.iter().any(|i| {
                matches!(i, Item::Word(w) if matches!(w.as_str(), "TOP" | "OFFSET" | "FETCH" | "INTO"))
            });
            if has_limiting_clause {
                return (sql.to_string(), false);
            }
            match select_head_insert_offset(sql) {
                Some(pos) => (format!("{} TOP {}{}", &sql[..pos], limit, &sql[pos..]), true),
                None => (sql.to_string(), false),
            }
        }
    }
}

/// This is a heuristic that:
/// - Only applies to statements starting with SELECT or FROM (not WITH;
///   the leading-FROM form is DuckDB's, widened in G16 §5 — see the
///   comment at the first-word check below)
/// - Does not apply if the statement contains a LIMIT, OFFSET, FETCH, or INTO
///   token (flat scan, not paren/subquery-depth aware -- see Task 6 security
///   review Issue 5: this can only under-apply the limit, e.g. a subquery's
///   own LIMIT suppresses the outer auto-limit, which is a missed
///   optimization, not a safety issue)
/// - Does not apply if the statement ends in an open string literal or
///   comment (including unterminated nested block comments)
/// - Appends " LIMIT {n}" before any trailing semicolon
fn apply_auto_limit_pg(sql: &str, limit: u64) -> (String, bool) {
    let items = match tokenize(sql) {
        Some(items) => items,
        None => return (sql.to_string(), false),
    };

    // G16 §5: `FROM t LIMIT n` is valid DuckDB and DuckDB maps to this
    // dialect path; a leading-FROM statement can't reach pg/sqlite (syntax
    // error before the limit would matter), and this body serves ONLY the
    // Postgres|Sqlite dialects — the Dialect::Mssql arm of
    // apply_auto_limit_d keeps its own SELECT-only first-word check and
    // never sees this widening (pinned by
    // mssql_auto_top_never_fires_for_leading_from). DESCRIBE/SUMMARIZE/
    // PIVOT stay un-limited (under-apply, never over-apply).
    let fw = first_word(&items);
    if fw != Some("SELECT") && fw != Some("FROM") {
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

/// Byte offset just past the leading `SELECT [ALL|DISTINCT]` head of `sql`,
/// skipping leading whitespace and comments. `None` if the head isn't
/// found (caller then returns the SQL unchanged -- under-apply, never
/// over-apply, same posture as the flat token scan).
fn select_head_insert_offset(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    // skip whitespace and comments, iteratively
    loop {
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if sql[i..].starts_with("--") {
            i += sql[i..].find('\n').map(|p| p + 1).unwrap_or(sql.len() - i);
            continue;
        }
        if sql[i..].starts_with("/*") {
            let mut depth = 1u32;
            let mut j = i + 2;
            while j < bytes.len() && depth > 0 {
                if sql[j..].starts_with("/*") {
                    depth += 1;
                    j += 2;
                } else if sql[j..].starts_with("*/") {
                    depth -= 1;
                    j += 2;
                } else {
                    // BLOCKER fix (whole-branch review B1): advance by one
                    // full character, not one byte. `sql[j..]` above needs
                    // `j` on a char boundary on every iteration; a bare
                    // `j += 1` walks into the middle of any multi-byte
                    // UTF-8 character (e.g. `é`, or any Czech diacritic)
                    // and the NEXT loop iteration's `sql[j..]` slice panics
                    // ("byte index N is not a char boundary") -- live-
                    // reproduced with `/* é */ SELECT x FROM t` under
                    // `Dialect::Mssql`, reachable from the GPUI main thread
                    // via `apply_auto_limit_d` the moment a user types a
                    // non-ASCII leading block comment on any MSSQL
                    // connection with auto-limit on. `j < bytes.len()`
                    // (the loop condition) guarantees a char exists here.
                    let c = sql[j..].chars().next().expect("j < bytes.len(), so a char exists at j");
                    j += c.len_utf8();
                }
            }
            if depth > 0 {
                return None; // unterminated -- tokenize() already refused anyway
            }
            i = j;
            continue;
        }
        break;
    }
    let word_end = |start: usize| -> usize {
        sql[start..]
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|p| start + p)
            .unwrap_or(sql.len())
    };
    let end = word_end(i);
    if !sql[i..end].eq_ignore_ascii_case("SELECT") {
        return None;
    }
    // optionally consume one ALL/DISTINCT
    let mut k = end;
    while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
        k += 1;
    }
    let k_end = word_end(k);
    if sql[k..k_end].eq_ignore_ascii_case("DISTINCT") || sql[k..k_end].eq_ignore_ascii_case("ALL") {
        return Some(k_end);
    }
    Some(end)
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

    // Review round 1 finding #2: PostgreSQL's legacy `SELECT ... INTO`
    // spelling of `CREATE TABLE AS` lexically starts with the allowlisted
    // `SELECT` keyword and (before this fix) contained no other write
    // keyword -- layer 1 must reject it directly, not rely solely on
    // dbc-mcp forcing `default_transaction_read_only=on` at the driver
    // layer to catch it.
    #[test]
    fn select_into_is_not_a_read_statement() {
        assert!(!is_read_statement("SELECT * INTO new_tbl FROM t"));
        assert!(!is_read_statement("select id into backup_t from t where id > 10"));
        // Still fails closed inside a WITH/CTE wrapper too.
        assert!(!is_read_statement("WITH x AS (SELECT 1) SELECT * INTO y FROM x"));
    }

    #[test]
    fn select_into_does_not_change_auto_limit_behavior() {
        // apply_auto_limit already treated a bare INTO token as a
        // limiting/target clause before this change (skips appending
        // LIMIT); adding INTO to WRITE_KEYWORDS only affects
        // is_read_statement, not this function -- verified unchanged here.
        assert!(!apply_auto_limit("select * into new_tbl from t", 1000).1);
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

    // ---------- Mssql TOP auto-limit (G15 T1) ----------

    #[test]
    fn auto_top_inserts_after_select() {
        let (sql, changed) = apply_auto_limit_d("select * from big", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "select TOP 1000 * from big");
    }

    #[test]
    fn auto_top_after_distinct() {
        let (sql, changed) =
            apply_auto_limit_d("SELECT DISTINCT x FROM t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "SELECT DISTINCT TOP 1000 x FROM t");
    }

    #[test]
    fn auto_top_leaves_top_offset_fetch_into_alone() {
        assert!(!apply_auto_limit_d("SELECT TOP 5 * FROM t", 1000, Dialect::Mssql).1);
        assert!(
            !apply_auto_limit_d(
                "SELECT * FROM t ORDER BY x OFFSET 5 ROWS FETCH NEXT 10 ROWS ONLY",
                1000,
                Dialect::Mssql
            )
            .1
        );
        assert!(!apply_auto_limit_d("SELECT * INTO new_tbl FROM t", 1000, Dialect::Mssql).1);
    }

    #[test]
    fn auto_top_with_trailing_semicolon() {
        let (sql, changed) = apply_auto_limit_d("select * from t;", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "select TOP 1000 * from t;");
    }

    #[test]
    fn auto_top_after_leading_comment() {
        let (sql, changed) =
            apply_auto_limit_d("/* hint */ SELECT x FROM t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "/* hint */ SELECT TOP 1000 x FROM t");
    }

    /// BLOCKER fix regression (whole-branch review B1): the reviewer's
    /// exact live repro — `select_head_insert_offset`'s block-comment skip
    /// used to advance byte-wise (`j += 1`) then slice `sql[j..]`, panicking
    /// mid-UTF-8-character. Typing this on any MSSQL connection with
    /// auto-limit on used to abort the whole app (GPUI main thread, no
    /// `catch_unwind` between `apply_auto_limit_d` and the UI).
    #[test]
    fn auto_top_after_leading_comment_with_non_ascii_does_not_panic() {
        let (sql, changed) =
            apply_auto_limit_d("/* é */ SELECT x FROM t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "/* é */ SELECT TOP 1000 x FROM t");
    }

    /// Same class, Czech diacritics (multiple multi-byte characters in a
    /// row, not just one) — the exact shape the reviewer's report describes
    /// a real user typing.
    #[test]
    fn auto_top_after_leading_comment_with_czech_diacritics_does_not_panic() {
        let (sql, changed) =
            apply_auto_limit_d("/* Příliš žluťoučký kůň */ SELECT * FROM t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "/* Příliš žluťoučký kůň */ SELECT TOP 1000 * FROM t");
    }

    /// Non-ASCII inside a NESTED block comment — stresses the char-boundary
    /// fix together with the depth counter (both `/*`/`*/` re-matching and
    /// the byte-vs-char advance must agree on where `j` lands).
    #[test]
    fn auto_top_after_nested_leading_comment_with_non_ascii_does_not_panic() {
        let (sql, changed) =
            apply_auto_limit_d("/* outer /* café */ still outer */ SELECT 1", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "/* outer /* café */ still outer */ SELECT TOP 1000 1");
    }

    /// Sibling scan (`sql[i..].find('\n')`, `str::find` on a `char`
    /// pattern) was already char-boundary-safe before this fix — this test
    /// pins that down explicitly rather than leaving it merely asserted in
    /// the review response.
    #[test]
    fn auto_top_after_leading_line_comment_with_non_ascii_does_not_panic() {
        let (sql, changed) =
            apply_auto_limit_d("-- Příliš žluťoučký kůň\nSELECT * FROM t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "-- Příliš žluťoučký kůň\nSELECT TOP 1000 * FROM t");
    }

    /// The pg/sqlite path (`apply_auto_limit_pg`) never calls
    /// `select_head_insert_offset` at all (it appends a trailing `LIMIT`
    /// via `trim_end`/`ends_with(';')`, not a head-insert scan) — was never
    /// vulnerable to this class of bug, and stays byte-identical with a
    /// non-ASCII leading comment before and after this fix.
    #[test]
    fn auto_limit_pg_path_unaffected_by_non_ascii_leading_comment() {
        let (sql, changed) =
            apply_auto_limit_d("/* Příliš žluťoučký kůň */ select * from t", 1000, Dialect::Postgres);
        assert!(changed);
        assert_eq!(sql, "/* Příliš žluťoučký kůň */ select * from t LIMIT 1000");
    }

    #[test]
    fn auto_top_string_literal_top_is_not_a_blocker() {
        // Token scan ignores strings -- `TOP` inside a string literal must
        // not suppress the auto-TOP.
        let (sql, changed) =
            apply_auto_limit_d("select 'top secret' from t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "select TOP 1000 'top secret' from t");
    }

    #[test]
    fn apply_auto_limit_wrapper_is_byte_identical_pg() {
        assert_eq!(
            apply_auto_limit("select * from big", 1000),
            apply_auto_limit_d("select * from big", 1000, Dialect::Postgres)
        );
    }

    // ---------- Mssql bracket-ident awareness in tokenize (G15 T1 review fix) ----------

    #[test]
    fn mssql_bracket_reserved_word_is_not_mistaken_for_a_write_keyword() {
        // Review probe case 1: a pure SELECT over bracket-quoted
        // reserved-word column names must not be misclassified as a write.
        assert!(is_read_statement_d("SELECT [Delete], [Update] FROM AuditLog", Dialect::Mssql));
    }

    #[test]
    fn mssql_bracket_order_and_user_variants_are_read() {
        assert!(is_read_statement_d("SELECT [Order], [User] FROM t", Dialect::Mssql));
    }

    #[test]
    fn mssql_bracket_escaped_close_bracket_does_not_break_tokenizing() {
        assert!(is_read_statement_d("SELECT [we]]ird] FROM t", Dialect::Mssql));
    }

    #[test]
    fn mssql_bracket_does_not_hide_a_bare_write_keyword_elsewhere() {
        // A bracket only protects its OWN contents -- a genuine bare write
        // keyword elsewhere in the batch must still be rejected.
        assert!(!is_read_statement_d("SELECT [Order] FROM t; DELETE FROM t", Dialect::Mssql));
    }

    #[test]
    fn mssql_unterminated_bracket_fails_closed_in_guards_too() {
        assert!(!is_read_statement_d("SELECT [oops", Dialect::Mssql));
        assert!(!apply_auto_limit_d("SELECT [oops", 1000, Dialect::Mssql).1);
    }

    #[test]
    fn mssql_bracket_reserved_word_gets_auto_top() {
        // Review probe case 2: `[Top]` as a bracket-quoted column name must
        // not be mistaken for the TOP auto-limit blocker.
        let (sql, changed) =
            apply_auto_limit_d("SELECT [Top] FROM Rankings", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "SELECT TOP 1000 [Top] FROM Rankings");
    }

    #[test]
    fn is_read_statement_wrapper_is_byte_identical_pg() {
        assert_eq!(
            is_read_statement("select * from t"),
            is_read_statement_d("select * from t", Dialect::Postgres)
        );
    }

    #[test]
    fn pg_array_subscript_bracket_handling_is_unchanged() {
        // Postgres/Sqlite: `[`/`]` are never quoting syntax (Postgres uses
        // them for array subscripts) -- tokenize_d(_, Postgres) must stay
        // byte-identical to the pre-fix behavior: content inside brackets
        // still tokenizes as ordinary words.
        assert_eq!(
            apply_auto_limit("select arr[1] from t", 1000),
            ("select arr[1] from t LIMIT 1000".to_string(), true)
        );
        assert!(is_read_statement("select arr[1] from t"));
        // Pins that bracket-skipping is NOT applied outside Mssql: a bare
        // WRITE_KEYWORDS token inside brackets still poisons the pg read
        // guard exactly as before this fix (dialect-scoping regression
        // guard).
        assert!(!is_read_statement("select arr[delete] from t"));
    }

    // ---------- G16 §5: DuckDB's leading-FROM family ----------

    #[test]
    fn duckdb_bare_from_describe_summarize_pivot_are_reads() {
        for sql in [
            "FROM t",
            "from big_table where x > 1 order by 1",
            "DESCRIBE t",
            "SUMMARIZE t",
            "PIVOT cities ON year USING sum(population)",
        ] {
            assert!(is_read_statement(sql), "{sql} must classify as a read");
        }
    }

    /// KNOWN, SAFE limitation (pinned, resolved against the design's §5
    /// list): DuckDB's simplified UNPIVOT syntax REQUIRES an `INTO NAME …
    /// VALUE …` clause, and `INTO` sits on the WRITE_KEYWORDS blacklist
    /// (SELECT-INTO protection, layer 2) — so the statement still
    /// classifies as "not provably read" and fails CLOSED (routed to
    /// execute on a writable connection, rejected on read-only). The
    /// leading keyword stays on the allowlist (costless, and the
    /// SQL-standard `FROM t UNPIVOT (…)` form works via `FROM`); lifting
    /// the INTO collision would mean weakening the blacklist — the wrong
    /// trade.
    #[test]
    fn duckdb_unpivot_into_form_still_fails_closed_on_the_into_blacklist() {
        assert!(!is_read_statement("UNPIVOT monthly ON jan, feb INTO NAME month VALUE amount"));
        // The SQL-standard rewrite IS a read (leading FROM, no INTO):
        assert!(is_read_statement("FROM monthly UNPIVOT (amount FOR month IN (jan, feb))"));
    }

    #[test]
    fn from_leading_statement_with_a_write_keyword_anywhere_is_still_rejected() {
        // Layer 2 (the every-token WRITE_KEYWORDS scan) is untouched by the
        // allowlist widening — fail-closed posture preserved.
        assert!(!is_read_statement("FROM t SELECT * INTO backup_t"));
        assert!(!is_read_statement("FROM t, (UPDATE u SET x = 1) s"));
        assert!(!is_read_statement("FROM t; DROP TABLE t"));
    }

    #[test]
    fn auto_limit_fires_for_leading_from() {
        assert_eq!(apply_auto_limit("FROM t", 1000), ("FROM t LIMIT 1000".to_string(), true));
        assert_eq!(apply_auto_limit("from t;", 1000), ("from t LIMIT 1000;".to_string(), true));
        // unchanged skip conditions:
        assert!(!apply_auto_limit("FROM t LIMIT 5", 1000).1);
        assert!(!apply_auto_limit("FROM t OFFSET 2", 1000).1);
        // DESCRIBE/SUMMARIZE do NOT get a limit (first word is neither
        // SELECT nor FROM — under-apply, never over-apply):
        assert!(!apply_auto_limit("DESCRIBE t", 1000).1);
    }

    /// Poison probe for the CRITICAL invariant: the `Dialect::Mssql` arm of
    /// `apply_auto_limit_d` is untouched by the G16 widening — its own
    /// `first_word == SELECT` check keeps a leading `FROM` un-limited there
    /// (leading FROM is not T-SQL), byte-identical to pre-G16.
    #[test]
    fn mssql_auto_top_never_fires_for_leading_from() {
        assert!(!apply_auto_limit_d("FROM t", 1000, Dialect::Mssql).1);
    }

    /// BLOCKER fix pin (T2 review round 1): the G16 allowlist widening is
    /// DIALECT-GATED — under `Dialect::Mssql` read classification is
    /// byte-identical to pre-G16. T-SQL executes the first statement of a
    /// batch as an implicit stored-procedure call with no EXEC keyword (the
    /// `sp_help t` convention), and DESCRIBE/SUMMARIZE are NOT T-SQL
    /// reserved words — `CREATE PROCEDURE DESCRIBE ...` is legal, so the
    /// batch `DESCRIBE t` would EXECUTE procedure [DESCRIBE] with 't' as an
    /// argument. On MSSQL this client-side guard is the ONLY read-only
    /// enforcement (no server-side read-only session backstop) — the
    /// pre-G16 refusal must be preserved.
    #[test]
    fn mssql_describe_and_summarize_stay_refused_implicit_proc_call_hazard() {
        assert!(!is_read_statement_d("DESCRIBE t", Dialect::Mssql));
        assert!(!is_read_statement_d("SUMMARIZE t", Dialect::Mssql));
    }

    /// FROM/PIVOT/UNPIVOT are T-SQL reserved words (cannot lead a batch or
    /// name an unbracketed procedure), so allowing them would be harmless —
    /// but the gate excludes ALL FIVE new keywords for Mssql, keeping its
    /// classification byte-identical to pre-G16 for every input (simplest
    /// robust shape; no per-keyword reserved-word reasoning to maintain).
    #[test]
    fn mssql_from_pivot_unpivot_stay_refused_byte_identical_pre_g16() {
        assert!(!is_read_statement_d("FROM t", Dialect::Mssql));
        assert!(!is_read_statement_d(
            "PIVOT cities ON year USING sum(population)",
            Dialect::Mssql
        ));
        assert!(!is_read_statement_d("UNPIVOT monthly ON jan, feb", Dialect::Mssql));
    }

    /// Positive control for the dialect gate: genuine Mssql reads still
    /// classify as reads after the exclusion (the gate only removes the
    /// five G16 keywords, never the base allowlist).
    #[test]
    fn mssql_reads_still_classify_after_the_dialect_gate() {
        assert!(is_read_statement_d("SELECT 1", Dialect::Mssql));
        assert!(is_read_statement_d("WITH x AS (SELECT 1) SELECT * FROM x", Dialect::Mssql));
        assert!(!is_read_statement_d("DELETE FROM t", Dialect::Mssql));
    }

    #[test]
    fn select_auto_limit_behavior_is_byte_identical_to_pre_g16() {
        // The SELECT path through apply_auto_limit_pg is untouched.
        assert_eq!(
            apply_auto_limit("select * from big", 1000),
            ("select * from big LIMIT 1000".to_string(), true)
        );
        assert!(!apply_auto_limit("select * from t limit 5", 1000).1);
    }

    // ---------- Mssql CTE/WITH auto-limit: documented no-op (G15 T1 review, MINOR) ----------

    #[test]
    fn auto_top_leaves_single_cte_alone_documented_no_op() {
        let sql = "WITH cte AS (SELECT * FROM t) SELECT * FROM cte";
        assert!(!apply_auto_limit_d(sql, 1000, Dialect::Mssql).1);
    }

    #[test]
    fn auto_top_leaves_multiple_ctes_alone_documented_no_op() {
        let sql = "WITH a AS (SELECT * FROM t1), b AS (SELECT * FROM t2) \
                   SELECT * FROM a JOIN b ON a.id = b.id";
        assert!(!apply_auto_limit_d(sql, 1000, Dialect::Mssql).1);
    }

    #[test]
    fn auto_top_leaves_nested_cte_alone_documented_no_op() {
        let sql = "WITH outer_cte AS (WITH inner_cte AS (SELECT 1 AS x) SELECT x FROM inner_cte) \
                   SELECT * FROM outer_cte";
        assert!(!apply_auto_limit_d(sql, 1000, Dialect::Mssql).1);
    }

    #[test]
    fn auto_top_with_inside_a_string_literal_is_still_safely_a_no_op() {
        // A genuine SELECT (no real WITH clause) whose string literal
        // happens to contain the text "WITH" must NOT be confused with a
        // real WITH-headed statement -- the flat first-word check only
        // ever looks at items[0], and tokenize discards string content, so
        // auto-TOP correctly still fires here (the mirror of the CTE
        // no-op tests above: proves the two cases aren't conflated).
        let (sql, changed) =
            apply_auto_limit_d("SELECT 'WITH cte AS (x)' FROM t", 1000, Dialect::Mssql);
        assert!(changed);
        assert_eq!(sql, "SELECT TOP 1000 'WITH cte AS (x)' FROM t");
    }
}
