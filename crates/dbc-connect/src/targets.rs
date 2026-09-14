//! The target model shared by the GUI and the CLI: what `conn/db` means,
//! how a glob expands, how a set is deduplicated, and the one decision
//! that must happen before anything is opened (a write over a read-only
//! connection refuses the whole run). Pure — no I/O, no policy beyond
//! that one decision, which takes already-classified inputs (this crate
//! does not classify SQL; callers do, see the module doc in `lib.rs`).

/// One place a query runs: a saved connection and a database on it.
/// `conn_name` is carried so labels never need a config lookup later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub conn_id: String,
    pub conn_name: String,
    pub database: String,
}

impl Target {
    /// `conn/db` — the same shape the CLI's `--db` history label uses,
    /// always WITH the database (a target is never "the default").
    pub fn label(&self) -> String {
        format!("{}/{}", self.conn_name, self.database)
    }
}

/// `conn/db` as typed: the connection part and the optional database
/// (or glob) part. Split on the FIRST slash — a connection whose name
/// contains a slash has to be referenced by id (spec §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetText<'a> {
    pub conn: &'a str,
    pub db: Option<&'a str>,
}

pub fn parse_target_text(s: &str) -> Result<TargetText<'_>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("prázdný cíl — čekám conn nebo conn/db".to_string());
    }
    match s.split_once('/') {
        None => Ok(TargetText { conn: s, db: None }),
        Some((conn, db)) => {
            if conn.is_empty() {
                return Err(format!("cíl {s}: chybí jméno připojení před lomítkem"));
            }
            if db.is_empty() {
                return Err(format!("cíl {s}: chybí databáze za lomítkem"));
            }
            Ok(TargetText { conn, db: Some(db) })
        }
    }
}

pub fn is_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

/// Every name matching `pattern` (`*` = any run, `?` = one char, both
/// case-insensitive; everything else literal), in the order given.
pub fn expand_glob(pattern: &str, names: &[String]) -> Vec<String> {
    let pat: Vec<char> = pattern.to_lowercase().chars().collect();
    names
        .iter()
        .filter(|n| {
            let text: Vec<char> = n.to_lowercase().chars().collect();
            glob_match(&pat, &text)
        })
        .cloned()
        .collect()
}

fn glob_match(pat: &[char], text: &[char]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => (0..=text.len()).any(|i| glob_match(rest, &text[i..])),
        Some(('?', rest)) => !text.is_empty() && glob_match(rest, &text[1..]),
        Some((c, rest)) => text.first() == Some(c) && glob_match(rest, &text[1..]),
    }
}

/// First occurrence wins; identity is `(conn_id, database)`, never the
/// name (two connections may share a name).
pub fn dedupe(targets: Vec<Target>) -> Vec<Target> {
    let mut seen = std::collections::HashSet::new();
    targets
        .into_iter()
        .filter(|t| seen.insert((t.conn_id.clone(), t.database.clone())))
        .collect()
}

/// The names of the connections that would refuse a WRITING run. Empty
/// means the run may proceed. `conns` is `(name, read_only)` per
/// mentioned connection; the caller decided `writes` by classifying the
/// SQL with each connection's dialect.
pub fn refuse_read_only(writes: bool, conns: &[(&str, bool)]) -> Vec<String> {
    if !writes {
        return Vec::new();
    }
    conns.iter().filter(|(_, ro)| *ro).map(|(name, _)| name.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, name: &str, db: &str) -> Target {
        Target { conn_id: id.into(), conn_name: name.into(), database: db.into() }
    }

    #[test]
    fn parse_splits_on_first_slash_only() {
        let p = parse_target_text("prod/klient_a").unwrap();
        assert_eq!((p.conn, p.db), ("prod", Some("klient_a")));
        let p = parse_target_text("conn-1/a/b").unwrap();
        assert_eq!((p.conn, p.db), ("conn-1", Some("a/b")));
    }

    #[test]
    fn parse_without_slash_means_default_database() {
        let p = parse_target_text("prod").unwrap();
        assert_eq!((p.conn, p.db), ("prod", None));
    }

    #[test]
    fn parse_rejects_empty_parts() {
        assert!(parse_target_text("").is_err());
        assert!(parse_target_text("/db").is_err());
        assert!(parse_target_text("prod/").is_err());
        assert!(parse_target_text("  ").is_err());
    }

    #[test]
    fn glob_detection() {
        assert!(is_glob("klient_*"));
        assert!(is_glob("k?ient"));
        assert!(!is_glob("klient_a"));
    }

    #[test]
    fn glob_expands_case_insensitively_in_input_order() {
        let names = vec!["Klient_A".to_string(), "other".into(), "klient_b".into(), "klient".into()];
        assert_eq!(expand_glob("klient_*", &names), vec!["Klient_A", "klient_b"]);
        assert_eq!(expand_glob("KLIENT_?", &names), vec!["Klient_A", "klient_b"]);
        assert_eq!(expand_glob("nic*", &names), Vec::<String>::new());
    }

    #[test]
    fn glob_star_matches_empty_and_dots_are_literal() {
        let names = vec!["a".to_string(), "a.b".into(), "axb".into()];
        assert_eq!(expand_glob("a*", &names), vec!["a", "a.b", "axb"]);
        assert_eq!(expand_glob("a.b", &names), vec!["a.b"]);
    }

    #[test]
    fn dedupe_keeps_first_occurrence_order() {
        let out = dedupe(vec![t("c1", "prod", "a"), t("c2", "prod2", "a"), t("c1", "prod", "a"), t("c1", "prod", "b")]);
        let labels: Vec<String> = out.iter().map(Target::label).collect();
        assert_eq!(labels, vec!["prod/a", "prod2/a", "prod/b"]);
    }

    #[test]
    fn dedupe_is_by_id_and_database_not_by_name() {
        let out = dedupe(vec![t("c1", "same", "a"), t("c2", "same", "a")]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn refuse_lists_read_only_connections_only_when_writing() {
        let conns = [("prod", false), ("archiv", true), ("stary", true)];
        assert_eq!(refuse_read_only(true, &conns), vec!["archiv", "stary"]);
        assert!(refuse_read_only(false, &conns).is_empty());
        assert!(refuse_read_only(true, &[("prod", false)]).is_empty());
    }

    #[test]
    fn label_is_name_slash_database() {
        assert_eq!(t("c1", "prod", "klient_a").label(), "prod/klient_a");
    }
}
