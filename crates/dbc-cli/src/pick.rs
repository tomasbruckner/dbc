//! Finding the connection the user named — pure.
//!
//! The GUI identifies a connection by an opaque `conn-…` id nobody types.
//! On the command line the natural handle is the NAME, which is the label
//! shown in the sidebar — but names are user-chosen and nothing stops two
//! from matching. So both are accepted, ids win, and an ambiguous name is
//! reported with the candidates rather than resolved by picking one.

use dbc_state::ConnectionConfig;

#[derive(Debug, PartialEq, Eq)]
pub enum PickError {
    /// Nothing matched. Carries the names that DO exist — a list of three
    /// answers is more useful than „not found", and this is exactly the
    /// moment a typo is discovered.
    NotFound { available: Vec<String> },
    /// Several connections carry that name.
    Ambiguous { name: String, ids: Vec<String> },
}

impl PickError {
    pub fn message(&self, asked_for: &str) -> String {
        match self {
            PickError::NotFound { available } if available.is_empty() => {
                "v configu nejsou uložená žádná připojení".to_string()
            }
            PickError::NotFound { available } => {
                format!("připojení {asked_for} neexistuje — mám: {}", available.join(", "))
            }
            PickError::Ambiguous { name, ids } => format!(
                "jméno {name} má víc připojení — vyber podle id: {}",
                ids.join(", ")
            ),
        }
    }
}

/// Resolve `asked_for` against the saved connections.
///
/// The id is checked FIRST and exactly: it is the unambiguous handle, so a
/// connection someone named after another one's id still resolves the way
/// the id says. Names are matched case-insensitively, because typing
/// `dbc query PROD` and being told `prod` does not exist is the kind of
/// precision nobody wants from a terminal.
pub fn pick<'a>(
    connections: &'a [ConnectionConfig],
    asked_for: &str,
) -> Result<&'a ConnectionConfig, PickError> {
    if let Some(hit) = connections.iter().find(|c| c.id == asked_for) {
        return Ok(hit);
    }
    let by_name: Vec<&ConnectionConfig> =
        connections.iter().filter(|c| c.name.eq_ignore_ascii_case(asked_for)).collect();
    match by_name.len() {
        1 => Ok(by_name[0]),
        0 => Err(PickError::NotFound {
            available: connections.iter().map(|c| c.name.clone()).collect(),
        }),
        _ => Err(PickError::Ambiguous {
            name: asked_for.to_string(),
            ids: by_name.iter().map(|c| c.id.clone()).collect(),
        }),
    }
}

/// The database this invocation should talk to: `--db` if given, otherwise
/// whatever the connection has saved.
pub fn database_for(cfg: &ConnectionConfig, override_db: Option<&str>) -> String {
    override_db.unwrap_or(&cfg.database).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbc_state::Engine;

    fn cfg(id: &str, name: &str) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: name.into(),
            folder: vec![],
            engine: Engine::Postgres,
            host: "h".into(),
            port: None,
            database: "saved_db".into(),
            user: "u".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite: false,
            mssql: None,
        }
    }

    #[test]
    fn a_name_resolves() {
        let list = [cfg("conn-1", "prod"), cfg("conn-2", "staging")];
        assert_eq!(pick(&list, "staging").unwrap().id, "conn-2");
    }

    #[test]
    fn a_name_is_case_insensitive() {
        let list = [cfg("conn-1", "prod")];
        assert_eq!(pick(&list, "PROD").unwrap().id, "conn-1");
    }

    #[test]
    fn an_id_resolves_too() {
        let list = [cfg("conn-1", "prod")];
        assert_eq!(pick(&list, "conn-1").unwrap().name, "prod");
    }

    /// The id is the unambiguous handle, so it must win even when someone
    /// has named another connection after it.
    #[test]
    fn an_id_beats_a_name_that_collides_with_it() {
        let list = [cfg("conn-1", "prod"), cfg("conn-2", "conn-1")];
        assert_eq!(pick(&list, "conn-1").unwrap().name, "prod");
    }

    #[test]
    fn a_duplicated_name_is_reported_with_its_ids_not_resolved() {
        let list = [cfg("conn-1", "prod"), cfg("conn-2", "prod")];
        let e = pick(&list, "prod").unwrap_err();
        assert_eq!(
            e,
            PickError::Ambiguous { name: "prod".into(), ids: vec!["conn-1".into(), "conn-2".into()] }
        );
        let msg = e.message("prod");
        assert!(msg.contains("conn-1") && msg.contains("conn-2"), "{msg}");
    }

    #[test]
    fn a_miss_lists_what_does_exist() {
        let list = [cfg("conn-1", "prod"), cfg("conn-2", "staging")];
        let msg = pick(&list, "prd").unwrap_err().message("prd");
        assert!(msg.contains("prod") && msg.contains("staging"), "{msg}");
    }

    /// „not found" against an EMPTY config is a different problem — the
    /// user has not set anything up yet — and saying „mám: " with nothing
    /// after it would be a worse answer than naming the real situation.
    #[test]
    fn a_miss_against_an_empty_config_says_that_instead() {
        let msg = pick(&[], "prod").unwrap_err().message("prod");
        assert!(msg.contains("žádná připojení"), "{msg}");
    }

    #[test]
    fn the_database_override_wins_over_the_saved_one() {
        let c = cfg("conn-1", "prod");
        assert_eq!(database_for(&c, None), "saved_db");
        assert_eq!(database_for(&c, Some("other")), "other");
    }
}
