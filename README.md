# dbc

Databázový klient pro Windows: SQL Server, PostgreSQL, SQLite a DuckDB
v jednom okně. Editor s napovídáním, výsledky v mřížce, historie dotazů,
plány, monitor serveru, správa uživatelů, zálohy, porovnání schémat.

## Instalace (pro kolegy)

1. Stáhni `dbc-<verze>-windows-x64.zip` a rozbal ho kamkoli, třeba do
   `C:\Users\<ty>\Apps\dbc`. Uvnitř je `dbc-ui.exe` (aplikace), `dbc.exe`
   (příkazová řádka) a `dbc-mcp.exe` (MCP server pro AI nástroje).
2. Spusť `dbc-ui.exe`. Windows SmartScreen se při prvním spuštění ozve,
   protože exe není podepsané: klikni na **Další informace → Přesto
   spustit**. Stane se to jednou.
3. Pro **SQL Server** je potřeba mít nainstalovaný
   [ODBC Driver 18 for SQL Server](https://learn.microsoft.com/sql/connect/odbc/download-odbc-driver-for-sql-server)
   od Microsoftu. Bez něj připojení k MSSQL skončí hláškou, která na
   tenhle odkaz ukáže. PostgreSQL, SQLite a DuckDB nic nepotřebují.

Aktualizace: zatím ručně, stáhni nový zip a přepiš soubory. Nastavení
i historie zůstanou, nejsou u exe.

## Kde jsou moje data

Všechno je v `%APPDATA%\dbc`:

| soubor | co v něm je |
|---|---|
| `config.toml` | připojení, složky, vzhled |
| `vault.bin` | hesla, šifrovaná master heslem (Argon2id + ChaCha20-Poly1305) |
| `history.sqlite` | historie dotazů, jen na tomhle počítači |
| `dbc.log` | log aplikace, sem se dívej, když se něco pokazí |

Nastavení jde přenést jinam přes **☰ → Export nastavení**, hesla jdou
s ním jen zašifrovaně.

## Základní ovládání

| zkratka | akce |
|---|---|
| Ctrl+Enter | spustit dotaz |
| Ctrl+Space | napovídání |
| Ctrl+N / Ctrl+W | nový / zavřít tab s dotazem |
| Ctrl+Tab | další tab |
| Ctrl+D | vybrat databázi napříč připojeními |
| Ctrl+K | paleta příkazů |
| Ctrl+H | historie |
| Ctrl+Shift+F | naformátovat SQL |
| Ctrl+Shift+E | rozbalit `*` na sloupce |
| F1 | přehled všech zkratek |

## Vývoj

Rust (stable), GPUI (Zed, pinovaná revize). Buildy jdou do
`target-shared` (viz `.cargo/config.toml`, lokální, netrackovaný soubor).

```powershell
cargo run -p dbc-ui          # dev build, data v .\data místo %APPDATA%
cargo test --workspace       # ~1 800 testů, bez Dockeru
.\release.ps1                # release zip do .\dist
```

Verze je jen ve workspace `Cargo.toml`; do exe se dostane přes
`crates/dbc-ui/build.rs` (version resource + ikona `assets/dbc.ico`).
