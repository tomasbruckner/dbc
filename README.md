# dbc

[![CI](https://github.com/tomasbruckner/dbc/actions/workflows/ci.yml/badge.svg)](https://github.com/tomasbruckner/dbc/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Databázový klient pro Windows: SQL Server, PostgreSQL, SQLite a DuckDB
v jednom okně. Editor s napovídáním, výsledky v mřížce, historie dotazů,
plány, monitor serveru, správa uživatelů, zálohy, porovnání schémat.

Rust + [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).
Rozhraní je česky. Licence MIT. Změny podle verzí v [CHANGELOG.md](CHANGELOG.md),
hlášení bezpečnostních chyb v [SECURITY.md](SECURITY.md), jak přispět
v [CONTRIBUTING.md](CONTRIBUTING.md).

## Instalace (pro kolegy)

1. Z [poslední verze na GitHubu](https://github.com/tomasbruckner/dbc/releases/latest)
   stáhni **`dbc-win-Setup.exe`** a spusť ho. Nainstaluje se do
   `%LocalAppData%\dbc` a přidá zástupce do nabídky Start. Kdo nechce
   instalovat, vezme `dbc-win-Portable.zip` a rozbalí ho kamkoli.
   Vedle aplikace (`dbc-ui.exe`) je `dbc.exe` (příkazová řádka) a
   `dbc-mcp.exe` (MCP server pro AI nástroje); po instalaci leží v
   `%LocalAppData%\dbc\current\`, u portable verze ve složce `current\`.
2. Windows SmartScreen se při prvním spuštění ozve, protože soubory
   nejsou podepsané: klikni na **Další informace → Přesto spustit**.
   Stane se to jednou.
3. Pro **SQL Server** je potřeba mít nainstalovaný
   [ODBC Driver 18 for SQL Server](https://learn.microsoft.com/sql/connect/odbc/download-odbc-driver-for-sql-server)
   od Microsoftu. Bez něj připojení k MSSQL skončí hláškou, která na
   tenhle odkaz ukáže. PostgreSQL, SQLite a DuckDB nic nepotřebují.

Aktualizace: aplikace se po startu podívá na GitHub, novou verzi si
stáhne a v horní liště ukáže tlačítko **Aktualizovat na vX.Y.Z**. Klik ji
nainstaluje a aplikaci restartuje; kdo tlačítko ignoruje, dostane novou
verzi při příštím spuštění, protože se nainstaluje po zavření. Nastavení
i historie jsou v `%APPDATA%\dbc` a aktualizace se jich nedotkne.

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
dotnet tool install -g vpk   # jednou: Velopack CLI
.\release.ps1                # Setup.exe, Portable.zip a balíčky do .\dist\releases
```

Verze je jen ve workspace `Cargo.toml`; do exe se dostane přes `build.rs`
každého binárního crate (version resource + ikona
`crates/dbc-ui/assets/dbc.ico`). Vydání = tag `vX.Y.Z` na commitu
`chore: vX.Y.Z`; GitHub Actions z něj postaví Release, ze kterého se
nainstalované aplikace aktualizují. Pro zkoušku aktualizace bez GitHubu
nastav `DBC_UPDATE_SOURCE` na složku s výstupem `vpk pack`.
