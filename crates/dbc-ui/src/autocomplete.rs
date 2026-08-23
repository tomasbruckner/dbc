//! Autocomplete candidate engine (design doc §2).
//!
//! Pure module: no GPUI dependency. Computes ranked keyword/table/column
//! candidates for the SQL editor's popup. Suppression (cursor inside a
//! string/comment span) is caller-supplied (`in_suppressed_span`) — T7 wires
//! it from `sql_highlight::HighlightSpan::suppresses_completion` (T4/T5);
//! this module never scans for strings/comments itself for that purpose.
//!
//! Wrong suggestions are worse than none: every ambiguous case (duplicate
//! alias bound to two different tables, an unresolvable `FROM (`/`JOIN (`
//! subquery, a bare table name ambiguous across schemas, a name shadowed by
//! a CTE) degrades to offering nothing rather than guessing.
//!
//! T6 (this file) is a parallel-batch task with no dependency on T7 (the
//! `AppView` seam that wires `candidates()`/`resolve_aliases()` into the
//! editor) — T7 (`main.rs`) is now that caller.

use dbc_core::SchemaSnapshot;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub enum CandidateKind {
    Keyword,
    Table,
    Column,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Inserted at the cursor.
    pub text: String,
    /// Shown in the popup (may carry a schema qualifier, e.g. `public.users`).
    pub label: String,
    pub kind: CandidateKind,
}

/// Identifier prefix ending exactly at `cursor` (walking backward over
/// alnum/`_`), plus the qualifier token immediately before a `.` (if any).
#[derive(Debug, Clone, PartialEq)]
pub struct CursorContext {
    pub prefix: String,
    pub qualifier: Option<String>,
}

/// v1 keyword list (design §2 — "a static list", extended slightly for
/// coverage consistent with `guards.rs`'s own recognized vocabulary).
pub const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "ON", "LEFT", "RIGHT", "INNER", "OUTER", "FULL", "GROUP",
    "BY", "ORDER", "LIMIT", "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE",
    "AND", "OR", "NOT", "NULL", "IS", "IN", "LIKE", "BETWEEN", "AS", "DISTINCT", "HAVING",
    "UNION", "CASE", "WHEN", "THEN", "ELSE", "END", "EXISTS", "ALL", "ANY", "WITH", "EXPLAIN",
    "SHOW", "CREATE", "ALTER", "DROP", "TABLE", "INDEX", "VIEW", "PRIMARY", "KEY", "FOREIGN",
    "REFERENCES", "DEFAULT", "CHECK", "UNIQUE",
];

/// Words that appear immediately after a table reference in a `FROM`/`JOIN`
/// clause but are NEVER a valid alias there, on top of everything `KEYWORDS`
/// already covers (a clause boundary like `WHERE`/`GROUP`/`JOIN` itself
/// already stops alias parsing via that list). These are join-syntax words
/// too narrow/rare to belong in the general keyword-completion list but that
/// `resolve_aliases`' "is the next token an alias or a clause keyword?"
/// check MUST recognize — review round 1, finding 1: `FROM t CROSS JOIN u`
/// was binding `CROSS` as an alias for `t` (same bug for `USING`) because
/// the old code only consulted `KEYWORDS`, which doesn't list them.
const ALIAS_STOPWORDS: &[&str] = &["CROSS", "NATURAL", "USING", "LATERAL"];

/// True if `word_upper` (already uppercased) can never be a table alias —
/// the union of `KEYWORDS` (general SQL vocabulary, doubles as clause
/// boundaries) and `ALIAS_STOPWORDS` (join-syntax words with no other
/// reason to be in the completion keyword list).
fn is_alias_stopword(word_upper: &str) -> bool {
    KEYWORDS.iter().any(|k| *k == word_upper) || ALIAS_STOPWORDS.iter().any(|k| *k == word_upper)
}

const MAX_CANDIDATES: usize = 20;

/// True if `c` can be part of an identifier prefix/qualifier token, per
/// `cursor_context`'s Unicode-aware walk (review round 3, MAJOR 3 —
/// distinct from `is_ident_start`/`is_ident_char` below, which stay
/// ASCII-only since they're only ever used to recognize ASCII SQL keywords
/// in `resolve_aliases`' scanner, a narrower job than "is this byte/char
/// part of whatever identifier the user is typing").
fn is_ident_char_unicode(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Walks backward from `end` over a contiguous run of `is_ident_char_unicode`
/// characters, returning the byte offset the run starts at (`end` itself if
/// the character immediately before it, if any, isn't part of an
/// identifier). Char-based (not byte-based) so a multi-byte identifier char
/// (`č`, `užc`, ...) is walked as ONE unit rather than stopping mid-character
/// — review round 3, MAJOR 3: the previous byte-only `is_ident_byte` walk
/// silently truncated a non-ASCII prefix (`čas` -> only `as`), which then
/// corrupted `completion_edit`'s replace-range math (`č` left in place AND
/// duplicated). This is the ONE shared place both the popup filter
/// (`candidates`, via this function) and `main.rs`'s `completion_edit`
/// (via `cursor_context`) derive the prefix range from, so both stay
/// consistent by construction.
fn walk_ident_prefix_start(text: &str, end: usize) -> usize {
    let mut start = end;
    for (i, c) in text[..end].char_indices().rev() {
        if !is_ident_char_unicode(c) {
            break;
        }
        start = i;
    }
    start
}

/// Walks backward from `cursor` over identifier characters (Unicode-aware —
/// see `walk_ident_prefix_start`) to find the partial token under the
/// cursor, and (if that token is preceded by a `.`) the qualifier token
/// before the dot.
pub fn cursor_context(text: &str, cursor: usize) -> CursorContext {
    let mut cursor = cursor.min(text.len());
    // A cursor mid-way through a multi-byte UTF-8 character (e.g. a
    // caller passing a stale/out-of-sync byte offset) would otherwise
    // panic on the `text[..]` slicing below — snap DOWN to the nearest
    // character boundary first, same "floor, don't round" convention as
    // `text_model.rs`'s grapheme-boundary snapping (review round 1, finding
    // 4). Once `cursor` sits on a character boundary, `walk_ident_prefix_start`
    // below only ever steps back whole characters at a time, so it can
    // never re-enter the middle of a multi-byte sequence either.
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }

    let start = walk_ident_prefix_start(text, cursor);
    let prefix = text[start..cursor].to_string();

    // `.` is always exactly 1 ASCII byte, so `start - 1` is always a valid
    // boundary immediately preceding `start`.
    let qualifier = if start > 0 && text.as_bytes()[start - 1] == b'.' {
        let dot = start - 1;
        let qbegin = walk_ident_prefix_start(text, dot);
        if qbegin < dot {
            Some(text[qbegin..dot].to_string())
        } else {
            None
        }
    } else {
        None
    };

    CursorContext { prefix, qualifier }
}

// --- Ranking ---

/// `(match_tier, case_tier)` — lower is a better match. `None` = excluded
/// (neither a prefix nor a substring match). Match tier: 0 = case-insensitive
/// prefix match, 1 = case-insensitive substring match. Case tier (within a
/// match tier): 0 = exact-case match, 1 = case-insensitive-only match. An
/// empty `prefix` matches everything trivially at the best tier (used for
/// Ctrl+Space's "full set" and no-context lookups).
fn match_score(candidate: &str, prefix: &str) -> Option<(u8, u8)> {
    if prefix.is_empty() {
        return Some((0, 0));
    }
    let cand_lower = candidate.to_lowercase();
    let prefix_lower = prefix.to_lowercase();
    if cand_lower.starts_with(&prefix_lower) {
        let case_tier = if candidate.starts_with(prefix) { 0 } else { 1 };
        Some((0, case_tier))
    } else if cand_lower.contains(&prefix_lower) {
        let case_tier = if candidate.contains(prefix) { 0 } else { 1 };
        Some((1, case_tier))
    } else {
        None
    }
}

fn distinct_schema_count(snapshot: &SchemaSnapshot) -> usize {
    let mut set = HashSet::new();
    for t in &snapshot.tables {
        set.insert(t.schema.clone());
    }
    set.len()
}

/// Sorts by (match_tier, case_tier, kind_tier, declaration_index) — a stable
/// sort so items tied on every ranking rule keep their original declared
/// order (e.g. `KEYWORDS`' authored order, which happens to lead with the
/// most common clauses) rather than an arbitrary/alphabetical shuffle that
/// would otherwise push common keywords out of the 20-item cap when the
/// match prefix is empty (Ctrl+Space full-set mode).
fn rank_and_cap(mut items: Vec<(u8, u8, u8, usize, Candidate)>) -> Vec<Candidate> {
    items.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));
    items.into_iter().take(MAX_CANDIDATES).map(|i| i.4).collect()
}

const KIND_TIER_SCHEMA_OBJECT: u8 = 0;
const KIND_TIER_KEYWORD: u8 = 1;

fn keyword_and_table_candidates(prefix: &str, snapshot: Option<&SchemaSnapshot>) -> Vec<Candidate> {
    let mut scored: Vec<(u8, u8, u8, usize, Candidate)> = Vec::new();

    for (idx, kw) in KEYWORDS.iter().enumerate() {
        if let Some((mt, ct)) = match_score(kw, prefix) {
            scored.push((
                mt,
                ct,
                KIND_TIER_KEYWORD,
                idx,
                Candidate { text: (*kw).to_string(), label: (*kw).to_string(), kind: CandidateKind::Keyword },
            ));
        }
    }

    if let Some(snapshot) = snapshot {
        let multi_schema = distinct_schema_count(snapshot) > 1;
        for (idx, t) in snapshot.tables.iter().enumerate() {
            if let Some((mt, ct)) = match_score(&t.name, prefix) {
                let label = if multi_schema {
                    match &t.schema {
                        Some(s) => format!("{s}.{}", t.name),
                        None => t.name.clone(),
                    }
                } else {
                    t.name.clone()
                };
                scored.push((
                    mt,
                    ct,
                    KIND_TIER_SCHEMA_OBJECT,
                    idx,
                    Candidate { text: t.name.clone(), label, kind: CandidateKind::Table },
                ));
            }
        }
    }

    rank_and_cap(scored)
}

/// Column candidates for a `qualifier.` (alias or bare table name) dot
/// completion. Empty if there's no snapshot, the alias scan is ambiguous,
/// the qualifier is shadowed by a CTE name, the qualifier doesn't resolve to
/// a known table, or (for a schema-less/bare reference) the table name is
/// ambiguous across more than one schema in the snapshot.
fn column_candidates(text: &str, qualifier: &str, prefix: &str, snapshot: Option<&SchemaSnapshot>) -> Vec<Candidate> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let Some(alias_map) = resolve_aliases(text) else {
        return Vec::new();
    };
    let qualifier_lower = qualifier.to_lowercase();
    let Some(target) = alias_map
        .iter()
        .find(|(k, _)| k.to_lowercase() == qualifier_lower)
        .map(|(_, v)| v.clone())
    else {
        return Vec::new();
    };

    // CTE shadowing (review round 1, finding 3): `resolve_aliases` is a
    // schema-blind text scan — it happily self-maps `FROM x` even when `x`
    // is actually a CTE name, not a real table. A CTE's result shape isn't
    // modeled by `SchemaSnapshot` at all, so if `target`'s name is ALSO
    // bound by a `WITH ... AS (...)` in this query, the CTE shadows
    // whatever same-named real table might exist — offer nothing rather
    // than that real table's (likely wrong) columns.
    let cte_scan_masked = mask_for_cte_scan(text);
    let cte_scan_chars: Vec<char> = cte_scan_masked.chars().collect();
    if cte_names(&cte_scan_chars).contains(&target.name.to_uppercase()) {
        return Vec::new();
    }

    let table = match &target.schema {
        // Schema-qualified reference (`hr.users u`) — match schema AND name
        // exactly, and require EXACTLY ONE such row (same "ambiguous ->
        // offer nothing" invariant as the bare-name path below; review
        // round 2 nit — a `.find()` here would silently pick the first of
        // two literally-duplicate schema+name rows in a corrupt snapshot).
        // Never fall back to a different schema's same-named table (review
        // round 1, finding 2).
        Some(schema) => {
            let mut matching = snapshot.tables.iter().filter(|t| {
                t.name.eq_ignore_ascii_case(&target.name)
                    && t.schema.as_deref().map(|s| s.eq_ignore_ascii_case(schema)).unwrap_or(false)
            });
            match (matching.next(), matching.next()) {
                (Some(t), None) => Some(t),
                _ => None,
            }
        }
        // Bare (schema-less) reference — resolve only if exactly one table
        // in the snapshot has this name; more than one (across different
        // schemas) is ambiguous, same "offer nothing" invariant as alias
        // ambiguity (review round 1, finding 2).
        None => {
            let mut matching = snapshot.tables.iter().filter(|t| t.name.eq_ignore_ascii_case(&target.name));
            match (matching.next(), matching.next()) {
                (Some(t), None) => Some(t),
                _ => None,
            }
        }
    };
    let Some(table) = table else {
        return Vec::new();
    };

    let mut scored: Vec<(u8, u8, u8, usize, Candidate)> = Vec::new();
    for (idx, col) in table.columns.iter().enumerate() {
        if let Some((mt, ct)) = match_score(&col.name, prefix) {
            scored.push((
                mt,
                ct,
                KIND_TIER_SCHEMA_OBJECT,
                idx,
                Candidate { text: col.name.clone(), label: col.name.clone(), kind: CandidateKind::Column },
            ));
        }
    }
    rank_and_cap(scored)
}

/// True if the identifier prefix `cursor_context` would compute at `cursor`
/// is immediately preceded by an (unclosed, from this local check's point of
/// view) `"` — review round 3, MAJOR 2: tree-sitter's `suppresses_completion`
/// mask only covers string/comment captures, NOT double-quoted identifiers,
/// and `is_ident_char_unicode` doesn't treat `"` as an identifier char
/// either — so typing inside `SELECT "Us` would otherwise both open the
/// popup AND (on accept) leave the stray opening quote in place while
/// inserting an unquoted candidate (`SELECT "Users`, no closing quote — or,
/// for a force-triggered accept via `completion_edit`, `SELECT "SELECT`).
/// v1 posture: suppress entirely (never open, so never accept either)
/// rather than attempting a full quoted-identifier grammar. Pure and
/// directly unit-tested — the ONE seam `candidates` (both the typing-
/// trigger and Ctrl+Space paths) checks it through.
fn prefix_preceded_by_open_quote(text: &str, cursor: usize) -> bool {
    let ctx = cursor_context(text, cursor);
    let prefix_start = cursor.min(text.len()).saturating_sub(ctx.prefix.len());
    prefix_start > 0 && text.as_bytes().get(prefix_start - 1) == Some(&b'"')
}

/// Ranked candidates (design §2's ranking rules; capped at 20).
/// `in_suppressed_span` is caller-supplied (T7 wires it from
/// `SqlInput.highlights`' `suppresses_completion` flags, T4/T5) — this
/// module never needs its own string/comment scan.
pub fn candidates(
    text: &str,
    cursor: usize,
    snapshot: Option<&SchemaSnapshot>,
    force: bool, // true = Ctrl+Space, empty-prefix, full set
    in_suppressed_span: bool,
) -> Vec<Candidate> {
    if in_suppressed_span {
        return Vec::new();
    }

    // Checked unconditionally (even under `force`, review round 3 MAJOR 2):
    // a Ctrl+Space accept still goes through `completion_edit`'s REAL
    // `cursor_context`-derived range, not `force`'s empty-prefix bypass, so
    // sitting right after an unclosed `"` is just as unsafe to accept into
    // under force-trigger as under the typing trigger.
    if prefix_preceded_by_open_quote(text, cursor) {
        return Vec::new();
    }

    // Ctrl+Space force-opens "regardless of context" (design §2) — bypass
    // both the typed prefix and any dot-qualifier, always offering the full
    // keyword+table set.
    let (prefix, qualifier) = if force {
        (String::new(), None)
    } else {
        let ctx = cursor_context(text, cursor);
        (ctx.prefix, ctx.qualifier)
    };

    if let Some(qualifier) = qualifier {
        return column_candidates(text, &qualifier, &prefix, snapshot);
    }

    keyword_and_table_candidates(&prefix, snapshot)
}

// --- Alias resolution ---

/// A resolved `alias -> table` (or bare `table -> table`) binding,
/// preserving the schema qualifier the user typed, if any. Needed so
/// `hr.users u` and `public.users u` resolve to DIFFERENT tables when both
/// schemas are present in the snapshot (review round 1, finding 2 — the
/// previous schema-blind lookup returned whichever same-named table
/// happened to come first).
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

fn table_ref_eq_ignore_case(a: &TableRef, b: &TableRef) -> bool {
    a.name.eq_ignore_ascii_case(&b.name)
        && match (&a.schema, &b.schema) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            (None, None) => true,
            _ => false,
        }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn skip_ws(chars: &[char], mut j: usize) -> usize {
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    j
}

/// Reads a single (non-dotted) identifier starting at `j`. Returns `None`
/// if `j` isn't at an identifier start.
fn read_word(chars: &[char], j: usize) -> Option<(String, usize)> {
    if j >= chars.len() || !is_ident_start(chars[j]) {
        return None;
    }
    let mut k = j;
    while k < chars.len() && is_ident_char(chars[k]) {
        k += 1;
    }
    Some((chars[j..k].iter().collect(), k))
}

/// Reads a possibly dot-chained identifier (`schema.table`) starting at
/// `j`. Returns `(schema, name, end_index)` — `schema` is the segment
/// before the LAST dot, if there was one (`schema.table` -> `(Some(
/// "schema"), "table")`), else `None` (`table` -> `(None, "table")`).
/// Three-or-more-segment chains (`db.schema.table`) collapse to the last
/// two segments — v1 doesn't model catalog-level qualification, only
/// schema.table, consistent with `SchemaSnapshot`'s own shape.
fn read_qualified_word(chars: &[char], j: usize) -> Option<(Option<String>, String, usize)> {
    let (mut segment, mut k) = read_word(chars, j)?;
    let mut prev_segment: Option<String> = None;
    loop {
        if k < chars.len() && chars[k] == '.' && k + 1 < chars.len() && is_ident_start(chars[k + 1]) {
            let (next_seg, next_k) = read_word(chars, k + 1)?;
            prev_segment = Some(segment);
            segment = next_seg;
            k = next_k;
        } else {
            break;
        }
    }
    Some((prev_segment, segment, k))
}

/// Inserts `key -> value`; returns `false` (conflict) if `key` is already
/// bound to a DIFFERENT value (case-insensitively) — the caller treats that
/// as whole-query ambiguity.
fn insert_alias(map: &mut HashMap<String, TableRef>, key: String, value: TableRef) -> bool {
    match map.get(&key) {
        Some(existing) if !table_ref_eq_ignore_case(existing, &value) => false,
        _ => {
            map.insert(key, value);
            true
        }
    }
}

/// Replaces single-quoted strings, double-quoted identifiers, `--` line
/// comments and `/* */` block comments with spaces (preserving newlines),
/// so `resolve_aliases`'s text scan never mistakes their contents for SQL
/// syntax. Best-effort: an unterminated construct is masked to end-of-input
/// rather than failing closed (this scanner's contract has no
/// `None`-for-parse-error case, only `None`-for-ambiguity).
///
/// NOT used for `cte_names` — see `mask_for_cte_scan` below, which needs a
/// different tradeoff (quoted identifier content stays visible).
fn mask_strings_and_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut block_depth: u32 = 0;

    while i < chars.len() {
        let c = chars[i];

        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }

        if block_depth > 0 {
            if c == '*' && chars.get(i + 1) == Some(&'/') {
                block_depth -= 1;
                out.push_str("  ");
                i += 2;
                continue;
            }
            if c == '/' && chars.get(i + 1) == Some(&'*') {
                block_depth += 1;
                out.push_str("  ");
                i += 2;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }

        if in_single {
            if c == '\'' {
                if chars.get(i + 1) == Some(&'\'') {
                    out.push_str("  ");
                    i += 2;
                    continue;
                }
                in_single = false;
                out.push(' ');
                i += 1;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }

        if in_double {
            if c == '"' {
                if chars.get(i + 1) == Some(&'"') {
                    out.push_str("  ");
                    i += 2;
                    continue;
                }
                in_double = false;
                out.push(' ');
                i += 1;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }

        // Not inside any construct.
        if c == '\'' {
            in_single = true;
            out.push(' ');
            i += 1;
            continue;
        }
        if c == '"' {
            in_double = true;
            out.push(' ');
            i += 1;
            continue;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            in_line_comment = true;
            out.push_str("  ");
            i += 2;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            block_depth = 1;
            out.push_str("  ");
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }

    out
}

/// Like `mask_strings_and_comments`, but PRESERVES double-quoted identifier
/// content (with the surrounding quote characters stripped, not appended to
/// the output) instead of blanking it, while still fully blanking
/// single-quoted strings and comments. Used ONLY by `cte_names` (review
/// round 2 fix): `mask_strings_and_comments` blanks `"orders"` to spaces
/// entirely, which made a quoted CTE name like `WITH "orders" AS (...)`
/// invisible to shadow detection — a real `orders` table would then
/// silently un-shadow the CTE and return the wrong columns. A `WITH`
/// keyword or CTE-shaped text that appears INSIDE a single-quoted string or
/// a comment must still never trigger (those regions stay blanked here,
/// same as the general mask), which is why this isn't as simple as "don't
/// mask anything" — only double-quoted identifier content gets the
/// quotes-stripped treatment.
fn mask_for_cte_scan(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut block_depth: u32 = 0;

    while i < chars.len() {
        let c = chars[i];

        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }

        if block_depth > 0 {
            if c == '*' && chars.get(i + 1) == Some(&'/') {
                block_depth -= 1;
                out.push_str("  ");
                i += 2;
                continue;
            }
            if c == '/' && chars.get(i + 1) == Some(&'*') {
                block_depth += 1;
                out.push_str("  ");
                i += 2;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }

        if in_single {
            if c == '\'' {
                if chars.get(i + 1) == Some(&'\'') {
                    out.push_str("  ");
                    i += 2;
                    continue;
                }
                in_single = false;
                out.push(' ');
                i += 1;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }

        if in_double {
            if c == '"' {
                if chars.get(i + 1) == Some(&'"') {
                    // Escaped quote inside the identifier — not a valid
                    // identifier character either way; drop it.
                    out.push_str("  ");
                    i += 2;
                    continue;
                }
                in_double = false;
                i += 1; // drop the closing quote, don't append it
                continue;
            }
            out.push(c); // preserve identifier content verbatim
            i += 1;
            continue;
        }

        // Not inside any construct.
        if c == '\'' {
            in_single = true;
            out.push(' ');
            i += 1;
            continue;
        }
        if c == '"' {
            in_double = true;
            i += 1; // drop the opening quote, don't append it
            continue;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            in_line_comment = true;
            out.push_str("  ");
            i += 2;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            block_depth = 1;
            out.push_str("  ");
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }

    out
}

/// `FROM <table> [AS] <alias>` / `JOIN <table> [AS] <alias>` text scan (NOT
/// the tree-sitter tree — decouples this module from tree-sitter-sequel's
/// node shapes, design §2). `<table>` may be schema-qualified
/// (`schema.table`), and the schema is preserved in the returned
/// `TableRef` (review round 1, finding 2). Also self-maps each named table
/// to itself (lowercased lookups elsewhere), so a bare `table.` qualifier
/// resolves without a separate lookup path. The word immediately after a
/// table reference is treated as an alias UNLESS it's a stopword (`AS` is
/// handled specially; every other `KEYWORDS`/`ALIAS_STOPWORDS` entry —
/// including `CROSS`/`NATURAL`/`USING`/`LATERAL`, review round 1 finding 1
/// — is a clause boundary, not an alias). `None` = ambiguous (duplicate
/// alias bound to two different tables, or an unresolvable `FROM (`/`JOIN (`
/// subquery) — "offers nothing" rather than a guess.
pub fn resolve_aliases(text: &str) -> Option<HashMap<String, TableRef>> {
    let masked = mask_strings_and_comments(text);
    let chars: Vec<char> = masked.chars().collect();
    let len = chars.len();
    let mut map: HashMap<String, TableRef> = HashMap::new();
    let mut i = 0usize;

    while i < len {
        if !is_ident_start(chars[i]) {
            i += 1;
            continue;
        }

        let Some((word, j)) = read_word(&chars, i) else {
            i += 1;
            continue;
        };
        let upper = word.to_uppercase();

        if upper != "FROM" && upper != "JOIN" {
            i = j;
            continue;
        }

        let after_kw = skip_ws(&chars, j);
        if after_kw < len && chars[after_kw] == '(' {
            // Subquery in FROM/JOIN position — cannot resolve to a real
            // table; per design §2, this makes the whole scan ambiguous.
            return None;
        }

        let Some((schema, table, after_table)) = read_qualified_word(&chars, after_kw) else {
            i = after_kw;
            continue;
        };
        let table_ref = TableRef { schema, name: table.clone() };

        if !insert_alias(&mut map, table.clone(), table_ref.clone()) {
            return None;
        }

        let after_ws = skip_ws(&chars, after_table);
        let Some((next_word, after_next)) = read_word(&chars, after_ws) else {
            i = after_table;
            continue;
        };
        let next_upper = next_word.to_uppercase();

        if next_upper == "AS" {
            let after_as_ws = skip_ws(&chars, after_next);
            if let Some((alias, after_alias)) = read_word(&chars, after_as_ws) {
                if !insert_alias(&mut map, alias, table_ref) {
                    return None;
                }
                i = after_alias;
            } else {
                i = after_next;
            }
        } else if !is_alias_stopword(&next_upper) {
            if !insert_alias(&mut map, next_word, table_ref) {
                return None;
            }
            i = after_next;
        } else {
            i = after_table;
        }
    }

    Some(map)
}

/// Names bound by a `WITH <name> [(cols)] AS (...)` / `, <name> [(cols)] AS
/// (...)` CTE list (an optional leading `RECURSIVE` is skipped, and `<name>`
/// may be a double-quoted identifier). Used only to detect column-lookup
/// shadowing (review round 1, finding 3): a `FROM`/`JOIN` reference to a
/// name that's ALSO a CTE name must never resolve to a same-named real
/// table's columns, since the actual bound source is the CTE's (unmodeled)
/// result shape, not the table's. `chars` must come from
/// `mask_for_cte_scan`, NOT `mask_strings_and_comments` — the latter would
/// blank a quoted CTE name (`WITH "orders" AS (...)`) to spaces, making it
/// invisible here (review round 2 fix).
fn cte_names(chars: &[char]) -> HashSet<String> {
    let len = chars.len();
    let mut names = HashSet::new();
    let mut i = 0usize;

    while i < len {
        if !is_ident_start(chars[i]) {
            i += 1;
            continue;
        }
        let Some((word, j)) = read_word(chars, i) else {
            i += 1;
            continue;
        };
        if word.eq_ignore_ascii_case("WITH") {
            i = parse_cte_name_list(chars, skip_ws(chars, j), &mut names);
        } else {
            i = j;
        }
    }

    names
}

/// Parses a comma-separated `<name> [(cols)] AS (...)` list starting at `i`
/// (right after `WITH`), inserting each `<name>` (uppercased) into `names`.
/// Stops (returning the index it stopped at) at the first entry that
/// doesn't match the expected shape — degrades gracefully on anything this
/// lightweight scan doesn't understand, never panics.
fn parse_cte_name_list(chars: &[char], mut i: usize, names: &mut HashSet<String>) -> usize {
    let len = chars.len();
    loop {
        i = skip_ws(chars, i);
        let Some((mut word, mut after)) = read_word(chars, i) else {
            return i;
        };
        if word.eq_ignore_ascii_case("RECURSIVE") {
            i = skip_ws(chars, after);
            match read_word(chars, i) {
                Some((w2, a2)) => {
                    word = w2;
                    after = a2;
                }
                None => return i,
            }
        }
        let name = word;
        i = skip_ws(chars, after);
        if i < len && chars[i] == '(' {
            i = skip_balanced_parens(chars, i);
            i = skip_ws(chars, i);
        }
        let Some((as_word, after_as)) = read_word(chars, i) else {
            return i;
        };
        if !as_word.eq_ignore_ascii_case("AS") {
            return i;
        }
        i = skip_ws(chars, after_as);
        if i >= len || chars[i] != '(' {
            return i;
        }
        names.insert(name.to_uppercase());
        i = skip_balanced_parens(chars, i);
        i = skip_ws(chars, i);
        if i < len && chars[i] == ',' {
            i += 1;
            continue;
        }
        return i;
    }
}

/// Skips a `(...)` group starting at `i` (which must be `(`), respecting
/// nesting. Returns the index right after the matching `)`, or the end of
/// input if unterminated (best-effort, no panic).
fn skip_balanced_parens(chars: &[char], i: usize) -> usize {
    let len = chars.len();
    if i >= len || chars[i] != '(' {
        return i;
    }
    let mut depth = 0i32;
    let mut k = i;
    while k < len {
        match chars[k] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return k + 1;
                }
            }
            _ => {}
        }
        k += 1;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::{ColumnInfo, SchemaSnapshot, TableInfo};

    fn snapshot_two_schemas() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![
                TableInfo {
                    schema: Some("public".into()),
                    name: "users".into(),
                    columns: vec![
                        ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                        ColumnInfo { name: "email".into(), ..Default::default() },
                    ],
                    ..Default::default()
                },
                TableInfo {
                    schema: Some("audit".into()),
                    name: "log".into(),
                    columns: vec![ColumnInfo { name: "id".into(), ..Default::default() }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn snapshot_one_schema() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![TableInfo {
                schema: Some("public".into()),
                name: "orders".into(),
                columns: vec![
                    ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                    ColumnInfo { name: "total".into(), ..Default::default() },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// `public.users(id, email)` + `hr.users(ssn, salary)` — same table
    /// name, two different schemas (review round 1, finding 2 fixtures).
    fn snapshot_duplicate_table_name_across_schemas() -> SchemaSnapshot {
        SchemaSnapshot {
            tables: vec![
                TableInfo {
                    schema: Some("public".into()),
                    name: "users".into(),
                    columns: vec![
                        ColumnInfo { name: "id".into(), is_pk: true, ..Default::default() },
                        ColumnInfo { name: "email".into(), ..Default::default() },
                    ],
                    ..Default::default()
                },
                TableInfo {
                    schema: Some("hr".into()),
                    name: "users".into(),
                    columns: vec![
                        ColumnInfo { name: "ssn".into(), ..Default::default() },
                        ColumnInfo { name: "salary".into(), ..Default::default() },
                    ],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn keywords_offered_regardless_of_snapshot() {
        let cs = candidates("sel", 3, None, false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Keyword && c.text == "SELECT"));
    }

    #[test]
    fn table_names_schema_qualified_when_snapshot_spans_multiple_schemas() {
        let cs = candidates("us", 2, Some(&snapshot_two_schemas()), false, false);
        let table = cs.iter().find(|c| c.kind == CandidateKind::Table && c.text.contains("users")).unwrap();
        assert!(table.label.contains("public"));
    }

    #[test]
    fn table_names_bare_when_snapshot_is_single_schema() {
        let cs = candidates("ord", 3, Some(&snapshot_one_schema()), false, false);
        let table = cs.iter().find(|c| c.kind == CandidateKind::Table).unwrap();
        assert_eq!(table.text, "orders");
    }

    #[test]
    fn suppressed_span_returns_no_candidates() {
        let cs = candidates("sel", 3, None, false, true);
        assert!(cs.is_empty());
    }

    #[test]
    fn force_ctrl_space_returns_full_set_with_empty_prefix() {
        let cs = candidates("", 0, None, true, false);
        assert!(cs.iter().any(|c| c.text == "SELECT"));
        assert!(cs.len() > 1);
    }

    #[test]
    fn column_completion_after_bare_table_dot() {
        let sql = "SELECT o.total FROM orders o WHERE orders.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "id"));
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "total"));
    }

    #[test]
    fn column_completion_after_alias_dot() {
        let sql = "SELECT * FROM orders o WHERE o.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "total"));
    }

    // Design §5's mandated risk-mitigation test: an ambiguous alias must
    // offer NOTHING, never a wrong guess.
    #[test]
    fn alias_ambiguity_offers_nothing() {
        let sql = "SELECT * FROM orders x JOIN users x ON x.id = x.id WHERE x.";
        assert_eq!(resolve_aliases(sql), None);
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_two_schemas()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn subquery_from_paren_is_ambiguous_offers_nothing() {
        let sql = "SELECT * FROM (SELECT 1) x WHERE x.";
        assert_eq!(resolve_aliases(sql), None);
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn unqualified_bare_column_completion_is_a_non_goal_returns_no_columns() {
        // design §2: bare column completion is explicitly out of scope for v1.
        let sql = "SELECT tot FROM orders";
        let cursor = 10; // inside "tot"
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn ranking_schema_objects_beat_keywords_on_equal_match() {
        // "orders" (table) vs no keyword literally named "orders" — use a
        // prefix that matches both a keyword and a table to prove
        // ordering: "o" alone is too broad; instead assert table entries
        // sort before keyword entries when both match the same prefix
        // tier by construction of the ranking, using "order" (keyword
        // ORDER exists) vs a same-prefixed table.
        let mut snap = snapshot_one_schema();
        snap.tables[0].name = "order".to_string();
        let cs = candidates("order", 5, Some(&snap), false, false);
        let table_ix = cs.iter().position(|c| c.kind == CandidateKind::Table).unwrap();
        let keyword_ix = cs.iter().position(|c| c.kind == CandidateKind::Keyword && c.text == "ORDER").unwrap();
        assert!(table_ix < keyword_ix);
    }

    #[test]
    fn cursor_context_extracts_prefix_and_qualifier_across_dot() {
        let ctx = cursor_context("SELECT o.tot", 12);
        assert_eq!(ctx.prefix, "tot");
        assert_eq!(ctx.qualifier, Some("o".to_string()));
    }

    #[test]
    fn cursor_context_no_qualifier_when_no_dot() {
        let ctx = cursor_context("SELECT sel", 10);
        assert_eq!(ctx.prefix, "sel");
        assert_eq!(ctx.qualifier, None);
    }

    // --- Review round 1 fixes ---

    #[test]
    fn cross_join_binds_no_alias() {
        let sql = "SELECT * FROM users CROSS JOIN log WHERE users.";
        let map = resolve_aliases(sql).expect("not ambiguous");
        assert!(!map.contains_key("CROSS"));
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_two_schemas()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "id"));
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "email"));
    }

    #[test]
    fn using_clause_binds_no_alias() {
        let sql = "SELECT * FROM users JOIN log USING (id) WHERE users.";
        let map = resolve_aliases(sql).expect("not ambiguous");
        assert!(!map.contains_key("USING"));
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_two_schemas()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "id"));
    }

    #[test]
    fn schema_qualified_alias_gets_that_schemas_columns() {
        let snap = snapshot_duplicate_table_name_across_schemas();
        let sql = "SELECT * FROM hr.users u WHERE u.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snap), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "ssn"));
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "salary"));
        assert!(!cs.iter().any(|c| c.kind == CandidateKind::Column && (c.text == "id" || c.text == "email")));
    }

    #[test]
    fn bare_name_ambiguous_across_schemas_offers_nothing() {
        let snap = snapshot_duplicate_table_name_across_schemas();
        let sql = "SELECT * FROM users u WHERE u.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snap), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn cte_name_shadowing_real_table_offers_nothing() {
        // "orders" is both a CTE name AND a real table in the snapshot; the
        // CTE shadows the real table within this query, so `o.` must offer
        // nothing rather than the real table's columns.
        let sql = "WITH orders AS (SELECT 1 AS x) SELECT * FROM orders o WHERE o.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn mid_multibyte_cursor_does_not_panic() {
        let text = "SELECT café";
        // Byte 11 sits mid-way through the 2-byte 'é' (bytes 10-11).
        let ctx = cursor_context(text, 11);
        assert!(ctx.prefix.chars().all(|c| c.is_ascii()));

        // 4-byte emoji: 'SELECT ' (7 bytes) + \u{1F600} (bytes 7..11) + 'x'.
        let text2 = "SELECT \u{1F600}x";
        let ctx2 = cursor_context(text2, 9); // mid-emoji
        assert!(ctx2.prefix.chars().all(|c| c.is_ascii()));
        let ctx3 = cursor_context(text2, 10); // also mid-emoji
        assert!(ctx3.prefix.chars().all(|c| c.is_ascii()));
    }

    // --- Review round 2 fixes ---

    #[test]
    fn quoted_cte_name_shadowing_real_table_offers_nothing() {
        // The CTE name is double-quoted; the general string/comment mask
        // used to blank it entirely, making it invisible to shadow
        // detection and letting the real `orders` table's columns leak
        // through. Must offer nothing, same as the unquoted case.
        let sql = "WITH \"orders\" AS (SELECT 1 AS x) SELECT * FROM orders WHERE orders.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    #[test]
    fn quoted_string_content_does_not_create_phantom_cte() {
        // A single-quoted string literal that merely LOOKS like a quoted
        // CTE definition (`'"WITH x AS"'`) must not be parsed as one — the
        // real `orders` table (no shadowing CTE here) must resolve
        // normally.
        let sql = "SELECT '\"WITH x AS\"' FROM orders WHERE orders.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snapshot_one_schema()), false, false);
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "id"));
        assert!(cs.iter().any(|c| c.kind == CandidateKind::Column && c.text == "total"));
    }

    #[test]
    fn schema_qualified_duplicate_rows_offers_nothing() {
        // Degenerate/corrupt snapshot: two literally identical schema+name
        // rows. The schema-qualified lookup must require exactly one
        // match, same invariant as the bare-name path — not silently pick
        // the first (review round 2 nit).
        let mut snap = snapshot_duplicate_table_name_across_schemas();
        let dup = snap.tables[1].clone(); // hr.users again
        snap.tables.push(dup);
        let sql = "SELECT * FROM hr.users u WHERE u.";
        let cursor = sql.len();
        let cs = candidates(sql, cursor, Some(&snap), false, false);
        assert!(cs.iter().all(|c| c.kind != CandidateKind::Column));
    }

    // --- Review round 3 fixes ---

    #[test]
    fn prefix_immediately_after_an_open_double_quote_is_detected() {
        assert!(prefix_preceded_by_open_quote("SELECT \"Us", 10));
    }

    #[test]
    fn prefix_not_after_a_quote_is_not_flagged() {
        assert!(!prefix_preceded_by_open_quote("SELECT sel", 10));
    }

    // MAJOR 2: an unclosed double-quoted identifier prefix must suppress
    // the popup entirely (never open, so never accept either) — accepting
    // would otherwise leave the stray opening quote in place while
    // inserting an unquoted, unclosed candidate.
    #[test]
    fn unclosed_quoted_identifier_prefix_offers_no_candidates() {
        let cs = candidates("SELECT \"Us", 10, None, false, false);
        assert!(cs.is_empty());
    }

    #[test]
    fn unclosed_quoted_identifier_prefix_suppresses_even_under_force_trigger() {
        // Ctrl+Space itself bypasses the typed prefix for RANKING purposes,
        // but an accept from that force-trigger still goes through
        // `completion_edit`'s real `cursor_context`-derived range — so this
        // must be suppressed too, not just the typing-trigger path.
        let cs = candidates("SELECT \"Us", 10, None, true, false);
        assert!(cs.is_empty());
    }

    // MAJOR 3: the identifier-prefix walk must be Unicode-aware (whole
    // chars, not bytes) — a non-ASCII char immediately before the cursor
    // must be included in the prefix, not silently dropped.
    #[test]
    fn cursor_context_captures_a_non_ascii_prefix_in_full() {
        let ctx = cursor_context("SELECT čas", 11);
        assert_eq!(ctx.prefix, "čas");
    }

    #[test]
    fn cursor_context_captures_a_mixed_ascii_non_ascii_prefix() {
        let ctx = cursor_context("SELECT užc", 11);
        assert_eq!(ctx.prefix, "užc");
    }
}
