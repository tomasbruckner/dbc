//! G7: thin wrapper around `similar` (design CURATION §0.1(c), approved new
//! workspace dependency) for line-level DDL diffing (drill-down, design
//! §3). Pure text in, pure text out — no knowledge of SQL/DDL structure.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffTag { Equal, Insert, Delete }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine { pub tag: DiffTag, pub text: String }

pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|c| {
            let tag = match c.tag() {
                ChangeTag::Equal => DiffTag::Equal,
                ChangeTag::Insert => DiffTag::Insert,
                ChangeTag::Delete => DiffTag::Delete,
            };
            // Trim `\r` too: engine-reported DDL may be CRLF on Windows, and a
            // dangling `\r` would render as a ghost glyph in the diff panel.
            DiffLine { tag, text: c.to_string().trim_end_matches('\n').trim_end_matches('\r').to_string() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_has_no_insert_or_delete_lines() {
        let lines = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.tag == DiffTag::Equal));
    }

    #[test]
    fn single_line_change_is_one_delete_one_insert() {
        let lines = diff_lines("a\nb\nc", "a\nX\nc");
        let deletes: Vec<&DiffLine> = lines.iter().filter(|l| l.tag == DiffTag::Delete).collect();
        let inserts: Vec<&DiffLine> = lines.iter().filter(|l| l.tag == DiffTag::Insert).collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].text, "b");
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].text, "X");
    }

    #[test]
    fn works_on_synthesized_and_engine_ddl_alike_since_it_only_sees_strings() {
        let synthesized = "CREATE TABLE \"t\" (\n  \"id\" integer NOT NULL\n);";
        let engine_ddl = "CREATE TABLE t (\n    id integer NOT NULL\n);";
        let lines = diff_lines(synthesized, engine_ddl);
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l.tag == DiffTag::Delete) || lines.iter().any(|l| l.tag == DiffTag::Insert));
    }
}
