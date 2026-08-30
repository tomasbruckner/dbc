//! Dialect-aware SQL lexer and a conservative pretty-printer.
//!
//! Two jobs, one lexer:
//!
//! * [`format_sql`] — the editor's „Formátovat" button / Ctrl+Shift+F.
//! * [`lex`] + [`is_keyword`] — the per-dialect keyword knowledge
//!   `dbc-ui`'s highlighter overlays on top of tree-sitter's generic
//!   grammar.
//!
//! **The safety property.** Formatting rewrites WHITESPACE and the CASE of
//! keywords, and nothing else. The text of every string, comment and quoted
//! identifier is copied byte for byte. `lex(format_sql(s)) == lex(s)` once
//! whitespace is dropped and keywords are folded — pinned by
//! [`tests::formatting_preserves_every_significant_token`]. A formatter that
//! can silently corrupt a literal is worse than no formatter at all, so this
//! is the invariant to keep if any rule below ever changes.
//!
//! There is no `Dialect::Duckdb`: DuckDB is `Dialect::Postgres` everywhere
//! in this codebase (`sql_dialect` in `dbc-ui`), and its lexical rules —
//! `"…"` identifiers, `$tag$` bodies, `::` casts — really are Postgres's.
//! Its extra KEYWORDS ride along in the Postgres set; see [`is_keyword`].

use crate::split::Dialect;

/// What a token is, for the two consumers above. Deliberately coarse: this
/// is a lexer, not a parser, and every consumer only needs to know "may I
/// touch this text?" and "could this be a keyword?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    /// Whitespace run. The ONLY kind the formatter is allowed to invent,
    /// drop or resize.
    Ws,
    /// `-- …` to end of line (the newline itself is a separate [`TokKind::Ws`]).
    LineComment,
    /// `/* … */`, nesting-aware.
    BlockComment,
    /// `'…'`, `N'…'`, `E'…'`, `$tag$…$tag$`. Copied verbatim, always.
    Str,
    /// `"…"`, `[…]`, `` `…` `` — a quoted identifier. Copied verbatim: the
    /// quoting is what makes it case-sensitive, so folding it would change
    /// which object the SQL names.
    QuotedIdent,
    /// A bare word: a keyword, an identifier, or a `@variable`.
    Word,
    /// A numeric literal.
    Num,
    /// Anything else — punctuation and operators, one token per operator.
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tok {
    pub kind: TokKind,
    pub text: String,
}

impl Tok {
    /// Uppercased text, but only for a bare [`TokKind::Word`]. Quoted
    /// identifiers and literals answer `None` because their case is
    /// meaningful.
    fn word_upper(&self) -> Option<String> {
        (self.kind == TokKind::Word).then(|| self.text.to_uppercase())
    }
}

// --- Keywords ---

/// Keywords every dialect here shares. Used for BOTH highlighting and the
/// formatter's uppercasing, so a word listed here changes case on format.
const COMMON: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "BEGIN", "BETWEEN", "BY", "CASE", "CAST",
    "CHECK", "COLLATE", "COLUMN", "COMMIT", "CONSTRAINT", "CREATE", "CROSS", "CURRENT_DATE",
    "CURRENT_TIME", "CURRENT_TIMESTAMP", "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE",
    "END", "ESCAPE", "EXCEPT", "EXISTS", "FALSE", "FOREIGN", "FROM", "FULL", "GROUP", "HAVING",
    "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE",
    "LIMIT", "NATURAL", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "PRIMARY",
    "REFERENCES", "RIGHT", "ROLLBACK", "SELECT", "SET", "TABLE", "THEN", "TRANSACTION", "TRIGGER",
    "TRUE", "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WITH",
];

/// T-SQL. `TOP` and `IDENTITY` are the ones a Postgres-shaped keyword list
/// visibly misses in this app, since MSSQL is a first-class engine here.
const MSSQL_ONLY: &[&str] = &[
    "APPLY", "BIGINT", "BIT", "CLUSTERED", "DATETIME2", "DECLARE", "EXEC", "EXECUTE", "GO",
    "IDENTITY", "MERGE", "NCHAR", "NOLOCK", "NONCLUSTERED", "NVARCHAR", "OUTPUT", "PIVOT", "PRINT",
    "PROCEDURE", "RAISERROR", "ROWCOUNT", "TOP", "TRY", "UNPIVOT", "VARCHAR", "XACT_ABORT",
];

/// Postgres — and DuckDB, which this dialect stands in for. `QUALIFY` and
/// `SUMMARIZE` are DuckDB's; they are harmless extra colour on a real
/// Postgres connection and the alternative is a fourth `Dialect` variant
/// that nothing else in the codebase wants.
const POSTGRES_ONLY: &[&str] = &[
    "ANALYZE", "ARRAY", "BOOLEAN", "CONFLICT", "DO", "EXCLUDED", "EXPLAIN", "ILIKE", "INTERVAL",
    "JSONB", "LATERAL", "MATERIALIZED", "NOTHING", "NUMERIC", "QUALIFY", "RETURNING", "SERIAL",
    "SIMILAR", "SUMMARIZE", "TEXT", "TIMESTAMPTZ", "UNNEST", "VACUUM", "WINDOW",
];

/// SQLite.
const SQLITE_ONLY: &[&str] =
    &["AUTOINCREMENT", "CONFLICT", "GLOB", "INTEGER", "PRAGMA", "REAL", "REPLACE", "VACUUM"];

/// Is `word` a keyword of `dialect`? Case-insensitive.
///
/// The per-dialect half is the whole point of the user's request
/// (2026-08-28: „chci tam syntax highlighting podle enginu") — `TOP` is a
/// keyword against MSSQL and an ordinary identifier against Postgres, and a
/// single global list has to be wrong for one of them.
pub fn is_keyword(word: &str, dialect: Dialect) -> bool {
    let upper = word.to_uppercase();
    let extra = match dialect {
        Dialect::Mssql => MSSQL_ONLY,
        Dialect::Postgres => POSTGRES_ONLY,
        Dialect::Sqlite => SQLITE_ONLY,
    };
    COMMON.contains(&upper.as_str()) || extra.contains(&upper.as_str())
}

// --- Lexer ---

/// Splits `sql` into tokens. Total: every byte of the input lands in exactly
/// one token, so `toks.concat() == sql`, including for unterminated
/// literals at EOF (the trailing token simply runs to the end). That
/// totality is what lets the formatter reassemble text it does not
/// understand instead of dropping it.
pub fn lex(sql: &str, dialect: Dialect) -> Vec<Tok> {
    let b: Vec<char> = sql.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0usize;

    while i < b.len() {
        let c = b[i];

        if c.is_whitespace() {
            let start = i;
            while i < b.len() && b[i].is_whitespace() {
                i += 1;
            }
            out.push(tok(TokKind::Ws, &b[start..i]));
            continue;
        }

        // `-- …`
        if c == '-' && b.get(i + 1) == Some(&'-') {
            let start = i;
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            out.push(tok(TokKind::LineComment, &b[start..i]));
            continue;
        }

        // `/* … */`, nesting-aware — Postgres nests these, and a
        // non-nesting scan would end the comment at the first `*/` and
        // start lexing comment text as code.
        if c == '/' && b.get(i + 1) == Some(&'*') {
            let start = i;
            let mut depth = 0usize;
            while i < b.len() {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            out.push(tok(TokKind::BlockComment, &b[start..i]));
            continue;
        }

        // `$tag$ … $tag$` (Postgres/DuckDB only).
        if dialect == Dialect::Postgres && c == '$' {
            if let Some(end) = dollar_quote_end(&b, i) {
                out.push(tok(TokKind::Str, &b[i..end]));
                i = end;
                continue;
            }
        }

        // `'…'`, with the `N`/`E` prefixes attached to the SAME token so the
        // formatter can never separate a prefix from its literal.
        if c == '\'' {
            let end = single_quote_end(&b, i, false);
            out.push(tok(TokKind::Str, &b[i..end]));
            i = end;
            continue;
        }
        if (c == 'N' || c == 'n' || c == 'E' || c == 'e') && b.get(i + 1) == Some(&'\'') {
            // Postgres `E'…'` takes BACKSLASH escapes, so `E'x\'y'` is ONE
            // literal. Scanning it with the doubled-quote rule alone ends
            // the token inside the string and lexes its tail as code — which
            // is exactly what `formatting_preserves_every_significant_token`
            // caught. T-SQL's `N'…'` has no such rule.
            let backslash = matches!(c, 'E' | 'e') && dialect == Dialect::Postgres;
            let end = single_quote_end(&b, i + 1, backslash);
            out.push(tok(TokKind::Str, &b[i..end]));
            i = end;
            continue;
        }

        // Quoted identifiers. `"` is standard; `[` is MSSQL (and SQLite,
        // which accepts it for compatibility); `` ` `` is SQLite's.
        if c == '"' {
            let end = doubled_close(&b, i, '"', '"');
            out.push(tok(TokKind::QuotedIdent, &b[i..end]));
            i = end;
            continue;
        }
        if c == '[' && matches!(dialect, Dialect::Mssql | Dialect::Sqlite) {
            let end = doubled_close(&b, i, '[', ']');
            out.push(tok(TokKind::QuotedIdent, &b[i..end]));
            i = end;
            continue;
        }
        if c == '`' && dialect == Dialect::Sqlite {
            let end = doubled_close(&b, i, '`', '`');
            out.push(tok(TokKind::QuotedIdent, &b[i..end]));
            i = end;
            continue;
        }

        // Words. `@` leads a T-SQL variable/parameter and `#` a temp table;
        // both are part of the name, not punctuation next to it.
        if c.is_alphabetic() || c == '_' || c == '@' || c == '#' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '@' || b[i] == '#')
            {
                i += 1;
            }
            out.push(tok(TokKind::Word, &b[start..i]));
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '.') {
                i += 1;
            }
            out.push(tok(TokKind::Num, &b[start..i]));
            continue;
        }

        // Multi-character operators, longest first, so `::` never becomes
        // two `:` tokens the formatter might space apart.
        const OPS: &[&str] = &["::", "<=", ">=", "<>", "!=", "||", "->>", "->"];
        let rest: String = b[i..].iter().take(3).collect();
        if let Some(op) = OPS.iter().find(|o| rest.starts_with(**o)) {
            out.push(Tok { kind: TokKind::Punct, text: (*op).to_string() });
            i += op.chars().count();
            continue;
        }

        out.push(tok(TokKind::Punct, &b[i..i + 1]));
        i += 1;
    }
    out
}

fn tok(kind: TokKind, chars: &[char]) -> Tok {
    Tok { kind, text: chars.iter().collect() }
}

/// End (exclusive) of a `'…'` literal starting at `open`, honouring the
/// doubled-quote escape and — for a Postgres `E'…'` — the backslash escape.
/// Runs to EOF when unterminated.
fn single_quote_end(b: &[char], open: usize, backslash_escapes: bool) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if backslash_escapes && b[i] == '\\' {
            i += 2;
            continue;
        }
        if b[i] == '\'' {
            if b.get(i + 1) == Some(&'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// End (exclusive) of a quoted identifier, honouring the doubled-close
/// escape (`""`, `]]`, ` `` `).
fn doubled_close(b: &[char], open: usize, _open_ch: char, close: char) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if b[i] == close {
            if b.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// End (exclusive) of a `$tag$…$tag$` body starting at `start`, or `None`
/// if what follows is not a dollar-quote open (a bare `$` or `$1`
/// placeholder).
fn dollar_quote_end(b: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
        j += 1;
    }
    if b.get(j) != Some(&'$') {
        return None;
    }
    let tag: Vec<char> = b[start..=j].to_vec();
    let mut k = j + 1;
    while k < b.len() {
        if b[k] == '$' && b[k..].starts_with(tag.as_slice()) {
            return Some(k + tag.len());
        }
        k += 1;
    }
    Some(b.len())
}

// --- Formatter ---

/// Words that start a new line at their current nesting depth.
///
/// `ON` is deliberately absent: keeping it on the `JOIN` line reads better
/// than a two-line join, and it is the shape this app's own generated SQL
/// already uses.
const CLAUSE_LEAD: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "OFFSET", "UNION", "EXCEPT",
    "INTERSECT", "INSERT", "UPDATE", "DELETE", "VALUES", "SET", "JOIN", "LEFT", "RIGHT", "INNER",
    "FULL", "CROSS", "NATURAL", "WITH", "RETURNING", "QUALIFY", "WINDOW",
];

/// Join modifiers. A `JOIN` right after one of these must NOT break the
/// line again — `LEFT JOIN` is one clause lead, not two.
const JOIN_MODIFIER: &[&str] = &["LEFT", "RIGHT", "INNER", "FULL", "CROSS", "NATURAL", "OUTER"];

/// Pretty-prints `sql`.
///
/// Rules, all deliberately conservative:
/// * keywords uppercased (bare words only — see [`Tok::word_upper`]);
/// * one clause per line, indented two spaces per open paren;
/// * a line break after a top-level comma, so a wide `SELECT` list reads
///   as one column per line, while commas INSIDE parens (function
///   arguments, `VALUES` tuples) stay inline where they are short;
/// * `;` ends the statement and is followed by a blank line;
/// * strings, comments and quoted identifiers reproduced byte for byte.
///
/// Anything it does not recognise is passed through with normalised
/// spacing rather than reordered, which is why an unparseable fragment
/// comes back intact instead of mangled.
/// The kind of statement `sql` is, as one of a CLOSED set of keywords.
///
/// Exists for the diagnostic log, which records WHAT kind of statement ran
/// but never the statement itself. The `&'static str` return is the
/// mechanism, not a detail: every value it can produce is a literal in this
/// function, so no fragment of the user's SQL — a table name, a WHERE
/// literal, a password in an UPDATE — can reach the log through here. An
/// unrecognised leading word is `"OTHER"`, deliberately, rather than being
/// passed through.
///
/// Comments and whitespace before the statement are skipped; a leading
/// `(SELECT …)` or a CTE reports `OTHER`/`WITH` rather than guessing.
pub fn statement_kind(sql: &str) -> &'static str {
    const KINDS: &[&str] = &[
        "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "WITH", "CREATE", "ALTER", "DROP",
        "TRUNCATE", "GRANT", "REVOKE", "EXEC", "EXECUTE", "CALL", "BEGIN", "COMMIT", "ROLLBACK",
        "SET", "SHOW", "EXPLAIN", "ANALYZE", "VACUUM", "PRAGMA", "USE", "DECLARE", "COPY",
        "REFRESH", "REINDEX", "COMMENT", "VALUES", "TABLE",
    ];
    // Postgres is a neutral choice here: the three dialects agree on
    // comment and whitespace syntax, which is all that is skipped before
    // the first bare word.
    let first = lex(sql, Dialect::Postgres)
        .into_iter()
        .find(|t| matches!(t.kind, TokKind::Word))
        .map(|t| t.text.to_uppercase());
    match first {
        Some(w) => KINDS.iter().find(|k| **k == w).copied().unwrap_or("OTHER"),
        None => "OTHER",
    }
}

pub fn format_sql(sql: &str, dialect: Dialect) -> String {
    let toks = lex(sql, dialect);
    let sig: Vec<&Tok> = toks.iter().filter(|t| t.kind != TokKind::Ws).collect();
    let mut out = String::new();
    let mut depth: usize = 0;
    // `None` = at the start of a line, so nothing needs a space before it.
    let mut line_has_content = false;
    let mut prev_word: Option<String> = None;

    for (idx, t) in sig.iter().enumerate() {
        let upper = t.word_upper();
        let is_close = t.text == ")";
        let is_open = t.text == "(";

        if is_close {
            depth = depth.saturating_sub(1);
        }

        let mut break_before = false;
        if let Some(u) = &upper {
            if CLAUSE_LEAD.contains(&u.as_str()) && line_has_content {
                // `LEFT JOIN` / `INNER JOIN`: the modifier already broke the
                // line, so the `JOIN` itself must not break it again.
                let after_modifier = u == "JOIN"
                    && prev_word.as_deref().is_some_and(|p| JOIN_MODIFIER.contains(&p));
                // `GROUP`/`ORDER` are only clause leads when followed by
                // `BY` — `ORDER` is also an ordinary column name.
                let needs_by = (u == "GROUP" || u == "ORDER")
                    && next_word(&sig, idx).as_deref() != Some("BY");
                break_before = !after_modifier && !needs_by;
            }
        }
        if t.kind == TokKind::LineComment && line_has_content {
            // A `--` comment must never be appended after code on a line it
            // did not start on: everything after it to end of line would be
            // swallowed.
            break_before = true;
        }

        if break_before {
            newline(&mut out, depth, &mut line_has_content);
        }

        if line_has_content && needs_space_before(t, sig.get(idx.wrapping_sub(1)).copied()) {
            out.push(' ');
        }

        match &upper {
            Some(u) if is_keyword(&t.text, dialect) => out.push_str(u),
            _ => out.push_str(&t.text),
        }
        line_has_content = true;

        if t.kind == TokKind::Word {
            prev_word = upper;
        } else if t.kind != TokKind::LineComment && t.kind != TokKind::BlockComment {
            prev_word = None;
        }

        if is_open {
            depth += 1;
        }
        if t.text == ";" {
            out.push('\n');
            out.push('\n');
            line_has_content = false;
            depth = 0;
            prev_word = None;
            continue;
        }
        if t.text == "," && depth == 0 {
            newline(&mut out, depth + 1, &mut line_has_content);
        }
        if t.kind == TokKind::LineComment {
            newline(&mut out, depth, &mut line_has_content);
        }
    }

    while out.ends_with('\n') || out.ends_with(' ') {
        out.pop();
    }
    out.push('\n');
    out
}

fn newline(out: &mut String, depth: usize, line_has_content: &mut bool) {
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
    *line_has_content = false;
}

/// Spacing between two adjacent significant tokens.
fn needs_space_before(t: &Tok, prev: Option<&Tok>) -> bool {
    let Some(prev) = prev else { return false };
    // Never space a closer/comma/semicolon away from what it closes.
    if matches!(t.text.as_str(), ")" | "," | ";" | "::") {
        return false;
    }
    if matches!(prev.text.as_str(), "(" | "::") {
        return false;
    }
    // `schema.table` and `t.col` are one name.
    if t.text == "." || prev.text == "." {
        return false;
    }
    // A function call: `count(` not `count (`. A keyword before `(` keeps
    // its space, since `IN (…)` and `VALUES (…)` read as two things.
    if t.text == "("
        && matches!(prev.kind, TokKind::Word | TokKind::QuotedIdent)
        && prev.word_upper().is_none_or(|u| !CLAUSE_LEAD.contains(&u.as_str()) && u != "IN")
    {
        return false;
    }
    true
}

/// The next significant WORD after `idx`, uppercased.
fn next_word(sig: &[&Tok], idx: usize) -> Option<String> {
    sig.get(idx + 1).and_then(|t| t.word_upper())
}

#[cfg(test)]
mod statement_kind_tests {
    use super::statement_kind;

    #[test]
    fn it_names_the_leading_keyword() {
        assert_eq!(statement_kind("select 1"), "SELECT");
        assert_eq!(statement_kind("  
	UPDATE t SET a = 1"), "UPDATE");
        assert_eq!(statement_kind("-- a note
/* another */ insert into t values (1)"), "INSERT");
    }

    /// The property the diagnostic log depends on: nothing the user typed
    /// can come out of this function.
    #[test]
    fn an_unrecognised_statement_is_other_and_never_the_users_text() {
        for sql in [
            "",
            "   ",
            "-- only a comment",
            "'a string literal'",
            "\"quoted ident\"",
            "s3cret_table_name",
            "sp_who2",
        ] {
            let k = statement_kind(sql);
            assert!(
                !sql.to_uppercase().contains(k) || k == "OTHER",
                "{sql:?} leaked {k:?}"
            );
        }
        assert_eq!(statement_kind("s3cret_table_name"), "OTHER");
        assert_eq!(statement_kind("'literal'"), "OTHER");
    }

    #[test]
    fn a_password_in_an_update_cannot_reach_the_kind() {
        let k = statement_kind("UPDATE users SET pw = 'hunter2' WHERE id = 1");
        assert_eq!(k, "UPDATE");
        assert!(!k.contains("hunter2"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE safety property: formatting may change whitespace and keyword
    /// case, and nothing else. If this fails, the formatter is corrupting
    /// user SQL — fix the rule, never this test.
    fn assert_token_preserving(sql: &str, dialect: Dialect) {
        let strip = |s: &str| -> Vec<(TokKind, String)> {
            lex(s, dialect)
                .into_iter()
                .filter(|t| t.kind != TokKind::Ws)
                .map(|t| {
                    // Keyword case is allowed to change; every other kind's
                    // text must survive byte for byte.
                    let text = if t.kind == TokKind::Word && is_keyword(&t.text, dialect) {
                        t.text.to_uppercase()
                    } else {
                        t.text
                    };
                    (t.kind, text)
                })
                .collect()
        };
        let out = format_sql(sql, dialect);
        assert_eq!(strip(sql), strip(&out), "formatting changed the tokens\n--- in\n{sql}\n--- out\n{out}");
    }

    #[test]
    fn formatting_preserves_every_significant_token() {
        for (sql, d) in [
            ("select a,b from t where x=1", Dialect::Postgres),
            ("SELECT * FROM [dbo].[Order Lines] WHERE [x]=N'ř'", Dialect::Mssql),
            ("select $$a'b$$, e'x\\'y' from t", Dialect::Postgres),
            ("select \"Odd\"\"Name\" from t -- trailing\n", Dialect::Postgres),
            ("select /* a /* nested */ b */ 1", Dialect::Postgres),
            ("select `q` from t", Dialect::Sqlite),
            ("select count(*) from t group by a order by b", Dialect::Postgres),
            ("insert into t (a,b) values (1,2);select 1;", Dialect::Postgres),
        ] {
            assert_token_preserving(sql, d);
        }
    }

    #[test]
    fn keywords_are_uppercased_and_identifiers_are_not() {
        let out = format_sql("select id from users", Dialect::Postgres);
        assert!(out.contains("SELECT"), "{out}");
        assert!(out.contains("FROM"), "{out}");
        assert!(out.contains("id"), "identifier case must survive: {out}");
        assert!(out.contains("users"), "{out}");
    }

    /// A quoted identifier's case is what makes it resolve — folding it
    /// would change which object the SQL names.
    #[test]
    fn a_quoted_identifier_is_never_recased_even_when_it_spells_a_keyword() {
        let out = format_sql("select \"select\" from t", Dialect::Postgres);
        assert!(out.contains("\"select\""), "{out}");
    }

    #[test]
    fn clauses_go_on_their_own_lines() {
        let out = format_sql("select a from t where x=1", Dialect::Postgres);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "SELECT a");
        assert_eq!(lines[1], "FROM t");
        assert_eq!(lines[2], "WHERE x = 1");
    }

    #[test]
    fn a_join_modifier_does_not_break_the_line_twice() {
        let out = format_sql("select 1 from a left join b on a.id=b.id", Dialect::Postgres);
        assert!(out.contains("LEFT JOIN b ON a.id = b.id"), "{out}");
    }

    /// `ORDER` and `GROUP` are clause leads only before `BY`; both are also
    /// perfectly ordinary column names.
    #[test]
    fn order_as_a_column_name_does_not_start_a_line() {
        let out = format_sql("select order from t", Dialect::Postgres);
        // ORDER uppercases (it IS a keyword); what this pins is that it
        // does not START A LINE without a following BY.
        assert_eq!(out.lines().next(), Some("SELECT ORDER"), "{out}");
    }

    #[test]
    fn top_level_commas_break_but_commas_inside_parens_do_not() {
        let out = format_sql("select a, b from t", Dialect::Postgres);
        assert!(out.contains("SELECT a,\n  b"), "{out}");
        let call = format_sql("select coalesce(a, b) from t", Dialect::Postgres);
        assert!(call.contains("COALESCE(a, b)") || call.contains("coalesce(a, b)"), "{call}");
    }

    #[test]
    fn a_line_comment_never_swallows_the_code_after_it() {
        let out = format_sql("select 1 -- note\n, 2", Dialect::Postgres);
        let after: Vec<&str> = out.lines().skip_while(|l| !l.contains("-- note")).collect();
        assert!(after.len() > 1, "nothing followed the comment: {out}");
        assert!(out.contains("2"), "{out}");
    }

    #[test]
    fn statements_are_separated_by_a_blank_line() {
        let out = format_sql("select 1;select 2;", Dialect::Postgres);
        assert!(out.contains(";\n\nSELECT 2"), "{out}");
    }

    /// The per-dialect half of the user's request. `TOP` is T-SQL; against
    /// Postgres the same word is an ordinary identifier.
    #[test]
    fn keyword_sets_differ_by_dialect() {
        assert!(is_keyword("top", Dialect::Mssql));
        assert!(!is_keyword("top", Dialect::Postgres));
        assert!(is_keyword("ilike", Dialect::Postgres));
        assert!(!is_keyword("ilike", Dialect::Mssql));
        assert!(is_keyword("autoincrement", Dialect::Sqlite));
        assert!(!is_keyword("autoincrement", Dialect::Postgres));
        // DuckDB rides the Postgres set — there is no fourth Dialect.
        assert!(is_keyword("qualify", Dialect::Postgres));
        // Shared vocabulary is shared.
        for d in [Dialect::Postgres, Dialect::Mssql, Dialect::Sqlite] {
            assert!(is_keyword("select", d));
        }
    }

    #[test]
    fn the_lexer_is_total_every_byte_lands_in_exactly_one_token() {
        for (sql, d) in [
            ("select 'unterminated", Dialect::Postgres),
            ("select [unterminated", Dialect::Mssql),
            ("select /* unterminated", Dialect::Postgres),
            ("select $$unterminated", Dialect::Postgres),
            ("select 1 -- x", Dialect::Postgres),
            ("", Dialect::Postgres),
            ("   ", Dialect::Postgres),
        ] {
            let joined: String = lex(sql, d).into_iter().map(|t| t.text).collect();
            assert_eq!(joined, sql, "lexer lost or duplicated text for {sql:?}");
        }
    }

    /// Brackets are MSSQL/SQLite quoting; in Postgres the same character
    /// opens an array subscript and must stay punctuation.
    #[test]
    fn brackets_are_identifiers_only_where_the_dialect_says_so() {
        let ms = lex("[a b]", Dialect::Mssql);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].kind, TokKind::QuotedIdent);
        let pg = lex("[a b]", Dialect::Postgres);
        assert!(pg.len() > 1, "postgres must not treat [..] as one identifier: {pg:?}");
    }

    #[test]
    fn an_n_prefixed_literal_stays_one_token() {
        let t = lex("N'ř'", Dialect::Mssql);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, TokKind::Str);
        assert_eq!(t[0].text, "N'ř'");
    }

    #[test]
    fn formatting_is_idempotent() {
        for (sql, d) in [
            ("select a,b from t where x=1 order by a", Dialect::Postgres),
            ("SELECT TOP 10 * FROM [dbo].[T]", Dialect::Mssql),
            ("select 1;select 2;", Dialect::Postgres),
        ] {
            let once = format_sql(sql, d);
            let twice = format_sql(&once, d);
            assert_eq!(once, twice, "second pass changed the text\n{once}\n---\n{twice}");
        }
    }

    #[test]
    fn empty_and_whitespace_only_input_do_not_panic() {
        assert_eq!(format_sql("", Dialect::Postgres), "\n");
        assert_eq!(format_sql("   \n\t", Dialect::Postgres), "\n");
    }
}
