//! Tree-sitter SQL syntax highlighting (G6 T4).
//!
//! Pure module: parses raw `&str` and returns byte-range colored spans. The
//! only GPUI type touched is `gpui::Hsla` for the resolved color (design §1:
//! "parsing itself has no GPUI dependency, but `Hsla` color resolution
//! does"). No panics — a parse/query error degrades to no highlighting for
//! the affected sub-tree, never a crash.

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
/// fixes just the two numeric regexes to real regex syntax; every other
/// pattern's node names are kept verbatim from the upstream file (plan
/// Task 4, step 1, point 2).
const HIGHLIGHTS_SCM: &str = r#"
(literal) @string

((literal) @number
  (#match? @number "^[-+]?[0-9]+$"))

((literal) @number
  (#match? @number "^[-+]?[0-9]*\.[0-9]+$"))

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
