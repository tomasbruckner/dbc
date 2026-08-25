# Vynucená změna hesla serverem — design (malá fáze, v0.21.0)

Požadavek (uživatel, doslova): „nekdy muze prijit ze serveru ze si uzivatel
musi zmenit heslo, tak by to mel byt schopny zmenit" — když server při
připojení oznámí, že heslo musí být změněno (nebo mu vypršela platnost),
aplikace nabídne dialog změny a po úspěchu nové heslo ihned uloží do
Argon2id trezoru. Nikdy nemění heslo automaticky.

## 0. Výsledky investigace (vše ověřeno ŽIVĚ 2026-08-25)

Živé sondy: SQL Server 2022 v dockeru + msodbcsql18 + odbc-api 29 (probe
binárka), PostgreSQL 16 v dockeru. Kontejnery po sondě odstraněny.

### MSSQL (rozhoduje o proveditelnosti — POTVRZENO)

* Login s `MUST_CHANGE` (chyba 18488) i s expirovaným heslem (18487)
  blokuje KAŽDÝ normální connect. `SQLDriverConnect` selže a odbc-api to
  vrací jako `Error::Diagnostics { record, function: "SQLDriverConnect" }`
  s `record.state = "28000"` a **`record.native_error = 18488`** (změřeno:
  message „Login failed for user 'pwuser'. Reason: The password of the
  account must be changed."). SQLSTATE 28000 sdílí i obyčejné špatné heslo
  (18456) — rozlišení je JEN v `native_error`, které dnešní
  `types.rs::odbc_err` zahazuje (mapuje jen SQLSTATE do `QueryError.code`).
* Mechanismus změny: driver-level, při loginu. msodbcsql podporuje
  `SQL_COPT_SS_OLDPWD` (= 1226, ověřeno v instalovaném
  `msodbcsql.h`: `SQL_COPT_SS_BASE(1200)+26`, „Old Password, used when
  changing password during login"): `SQLSetConnectAttrW` PŘED connectem
  nastaví staré heslo, `Pwd=` v connection stringu nese NOVÉ heslo, driver
  změnu provede během přihlášení. **Záměrně neexistuje connection-string
  klíčové slovo** (kolidovalo by s poolingem) — musí to být atribut.
* Živě ověřený průběh přes odbc-api 29: `handles::Environment::new()` +
  `declare_version(Odbc3_80)` + `allocate_connection()` +
  `odbc_api::sys::SQLSetConnectAttrW(conn.as_sys(),
  ConnectionAttribute(1226), ptr_utf16_nul, NTS as i32)` → `SqlReturn(0)`,
  `connect_with_connection_string` → `SuccessWithInfo`, a následný běžný
  connect s novým heslem uspěl. Trait `SetConnectionAttribute` je v
  privátním modulu → jde to POUZE přes `as_sys()` + přímé
  `odbc_api::sys::SQLSetConnectAttrW` (žádná nová závislost, odbc-api
  re-exportuje odbc-sys jako `odbc_api::sys`).
* Důsledek pro flow: **admin účet není potřeba** — login si mění heslo
  sám při připojení. 18488 zároveň implikuje, že staré heslo bylo SPRÁVNÉ
  (jinak by přišlo 18456) ⇒ trezor byl odemčený a staré heslo známe.

### PostgreSQL

* `VALID UNTIL` v minulosti: klient dostane **jen generické 28P01**
  „password authentication failed for user…" — detail „User has an
  expired password" jde POUZE do server logu (ověřeno živě; odpovídá
  `src/backend/libpq/auth.c`, errdetail_log). Expirované a špatné heslo
  jsou z klienta NEROZLIŠITELNÉ; `tokio-postgres` to vrací jako db error
  → `pg_err` už dnes plní `code = Some("28P01")`.
* Expirovaný uživatel se přes password-auth NEPŘIPOJÍ, sám si tedy heslo
  nezmění (přes trust/peer hba by se připojil a řeší to existující admin
  panel — mimo tento flow). Záchrana = jiný účet s CREATEROLE/superuser.
* **Klíčový živý nález:** `ALTER ROLE x PASSWORD 'new'` samo o sobě
  NEPOMŮŽE — `rolvaliduntil` zůstává v minulosti a login dál padá.
  Funguje až `ALTER ROLE x PASSWORD 'new' VALID UNTIL 'infinity'`
  (ověřeno v obou krocích). Záchranný příkaz proto VŽDY obsahuje
  `VALID UNTIL 'infinity'` a dialog to zobrazuje (redigovaný display_sql).

### SQLite / DuckDB

Žádná serverová autentizace — mimo rozsah (stejná výjimka jako celé G10).

## 1. Detekce (nikdy auto-změna)

* Driver MSSQL: `odbc_err` nově mapuje `state == "28000" &&
  native_error ∈ {18487, 18488}` na sentinel
  `QueryError.code = Some("password_change_required")` — stejný vzor jako
  existující normalizace `HY008 → "cancelled"`. Konstanta
  `PASSWORD_CHANGE_REQUIRED_CODE` žije v `dbc-driver-mssql::types` (dbc-ui
  na driver už závisí).
* UI: čistá fn `pwchange::detect(engine, &QueryError) ->
  Option<PwChangeKind>`; `Mssql` + sentinel → `MssqlMustChange`;
  `Postgres` + `"28P01"` → `PgMaybeExpired`; jinak `None`.
* Kde se detekce napojí: selhání v `switch_to_database` (primární cesta
  „kliknu na připojení") → otevře dialog (deferred-focus vzor
  `open_vault_prompt`; když už jiný modal běží, zůstane jen status).
  Tlačítko „Test" v dialogu připojení modal otevřít nesmí (single-modal
  invariant) — jen připojí českou nápovědu k chybovému textu.
  Sidebar/query cesty nechávají dosavadní chování (status/retry řádek).

## 2. Dialog „Změna hesla na serveru" (ModalState varianta)

Pole „Nové heslo" + „Nové heslo znovu" (obě maskovaná, `form_field`
vzor); PG navíc „Admin uživatel" + „Heslo admin uživatele". Esc nezavírá,
je-li v libovolném heslovém poli text (vzor `admin_esc_closable`).
Transparentnost: PG větev zobrazuje redigovaný `display_sql`
(`ALTER ROLE „x" PASSWORD '***' VALID UNTIL 'infinity'`), MSSQL větev
text, že změna proběhne ODBC mechanismem při přihlášení (žádné SQL).

## 3. Provedení

* **MSSQL:** nová runner metoda `change_mssql_password(cfg, old, new)`
  (spawn_blocking, jediná sankcionovaná cesta z UI) → `connect.rs` →
  `dbc_driver_mssql::change_password_at_connect(&MssqlConfig{password:
  new,…}, old)` (nový modul `password.rs`, per-call krátkoživotný
  `handles::Environment` — druhé env handle je na Windows driver manageru
  v pořádku, komentář v lib.rs o „jednom env" je doporučení odbc-api pro
  sdílení, ne tvrdý zákaz; dokumentováno u fn). Staré heslo jako UTF-16
  NUL-terminated buffer, po použití přepsán nulami; parametry
  `Zeroizing<String>`. `read_only` připojení změnu NEblokuje — jde o
  údržbu přihlášení, ne o zápis dat (guard `guard_not_read_only` se týká
  SQL write cesty).
* **PG:** existující sankcionovaná cesta `run_write_transaction` se
  spec-em z klonu configu: `user = admin_user`, `secret = admin_password`,
  `read_only = false` (server-side by `default_transaction_read_only=on`
  ALTER stejně odmítl; jde o explicitně potvrzenou credential operaci).
  Statement z nového builderu `admin_sql::alter_password_rescue_pg(name,
  password) -> WriteStatement` — paralelní konstrukce display/exec jako
  `alter_password` (display `'***'`, exec skutečné heslo, hand-written
  Debug už `WriteStatement` má). Úspěch se zapíše do historie kind
  `"admin"` s `display_sql` (stejně jako apply dialog); MSSQL do historie
  nezapisuje nic (žádné SQL neproběhlo).

## 4. Trezor a bezpečnost

* Po úspěchu OBOU větví ihned `vault.set_secret(&cfg.id, new)` — heslo
  nikdy neleží v configu, logu ani historii; jediná plaintext existence:
  maskovaná pole dialogu + `Zeroizing` hodnoty v letu + `exec_sql`
  (pokrytý existujícím redigujícím Debugem a display/exec disciplínou).
* MSSQL staré heslo se čte z trezoru až při potvrzení (18488 ⇒ trezor
  odemčený); defenzivní větev: trezor nedostupný → chyba v dialogu,
  žádné PendingAfterUnlock rozšíření není potřeba.
* PG admin credentials se NIKAM neukládají — žijí jen v dialogu a spec-u.

## 5. Rozhodnutí učiněná bez dotazu (s odůvodněním)

1. PG dialog se otevírá na KAŽDÉ 28P01 při přepnutí na připojení — server
   expiraci od překlepu nerozlišuje; text to říká a „Zrušit"/Esc nic
   nestojí. Alternativa (neotvírat nic) by nesplnila zadání.
2. `VALID UNTIL 'infinity'` je pevná součást PG záchrany (živě dokázáno,
   že bez toho záchrana nefunguje); checkbox by byl faleš volby.
3. MSSQL live test je `#[ignore]` v existující testcontainers harness
   (`tests/mssql_integration.rs` + `conn_str_or_skip`) — gate zůstává
   bez serveru zelený, mechanismus byl nadto ověřen touto sondou.

## 6. Mimo rozsah

SQLite/DuckDB (bez auth); Windows/Entra auth MSSQL; nabídka změny ze
sidebar/query chybových cest; proaktivní hlídání blížící se expirace
(`LOGINPROPERTY('DaysUntilExpiration')`, `rolvaliduntil`) — případná
další fáze.
