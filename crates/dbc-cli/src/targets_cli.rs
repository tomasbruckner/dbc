//! `dbc query --on …`: turning target texts into connections, deciding
//! before anything opens, and the report each target produces.
//!
//! The order is the point (spec §4): config lookup → refusal → vault once
//! → glob expansion → run. A refused write must not have cost a master
//! password or a single connection, and a glob is expanded only after
//! that decision — a glob needs the server, and asking the server before
//! deciding would put the connection in front of the „no".
//!
//! Pure except for `read_targets_file`; `expand` takes the live database
//! enumeration as a closure, so the ordering is testable without a server.

use std::collections::HashMap;
use std::path::Path;

use dbc_connect::targets::{self, Target};
use dbc_state::{AppConfig, ConnectionConfig};

use crate::pick;
use crate::policy;
use crate::render::Table;

/// One target per line; blank lines and `#` comments are skipped so the
/// file can double as a documented tenant list.
pub fn read_targets_file(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("soubor s cíli {} nejde přečíst: {e}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// The database half of `conn/db` as typed: absent, a literal name, or a
/// pattern that needs the server's list to become names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbPart {
    Default,
    Named(String),
    Glob(String),
}

/// A target text bound to its saved connection — everything the refusal
/// needs (engine, read-only flag) without having opened anything.
#[derive(Debug, Clone)]
pub struct ResolvedText {
    pub cfg: ConnectionConfig,
    pub db: DbPart,
}

/// Step 1: every text against the config. An unknown or ambiguous
/// connection is reported the same way the positional form reports it.
pub fn resolve_texts(config: &AppConfig, texts: &[String]) -> Result<Vec<ResolvedText>, String> {
    let mut out = Vec::with_capacity(texts.len());
    for text in texts {
        let parsed = targets::parse_target_text(text)?;
        let cfg = pick::pick(&config.connections, parsed.conn)
            .map_err(|e| e.message(parsed.conn))?
            .clone();
        let db = match parsed.db {
            None => DbPart::Default,
            Some(d) if targets::is_glob(d) => DbPart::Glob(d.to_string()),
            Some(d) => DbPart::Named(d.to_string()),
        };
        out.push(ResolvedText { cfg, db });
    }
    Ok(out)
}

/// Step 2: `Ok(writes)`, decided from the SQL and the saved flags alone.
///
/// Every mentioned connection classifies the SQL with its own dialect
/// through the existing `policy::plan`. A read-only connection facing a
/// write is collected so the message names all of them at once; any
/// other refusal (unparsable, empty, needs `--write`) is remembered and
/// reported only when no read-only refusal applies — the standing flag
/// is the one `--write` cannot fix, so it is the one worth hearing first.
pub fn preflight(sql: &str, resolved: &[ResolvedText], write_flag: bool) -> Result<bool, String> {
    let mut writes = false;
    let mut other: Option<policy::Refusal> = None;
    for r in resolved {
        match policy::plan(sql, crate::dialect_for(r.cfg.engine), r.cfg.read_only, write_flag) {
            Ok(plan) => writes |= plan.iter().any(|s| !s.is_read),
            Err(policy::Refusal::ConnectionIsReadOnly { .. }) => writes = true,
            Err(e) => {
                if matches!(e, policy::Refusal::NeedsWriteFlag { .. }) {
                    writes = true;
                }
                other.get_or_insert(e);
            }
        }
    }
    let conns: Vec<(&str, bool)> =
        resolved.iter().map(|r| (r.cfg.name.as_str(), r.cfg.read_only)).collect();
    let mut refused = targets::refuse_read_only(writes, &conns);
    if !refused.is_empty() {
        refused.sort();
        refused.dedup();
        return Err(format!(
            "zápis odmítnut — připojení jen pro čtení: {} (ani s --write; příznak se mění \
             v aplikaci); nic se nespustilo",
            refused.join(", ")
        ));
    }
    if let Some(e) = other {
        return Err(e.message());
    }
    Ok(writes)
}

/// One statement of one target, as the app's history stores it.
pub struct HistRow {
    pub sql: String,
    pub started_at: i64,
    pub ms: Option<i64>,
    pub rows: Option<i64>,
    pub error: Option<String>,
}

/// What one target produced. `table` is the last read's result (`None`
/// for a batch of pure writes, and cleared on failure so an error never
/// arrives with a half-result beside it); `history` is recorded by the
/// caller after every target has finished, because the recorder is not
/// `Send` and a target runs on a runtime worker.
pub struct TargetReport {
    pub label: String,
    pub table: Option<Table>,
    pub error: Option<String>,
    /// `(statement index, affected rows)` for writes — printed to stderr.
    pub affected: Vec<(usize, u64)>,
    pub history: Vec<HistRow>,
    /// Connect included; what the table header shows.
    pub elapsed_ms: u128,
}

impl TargetReport {
    pub fn failed(label: String, error: String) -> Self {
        TargetReport {
            label,
            table: None,
            error: Some(error),
            affected: Vec::new(),
            history: Vec::new(),
            elapsed_ms: 0,
        }
    }
}

/// Step 4: the concrete targets after glob expansion, deduplicated.
///
/// `list_dbs(cfg)` is the live enumeration (`main.rs` wires it to the
/// `databases` command's code) and returns `(names, truncated)`; it is
/// called at most once per connection, however many globs name it. A
/// truncated list or a glob with no match is an error and nothing runs —
/// a run over „whatever happened to fit" is not the run that was asked for.
pub fn expand(
    resolved: Vec<ResolvedText>,
    mut list_dbs: impl FnMut(&ConnectionConfig) -> Result<(Vec<String>, bool), String>,
) -> Result<Vec<Target>, String> {
    let mut out = Vec::new();
    let mut cache: HashMap<String, (Vec<String>, bool)> = HashMap::new();
    for r in resolved {
        let mk = |db: String| Target {
            conn_id: r.cfg.id.clone(),
            conn_name: r.cfg.name.clone(),
            database: db,
        };
        match r.db {
            DbPart::Default => out.push(mk(r.cfg.database.clone())),
            DbPart::Named(d) => out.push(mk(d)),
            DbPart::Glob(g) => {
                if !cache.contains_key(&r.cfg.id) {
                    let listed = list_dbs(&r.cfg)?;
                    cache.insert(r.cfg.id.clone(), listed);
                }
                let (names, truncated) = &cache[&r.cfg.id];
                if *truncated {
                    return Err(format!(
                        "{}: seznam databází je neúplný (víc než {}), vyjmenuj cíle ručně",
                        r.cfg.name,
                        dbc_connect::DB_LIST_CAP
                    ));
                }
                let hits = targets::expand_glob(&g, names);
                if hits.is_empty() {
                    return Err(format!("{}/{g} neodpovídá žádné databázi", r.cfg.name));
                }
                for h in hits {
                    out.push(mk(h));
                }
            }
        }
    }
    Ok(targets::dedupe(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: &str, name: &str, ro: bool) -> dbc_state::ConnectionConfig {
        dbc_state::ConnectionConfig {
            id: id.into(),
            name: name.into(),
            folder: Vec::new(),
            engine: dbc_state::Engine::Sqlite,
            database: "default.db".into(),
            host: String::new(),
            port: None,
            user: String::new(),
            read_only: ro,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    fn config(conns: Vec<dbc_state::ConnectionConfig>) -> dbc_state::AppConfig {
        dbc_state::AppConfig { connections: conns, ..Default::default() }
    }

    #[test]
    fn read_targets_file_skips_blank_and_comment_lines() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "# tenants\nprod/a\n\n  prod/b  \n#x\n").unwrap();
        assert_eq!(read_targets_file(f.path()).unwrap(), vec!["prod/a", "prod/b"]);
    }

    #[test]
    fn a_missing_targets_file_names_its_path() {
        let e = read_targets_file(Path::new("nope-targets.txt")).unwrap_err();
        assert!(e.contains("nope-targets.txt"), "{e}");
    }

    #[test]
    fn resolve_texts_classifies_default_named_glob() {
        let c = config(vec![cfg("c1", "prod", false)]);
        let r = resolve_texts(&c, &["prod".into(), "prod/a".into(), "c1/k_*".into()]).unwrap();
        assert!(matches!(r[0].db, DbPart::Default));
        assert!(matches!(&r[1].db, DbPart::Named(n) if n == "a"));
        assert!(matches!(&r[2].db, DbPart::Glob(g) if g == "k_*"));
    }

    #[test]
    fn resolve_texts_reports_unknown_connection() {
        let c = config(vec![cfg("c1", "prod", false)]);
        let e = resolve_texts(&c, &["nope/a".into()]).unwrap_err();
        assert!(e.contains("nope") && e.contains("prod"));
    }

    #[test]
    fn preflight_refuses_read_only_write_naming_connections_only() {
        let r = resolve_texts(
            &config(vec![cfg("c1", "prod", false), cfg("c2", "archiv", true)]),
            &["prod/a".into(), "archiv/k_*".into()],
        )
        .unwrap();
        let e = preflight("delete from t", &r, true).unwrap_err();
        assert!(e.contains("archiv") && !e.contains("prod"), "{e}");
        assert!(!preflight("select 1", &r, false).unwrap());
        let e = preflight("delete from t", &r[..1], false).unwrap_err();
        assert!(e.contains("--write"), "{e}");
        assert!(preflight("delete from t", &r[..1], true).unwrap());
    }

    fn server_cfg(id: &str, name: &str, ro: bool) -> dbc_state::ConnectionConfig {
        let mut c = cfg(id, name, ro);
        c.engine = dbc_state::Engine::Postgres;
        // TEST-NET-3 (RFC 5737): never routable, so an accidental connect
        // attempt would hang until the connect timeout instead of passing.
        c.host = "203.0.113.1".into();
        c.port = Some(5432);
        c.database = "db".into();
        c
    }

    /// Spec §4 ordering: the refusal comes BEFORE the vault and before any
    /// glob is expanded. A read-only connection named with a glob and a
    /// write statement is refused from the saved flags alone — `run()` in
    /// `main.rs` calls `resolve_texts` → `preflight` → vault → `expand`
    /// in that order, and this test proves the first two need no server:
    /// the host is unreachable, and the test passes only if the refusal
    /// arrives at once.
    #[test]
    fn a_read_only_glob_write_is_refused_before_anything_is_opened() {
        let c = config(vec![server_cfg("c1", "archiv", true)]);
        let clock = std::time::Instant::now();
        let r = resolve_texts(&c, &["archiv/klient_*".into()]).unwrap();
        let e = preflight("delete from t", &r, true).unwrap_err();
        assert!(e.contains("archiv") && e.contains("jen pro čtení"), "{e}");
        assert!(
            clock.elapsed() < std::time::Duration::from_secs(1),
            "a connection was attempted"
        );
    }

    /// Without `--write` a read-only connection still gets the read-only
    /// refusal, not the `--write` hint — adding the flag would not help.
    #[test]
    fn preflight_prefers_the_read_only_refusal_over_the_write_flag_hint() {
        let r = resolve_texts(
            &config(vec![cfg("c2", "archiv", true), cfg("c1", "prod", false)]),
            &["archiv/a".into(), "prod/a".into()],
        )
        .unwrap();
        let e = preflight("delete from t", &r, false).unwrap_err();
        assert!(e.contains("archiv") && e.contains("jen pro čtení"), "{e}");
    }

    #[test]
    fn preflight_refuses_empty_and_unparsable_sql() {
        let r = resolve_texts(&config(vec![cfg("c1", "prod", false)]), &["prod".into()]).unwrap();
        assert!(preflight("", &r, false).is_err());
        assert!(preflight("select 'open", &r, false).is_err());
    }

    #[test]
    fn expand_lists_each_connection_once_and_dedupes() {
        let c = config(vec![cfg("c1", "prod", false)]);
        let r = resolve_texts(
            &c,
            &["prod/k_*".into(), "prod/K_A".into(), "prod/k_?".into(), "prod".into()],
        )
        .unwrap();
        let mut calls = 0;
        let out = expand(r, |_| {
            calls += 1;
            Ok((vec!["k_a".into(), "k_b".into(), "other".into()], false))
        })
        .unwrap();
        assert_eq!(calls, 1);
        let labels: Vec<String> = out.iter().map(Target::label).collect();
        // `K_A` is a literal name, not the glob's `k_a` — identity is exact.
        assert_eq!(labels, vec!["prod/k_a", "prod/k_b", "prod/K_A", "prod/default.db"]);
    }

    #[test]
    fn expand_does_not_list_when_no_glob_is_named() {
        let c = config(vec![cfg("c1", "prod", false)]);
        let r = resolve_texts(&c, &["prod/a".into(), "prod".into()]).unwrap();
        let out = expand(r, |_| panic!("no enumeration without a glob")).unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn expand_refuses_a_glob_without_match_and_a_truncated_list() {
        let c = config(vec![cfg("c1", "prod", false)]);
        let r = resolve_texts(&c, &["prod/zz*".into()]).unwrap();
        let e = expand(r, |_| Ok((vec!["k_a".into()], false))).unwrap_err();
        assert!(e.contains("prod/zz*"), "{e}");
        let r = resolve_texts(&c, &["prod/k*".into()]).unwrap();
        let e = expand(r, |_| Ok((vec!["k_a".into()], true))).unwrap_err();
        assert!(e.contains("neúplný"), "{e}");
    }
}
