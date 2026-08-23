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
//!
//! **Beyond `guards::tokenize`: PostgreSQL dollar-quoted strings
//! (`$tag$ ... $tag$`, tag optional).** `guards::tokenize` documents not
//! recognizing these as a known, accepted limitation -- because for its two
//! callers (`is_read_statement`/`apply_auto_limit`) an unrecognized
//! dollar-quote body only ever fails *safe* (a write keyword inside the body
//! false-positives as a bare top-level token and gets rejected; a limiting
//! keyword inside the body false-positives and suppresses an optimization).
//! For this module the same gap fails *unsafe*: a `:name`-shaped token
//! inside a dollar-quoted function/procedure body (e.g.
//! `CREATE FUNCTION ... AS $$ ... :bonus ... $$`) would be misdetected as a
//! live parameter, the values dialog would open uninvited, and substitution
//! would splice a literal into the middle of the function body -- corrupting
//! it. So this scanner, unlike `tokenize`, DOES track dollar-quoted spans as
//! a fifth "in a construct" state: their entire content (including anything
//! that looks like a string/comment/`:name`) is opaque literal text, and an
//! unterminated one fails closed exactly like the other four constructs.

/// One event produced while scanning `sql`: either a run of literal text to
/// copy verbatim, or a recognized `:name` parameter occurrence.
enum ScanEvent<'a> {
    Literal(&'a str),
    Param(String),
}

/// Shared scanner: walks `sql` once, tracking the same four "in a construct"
/// states as `guards::tokenize` (single-quoted string, double-quoted
/// identifier, line comment, nested block comment via a depth counter) plus
/// a fifth, dollar-quoted-string state this module adds beyond `tokenize`
/// (see module doc), and invokes `on_event` for each literal run /
/// recognized param in order.
///
/// Outside all five states: a `:` immediately followed by
/// `[A-Za-z_][A-Za-z0-9_]*` is a parameter (name = the identifier); a `:`
/// immediately followed by `:` is an inert 2-char `::` token; a `:`
/// immediately followed by `=` is an inert 2-char `:=` token; any other bare
/// `:` is not special and is just ordinary text. A `$` opens a dollar-quoted
/// span only if followed by a valid PostgreSQL tag shape (empty, or
/// `[A-Za-z_][A-Za-z0-9_]*`) and then another `$`; otherwise (e.g. `$1`
/// positional params, `a$b` identifiers) it's an ordinary character.
///
/// Returns `false` if the input ends inside an open construct (unterminated
/// string/quoted-ident/comment/dollar-quote) -- callers must treat that as
/// fail-closed.
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
    // `Some(tag)` while inside a dollar-quoted span opened with that tag
    // (empty `Vec` for the untagged `$$...$$` form). Only the exact `$tag$`
    // closer ends it -- a different tag's opener/closer sequence inside is
    // just content (PostgreSQL dollar-quotes don't nest).
    let mut dollar_tag: Option<Vec<u8>> = None;

    macro_rules! flush_literal {
        ($end:expr) => {
            if literal_start < $end {
                on_event(ScanEvent::Literal(&sql[literal_start..$end]));
            }
        };
    }

    while i < len {
        let c = bytes[i];

        if let Some(tag) = &dollar_tag {
            if c == b'$' {
                let tag_len = tag.len();
                if i + 1 + tag_len + 1 <= len
                    && &bytes[i + 1..i + 1 + tag_len] == tag.as_slice()
                    && bytes[i + 1 + tag_len] == b'$'
                {
                    i += 1 + tag_len + 1;
                    dollar_tag = None;
                    continue;
                }
            }
            i += 1;
            continue;
        }

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

        if c == b'$' {
            // Try to parse a dollar-quote opener: `$` + tag + `$`, where
            // tag is empty or `[A-Za-z_][A-Za-z0-9_]*`. Anything else
            // (`$1` positional param, `a$b` identifier char, a lone `$`) is
            // an ordinary character -- do not enter the dollar-quote state.
            let mut j = i + 1;
            let tag_start = j;
            if j < len {
                let first = bytes[j];
                if first.is_ascii_alphabetic() || first == b'_' {
                    j += 1;
                    while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                        j += 1;
                    }
                }
            }
            if j < len && bytes[j] == b'$' {
                dollar_tag = Some(bytes[tag_start..j].to_vec());
                i = j + 1;
                continue;
            }
            // Not a valid opener shape -- ordinary `$`.
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

    !(in_single_string
        || in_double_ident
        || in_line_comment
        || block_comment_depth > 0
        || dollar_tag.is_some())
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

    // --- dollar-quoted strings (review round 1: fail-unsafe gap) ---

    #[test]
    fn ignores_param_inside_untagged_dollar_quote() {
        assert_eq!(find_params("$$ :a $$"), Some(vec![]));
    }

    #[test]
    fn ignores_param_inside_tagged_dollar_quote() {
        assert_eq!(find_params("$tag$ :a $tag$"), Some(vec![]));
    }

    #[test]
    fn finds_param_after_dollar_quote_closes() {
        assert_eq!(
            find_params("$$ :a $$ :b"),
            Some(vec!["b".to_string()])
        );
    }

    #[test]
    fn unterminated_dollar_quote_fails_closed() {
        assert_eq!(find_params("$$ :a"), None);
    }

    #[test]
    fn positional_param_does_not_open_dollar_quote() {
        assert_eq!(find_params("$1 + :a"), Some(vec!["a".to_string()]));
    }

    #[test]
    fn dollar_as_identifier_char_does_not_open_dollar_quote() {
        assert_eq!(find_params("a$b + :c"), Some(vec!["c".to_string()]));
    }

    #[test]
    fn substitute_leaves_dollar_quoted_span_byte_identical() {
        let out = substitute_params("$$ :a $$ :b", &mut |name| {
            assert_eq!(name, "b");
            "5".to_string()
        });
        assert_eq!(out, Some("$$ :a $$ 5".to_string()));
    }

    #[test]
    fn mismatched_inner_tag_is_just_content_only_matching_outer_closer_ends_it() {
        // No nesting semantics -- `$inner$` occurrences are opaque content;
        // only the exact `$outer$` closer ends the span, so `:x` (inside)
        // is never detected and `:y` (after) is the only live param.
        assert_eq!(
            find_params("$outer$ $inner$ :x $inner$ $outer$ :y"),
            Some(vec!["y".to_string()])
        );
    }

    #[test]
    fn empty_tag_body_containing_a_tagged_looking_sequence_is_still_unterminated() {
        // `$tag$` inside a `$$...$$` body is just content, not a closer for
        // the untagged form -- the span never closes, so this fails closed.
        assert_eq!(find_params("$$ :a $tag$"), None);
    }
}
