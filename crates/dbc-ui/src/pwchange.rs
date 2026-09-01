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
    if let Some(hint) = network_address_hint(engine, err) {
        return format!("{err}\n{hint}");
    }
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

/// „SQL Server Network Interfaces: Connection string is not valid [87]".
///
/// SNI is the layer that resolves and dials the ADDRESS, and it says this
/// before any login is attempted — so whatever is wrong, it is not the
/// user, the password or the database. The message says none of that, and
/// the user who hit it read „connection string" and guessed their password
/// was too long (2026-09-01). It is not: a rejected password is 18456.
///
/// `dbc_connect::normalise_mssql_host` now refuses the shapes that
/// predictably cause this before the driver is reached, so anything still
/// arriving here is something it did not anticipate — which is exactly when
/// the user needs to be told where to look rather than left with the
/// driver's word „string".
fn network_address_hint(engine: Engine, err: &QueryError) -> Option<String> {
    if engine != Engine::Mssql || !err.message.contains("Connection string is not valid") {
        return None;
    }
    Some(
        "adresu serveru odmítla síťová vrstva SQL Serveru — na přihlášení vůbec nedošlo, \
         takže to není heslem ani databází. Zkontroluj Host (jen jméno serveru nebo IP) \
         a Port."
            .to_string(),
    )
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

    /// The 2026-09-01 report: the user read „Connection string is not
    /// valid" and asked whether their password was too long. The answer has
    /// to be in the message, because that is all they get to see.
    #[test]
    fn error_87_says_it_is_the_address_and_not_the_password() {
        let sni = QueryError {
            code: Some("08001".into()),
            message: "[Microsoft][ODBC Driver 18 for SQL Server]SQL Server Network \
                      Interfaces: Connection string is not valid [87]."
                .into(),
            position: None,
        };
        let out = enrich_test_error(Engine::Mssql, &sni);
        assert!(out.contains("Zkontroluj Host"), "{out}");
        assert!(out.contains("není heslem"), "{out}");
        // The driver's own words survive — the hint is added, not a
        // replacement, or the detail needed to search for it is lost.
        assert!(out.contains("Connection string is not valid"), "{out}");
    }

    /// Only MSSQL, and only that message: a hint that fires on „connection
    /// refused" would be pointing at the wrong field.
    #[test]
    fn the_address_hint_does_not_fire_on_anything_else() {
        let other = QueryError {
            code: Some("08001".into()),
            message: "TCP Provider: No connection could be made".into(),
            position: None,
        };
        assert_eq!(enrich_test_error(Engine::Mssql, &other), other.to_string());
        let same_text_wrong_engine = QueryError {
            code: None,
            message: "Connection string is not valid".into(),
            position: None,
        };
        assert_eq!(
            enrich_test_error(Engine::Postgres, &same_text_wrong_engine),
            same_text_wrong_engine.to_string()
        );
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
