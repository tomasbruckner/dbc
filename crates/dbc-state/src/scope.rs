//! Sidebar rework (design §5 row 10, §7 items 4–5): the store-bucket key
//! rule shared by view_prefs and params callers.

/// The `connection_id` value callers should hand to `ViewPrefsStore`/
/// `ParamValuesStore` once the app's active context is `(connection,
/// database)`:
///
/// - `database == None` (the connection's DEFAULT database): the LEGACY
///   bare id, byte-identical to every key written before this phase —
///   existing views.toml/params.toml entries keep working with no rewrite
///   and no loss.
/// - `database == Some(db)` (a non-default database): one more
///   `\u{1F}`-separated component. The stores' own `encode_key` appends
///   further components with the same separator; keys stay unambiguous
///   because the id itself (`conn-{hex}`) and database names/file paths
///   can never contain the control character.
///
/// The COMPOSITE conn identity (`"{id}\u{1F}{db}"` for the default db too)
/// is deliberately NOT used here: it would orphan every existing stored
/// value (design §7 item 5's collision check).
pub fn connection_scope_key(connection_id: &str, database: Option<&str>) -> String {
    match database {
        None => connection_id.to_string(),
        Some(db) => format!("{connection_id}\u{1F}{db}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_db_key_is_the_legacy_bare_id() {
        assert_eq!(connection_scope_key("conn-abc", None), "conn-abc");
    }

    #[test]
    fn non_default_db_appends_one_separated_component() {
        assert_eq!(connection_scope_key("conn-abc", Some("sales")), "conn-abc\u{1F}sales");
    }

    #[test]
    fn different_databases_isolate() {
        assert_ne!(
            connection_scope_key("conn-abc", Some("sales")),
            connection_scope_key("conn-abc", Some("inventory"))
        );
        assert_ne!(connection_scope_key("conn-abc", Some("sales")), connection_scope_key("conn-abc", None));
    }
}
