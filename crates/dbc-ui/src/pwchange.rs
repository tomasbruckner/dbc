//! Vynucená/expirovaná změna hesla serverem — ČISTÁ logika (žádné GPUI,
//! žádné I/O; stejná disciplína jako admin_sql.rs). Detekce ze sentinel
//! kódů, validace dialogu, české texty. Spec:
//! docs/superpowers/specs/drafts/forced-password-change-design.md

use dbc_core::QueryError;
use dbc_state::Engine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PwChangeKind {
    /// MSSQL 18488/18487 — staré heslo bylo SPRÁVNÉ (jinak 18456), login si
    /// heslo mění sám při připojení; admin účet není potřeba.
    MssqlMustChange,
    /// PG 28P01 — špatné NEBO expirované heslo, server to nerozlišuje
    /// (spec §0); záchrana vyžaduje jiný účet s CREATEROLE.
    PgMaybeExpired,
}

/// Jediné rozhodovací místo „nabídnout změnu hesla?". Sentinel kódy:
/// MSSQL [`dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE`] (T1), PG
/// `28P01` (pg_err ho plní už dnes). SQLite/DuckDB nemají serverovou auth
/// — `None` konstrukcí. Nikdy auto-změna: volající smí jen OTEVŘÍT nabídku.
pub fn detect(engine: Engine, err: &QueryError) -> Option<PwChangeKind> {
    let code = err.code.as_deref()?;
    match engine {
        Engine::Mssql if code == dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE => {
            Some(PwChangeKind::MssqlMustChange)
        }
        Engine::Postgres if code == "28P01" => Some(PwChangeKind::PgMaybeExpired),
        _ => None,
    }
}

pub fn validate_new_password(new1: &str, new2: &str) -> Result<(), String> {
    if new1.is_empty() {
        return Err("zadejte nové heslo".to_string());
    }
    if new1 != new2 {
        return Err("hesla se neshodují".to_string());
    }
    Ok(())
}

/// Heslo admina může být legitimně prázdné (rozhodne server); prázdný
/// UŽIVATEL je vždy chyba.
pub fn validate_pg_admin(admin_user: &str) -> Result<(), String> {
    if admin_user.trim().is_empty() {
        return Err("zadejte administrátorský účet".to_string());
    }
    Ok(())
}

/// Esc zavírá dialog jen bez rozepsaného hesla a bez běžící změny — stejný
/// „no accidental dismissal while a password is typed" princip jako
/// `admin_esc_closable` (admin_panel.rs).
pub fn esc_closable(all_password_fields_empty: bool, running: bool) -> bool {
    all_password_fields_empty && !running
}

pub fn dialog_body(kind: PwChangeKind, user: &str) -> String {
    match kind {
        PwChangeKind::MssqlMustChange => format!(
            "Server vyžaduje změnu hesla pro přihlášení „{user}“. Změna proběhne \
             při přihlášení (ODBC mechanismus, žádné SQL) a nové heslo se uloží do trezoru."
        ),
        PwChangeKind::PgMaybeExpired => format!(
            "Přihlášení uživatele „{user}“ selhalo — heslo je nesprávné, nebo mu \
             vypršela platnost (PostgreSQL to nerozlišuje). Změnu provede zadaný \
             účet s právem CREATEROLE; nové heslo se uloží do trezoru."
        ),
    }
}

/// Redigovaný příkaz pro transparentní zobrazení v dialogu — display_sql
/// nikdy nezávisí na skutečném hesle, proto prázdný placeholder.
pub fn pg_rescue_display(user: &str) -> String {
    format!("Příkaz: {}", crate::admin_sql::alter_password_rescue_pg(user, "").display_sql)
}

/// Tlačítko „Test" v dialogu připojení modal otevřít nesmí (single-modal
/// invariant) — jen obohatí chybový text o českou nápovědu.
pub fn enrich_test_error(engine: Engine, err: &QueryError) -> String {
    match detect(engine, err) {
        Some(PwChangeKind::MssqlMustChange) => format!(
            "{err}\nserver vyžaduje změnu hesla — po přepnutí na toto připojení aplikace nabídne dialog změny"
        ),
        Some(PwChangeKind::PgMaybeExpired) => {
            format!("{err}\npokud heslu vypršela platnost, aplikace při přepnutí na připojení nabídne změnu")
        }
        None => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(code: Option<&str>) -> QueryError {
        QueryError { code: code.map(str::to_string), message: "login failed".into(), position: None }
    }

    #[test]
    fn detect_mssql_sentinel_and_pg_28p01_only() {
        assert_eq!(
            detect(Engine::Mssql, &err(Some(dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE))),
            Some(PwChangeKind::MssqlMustChange)
        );
        assert_eq!(detect(Engine::Postgres, &err(Some("28P01"))), Some(PwChangeKind::PgMaybeExpired));
        // Obyčejné špatné heslo na MSSQL (28000/18456) NENÍ nabídka změny.
        assert_eq!(detect(Engine::Mssql, &err(Some("28000"))), None);
        // Cizí kódy / žádný kód / špatný engine.
        assert_eq!(detect(Engine::Postgres, &err(Some("28000"))), None);
        assert_eq!(detect(Engine::Mssql, &err(None)), None);
        assert_eq!(detect(Engine::Sqlite, &err(Some("28P01"))), None);
        assert_eq!(detect(Engine::Duckdb, &err(Some("28P01"))), None);
        // Sentinel na špatném enginu (defenzivní — pg ho nikdy neprodukuje).
        assert_eq!(
            detect(Engine::Postgres, &err(Some(dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE))),
            None
        );
    }

    #[test]
    fn validate_new_password_rules() {
        assert_eq!(validate_new_password("", ""), Err("zadejte nové heslo".to_string()));
        assert_eq!(validate_new_password("a", "b"), Err("hesla se neshodují".to_string()));
        assert_eq!(validate_new_password("tajné", "tajné"), Ok(()));
    }

    #[test]
    fn validate_pg_admin_requires_user() {
        assert_eq!(validate_pg_admin("  "), Err("zadejte administrátorský účet".to_string()));
        assert_eq!(validate_pg_admin("postgres"), Ok(()));
    }

    #[test]
    fn esc_closable_blocks_typed_password_and_running() {
        assert!(esc_closable(true, false));
        assert!(!esc_closable(false, false));
        assert!(!esc_closable(true, true));
        assert!(!esc_closable(false, true));
    }

    /// Display nikdy neobsahuje heslo (je redigovaný konstrukcí) a nese
    /// VALID UNTIL — uživatel v dialogu vidí přesně to, co poběží.
    #[test]
    fn pg_rescue_display_is_redacted_and_shows_valid_until() {
        let d = pg_rescue_display("bob");
        assert_eq!(d, "Příkaz: ALTER ROLE \"bob\" PASSWORD '***' VALID UNTIL 'infinity'");
    }

    #[test]
    fn enrich_test_error_appends_hint_only_on_detection() {
        let e = err(Some(dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE));
        assert!(enrich_test_error(Engine::Mssql, &e).contains("server vyžaduje změnu hesla"));
        let e = err(Some("28P01"));
        assert!(enrich_test_error(Engine::Postgres, &e).contains("vypršela platnost"));
        let e = err(Some("28000"));
        assert_eq!(enrich_test_error(Engine::Mssql, &e), e.to_string());
    }
}
