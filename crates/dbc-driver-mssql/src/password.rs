//! Vynucená změna hesla při loginu (spec
//! docs/superpowers/specs/drafts/forced-password-change-design.md §0,
//! živě ověřeno 2026-08-25): msodbcsql podporuje `SQL_COPT_SS_OLDPWD` —
//! `SQLSetConnectAttrW` PŘED connectem nese STARÉ heslo, `Pwd=` v
//! connection stringu NOVÉ; driver změnu provede během přihlášení.
//! Connection-string klíčové slovo záměrně neexistuje (MS docs: kolidovalo
//! by s poolingem), a odbc-api trait `SetConnectionAttribute` je v
//! privátním modulu — proto `as_sys()` + přímé
//! `odbc_api::sys::SQLSetConnectAttrW` (žádná nová závislost, odbc-api
//! re-exportuje odbc-sys jako `odbc_api::sys`).
//!
//! Používá vlastní KRÁTKOŽIVOTNÉ `handles::Environment` (ne globální
//! [`crate::environment`]): safe `odbc_api::Environment` neexponuje
//! pre-connect handle (`allocate_connection` je privátní). Komentář v
//! lib.rs o „jednom env na proces" je doporučení odbc-api pro SDÍLENÉ
//! prostředí (diagnostics race při paralelní alokaci); druhé, sekvenčně
//! vytvořené env pro jeden connect je na Windows driver manageru v pořádku
//! a po návratu se uvolní.
use dbc_core::QueryError;
use odbc_api::handles::{Environment as RawEnvironment, SqlResult, SqlText};
use odbc_api::sys::{AttrOdbcVersion, ConnectionAttribute, Pointer, SqlReturn, NTS};
use zeroize::Zeroize;

use crate::config::MssqlConfig;
use crate::types::odbc_err;

/// msodbcsql.h: `SQL_COPT_SS_BASE (1200) + 26` — „Old Password, used when
/// changing password during login". Ověřeno v instalovaném headeru
/// (`Client SDK\ODBC\170\SDK\Include\msodbcsql.h:202`).
const SQL_COPT_SS_OLDPWD: i32 = 1226;

/// NUL-terminated UTF-16 buffer pro string hodnotu `SQLSetConnectAttrW`
/// (stejný wide-encoding důvod jako modul `wide` — narrow varianta by
/// překódovávala přes ANSI codepage a rozbila diakritiku v heslech).
fn utf16_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Připojí se s `cfg` (jehož `password` je NOVÉ heslo) a starým heslem
/// v `SQL_COPT_SS_OLDPWD`; úspěšný návrat znamená, že server heslo změnil
/// během loginu. Spojení se hned zavře — volající (dbc-ui) si po úspěchu
/// otevře běžné spojení s novým heslem.
///
/// Chybové cesty (špatné staré heslo 18456, policy-odmítnuté nové heslo
/// 18463–18466, nedostupný server, …) jdou přes [`odbc_err`], jehož text
/// je POUZE diagnostický záznam driveru — nikdy connection string, který
/// tady jako jediný nese obě hesla plaintext (viz negativní test
/// `change_password_error_never_contains_either_password` a stejnou
/// pojistku `probe_error_never_contains_the_password` v lib.rs). Starý
/// UTF-16 buffer se po použití nuluje; `conn_str` je lokální `String`
/// žijící jen po dobu fn a nikam se neloguje (stejná disciplína jako
/// `connect()` v lib.rs).
pub fn change_password_at_connect(cfg: &MssqlConfig, old_password: &str) -> Result<(), QueryError> {
    let env = match RawEnvironment::new() {
        SqlResult::Success(e) | SqlResult::SuccessWithInfo(e) => e,
        _ => return Err(QueryError::msg("ODBC: nelze alokovat environment pro změnu hesla")),
    };
    env.declare_version(AttrOdbcVersion::Odbc3_80).into_result(&env).map_err(odbc_err)?;
    let mut conn = env.allocate_connection().into_result(&env).map_err(odbc_err)?;

    let mut old_w = utf16_nul(old_password);
    // SAFETY: driver-specifický string atribut nastavovaný na alokovaném
    // (dosud nepřipojeném) DBC handle; buffer je NUL-terminated UTF-16 a
    // žije až za connect volání (driver si hodnotu kopíruje při setu, ale
    // držíme ho pro jistotu déle). `NTS` = délka „null-terminated string".
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
        assert_eq!(utf16_nul(""), vec![0u16]);
    }

    /// Security invariant (koordinátorův hard gate + stejná pojistka jako
    /// `probe_error_never_contains_the_password` v lib.rs): connection
    /// string téhle fn nese OBĚ hesla plaintext — chybová cesta (tady
    /// unreachable 127.0.0.1:1, selže před jakoukoli auth výměnou) nesmí
    /// ani jedno z nich echo-ovat do error textu.
    #[test]
    fn change_password_error_never_contains_either_password() {
        let old_password = "oLd$ecretAaa1111";
        let new_password = "nEw$ecretBbb2222";
        let cfg = MssqlConfig::new("127.0.0.1", 1, "x", "x", new_password)
            .connect_timeout_sec(3);
        let err = change_password_at_connect(&cfg, old_password)
            .expect_err("connecting to 127.0.0.1:1 must fail");
        for secret in [old_password, new_password] {
            assert!(
                !err.message.contains(secret),
                "error text must never contain a password, got: {}",
                err.message
            );
        }
    }
}
