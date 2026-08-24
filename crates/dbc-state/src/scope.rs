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
///   `\u{1F}`-separated component, with the database name ESCAPED (see
///   below). Connection ids are app-generated `conn-{hex}` and genuinely
///   cannot contain the separator; database names CAN — Postgres
///   identifiers allow any character except NUL (T1 review finding; the
///   design's original "never contain control characters" claim was
///   overbroad, corrected in design §7). The escape (`\` → `\\`, then
///   U+001F → the literal sequence `\u001F`) is injective — a legitimate
///   name already containing `\u001F` gains a doubled backslash and stays
///   distinct — so the emitted key contains exactly one raw U+001F (the
///   separator) and two different `(id, db)` scopes can never alias one
///   store bucket.
///
/// Caller obligation: never pass `Some(cfg.database)` for the default
/// database — normalizing default→`None` is the caller's job; see T3's
/// `switch_to_database`.
///
/// The COMPOSITE conn identity (`"{id}\u{1F}{db}"` for the default db too)
/// is deliberately NOT used here: it would orphan every existing stored
/// value (design §7 item 5's collision check).
pub fn connection_scope_key(connection_id: &str, database: Option<&str>) -> String {
    match database {
        None => connection_id.to_string(),
        Some(db) => format!("{connection_id}\u{1F}{}", escape_db_component(db)),
    }
}

/// Injective escape for the database component: backslash-doubling FIRST,
/// then U+001F → the 6-char literal `\u001F`. Order matters — doubling
/// first guarantees every backslash in the output is either half of `\\`
/// or the head of `\u001F`, so decoding (and therefore the key) is
/// unambiguous.
fn escape_db_component(db: &str) -> String {
    db.replace('\\', "\\\\").replace('\u{1F}', "\\u001F")
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

    /// T1 review: Postgres allows U+001F inside an identifier, so a hostile
    /// database name must not be able to smuggle a separator into the key —
    /// the emitted key always has exactly ONE raw U+001F (the separator),
    /// which is what keeps the stores' own `\u{1F}`-joined `encode_key`
    /// composition unambiguous downstream.
    #[test]
    fn hostile_database_name_cannot_smuggle_a_separator() {
        let key = connection_scope_key("conn-a", Some("x\u{1F}y"));
        assert_eq!(key.matches('\u{1F}').count(), 1);
        assert_eq!(key, "conn-a\u{1F}x\\u001Fy");
    }

    /// The escape itself cannot collide with a legitimate name that already
    /// contains the escape sequence: backslash-doubling keeps them apart.
    #[test]
    fn escape_sequence_in_a_real_name_stays_distinct_from_escaped_separator() {
        // db literally named `x<US>y` vs db literally named `x\u001F y`
        // (6 chars, leading backslash):
        assert_ne!(
            connection_scope_key("conn-a", Some("x\u{1F}y")),
            connection_scope_key("conn-a", Some("x\\u001Fy")),
        );
        // Mixed: a backslash immediately before the control char vs a
        // literal `\u001F` written out — still distinct.
        assert_ne!(
            connection_scope_key("conn-a", Some("a\\\u{1F}b")),
            connection_scope_key("conn-a", Some("a\\u001Fb")),
        );
    }

    /// File-engine database "names" are Windows paths — backslash escaping
    /// is deterministic and stable, so set/get through the same helper
    /// always land in the same bucket.
    #[test]
    fn windows_path_component_escapes_deterministically() {
        assert_eq!(
            connection_scope_key("conn-f", Some(r"D:\data\a.db")),
            "conn-f\u{1F}D:\\\\data\\\\a.db",
        );
    }
}
