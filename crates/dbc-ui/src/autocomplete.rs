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
//! subquery) degrades to offering nothing rather than guessing.
//!
//! T6 (this file) is a parallel-batch task with no dependency on T7 (the
//! `AppView` seam that wires `candidates()`/`resolve_aliases()` into the
//! editor) — until that wiring lands, this module's public surface has no
//! caller in `dbc-ui` outside its own tests, hence the module-level
//! `#![allow(dead_code)]` (same convention `sandbox.rs` used to carry for
//! the same reason; remove this once T7 lands).
#![allow(dead_code)]

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

const MAX_CANDIDATES: usize = 20;

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Walks backward from `cursor` over identifier bytes to find the partial
/// token under the cursor, and (if that token is preceded by a `.`) the
/// qualifier token before the dot.
pub fn cursor_context(text: &str, cursor: usize) -> CursorContext {
    let bytes = text.as_bytes();
    let cursor = cursor.min(bytes.len());

    let mut start = cursor;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    let prefix = text[start..cursor].to_string();

    let qualifier = if start > 0 && bytes[start - 1] == b'.' {
        let dot = start - 1;
        let mut qbegin = dot;
        while qbegin > 0 && is_ident_byte(bytes[qbegin - 1]) {
            qbegin -= 1;
        }
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
/// completion. Empty if there's no snapshot, the alias scan is ambiguous, or
/// the qualifier doesn't resolve to a known table.
fn column_candidates(text: &str, qualifier: &str, prefix: &str, snapshot: Option<&SchemaSnapshot>) -> Vec<Candidate> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let Some(alias_map) = resolve_aliases(text) else {
        return Vec::new();
    };
    let qualifier_lower = qualifier.to_lowercase();
    let Some(target_table) = alias_map
        .iter()
        .find(|(k, _)| k.to_lowercase() == qualifier_lower)
        .map(|(_, v)| v.clone())
    else {
        return Vec::new();
    };
    let target_lower = target_table.to_lowercase();
    let Some(table) = snapshot.tables.iter().find(|t| t.name.to_lowercase() == target_lower) else {
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

/// Reads a (possibly dot-chained, e.g. `schema.table`) identifier starting
/// at `j`, collapsing to the LAST segment. Returns `None` if `j` isn't at an
/// identifier start.
fn read_dotted_word(chars: &[char], j: usize) -> Option<(String, usize)> {
    if j >= chars.len() || !is_ident_start(chars[j]) {
        return None;
    }
    let mut k = j;
    let mut word_start = k;
    while k < chars.len() && is_ident_char(chars[k]) {
        k += 1;
    }
    let mut word: String = chars[word_start..k].iter().collect();
    loop {
        if k < chars.len() && chars[k] == '.' && k + 1 < chars.len() && is_ident_start(chars[k + 1]) {
            k += 1; // consume '.'
            word_start = k;
            while k < chars.len() && is_ident_char(chars[k]) {
                k += 1;
            }
            word = chars[word_start..k].iter().collect();
        } else {
            break;
        }
    }
    Some((word, k))
}

/// Inserts `key -> value`; returns `false` (conflict) if `key` is already
/// bound to a DIFFERENT value (case-insensitively) — the caller treats that
/// as whole-query ambiguity.
fn insert_alias(map: &mut HashMap<String, String>, key: String, value: String) -> bool {
    match map.get(&key) {
        Some(existing) if !existing.eq_ignore_ascii_case(&value) => false,
        _ => {
            map.insert(key, value);
            true
        }
    }
}

/// Replaces single-quoted strings, double-quoted identifiers, `--` line
/// comments and `/* */` block comments with spaces (preserving newlines),
/// so `resolve_aliases`' text scan never mistakes their contents for SQL
/// syntax. Best-effort: an unterminated construct is masked to end-of-input
/// rather than failing closed (this scanner's contract has no `None`-for-
/// parse-error case, only `None`-for-ambiguity).
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

/// `FROM <table> [AS] <alias>` / `JOIN <table> [AS] <alias>` text scan (NOT
/// the tree-sitter tree — decouples this module from tree-sitter-sequel's
/// node shapes, design §2). Also self-maps each named table to itself
/// (lowercased lookups elsewhere), so a bare `table.` qualifier resolves
/// without a separate lookup path. `None` = ambiguous (duplicate alias bound
/// to two different tables, or an unresolvable `FROM (`/`JOIN (` subquery)
/// — "offers nothing" rather than a guess.
pub fn resolve_aliases(text: &str) -> Option<HashMap<String, String>> {
    let masked = mask_strings_and_comments(text);
    let chars: Vec<char> = masked.chars().collect();
    let len = chars.len();
    let mut map: HashMap<String, String> = HashMap::new();
    let mut i = 0usize;

    while i < len {
        if !is_ident_start(chars[i]) {
            i += 1;
            continue;
        }

        let word_start = i;
        let mut j = i;
        while j < len && is_ident_char(chars[j]) {
            j += 1;
        }
        let word: String = chars[word_start..j].iter().collect();
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

        let Some((table, after_table)) = read_dotted_word(&chars, after_kw) else {
            i = after_kw;
            continue;
        };

        if !insert_alias(&mut map, table.clone(), table.clone()) {
            return None;
        }

        let after_ws = skip_ws(&chars, after_table);
        let Some((next_word, after_next)) = read_dotted_word(&chars, after_ws) else {
            i = after_table;
            continue;
        };
        let next_upper = next_word.to_uppercase();

        if next_upper == "AS" {
            let after_as_ws = skip_ws(&chars, after_next);
            if let Some((alias, after_alias)) = read_dotted_word(&chars, after_as_ws) {
                if !insert_alias(&mut map, alias, table) {
                    return None;
                }
                i = after_alias;
            } else {
                i = after_next;
            }
        } else if !KEYWORDS.iter().any(|k| *k == next_upper) {
            if !insert_alias(&mut map, next_word, table) {
                return None;
            }
            i = after_next;
        } else {
            i = after_table;
        }
    }

    Some(map)
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
}
