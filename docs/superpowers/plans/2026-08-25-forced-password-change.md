# Forced Server Password Change Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Když server při připojení oznámí, že heslo musí být změněno (MSSQL 18488/18487) nebo že password-auth selhal kvůli možné expiraci (PG 28P01), aplikace nabídne český dialog změny hesla, změnu provede sankcionovanou cestou a nové heslo ihned uloží do Argon2id trezoru.

**Architecture:** MSSQL mění heslo driver-level při loginu (`SQL_COPT_SS_OLDPWD` = 1226 přes `SQLSetConnectAttrW` na pre-connect handle — živě ověřeno, viz spec §0); PG přes existující `run_write_transaction` s admin credentials a novým builderem `alter_password_rescue_pg` (`ALTER ROLE … PASSWORD … VALID UNTIL 'infinity'` — bez VALID UNTIL záchrana nefunguje, živě ověřeno). Detekce = sentinel kód v `QueryError.code` (`"password_change_required"` z MSSQL driveru, `"28P01"` z pg), dialog = nová `ModalState` varianta podle vzoru `CreateMasterPassword`.

**Tech Stack:** Rust, GPUI (pin 907ed09), odbc-api 29 (+ `odbc_api::sys` re-export odbc-sys 0.31), tokio-postgres, zeroize.

**Spec:** `docs/superpowers/specs/drafts/forced-password-change-design.md` (tento worktree; čti PŘED implementací — obsahuje živě ověřené chování serverů, o které se každý task opírá).

## Global Constraints

- Cargo VŽDY přes `%USERPROFILE%\.cargo\bin\cargo.exe`, s explicitním `-p <crate>` (bare workspace příkazy jen ve finálním gate).
- Zero warnings v plain i test buildu, debug i release.
- Merge gate každého tasku: `cargo test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp` zelené (dbc-mcp = regresní kanárek; očekávaný diff NULA). Task 1 navíc `-p dbc-driver-mssql`.
- Heslo NIKDY plaintext na disku/v logu/historii: display_sql(`'***'`)/exec_sql paralelní konstrukce (existující `WriteStatement`), hand-written Debug, `Zeroizing` pro hodnoty v letu, po úspěchu okamžitě `vault.set_secret`.
- Každý zápis = sankcionovaná runner metoda (PG: existující `run_write_transaction`; MSSQL: nová `change_mssql_password` — není to SQL write, ale UI nikdy nesahá na driver přímo).
- Nikdy auto-změna: detekce jen OTEVŘE dialog, tlačítko „Zrušit"/Esc nic nestojí.
- UI texty česky.
- Verze na konci fáze: `[workspace.package] version = "0.21.0"` (root `Cargo.toml`).
- SQLite/DuckDB: mimo rozsah (žádná serverová auth) — nikde nepřidávat větve, `detect` pro ně vrací `None` konstrukcí.

---

### Task 1: MSSQL driver — sentinel 18487/18488 + změna hesla při loginu

**Files:**
- Modify: `crates/dbc-driver-mssql/src/types.rs` (fn `odbc_err`, ~ř. 47–56, + testy)
- Create: `crates/dbc-driver-mssql/src/password.rs`
- Modify: `crates/dbc-driver-mssql/src/lib.rs` (registrace modulu + re-exporty)
- Modify: `crates/dbc-driver-mssql/Cargo.toml` (zeroize)
- Modify: `crates/dbc-driver-mssql/tests/mssql_integration.rs` (live `#[ignore]` test)

**Interfaces:**
- Consumes: `odbc_api::handles::{Environment, SqlText}`, `odbc_api::sys::{SQLSetConnectAttrW, ConnectionAttribute, AttrOdbcVersion, Pointer, SqlReturn, NTS}`, `crate::config::MssqlConfig`, `dbc_core::QueryError`.
- Produces (Task 2 a 3 na tom staví):
  - `dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE: &'static str` (= `"password_change_required"`)
  - `dbc_driver_mssql::change_password_at_connect(cfg: &MssqlConfig, old_password: &str) -> Result<(), QueryError>` — `cfg.password` nese NOVÉ heslo.

- [ ] **Step 1: Failing testy sentinelu v `types.rs`**

Do `mod tests` v `crates/dbc-driver-mssql/src/types.rs` přidej (fixture `diagnostics_error` už existuje, ~ř. 70 — přidej vedle ní variantu s nastavitelným `native_error`):

```rust
    fn diagnostics_error_native(state: &[u8; 5], native_error: i32) -> odbc_api::Error {
        odbc_api::Error::Diagnostics {
            record: Record { state: State(*state), native_error, message: Vec::new() },
            function: "SQLDriverConnect",
        }
    }

    /// Vynucená změna hesla (spec §0): 18488 (MUST_CHANGE) i 18487
    /// (expirované) přicházejí se SQLSTATE 28000 — stejně jako obyčejné
    /// špatné heslo (18456). Rozlišení nese POUZE native_error, který
    /// odbc_err dosud zahazoval; normalizace na sentinel zrcadlí
    /// existující HY008 → "cancelled".
    #[test]
    fn odbc_err_maps_must_change_and_expired_to_sentinel() {
        for native in [18487, 18488] {
            let mapped = odbc_err(diagnostics_error_native(b"28000", native));
            assert_eq!(mapped.code.as_deref(), Some(PASSWORD_CHANGE_REQUIRED_CODE));
        }
    }

    /// Obyčejné login selhání (18456) sentinel dostat NESMÍ — jinak by
    /// UI nabízelo změnu hesla na každý překlep.
    #[test]
    fn odbc_err_keeps_plain_login_failure_as_28000() {
        let mapped = odbc_err(diagnostics_error_native(b"28000", 18456));
        assert_eq!(mapped.code.as_deref(), Some("28000"));
    }
```

- [ ] **Step 2: Ověř, že testy padají**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql odbc_err_maps -- --nocapture`
Expected: FAIL — `PASSWORD_CHANGE_REQUIRED_CODE` neexistuje (compile error).

- [ ] **Step 3: Implementace sentinelu**

V `crates/dbc-driver-mssql/src/types.rs` nad `odbc_err` přidej konstantu a rozšiř mapování (doc comment fn doplň o větu o 18487/18488, stejný styl jako stávající HY008 vysvětlení):

```rust
/// Sentinel `QueryError.code` pro „server vyžaduje změnu hesla" — login
/// chyby 18488 (MUST_CHANGE) a 18487 (expirované heslo), obě SQLSTATE
/// 28000. UI (dbc-ui::pwchange) na něm staví nabídku změny; stejná
/// normalizační konvence jako `"cancelled"` výše.
pub const PASSWORD_CHANGE_REQUIRED_CODE: &str = "password_change_required";
```

a v `odbc_err` nahraď tělo `Diagnostics` větve:

```rust
        odbc_api::Error::Diagnostics { record, .. } => {
            let state = record.state.as_str();
            Some(if state == "HY008" {
                "cancelled".to_string()
            } else if state == "28000" && matches!(record.native_error, 18487 | 18488) {
                PASSWORD_CHANGE_REQUIRED_CODE.to_string()
            } else {
                state.to_string()
            })
        }
```

V `crates/dbc-driver-mssql/src/lib.rs` přidej k existujícím re-exportům: `pub use types::PASSWORD_CHANGE_REQUIRED_CODE;` (pokud je `mod types;` privátní, re-export stačí; nezveřejňuj celý modul).

- [ ] **Step 4: Testy zelené**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql odbc_err`
Expected: PASS (včetně stávajících odbc_err testů — HY008 a 42S22 se nesmí rozbít).

- [ ] **Step 5: Failing test UTF-16 helperu + `password.rs`**

Přidej `zeroize.workspace = true` do `[dependencies]` v `crates/dbc-driver-mssql/Cargo.toml`. Vytvoř `crates/dbc-driver-mssql/src/password.rs` zatím jen s testem (fn ještě neexistuje):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// SQLSetConnectAttrW bere string atribut jako NUL-terminated UTF-16;
    /// česká diakritika se nesmí ztratit (stejný wide-encoding důvod jako
    /// modul `wide`).
    #[test]
    fn utf16_nul_terminates_and_keeps_diacritics() {
        let buf = utf16_nul("žluťoučké");
        assert_eq!(buf.last(), Some(&0u16));
        assert_eq!(buf.len(), "žluťoučké".encode_utf16().count() + 1);
        assert_eq!(String::from_utf16_lossy(&buf[..buf.len() - 1]), "žluťoučké");
    }
}
```

Do `crates/dbc-driver-mssql/src/lib.rs` přidej `mod password;` + `pub use password::change_password_at_connect;`.

- [ ] **Step 6: Ověř, že to nejde přeložit**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql utf16_nul`
Expected: FAIL — `utf16_nul` (a `change_password_at_connect` z re-exportu) neexistují.

- [ ] **Step 7: Implementace `password.rs`**

```rust
//! Vynucená změna hesla při loginu (spec §0, živě ověřeno 2026-08-25):
//! msodbcsql podporuje `SQL_COPT_SS_OLDPWD` — `SQLSetConnectAttrW` PŘED
//! connectem nese STARÉ heslo, `Pwd=` v connection stringu NOVÉ; driver
//! změnu provede během přihlášení. Connection-string klíčové slovo záměrně
//! neexistuje (MS docs: kolidovalo by s poolingem), a odbc-api trait
//! `SetConnectionAttribute` je v privátním modulu — proto `as_sys()` +
//! přímé `odbc_api::sys::SQLSetConnectAttrW` (žádná nová závislost).
//!
//! Používá vlastní KRÁTKOŽIVOTNÉ `handles::Environment` (ne globální
//! [`crate::environment`]): safe `odbc_api::Environment` neexponuje
//! pre-connect handle. Komentář v lib.rs o „jednom env na proces" je
//! doporučení odbc-api pro SDÍLENÉ prostředí (diagnostics race při
//! paralelní alokaci); druhé, sekvenčně vytvořené env pro jeden connect je
//! na Windows driver manageru v pořádku a po návratu se uvolní.
use dbc_core::QueryError;
use odbc_api::handles::{Environment as RawEnvironment, SqlResult, SqlText};
use odbc_api::sys::{AttrOdbcVersion, ConnectionAttribute, Pointer, SqlReturn, NTS};
use zeroize::Zeroize;

use crate::config::MssqlConfig;
use crate::types::odbc_err;

/// msodbcsql.h: `SQL_COPT_SS_BASE (1200) + 26` — „Old Password, used when
/// changing password during login". Ověřeno v instalovaném headeru
/// (Client SDK\ODBC\170\SDK\Include\msodbcsql.h:202).
const SQL_COPT_SS_OLDPWD: i32 = 1226;

/// NUL-terminated UTF-16 buffer pro string hodnotu `SQLSetConnectAttrW`.
fn utf16_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Připojí se s `cfg` (jehož `password` je NOVÉ heslo) a starým heslem
/// v `SQL_COPT_SS_OLDPWD`; úspěšný návrat znamená, že server heslo změnil
/// během loginu. Spojení se hned zavře — volající (dbc-ui) si po úspěchu
/// otevře běžné spojení s novým heslem. Chyby (špatné staré heslo 18456,
/// policy-odmítnuté nové heslo 18463–18466, …) jdou přes [`odbc_err`],
/// message nikdy neobsahuje heslo (login diagnostiky hesla nenesou).
pub fn change_password_at_connect(cfg: &MssqlConfig, old_password: &str) -> Result<(), QueryError> {
    let env = match RawEnvironment::new() {
        SqlResult::Success(e) | SqlResult::SuccessWithInfo(e) => e,
        _ => return Err(QueryError::msg("ODBC: nelze alokovat environment pro změnu hesla")),
    };
    env.declare_version(AttrOdbcVersion::Odbc3_80)
        .into_result(&env)
        .map_err(odbc_err)?;
    let mut conn = env.allocate_connection().into_result(&env).map_err(odbc_err)?;

    let mut old_w = utf16_nul(old_password);
    // SAFETY: driver-specifický string atribut, nastavovaný na alokovaném
    // (dosud nepřipojeném) DBC handle; buffer je NUL-terminated UTF-16 a
    // žije až za connect volání (driver si hodnotu kopíruje při setu, ale
    // držíme ho pro jistotu déle). NTS = délka „null-terminated string".
    let ret = unsafe {
        odbc_api::sys::SQLSetConnectAttrW(
            conn.as_sys(),
            ConnectionAttribute(SQL_COPT_SS_OLDPWD),
            old_w.as_ptr() as Pointer,
            NTS as i32,
        )
    };
    if !matches!(ret, SqlReturn::SUCCESS | SqlReturn::SUCCESS_WITH_INFO) {
        old_w.zeroize();
        return Err(QueryError::msg("ODBC: SQLSetConnectAttr(SQL_COPT_SS_OLDPWD) selhal"));
    }

    let conn_str = cfg.to_connection_string();
    let text = SqlText::new(&conn_str);
    let result = conn.connect_with_connection_string(&text).into_result(&conn).map_err(odbc_err);
    old_w.zeroize();
    if result.is_ok() {
        let _ = conn.disconnect();
    }
    result
}
```

Pozn.: `conn_str` obsahuje nové heslo — je to lokální `String`, žije jen po dobu fn a nikam se neloguje (stejná disciplína jako `MssqlConfig::to_connection_string` v `connect()`); pokud `cfg` build selže dřív, heslo se sem vůbec nedostane.

- [ ] **Step 8: Unit testy zelené + zero warnings**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql`
Expected: PASS, zero warnings.

- [ ] **Step 9: Live `#[ignore]` test**

Do `crates/dbc-driver-mssql/tests/mssql_integration.rs` (harness `connect_or_skip`/`common::conn_str_or_skip` už existuje — testcontainers nebo `DBC_MSSQL_TEST_CONN`):

```rust
/// Parsuje host,port ze `Server={tcp:host,port}` / `Server=tcp:host,port`
/// části connection stringu, který vrací `common::conn_str_or_skip`.
fn host_port_from(cs: &str) -> Option<(String, u16)> {
    let s = cs.split(';').find_map(|kv| kv.strip_prefix("Server="))?;
    let s = s.trim_start_matches('{').trim_end_matches('}');
    let s = s.strip_prefix("tcp:").unwrap_or(s);
    let (h, p) = s.rsplit_once(',')?;
    Some((h.to_string(), p.parse().ok()?))
}

/// Celý záchranný cyklus (spec §0, dříve ověřeno sondou 2026-08-25):
/// MUST_CHANGE login → probe padá se sentinelem → change_password_at_connect
/// uspěje → probe s novým heslem projde. Unikátní jméno loginu drží
/// reruny proti dlouhoživotnému kontejneru zelené.
#[tokio::test]
#[ignore]
async fn must_change_login_full_rescue_cycle() {
    let Some(cs) = common::conn_str_or_skip("must_change_login_full_rescue_cycle").await else { return };
    let mut sa = MssqlConnection::from_connection_string(cs.clone());
    if let Err(e) = sa.probe() {
        if common::skip_if_no_odbc_driver("must_change_login_full_rescue_cycle", &e) {
            return;
        }
        panic!("sa connect failed: {e}");
    }
    let Some((host, port)) = host_port_from(&cs) else { panic!("Server= nelze parsovat z {cs}") };
    let login = format!(
        "pw_rescue_{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
    );
    sa.execute(
        &format!(
            "CREATE LOGIN [{login}] WITH PASSWORD = N'Old!Passw0rd1' MUST_CHANGE, \
             CHECK_EXPIRATION = ON, CHECK_POLICY = ON"
        ),
        CancelToken::new(),
    )
    .await
    .unwrap();

    let cfg_old = dbc_driver_mssql::MssqlConfig::new(host.clone(), port, "master", login.clone(), "Old!Passw0rd1")
        .trust_server_certificate(true);
    let err = MssqlConnection::new(&cfg_old).probe().unwrap_err();
    assert_eq!(err.code.as_deref(), Some(dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE), "{err}");

    let cfg_new = dbc_driver_mssql::MssqlConfig::new(host, port, "master", login.clone(), "New!Passw0rd2")
        .trust_server_certificate(true);
    dbc_driver_mssql::change_password_at_connect(&cfg_new, "Old!Passw0rd1").unwrap();
    MssqlConnection::new(&cfg_new).probe().unwrap();

    sa.execute(&format!("DROP LOGIN [{login}]"), CancelToken::new()).await.unwrap();
}
```

Pokud `MssqlConfig` není z crate re-exportované (`pub use config::MssqlConfig;` v lib.rs), re-export přidej — `connect.rs` v dbc-ui ho už importuje, takže nejspíš existuje; jen ověř.

- [ ] **Step 10: Kompilace testů + volitelný live běh**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-driver-mssql --no-run`
Expected: kompiluje, zero warnings. (Live: `cargo test -p dbc-driver-mssql -- --ignored must_change` s běžícím SQL Serverem; po testcontainers běhu `docker rm -f` uniklý kontejner — viz test-module doc.)

- [ ] **Step 11: Gate + commit**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp -p dbc-driver-mssql`
Expected: vše zelené, zero warnings.

```bash
git add crates/dbc-driver-mssql
git commit -m "feat: MSSQL password_change_required sentinel + change-at-connect via SQL_COPT_SS_OLDPWD (pwchange T1)"
```

---

### Task 2: Čistá UI logika — `pwchange.rs` + PG záchranný builder

**Files:**
- Create: `crates/dbc-ui/src/pwchange.rs`
- Modify: `crates/dbc-ui/src/main.rs` (registrace `mod pwchange;` vedle `mod admin_sql;`)
- Modify: `crates/dbc-ui/src/admin_sql.rs` (nový builder + testy, vedle `alter_password` ~ř. 495)

**Interfaces:**
- Consumes: `dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE` (Task 1), `dbc_core::QueryError`, `dbc_state::Engine`, `admin_sql::{WriteStatement, quote_ident_for, sql_string_literal, REDACTED}`.
- Produces (Task 4/5 na tom staví):
  - `pwchange::PwChangeKind` (`MssqlMustChange | PgMaybeExpired`, derive `Debug, Clone, Copy, PartialEq, Eq`)
  - `pwchange::detect(engine: Engine, err: &QueryError) -> Option<PwChangeKind>`
  - `pwchange::validate_new_password(new1: &str, new2: &str) -> Result<(), String>`
  - `pwchange::validate_pg_admin(admin_user: &str) -> Result<(), String>`
  - `pwchange::esc_closable(all_password_fields_empty: bool, running: bool) -> bool`
  - `pwchange::dialog_body(kind: PwChangeKind, user: &str) -> String`
  - `pwchange::pg_rescue_display(user: &str) -> String`
  - `pwchange::enrich_test_error(engine: Engine, err: &QueryError) -> String`
  - `admin_sql::alter_password_rescue_pg(name: &str, password: &str) -> WriteStatement`

- [ ] **Step 1: Failing testy builderu v `admin_sql.rs`**

Do `mod mutation_tests` přidej:

```rust
    /// Spec §0 (živě ověřeno): samotné ALTER ROLE … PASSWORD expiraci
    /// NEZRUŠÍ — rolvaliduntil zůstává v minulosti a login dál padá.
    /// Záchrana proto VŽDY nese VALID UNTIL 'infinity'. Redakční pár
    /// stejný jako alter_password.
    #[test]
    fn alter_password_rescue_pg_resets_valid_until_and_redacts() {
        let ws = alter_password_rescue_pg("bob", "taj'ne");
        assert_eq!(ws.exec_sql, "ALTER ROLE \"bob\" PASSWORD 'taj''ne' VALID UNTIL 'infinity'");
        assert_eq!(ws.display_sql, "ALTER ROLE \"bob\" PASSWORD '***' VALID UNTIL 'infinity'");
        assert!(!ws.display_sql.contains("taj"));
        assert_eq!(ws.expected_affected, None);
    }
```

- [ ] **Step 2: Ověř FAIL**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui alter_password_rescue`
Expected: FAIL — fn neexistuje.

- [ ] **Step 3: Implementace builderu**

Do `admin_sql.rs` hned za `alter_password` (~ř. 513):

```rust
/// Záchrana expirovaného/vynuceného hesla (pwchange, spec §3) — pg-only:
/// MSSQL se zachraňuje driver-level při loginu (žádné SQL), SQLite/DuckDB
/// auth nemají. `VALID UNTIL 'infinity'` je pevná součást: bez ní změna
/// hesla expiraci nezruší (živě ověřeno, spec §0) a „záchrana" by
/// nezachránila. Stejný display/exec redakční pár jako [`alter_password`].
pub fn alter_password_rescue_pg(name: &str, password: &str) -> WriteStatement {
    let ident = quote_ident_for(Engine::Postgres, name);
    WriteStatement {
        exec_sql: format!(
            "ALTER ROLE {ident} PASSWORD {} VALID UNTIL 'infinity'",
            sql_string_literal(password)
        ),
        display_sql: format!("ALTER ROLE {ident} PASSWORD {REDACTED} VALID UNTIL 'infinity'"),
        expected_affected: None,
    }
}
```

- [ ] **Step 4: PASS**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui alter_password`
Expected: PASS (nový i stávající `alter_password_both_engines_redacts`).

- [ ] **Step 5: `pwchange.rs` s testy (failing přes neexistující modul)**

Vytvoř `crates/dbc-ui/src/pwchange.rs`:

```rust
//! Vynucená/expirovaná změna hesla serverem — ČISTÁ logika (žádné GPUI,
//! žádné I/O; stejná disciplína jako admin_sql.rs). Detekce ze sentinel
//! kódů, validace dialogu, české texty. Spec:
//! docs/superpowers/specs/drafts/forced-password-change-design.md
//!
//! Allow dead_code na úrovni modulu: T2 přistává před T4/T5 konzumenty
//! (dialog v connections_ui/main, enrich v on_test_clicked) — vše je
//! unit-testované, ale ještě nevolané z main. Odstraní T5.
#![allow(dead_code)]

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
/// MSSQL `PASSWORD_CHANGE_REQUIRED_CODE` (Task 1), PG `28P01` (pg_err ho
/// plní už dnes). SQLite/DuckDB nemají serverovou auth — `None` konstrukcí.
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
        // Obyčejné špatné heslo na MSSQL (28000) NENÍ nabídka změny.
        assert_eq!(detect(Engine::Mssql, &err(Some("28000"))), None);
        // Cizí kódy / žádný kód / špatný engine.
        assert_eq!(detect(Engine::Postgres, &err(Some("28000"))), None);
        assert_eq!(detect(Engine::Mssql, &err(None)), None);
        assert_eq!(detect(Engine::Sqlite, &err(Some("28P01"))), None);
        assert_eq!(detect(Engine::Duckdb, &err(Some("28P01"))), None);
        // Sentinel na špatném enginu (defenzivní — pg ho nikdy neprodukuje).
        assert_eq!(detect(Engine::Postgres, &err(Some(dbc_driver_mssql::PASSWORD_CHANGE_REQUIRED_CODE))), None);
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
```

Do `crates/dbc-ui/src/main.rs` přidej `mod pwchange;` k bloku ostatních `mod` deklarací (vedle `mod admin_sql;`).

- [ ] **Step 6: PASS + zero warnings**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui pwchange`
Expected: PASS. Poté `cargo test -p dbc-ui` celé — zero warnings (dead_code kryje module-level allow).

- [ ] **Step 7: Commit**

```bash
git add crates/dbc-ui/src/pwchange.rs crates/dbc-ui/src/admin_sql.rs crates/dbc-ui/src/main.rs
git commit -m "feat: pwchange detection/validation + pg rescue builder with VALID UNTIL (pwchange T2)"
```

---

### Task 3: Plumbing — `connect.rs` split + sankcionovaná runner metoda

**Files:**
- Modify: `crates/dbc-ui/src/connect.rs` (`mssql_connection_from_config` ~ř. 262–302 + testy ~ř. 482+)
- Modify: `crates/dbc-ui/src/runner.rs` (nová metoda vedle `test_connect` ~ř. 349)

**Interfaces:**
- Consumes: `dbc_driver_mssql::change_password_at_connect` (Task 1), `MssqlConfig`, `ConnectionConfig` (fixture `base_cfg()` v connect.rs testech).
- Produces (Task 4 volá):
  - `connect::change_mssql_password(cfg: &ConnectionConfig, old_password: &str, new_password: &str) -> Result<(), QueryError>` (`pub(crate)`)
  - `QueryRunner::change_mssql_password(&self, cfg: Box<ConnectionConfig>, old_password: zeroize::Zeroizing<String>, new_password: zeroize::Zeroizing<String>) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>>`

- [ ] **Step 1: Failing test refusal propagace**

Do `mod tests` v `connect.rs` (fixture `base_cfg()` už existuje):

```rust
    /// Změna hesla jde přes STEJNÝ config-builder jako connect — oba
    /// refusaly (SSH tunel, prázdný uživatel) platí i pro ni a extrakce
    /// `mssql_config_from_config` nesmí změnit chování
    /// `mssql_connection_from_config` (existující testy to jistí).
    #[test]
    fn change_mssql_password_propagates_config_refusals() {
        let mut cfg = base_cfg();
        cfg.user = String::new();
        let err = change_mssql_password(&cfg, "old", "new").unwrap_err();
        assert!(err.message.contains("zadejte uživatele"), "{err}");

        let mut cfg = base_cfg();
        cfg.ssh = Some(dbc_state::SshConfig::default());
        let err = change_mssql_password(&cfg, "old", "new").unwrap_err();
        assert!(err.message.contains("SSH tunel"), "{err}");
    }
```

Pokud `SshConfig` nemá `Default`, zkonstruuj ho stejným literálem, jaký používají existující ssh testy v tomto souboru (podívej se na sousední fixture — nekopíruj naslepo).

- [ ] **Step 2: FAIL**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui change_mssql_password`
Expected: FAIL — fn neexistuje.

- [ ] **Step 3: Implementace v `connect.rs`**

Rozděl `mssql_connection_from_config` (tělo ~ř. 266–301 se přesouvá BEZE ZMĚNY, jen návrat):

```rust
/// Vytažené z `mssql_connection_from_config` (pwchange T3): tentýž config
/// build — včetně obou refusalů a timeout defaultu — potřebuje i změna
/// hesla při loginu, která NEotevírá `MssqlConnection`.
pub(crate) fn mssql_config_from_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<MssqlConfig, QueryError> {
    // ... doslova dosavadní tělo mssql_connection_from_config až po
    // stavbu `mssql_cfg` (oba refusaly, MssqlConfig::new, encrypt/trust/
    // timeout/driver) ...
    Ok(mssql_cfg)
}

pub(crate) fn mssql_connection_from_config(
    cfg: &ConnectionConfig,
    secret: Option<String>,
) -> Result<MssqlConnection, QueryError> {
    Ok(MssqlConnection::new(&mssql_config_from_config(cfg, secret)?))
}

/// pwchange (spec §3): `cfg.password` = NOVÉ heslo, staré jde do
/// `SQL_COPT_SS_OLDPWD`. Volá se VÝHRADNĚ přes
/// `QueryRunner::change_mssql_password` (sankcionovaná cesta).
pub(crate) fn change_mssql_password(
    cfg: &ConnectionConfig,
    old_password: &str,
    new_password: &str,
) -> Result<(), QueryError> {
    let mssql_cfg = mssql_config_from_config(cfg, Some(new_password.to_string()))?;
    dbc_driver_mssql::change_password_at_connect(&mssql_cfg, old_password)
}
```

- [ ] **Step 4: PASS (nový test + stávající connect testy beze změny)**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui connect`
Expected: PASS včetně `mssql_connection_from_config_applies_options_and_defaults` a refusal-parity testů.

- [ ] **Step 5: Failing runner test**

Do runner testů (vedle testů používajících `QueryRunner::new()`, ~ř. 3013 — synchronní `#[test]` + `blocking_recv`, NE uvnitř async kontextu, viz varování u ~ř. 5515):

```rust
    /// Sankcionovaná metoda existuje a propaguje config-refusal přes
    /// oneshot — víc bez živého serveru testovat nejde (mechanismus sám
    /// je živě jištěn driver testem must_change_login_full_rescue_cycle).
    #[test]
    fn change_mssql_password_runner_propagates_refusal() {
        let runner = QueryRunner::new();
        let mut cfg = connect_test_mssql_cfg();
        cfg.user = String::new();
        let rx = runner.change_mssql_password(
            Box::new(cfg),
            zeroize::Zeroizing::new("old".to_string()),
            zeroize::Zeroizing::new("new".to_string()),
        );
        let err = rx.blocking_recv().expect("sender dropped").unwrap_err();
        assert!(err.message.contains("zadejte uživatele"), "{err}");
    }
```

`connect_test_mssql_cfg()`: pokud runner testy nemají MSSQL cfg fixture, přidej lokální helper se STEJNÝMI hodnotami jako `connect.rs::base_cfg()` (id "c1", engine Mssql, host "localhost", port 1433, database "master", user "sa", zbytek default/None/false).

- [ ] **Step 6: FAIL, pak implementace v `runner.rs`**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui change_mssql_password_runner`
Expected: FAIL. Pak vedle `test_connect`:

```rust
    /// pwchange (spec §3): jediná sankcionovaná cesta UI k
    /// `connect::change_mssql_password`. Blocking ODBC práce běží na
    /// spawn_blocking (stejné pravidlo jako `open_spec`); hesla přicházejí
    /// jako `Zeroizing` a zanikají s closure. ŽÁDNÝ `guard_not_read_only`:
    /// tohle není SQL write, ale údržba přihlášení — read-only připojení
    /// se zamčeným heslem by jinak nešlo zachránit (spec §3).
    pub fn change_mssql_password(
        &self,
        cfg: Box<dbc_state::ConnectionConfig>,
        old_password: zeroize::Zeroizing<String>,
        new_password: zeroize::Zeroizing<String>,
    ) -> tokio::sync::oneshot::Receiver<Result<(), QueryError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                connect::change_mssql_password(&cfg, &old_password, &new_password)
            })
            .await
            .unwrap_or_else(|_| Err(QueryError::msg("password change task panicked")));
            let _ = tx.send(result);
        });
        rx
    }
```

- [ ] **Step 7: PASS + gate + commit**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp`
Expected: zelené, zero warnings.

```bash
git add crates/dbc-ui/src/connect.rs crates/dbc-ui/src/runner.rs
git commit -m "feat: mssql_config_from_config split + sanctioned change_mssql_password runner method (pwchange T3)"
```

---

### Task 4: Dialog „Změna hesla na serveru" + oba confirm flow + detekce při přepnutí

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState` ~ř. 1084+, `ModalConfirmKind`/`modal_confirm_kind` ~ř. 1304–1333, `on_modal_confirm` ~ř. 1755, `render_modal_overlay` ~ř. 1472, nový render fn vedle `render_master_password_panel` ~ř. 3110)
- Modify: `crates/dbc-ui/src/main.rs` (`switch_to_database` failure arm ~ř. 4956, `on_cancel_query` closable match ~ř. 4055, nové metody `open_pw_change_dialog`/`confirm_pw_change`/`pw_change_set_error` vedle `open_vault_prompt` ~ř. 4986)

**Interfaces:**
- Consumes: `pwchange::{PwChangeKind, detect, validate_new_password, validate_pg_admin, esc_closable, dialog_body, pg_rescue_display}` (T2), `admin_sql::alter_password_rescue_pg` (T2), `QueryRunner::{change_mssql_password (T3), run_write_transaction}`, `Vault::{get_secret, set_secret}`, `record_history_with_kind` (history_panel.rs:152), `spec_for_database`-styl `ConnectSpec::Config`, `switch_to_database(&str, Option<String>, Option<PendingTreeAction>, cx)`.
- Produces: `AppView::open_pw_change_dialog(conn_id: String, user: String, kind: PwChangeKind, retry_db: Option<String>, cx)` (T5 hooky ho nevolají — volá ho už tento task ze `switch_to_database`).

- [ ] **Step 1: `ModalState` varianta + confirm-kind + Esc pravidlo**

V `connections_ui.rs` přidej do `ModalState`:

```rust
    /// pwchange (spec §2): nabídka změny hesla po detekovaném connect
    /// selhání. `user` = přihlášení, jehož heslo se mění (z configu);
    /// `retry_db` = databáze původního pokusu (po úspěchu se přepnutí
    /// opakuje). Admin pole se POUŽÍVAJÍ jen pro `PgMaybeExpired`
    /// (rendrují se podmíněně); MSSQL si heslo mění sám při loginu.
    ChangeServerPassword {
        conn_id: String,
        kind: crate::pwchange::PwChangeKind,
        user: String,
        retry_db: Option<String>,
        new1: Entity<TextField>,
        new2: Entity<TextField>,
        admin_user: Entity<TextField>,
        admin_password: Entity<TextField>,
        error: Option<String>,
        running: bool,
    },
```

Do `ModalConfirmKind` přidej variantu `ChangeServerPw`, do `modal_confirm_kind` arm `ModalState::ChangeServerPassword { .. } => ModalConfirmKind::ChangeServerPw` (match je exhaustivní — kompilátor tě sem dovede), a do `on_modal_confirm` arm `ModalConfirmKind::ChangeServerPw => self.confirm_pw_change(cx)`.

V `main.rs::on_cancel_query` (closable match, ~ř. 4055) přidej PŘED `_ => false`:

```rust
                connections_ui::ModalState::ChangeServerPassword {
                    new1, new2, admin_password, running, ..
                } => {
                    let empty = new1.read(cx).text().is_empty()
                        && new2.read(cx).text().is_empty()
                        && admin_password.read(cx).text().is_empty();
                    crate::pwchange::esc_closable(empty, *running)
                }
```

- [ ] **Step 2: `open_pw_change_dialog` + render**

V `main.rs` vedle `open_vault_prompt` (deferred-focus vzor — z async callbacku není `&mut Window`):

```rust
    /// pwchange (spec §1): otevírá nabídku změny hesla po detekovaném
    /// connect selhání. Nikdy nevytlačí existující modal (single-modal
    /// invariant) — v tom případě zůstane jen chybový status.
    pub(crate) fn open_pw_change_dialog(
        &mut self,
        conn_id: String,
        user: String,
        kind: pwchange::PwChangeKind,
        retry_db: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.modal.is_some() {
            return;
        }
        let new1 = cx.new(|cx| connections_ui::TextField::form_field(cx, "nové heslo", true));
        let new2 = cx.new(|cx| connections_ui::TextField::form_field(cx, "nové heslo znovu", true));
        let admin_user = cx.new(|cx| connections_ui::TextField::form_field(cx, "postgres", false));
        let admin_password =
            cx.new(|cx| connections_ui::TextField::form_field(cx, "heslo administrátora", true));
        self.modal = Some(connections_ui::ModalState::ChangeServerPassword {
            conn_id,
            kind,
            user,
            retry_db,
            new1,
            new2,
            admin_user,
            admin_password,
            error: None,
            running: false,
        });
        self.dropdown_open = false;
        self.modal_needs_focus = true;
        cx.notify();
    }
```

V `connections_ui.rs::render_modal_overlay` přidej arm:

```rust
            ModalState::ChangeServerPassword {
                kind, user, new1, new2, admin_user, admin_password, error, running, ..
            } => render_pw_change_panel(kind, &user, new1, new2, admin_user, admin_password, error, running, cx),
```

a vedle `render_master_password_panel` nový free fn (stejné panel chrome — `div().w(px(420.)).bg(cx.theme().bg_panel).border_1().border_color(cx.theme().border).rounded_md().p_4().flex().flex_col().gap_2().text_color(cx.theme().text_primary)`):

```rust
/// pwchange (spec §2). Admin pole jen pro PG; PG navíc transparentně
/// ukazuje redigovaný příkaz (display_sql nikdy nezávisí na hesle).
/// Tlačítka bez on_click, dokud `running` — stejný „disabled while
/// running" přístup jako apply dialog.
fn render_pw_change_panel(
    kind: crate::pwchange::PwChangeKind,
    user: &str,
    new1: Entity<TextField>,
    new2: Entity<TextField>,
    admin_user: Entity<TextField>,
    admin_password: Entity<TextField>,
    error: Option<String>,
    running: bool,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let mut panel: Div = div()
        .w(px(420.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Změna hesla na serveru"))
        .child(div().text_color(cx.theme().text_muted).child(crate::pwchange::dialog_body(kind, user)))
        .child(field_row("Nové heslo", new1, *cx.theme()))
        .child(field_row("Nové heslo znovu", new2, *cx.theme()));
    if kind == crate::pwchange::PwChangeKind::PgMaybeExpired {
        panel = panel
            .child(field_row("Admin uživatel", admin_user, *cx.theme()))
            .child(field_row("Admin heslo", admin_password, *cx.theme()))
            .child(div().text_color(cx.theme().text_muted).child(crate::pwchange::pg_rescue_display(user)));
    }
    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(e));
    }
    let mut cancel = styled_button("pwch-cancel", "Zrušit", *cx.theme());
    let mut submit = styled_button("pwch-submit", if running { "měním heslo…" } else { "Změnit heslo" }, *cx.theme());
    if !running {
        cancel = cancel.on_click(cx.listener(|v, _, _, cx| v.close_modal(cx)));
        submit = submit.on_click(cx.listener(|v, _, _, cx| v.confirm_pw_change(cx)));
    }
    panel = panel.child(div().flex().flex_row().gap_2().justify_end().mt_2().child(cancel).child(submit));
    panel.into_any_element()
}
```

Pozn.: `styled_button` bere `label: &'static str` — signaturu NEROZŠIŘUJ; oba submit labely v kódu výše jsou `&'static str` vybírané `if running`, takže projdou beze změny helperu.

- [ ] **Step 3: `confirm_pw_change` — validace + obě větve**

V `main.rs` (vedle `open_pw_change_dialog`; importy `pwchange`, `zeroize::Zeroizing`, `connections_ui::ModalState` už v souboru jsou nebo je doplň):

```rust
    /// pwchange (spec §3): Enter/„Změnit heslo". Self-guarding (validace
    /// nahoře, chyby zůstávají v dialogu) — `on_modal_confirm` nepřidává
    /// žádnou autoritu. MSSQL: staré heslo Z TREZORU (18488 implikuje, že
    /// bylo správné ⇒ trezor byl při connectu odemčený), změna driver-level.
    /// PG: admin credentials z dialogu, existující run_write_transaction,
    /// zápis do historie kind "admin" (display_sql, nikdy exec_sql).
    /// Po úspěchu OBOU větví: vault.set_secret + retry původního přepnutí.
    fn confirm_pw_change(&mut self, cx: &mut Context<Self>) {
        let Some(connections_ui::ModalState::ChangeServerPassword {
            conn_id, kind, user: _, retry_db, new1, new2, admin_user, admin_password, running, ..
        }) = self.modal.clone()
        else {
            return;
        };
        if running {
            return;
        }
        let new1_text = zeroize::Zeroizing::new(new1.read(cx).text());
        let new2_text = zeroize::Zeroizing::new(new2.read(cx).text());
        if let Err(m) = pwchange::validate_new_password(&new1_text, &new2_text) {
            self.pw_change_set_error(m, cx);
            return;
        }
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == conn_id).cloned() else {
            self.pw_change_set_error("připojení nenalezeno".to_string(), cx);
            return;
        };
        if self.vault.is_none() {
            // Defenzivní: detekce implikuje odemčený trezor (spec §4);
            // kdyby přesto nebyl, neměníme heslo, které bychom neuměli uložit.
            self.pw_change_set_error("trezor není odemčený — odemkněte ho a zkuste znovu".to_string(), cx);
            return;
        }
        match kind {
            pwchange::PwChangeKind::MssqlMustChange => {
                let Some(old) = self.vault.as_ref().and_then(|v| v.get_secret(&conn_id)) else {
                    self.pw_change_set_error(
                        "současné heslo není v trezoru — uložte ho v dialogu připojení".to_string(),
                        cx,
                    );
                    return;
                };
                if let Some(connections_ui::ModalState::ChangeServerPassword { running, error, .. }) =
                    &mut self.modal
                {
                    *running = true;
                    *error = None;
                }
                cx.notify();
                let rx = self.runner.change_mssql_password(
                    Box::new(cfg),
                    zeroize::Zeroizing::new(old),
                    zeroize::Zeroizing::new(new1_text.to_string()),
                );
                let new_password = zeroize::Zeroizing::new(new1_text.to_string());
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        match result {
                            Ok(Ok(())) => view.finish_pw_change_success(&conn_id, &new_password, retry_db.clone(), cx),
                            Ok(Err(e)) => view.pw_change_set_error(e.to_string(), cx),
                            Err(_) => view.pw_change_set_error("změna hesla zrušena".to_string(), cx),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            pwchange::PwChangeKind::PgMaybeExpired => {
                let admin_user_text = admin_user.read(cx).text();
                if let Err(m) = pwchange::validate_pg_admin(&admin_user_text) {
                    self.pw_change_set_error(m, cx);
                    return;
                }
                let admin_password_text = zeroize::Zeroizing::new(admin_password.read(cx).text());
                let stmt = admin_sql::alter_password_rescue_pg(&cfg.user, &new1_text);
                let sql_text = stmt.display_sql.clone();
                let mut rescue_cfg = cfg.clone();
                rescue_cfg.user = admin_user_text;
                // Server-side by default_transaction_read_only ALTER stejně
                // odmítl; explicitně potvrzená credential operace (spec §3).
                rescue_cfg.read_only = false;
                if let Some(db) = &retry_db {
                    rescue_cfg.database = db.clone();
                }
                if let Some(connections_ui::ModalState::ChangeServerPassword { running, error, .. }) =
                    &mut self.modal
                {
                    *running = true;
                    *error = None;
                }
                cx.notify();
                let history_conn_name = cfg.name.clone();
                let history_started_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let started = std::time::Instant::now();
                let spec = runner::ConnectSpec::Config {
                    cfg: Box::new(rescue_cfg),
                    secret: Some(admin_password_text.to_string()),
                };
                let rx = self.runner.run_write_transaction(spec, vec![stmt], Some(60));
                let new_password = zeroize::Zeroizing::new(new1_text.to_string());
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        match result {
                            Ok(Ok(_affected)) => {
                                let elapsed_ms = started.elapsed().as_millis() as i64;
                                view.record_history_with_kind(
                                    &sql_text,
                                    &history_conn_name,
                                    history_started_at,
                                    Some(elapsed_ms),
                                    None,
                                    None,
                                    "admin",
                                    cx,
                                );
                                view.finish_pw_change_success(&conn_id, &new_password, retry_db.clone(), cx);
                            }
                            Ok(Err(e)) => view.pw_change_set_error(e.to_string(), cx),
                            Err(_) => view.pw_change_set_error("změna hesla zrušena".to_string(), cx),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    /// Společný úspěchový konec obou větví: heslo OKAMŽITĚ do trezoru
    /// (spec §4), zavřít dialog, opakovat původní přepnutí. Selhání zápisu
    /// do trezoru se NESMÍ tvářit jako selhání změny — heslo na serveru UŽ
    /// je změněné; dialog zůstává s poctivou instrukcí.
    fn finish_pw_change_success(
        &mut self,
        conn_id: &str,
        new_password: &str,
        retry_db: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let saved = match self.vault.as_mut() {
            Some(v) => v.set_secret(conn_id, new_password).map_err(|e| e.message),
            None => Err("trezor není odemčený".to_string()),
        };
        match saved {
            Ok(()) => {
                self.modal = None;
                self.status = "heslo změněno a uloženo do trezoru".to_string();
                self.switch_to_database(conn_id, retry_db, None, cx);
            }
            Err(m) => self.pw_change_set_error(format!(
                "heslo na serveru ZMĚNĚNO, ale uložení do trezoru selhalo: {m} — uložte nové heslo v dialogu připojení"
            ), cx),
        }
    }

    /// Chyba zpět do otevřeného dialogu (a shodit `running`); když už
    /// dialog nestojí (uživatel Esc-nul během běhu — Esc je při running
    /// blokovaný, ale defenzivně), spadne do statusu.
    fn pw_change_set_error(&mut self, msg: String, cx: &mut Context<Self>) {
        if let Some(connections_ui::ModalState::ChangeServerPassword { error, running, .. }) =
            &mut self.modal
        {
            *error = Some(msg);
            *running = false;
        } else {
            self.status = format!("error: {msg}");
        }
        cx.notify();
    }
```

Pokud `record_history_with_kind`'s `row_count: Option<i64>` chceš vyplnit, nech `None` — ALTER ROLE affected count je bezvýznamný (drive_write_sequence vrací 0) a apply dialog tam dává skutečné počty jen u DML.

- [ ] **Step 4: Detekční hook ve `switch_to_database`**

V `main.rs` před `cx.spawn` v `switch_to_database` (~ř. 4924) přidej capture:

```rust
        let engine = cfg.engine;
        let conn_user = cfg.user.clone();
```

a failure arm (~ř. 4956) rozšiř:

```rust
                    Ok(Err(e)) => {
                        view.status = format!("error: {e}");
                        // pwchange (spec §1): nabídka změny hesla — nikdy
                        // auto-změna, dialog má Zrušit/Esc.
                        if let Some(kind) = crate::pwchange::detect(engine, &e) {
                            view.open_pw_change_dialog(
                                target_id.clone(),
                                conn_user.clone(),
                                kind,
                                db.clone(),
                                cx,
                            );
                        }
                    }
```

- [ ] **Step 5: Kompilace + celé testy**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: PASS, zero warnings (GPUI handler kód nemá unit testy — čistá logika je pokrytá z T2/T3; manuální smoke je v T5).

- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/main.rs
git commit -m "feat: server password change dialog + mssql/pg confirm flows + connect-failure offer (pwchange T4)"
```

---

### Task 5: Test-connect nápověda, úklid dead_code, manuální smoke

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`on_test_clicked` fold ~ř. 2072–2087)
- Modify: `crates/dbc-ui/src/pwchange.rs` (odstranit `#![allow(dead_code)]`)

**Interfaces:**
- Consumes: `pwchange::enrich_test_error` (T2).
- Produces: nic nového — uzavírá zapojení.

- [ ] **Step 1: Enrichment v Test tlačítku**

V `on_test_clicked` zachyť engine před dispatch (`let engine = ...` — engine je ve snapshot/ui datech, která handler už čte pro `spec`; použij tentýž zdroj) a fold výsledku (~ř. 2085) změň:

```rust
                Ok(Err(e)) => Err(crate::pwchange::enrich_test_error(engine, &e)),
```

(`e` je `QueryError` — dosavadní kód dělal `Err(e.to_string())`; `enrich_test_error` vrací tentýž text bez detekce, takže se nemění nic jiného než přidaná nápověda.)

- [ ] **Step 2: Odstranit `#![allow(dead_code)]` z `pwchange.rs`**

Smaž řádek i jeho doc-poznámku („Odstraní T5") — všechny fns už mají konzumenty (T4/T5).

- [ ] **Step 3: Zero warnings po úklidu**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: PASS, zero warnings — kdyby něco z `pwchange` zůstalo mrtvé, tady to vybuchne a buď se to zapojí, nebo smaže (nenechávej allow).

- [ ] **Step 4: Manuální smoke (docker, volitelný ale doporučený)**

MSSQL (ověřuje CELÝ uživatelský příběh):

```bash
docker run -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=Str0ng!Passw0rd" -p 14338:1433 -d --name pwsmoke mcr.microsoft.com/mssql/server:2022-latest
# počkej ~20 s
docker exec pwsmoke /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P 'Str0ng!Passw0rd' \
  -Q "CREATE LOGIN pwuser WITH PASSWORD='Old!Passw0rd1' MUST_CHANGE, CHECK_EXPIRATION=ON, CHECK_POLICY=ON"
```

V aplikaci (`cargo run -p dbc-ui`): připojení MSSQL localhost:14338, user `pwuser`, heslo `Old!Passw0rd1` (trust certificate ✓), ulož → přepni na něj → MUSÍ se otevřít dialog „Změna hesla na serveru" (bez admin polí) → nové heslo `New!Passw0rd2` 2× → „Změnit heslo" → status „heslo změněno a uloženo do trezoru" a připojení se povede. Ověř: Esc s rozepsaným heslem dialog NEzavře; „Test" v dialogu připojení se starým heslem ukazuje nápovědu, ne dialog.

PG (expirace):

```bash
docker run -d --name pgsmoke -e POSTGRES_PASSWORD=admpw -p 15438:5432 postgres:16-alpine
docker exec pgsmoke psql -U postgres -c "CREATE ROLE expired LOGIN PASSWORD 'oldpw' VALID UNTIL '2020-01-01';"
```

Připojení PG localhost:15438 user `expired` heslo `oldpw` → přepni → dialog (s admin poli + zobrazeným `ALTER ROLE "expired" PASSWORD '***' VALID UNTIL 'infinity'`) → admin `postgres`/`admpw`, nové heslo 2× → povede se, historie má „admin" záznam s `'***'` (NIKDY skutečné heslo). Úklid: `docker rm -f pwsmoke pgsmoke`.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/pwchange.rs
git commit -m "feat: test-connect password-change hint + drop pwchange dead_code allow (pwchange T5)"
```

---

### Task 6: Verze 0.21.0 + finální gate

**Files:**
- Modify: `Cargo.toml` (root, `[workspace.package] version` ř. 7)

- [ ] **Step 1: Bump**

`version = "0.20.0"` → `version = "0.21.0"` (před zápisem ověř na mainu, že 0.21.0 je pořád volné — konvence fáze).

- [ ] **Step 2: Finální gate**

Run (vše přes `%USERPROFILE%\.cargo\bin\cargo.exe`):
1. `cargo test -p dbc-core -p dbc-state -p dbc-ui -p dbc-mcp -p dbc-driver-mssql` — zelené, zero warnings.
2. `cargo build --workspace` a `cargo build --workspace --release` — zero warnings v obou.

Expected: vše zelené; okno aplikace hlásí `dbc v0.21.0`.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: v0.21.0 — forced server password change (pwchange T6)"
```
