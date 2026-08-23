//! `:name` parameter scanning and substitution for the SQL editor.
//!
//! Mirrors `guards::tokenize`'s state machine (same four "in a construct"
//! states: single-quoted string, double-quoted identifier, line comment,
//! nested block comment) but tracks byte positions instead of discarding
//! punctuation, since finding/substituting `:name` occurrences requires
//! knowing exactly where they are. This is a deliberate duplication, not a
//! refactor of `guards::tokenize` -- see that function's module doc and the
//! design doc (g6-editor-pro-design.md §3) for the rationale: `tokenize`
//! already serves two safety-critical callers and retrofitting
//! position-preserving colon-detection into it risks regressing either.
//!
//! Fail-closed: an unterminated string/quoted-ident/comment makes both
//! [`find_params`] and [`substitute_params`] return `None`, same contract as
//! `guards::tokenize`.

/// One event produced while scanning `sql`: either a run of literal text to
/// copy verbatim, or a recognized `:name` parameter occurrence.
enum ScanEvent<'a> {
    Literal(&'a str),
    Param(String),
}

/// Shared scanner: walks `sql` once, tracking the same four "in a construct"
/// states as `guards::tokenize` (single-quoted string, double-quoted
/// identifier, line comment, nested block comment via a depth counter), and
/// invokes `on_event` for each literal run / recognized param in order.
///
/// Outside all four states: a `:` immediately followed by
/// `[A-Za-z_][A-Za-z0-9_]*` is a parameter (name = the identifier); a `:`
/// immediately followed by `:` is an inert 2-char `::` token; a `:`
/// immediately followed by `=` is an inert 2-char `:=` token; any other bare
/// `:` is not special and is just ordinary text.
///
/// Returns `false` if the input ends inside an open construct (unterminated
/// string/quoted-ident/comment) -- callers must treat that as fail-closed.
fn scan<'a>(sql: &'a str, mut on_event: impl FnMut(ScanEvent<'a>)) -> bool {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    // Start of the current pending literal run (flushed before each param
    // and at the end).
    let mut literal_start = 0usize;

    let mut in_single_string = false;
    let mut in_double_ident = false;
    let mut in_line_comment = false;
    let mut block_comment_depth: u32 = 0;

    macro_rules! flush_literal {
        ($end:expr) => {
            if literal_start < $end {
                on_event(ScanEvent::Literal(&sql[literal_start..$end]));
            }
        };
    }

    while i < len {
        let c = bytes[i];

        if in_single_string {
            if c == b'\'' {
                if i + 1 < len && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_string = false;
            }
            i += 1;
            continue;
        }

        if in_double_ident {
            if c == b'"' {
                if i + 1 < len && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_double_ident = false;
            }
            i += 1;
            continue;
        }

        if in_line_comment {
            if c == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if block_comment_depth > 0 {
            if c == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                block_comment_depth += 1;
                i += 2;
                continue;
            }
            if c == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                block_comment_depth -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if c == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if c == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            block_comment_depth = 1;
            i += 2;
            continue;
        }

        if c == b'\'' {
            in_single_string = true;
            i += 1;
            continue;
        }

        if c == b'"' {
            in_double_ident = true;
            i += 1;
            continue;
        }

        if c == b':' {
            // `::` -- inert 2-char cast token.
            if i + 1 < len && bytes[i + 1] == b':' {
                i += 2;
                continue;
            }
            // `:=` -- inert 2-char assignment token.
            if i + 1 < len && bytes[i + 1] == b'=' {
                i += 2;
                continue;
            }
            // `:name` -- identifier must start with [A-Za-z_].
            let name_start = i + 1;
            if name_start < len {
                let first = bytes[name_start];
                if first.is_ascii_alphabetic() || first == b'_' {
                    let mut j = name_start + 1;
                    while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                        j += 1;
                    }
                    flush_literal!(i);
                    let name = sql[name_start..j].to_string();
                    on_event(ScanEvent::Param(name));
                    i = j;
                    literal_start = i;
                    continue;
                }
            }
            // Bare `:` not followed by an identifier start -- ordinary text.
            i += 1;
            continue;
        }

        i += 1;
    }

    flush_literal!(len);

    !(in_single_string || in_double_ident || in_line_comment || block_comment_depth > 0)
}

/// Distinct `:name` parameter names in `sql`, in first-occurrence order,
/// scanning OUTSIDE single-quoted strings, double-quoted identifiers, `--`
/// line comments, and nested `/* */` block comments. `::` (Postgres cast)
/// and `:=` (assignment) are recognized as inert 2-char tokens and never
/// emit a param. `None` = fail-closed ("cannot determine safety" -- an
/// unterminated string/quoted-ident/comment), same contract as
/// `guards::tokenize`.
pub fn find_params(sql: &str) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let ok = scan(sql, |event| {
        if let ScanEvent::Param(name) = event {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    });
    if ok {
        Some(names)
    } else {
        None
    }
}

/// Same scanner as `find_params`, replacing every valid `:name` occurrence
/// with `value(name)`'s return value (all other text copied verbatim);
/// `None` on the same fail-closed condition. Shares one scanner
/// implementation with `find_params` so substitution can never target
/// different positions than what `find_params` detected.
pub fn substitute_params(sql: &str, value: &mut dyn FnMut(&str) -> String) -> Option<String> {
    let mut out = String::with_capacity(sql.len());
    let ok = scan(sql, |event| match event {
        ScanEvent::Literal(text) => out.push_str(text),
        ScanEvent::Param(name) => {
            let replacement = value(&name);
            out.push_str(&replacement);
        }
    });
    if ok {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_simple_params_in_order() {
        assert_eq!(
            find_params("SELECT * FROM t WHERE id = :id AND name = :name"),
            Some(vec!["id".to_string(), "name".to_string()])
        );
    }

    #[test]
    fn distinct_names_first_occurrence_order() {
        assert_eq!(
            find_params("WHERE a = :x OR b = :y OR c = :x"),
            Some(vec!["x".to_string(), "y".to_string()])
        );
    }

    #[test]
    fn no_params_is_some_empty_not_none() {
        assert_eq!(find_params("SELECT 1"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_single_quoted_string() {
        assert_eq!(find_params("SELECT ':id' FROM t"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_double_quoted_ident() {
        assert_eq!(find_params("SELECT \"a:b\" FROM t"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_line_comment() {
        assert_eq!(find_params("-- :id\nSELECT 1"), Some(vec![]));
    }

    #[test]
    fn ignores_params_inside_nested_block_comment() {
        // Outer /* opens depth 1, inner /* depth 2, first */ drops to
        // depth 1 (still commented -- PostgreSQL nesting semantics, same
        // as guards::tokenize), final */ drops to depth 0; only the
        // trailing `:c` is live.
        assert_eq!(
            find_params("/* :a /* :b */ still commented */ SELECT :c"),
            Some(vec!["c".to_string()])
        );
    }

    #[test]
    fn double_colon_cast_is_not_a_param() {
        assert_eq!(find_params("SELECT x::int FROM t"), Some(vec![]));
    }

    #[test]
    fn walrus_assignment_is_not_a_param() {
        assert_eq!(find_params("DO $$ BEGIN a := 1; END $$"), Some(vec![]));
    }

    #[test]
    fn colon_not_followed_by_identifier_start_is_not_a_param() {
        // A bare `:` followed by a digit, space, or end-of-string is never
        // a valid parameter name -- just an ordinary character.
        assert_eq!(find_params("LIMIT :1"), Some(vec![]));
        assert_eq!(find_params("a : b"), Some(vec![]));
        assert_eq!(find_params("trailing:"), Some(vec![]));
    }

    #[test]
    fn names_are_case_sensitive_and_distinct() {
        assert_eq!(
            find_params(":Id = :id"),
            Some(vec!["Id".to_string(), "id".to_string()])
        );
    }

    #[test]
    fn unterminated_single_string_fails_closed() {
        assert_eq!(find_params("SELECT ':id"), None);
    }

    #[test]
    fn unterminated_double_ident_fails_closed() {
        assert_eq!(find_params("SELECT \"a"), None);
    }

    #[test]
    fn unterminated_block_comment_fails_closed() {
        assert_eq!(find_params("SELECT 1 /* :id"), None);
    }

    #[test]
    fn unterminated_nested_block_comment_fails_closed() {
        assert_eq!(find_params("/* /* :id */ SELECT 1"), None);
    }

    // --- substitute_params ---

    #[test]
    fn substitute_replaces_every_occurrence() {
        let out = substitute_params("WHERE a = :x OR b = :x", &mut |name| {
            assert_eq!(name, "x");
            "'lit'".to_string()
        });
        assert_eq!(out, Some("WHERE a = 'lit' OR b = 'lit'".to_string()));
    }

    #[test]
    fn substitute_leaves_double_colon_and_walrus_untouched() {
        let out = substitute_params("x::int := :v", &mut |name| {
            assert_eq!(name, "v");
            "1".to_string()
        });
        assert_eq!(out, Some("x::int := 1".to_string()));
    }

    #[test]
    fn substitute_skips_strings_and_comments_like_find_params() {
        let out = substitute_params("SELECT ':id', :id -- :id\n", &mut |name| {
            assert_eq!(name, "id");
            "5".to_string()
        });
        assert_eq!(out, Some("SELECT ':id', 5 -- :id\n".to_string()));
    }

    #[test]
    fn substitute_fails_closed_on_unterminated_construct() {
        let out = substitute_params("SELECT ':id", &mut |_| "5".to_string());
        assert_eq!(out, None);
    }
}
