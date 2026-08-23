//! Tree-sitter SQL syntax highlighting (G6 T4).
//!
//! Pure module: parses raw `&str` and returns byte-range colored spans. The
//! only GPUI type touched is `gpui::Hsla` for the resolved color (design §1:
//! "parsing itself has no GPUI dependency, but `Hsla` color resolution
//! does"). No panics — a parse/query error degrades to no highlighting for
//! the affected sub-tree, never a crash.
//!
//! Grammar pin: `tree-sitter-sequel` 0.3.11 generates `LANGUAGE_VERSION` 14,
//! which is ABI-compatible with the `tree-sitter` 0.25 runtime this crate
//! depends on today. These two dependencies are versioned independently;
//! any future bump of either must be a deliberate check that the ABI
//! versions still line up, not an incidental `cargo update`.
//!
//! `T5` (the `sql_input.rs` editor integration) consumes `highlight()` and
//! `HighlightSpan` directly; `suppresses_completion` itself is read by T7
//! (the autocomplete seam), not by T5.

use std::ops::Range;
use std::sync::OnceLock;

use tree_sitter::StreamingIterator;

/// A single colored span of source text.
#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub color: gpui::Hsla,
    /// True for `string`/`comment` captures — doubles as T6/T7's
    /// autocomplete-suppression mask (design §2's trigger model: "cursor is
    /// not inside a string/comment span", reusing this module's
    /// already-computed spans rather than a second scanner).
    pub suppresses_completion: bool,
}

/// Vendored, trimmed, and corrected copy of `tree-sitter-sequel`'s bundled
/// `queries/highlights.scm`. The upstream query's numeric-literal
/// predicates use Lua-pattern syntax (`%d`), which the Rust `regex` crate
/// (what tree-sitter's `#match?` predicate actually evaluates against)
/// doesn't understand — `@number` never fires against the unmodified
/// bundled query, and `3.14`/`42` are captured only as `@string`. This copy
/// fixes the numeric regexes to real regex syntax; every other pattern's
/// node names are kept verbatim from the upstream file (plan Task 4, step
/// 1, point 2).
///
/// The decimal-point regex writes `\\.` (double backslash) in this Rust raw
/// string, not `\.`. That's deliberate: tree-sitter's own query-string
/// lexer strips backslash escapes it doesn't recognize, so a single `\.`
/// in the `.scm` source text would reach the `regex` engine as a bare `.`
/// wildcard — which matches things with no decimal point at all (e.g.
/// `8e2`). Writing `\\.` puts a recognized `\\` escape in the `.scm` text,
/// which the query lexer turns into a literal `\.` by the time the regex
/// engine sees it. A third pattern below deliberately number-colors
/// scientific notation (`8e2`, `1.5E-3`) using the same double-backslash
/// discipline.
const HIGHLIGHTS_SCM: &str = r#"
(literal) @string

((literal) @number
  (#match? @number "^[-+]?[0-9]+$"))

((literal) @number
  (#match? @number "^[-+]?[0-9]*\\.[0-9]+$"))

((literal) @number
  (#match? @number "^[-+]?[0-9]+(\\.[0-9]+)?[eE][-+]?[0-9]+$"))

(comment) @comment
(marginalia) @comment

(invocation
  (object_reference
    name: (identifier) @function.call))

(object_reference
  name: (identifier) @type)

[
  (keyword_select) (keyword_from) (keyword_where) (keyword_join) (keyword_on)
  (keyword_left) (keyword_right) (keyword_outer) (keyword_inner) (keyword_full)
  (keyword_group) (keyword_order) (keyword_by) (keyword_having) (keyword_limit)
  (keyword_offset) (keyword_insert) (keyword_into) (keyword_values) (keyword_update)
  (keyword_set) (keyword_delete) (keyword_and) (keyword_or) (keyword_not)
  (keyword_null) (keyword_is) (keyword_in) (keyword_like) (keyword_between)
  (keyword_as) (keyword_distinct) (keyword_case) (keyword_when) (keyword_then)
  (keyword_else) (keyword_end) (keyword_union) (keyword_create) (keyword_table)
  (keyword_alter) (keyword_drop) (keyword_index) (keyword_primary) (keyword_key)
  (keyword_foreign) (keyword_references) (keyword_view) (keyword_with)
  (keyword_begin) (keyword_commit) (keyword_rollback) (keyword_explain)
  (keyword_returning) (keyword_truncate) (keyword_declare) (keyword_execute)
  (keyword_analyze) (keyword_true) (keyword_false)
] @keyword

[
  (keyword_int) (keyword_smallint) (keyword_bigint) (keyword_tinyint)
  (keyword_decimal) (keyword_numeric) (keyword_float) (keyword_double)
  (keyword_real) (keyword_char) (keyword_varchar) (keyword_nvarchar)
  (keyword_text) (keyword_string) (keyword_boolean) (keyword_date)
  (keyword_datetime) (keyword_timestamp) (keyword_uuid) (keyword_json)
  (keyword_jsonb)
] @type.builtin
"#;

/// (priority, color) — priority resolves same-range capture collisions (a
/// single node can legitimately satisfy two patterns at once, e.g. a
/// numeric literal gets both the unconditional `@string` pattern and the
/// predicate-gated `@number` pattern); higher priority wins.
fn color_for_capture(name: &str) -> Option<(u8, gpui::Hsla)> {
    match name {
        "keyword" => Some((1, gpui::rgb(0xcba6f7).into())), // mauve
        "string" => Some((1, gpui::rgb(0xa6e3a1).into())),  // green
        "comment" => Some((1, gpui::rgb(0x6c7086).into())), // overlay gray
        "type" | "type.builtin" => Some((1, gpui::rgb(0x94e2d5).into())), // teal
        "number" => Some((2, gpui::rgb(0xfab387).into())),  // peach — outranks "string"
        "function.call" => Some((2, gpui::rgb(0x89b4fa).into())), // blue — outranks "type"
        _ => None,
    }
}

/// Compiled once and reused for the process lifetime. `Query::new` against
/// this vendored, fixed source takes ~35ms — recompiling it on every
/// `highlight()` call (as this module originally did) made every keystroke
/// pay that cost, versus ~0.07ms for the actual parse. `Language` is cached
/// alongside it since `Parser::set_language` needs a `&Language` per call
/// too.
///
/// `Option`, not the bare tuple: `Query::new` can in principle fail (a
/// malformed query source), and even though `HIGHLIGHTS_SCM` is a vendored
/// compile-time constant that compiles today, defensively keeping this
/// fallible (rather than `.expect`-ing inside the `OnceLock` initializer,
/// which would poison every future call with the same panic) keeps
/// `highlight()` total: a hypothetical future edit to `HIGHLIGHTS_SCM` that
/// breaks compilation degrades to "no highlighting", never a panic.
static QUERY: OnceLock<Option<(tree_sitter::Language, tree_sitter::Query)>> = OnceLock::new();

fn cached_query() -> Option<(&'static tree_sitter::Language, &'static tree_sitter::Query)> {
    QUERY
        .get_or_init(|| {
            let language: tree_sitter::Language = tree_sitter_sequel::LANGUAGE.into();
            match tree_sitter::Query::new(&language, HIGHLIGHTS_SCM) {
                Ok(query) => Some((language, query)),
                Err(_) => None,
            }
        })
        .as_ref()
        .map(|(language, query)| (language, query))
}

/// Full-buffer parse + highlights query. Infallible in this codebase's
/// usage: `tree_sitter::Parser::parse` only returns `None` on an explicit
/// cancellation flag this code never sets, so every call returns real
/// (possibly empty, possibly partially-degraded) spans — never panics, even
/// on T-SQL-only syntax or an unterminated comment. If the cached query
/// failed to compile (defensive only — see `cached_query`), this returns
/// empty spans rather than panicking.
pub fn highlight(text: &str) -> Vec<HighlightSpan> {
    let Some((language, query)) = cached_query() else {
        return Vec::new();
    };

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(language)
        .expect("grammar embedded at compile time, cannot fail at runtime");
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new(); // cancellation only, never hit here
    };

    // (range, priority, capture name, color) — Vec, not HashMap: buffer
    // sizes are small (interactive SQL) and preserving a stable order
    // simplifies testing; O(n) linear scan per capture is fine at this
    // scale. The capture name is kept (not just derived color/priority) so
    // `suppresses_completion` can be derived from the FINAL winning
    // capture per range after priority resolution, rather than from the
    // raw pre-resolution `@string`/`@comment` captures — a numeric literal
    // wins `@number` over `@string` by priority, and must not be marked as
    // suppressing completion just because it was also captured by the
    // unconditional `@string` pattern along the way.
    let mut spans: Vec<(Range<usize>, u8, &'static str, gpui::Hsla)> = Vec::new();

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let name = query.capture_names()[cap.index as usize];
            let range = cap.node.byte_range();
            let Some((priority, color)) = color_for_capture(name) else {
                continue;
            };
            if let Some(existing) = spans.iter_mut().find(|(r, _, _, _)| *r == range) {
                if priority >= existing.1 {
                    *existing = (range, priority, name, color);
                }
            } else {
                spans.push((range, priority, name, color));
            }
        }
    }

    spans
        .into_iter()
        .map(|(range, _, name, color)| HighlightSpan {
            range,
            color,
            suppresses_completion: matches!(name, "string" | "comment"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color_at(spans: &[HighlightSpan], byte: usize) -> Option<gpui::Hsla> {
        spans.iter().find(|s| s.range.contains(&byte)).map(|s| s.color)
    }

    #[test]
    fn keyword_gets_keyword_color() {
        let spans = highlight("SELECT 1");
        let select_color = color_at(&spans, 0);
        assert!(select_color.is_some());
    }

    #[test]
    fn string_gets_string_color() {
        let spans = highlight("SELECT 'x' FROM t");
        assert!(color_at(&spans, 7).is_some()); // inside 'x'
    }

    #[test]
    fn numeric_literal_prefers_number_color_over_string_color() {
        let sql = "SELECT 42 FROM t";
        let spans = highlight(sql);
        let number_color = color_at(&spans, 7).unwrap(); // "42"
        let string_only = color_at(&highlight("SELECT 'x' FROM t"), 7).unwrap();
        assert_ne!(number_color, string_only);
    }

    #[test]
    fn function_call_prefers_function_color_over_type_color() {
        let spans = highlight("SELECT COUNT(x) FROM t");
        let count_color = color_at(&spans, 7).unwrap(); // "COUNT"
        let bare_table_color = color_at(&highlight("SELECT 1 FROM t"), 14).unwrap(); // "t"
        assert_ne!(count_color, bare_table_color);
    }

    #[test]
    fn line_comment_gets_comment_color_and_suppresses_completion() {
        let spans = highlight("SELECT 1 -- a note");
        let comment_span = spans.iter().find(|s| s.range.contains(&11)).unwrap();
        assert!(comment_span.suppresses_completion);
    }

    #[test]
    fn string_span_suppresses_completion_keyword_span_does_not() {
        let spans = highlight("SELECT 'x' FROM t");
        let string_span = spans.iter().find(|s| s.range.contains(&7)).unwrap();
        assert!(string_span.suppresses_completion);
        let keyword_span = spans.iter().find(|s| s.range.contains(&0)).unwrap();
        assert!(!keyword_span.suppresses_completion);
    }

    #[test]
    fn tsql_only_syntax_degrades_without_panicking_and_keeps_partial_highlighting() {
        // T-SQL's TOP against the generic grammar produces an ERROR node;
        // must not panic, and SELECT itself must still be colored.
        let spans = highlight("SELECT TOP 10 * FROM users");
        assert!(color_at(&spans, 0).is_some());
    }

    #[test]
    fn unterminated_block_comment_does_not_panic_and_keeps_partial_highlighting() {
        let spans = highlight("SELECT 1 /* unterminated");
        assert!(color_at(&spans, 0).is_some());
    }

    #[test]
    fn empty_text_returns_no_spans_without_panicking() {
        assert_eq!(highlight(""), Vec::new());
    }

    fn keyword_color() -> gpui::Hsla {
        color_for_capture("keyword").unwrap().1
    }

    fn number_color() -> gpui::Hsla {
        color_for_capture("number").unwrap().1
    }

    #[test]
    fn transaction_keywords_get_keyword_color() {
        // `BEGIN; COMMIT; ROLLBACK;` (issuing both COMMIT and ROLLBACK for
        // the same transaction) isn't accepted by the grammar's
        // `transaction` rule — it parses `BEGIN; COMMIT;` as a
        // `(transaction (keyword_begin) (keyword_commit))` and then hits an
        // `(ERROR)` node for the trailing `ROLLBACK;`, in which ROLLBACK
        // isn't even tokenized as `keyword_rollback` (confirmed via
        // `tree.root_node().to_sexp()`). BEGIN/COMMIT and BEGIN/ROLLBACK
        // are each independently valid, so this test covers all three
        // words via two separately-valid statements instead.
        let begin_commit = highlight("BEGIN; COMMIT;");
        assert_eq!(color_at(&begin_commit, 0), Some(keyword_color())); // BEGIN
        assert_eq!(color_at(&begin_commit, 7), Some(keyword_color())); // COMMIT

        let begin_rollback = highlight("BEGIN; ROLLBACK;");
        assert_eq!(color_at(&begin_rollback, 0), Some(keyword_color())); // BEGIN
        assert_eq!(color_at(&begin_rollback, 7), Some(keyword_color())); // ROLLBACK
    }

    #[test]
    fn explain_keyword_gets_keyword_color() {
        let spans = highlight("EXPLAIN SELECT 1");
        assert_eq!(color_at(&spans, 0), Some(keyword_color())); // EXPLAIN
        assert_eq!(color_at(&spans, 8), Some(keyword_color())); // SELECT
    }

    #[test]
    fn remaining_added_keywords_get_colored() {
        // Covers the rest of MAJOR 2's added node types with syntax the
        // grammar actually accepts (`keyword_declare`/`keyword_execute`
        // only appear deep inside function-body/trigger productions that
        // aren't worth constructing here; they're still verified present
        // in the grammar's node-types.json before being added to
        // HIGHLIGHTS_SCM, which is what actually protects against the
        // whole-query-compile-failure trap).
        let explain_analyze = highlight("EXPLAIN ANALYZE SELECT 1");
        assert_eq!(color_at(&explain_analyze, 8), Some(keyword_color())); // ANALYZE

        let truncate = highlight("TRUNCATE TABLE t;");
        assert_eq!(color_at(&truncate, 0), Some(keyword_color())); // TRUNCATE

        let returning = highlight("INSERT INTO t VALUES (1) RETURNING id;");
        assert_eq!(color_at(&returning, 26), Some(keyword_color())); // RETURNING

        // `true`/`false` literals: the grammar wraps `keyword_true`/
        // `keyword_false` inside a `literal` node at the same byte range,
        // so `@string` (unconditional) and `@keyword` both capture it;
        // either color is a real, deliberate color (not "uncolored"),
        // which is all this regression needs to confirm.
        let bools = highlight("SELECT true, false;");
        assert!(color_at(&bools, 7).is_some()); // true
        assert!(color_at(&bools, 13).is_some()); // false
    }

    #[test]
    fn select_1_returns_non_empty_spans() {
        // Guards against the whole-query-compile-failure trap: if a single
        // node name added to HIGHLIGHTS_SCM doesn't exist in the grammar,
        // `Query::new` fails for the ENTIRE query, and with the OnceLock
        // cache's defensive fallback, highlight() would silently return an
        // empty `Vec` for every input, forever, without panicking or
        // logging.
        let spans = highlight("SELECT 1");
        assert!(!spans.is_empty());
    }

    #[test]
    fn plain_integer_gets_number_color() {
        let spans = highlight("SELECT 42 FROM t");
        assert_eq!(color_at(&spans, 7), Some(number_color()));
    }

    #[test]
    fn decimal_literal_gets_number_color() {
        let spans = highlight("SELECT 3.14 FROM t");
        assert_eq!(color_at(&spans, 7), Some(number_color()));
    }

    #[test]
    fn scientific_notation_gets_number_color() {
        // Regression for the bare-`.`-wildcard bug: under the old
        // (unescaped) pattern this matched by accident because `.` matched
        // any character; under the fix it matches on purpose via the
        // dedicated scientific-notation pattern.
        let spans = highlight("SELECT +8e2 FROM t");
        assert_eq!(color_at(&spans, 7), Some(number_color()));
    }

    #[test]
    fn dotless_text_after_a_digit_is_not_dragged_in_by_the_old_wildcard_bug() {
        // Per the plan: assert `SELECT 8x2` does NOT get number color for
        // the "x2" tail, IF the grammar tokenizes `8x2` as one `literal`.
        // Verified (via `tree.root_node().to_sexp()`) that it does not:
        // `8x2` lexes as `(literal)` covering only "8", followed by a
        // separate `alias: (identifier)` covering "x2" (SQL's implicit
        // `<expr> <alias>` form) — never a single "8x2" literal token. So
        // the positive half of this regression doesn't apply to this
        // grammar; per the plan, that half is dropped and noted here
        // rather than asserted against a case that can't occur.
        //
        // What *is* still asserted: the leading digit "8" is its own
        // `literal` node and must get number color under the fixed regex
        // (same as `plain_integer_gets_number_color`), and the "x2" alias
        // identifier must NOT get number color — this is the closest real
        // analogue to the old bug (a bare `.` wildcard bleeding a number
        // match into adjacent non-numeric text) that this grammar allows.
        let spans = highlight("SELECT 8x2 FROM t");
        assert_eq!(color_at(&spans, 7), Some(number_color())); // "8"
        assert_ne!(color_at(&spans, 8), Some(number_color())); // "x2"
    }

    #[test]
    fn repeated_highlight_calls_use_the_cached_query_and_agree() {
        // Exercises the `OnceLock`-cached path: the first call initializes
        // the cache, the second reuses it. Both must produce identical
        // spans — caching must be purely a performance change, never an
        // observable one.
        let sql = "SELECT 42, 'x' FROM t WHERE t.id = 1 -- note";
        let first = highlight(sql);
        let second = highlight(sql);
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn numeric_literal_does_not_suppress_completion() {
        let spans = highlight("SELECT 42");
        let number_span = spans.iter().find(|s| s.range.contains(&7)).unwrap();
        assert!(!number_span.suppresses_completion);
    }
}
