# Privacy policy

Applies to dbc: the desktop app (`dbc-ui.exe`), the command line
(`dbc.exe`) and the MCP server (`dbc-mcp.exe`), all versions distributed
from <https://github.com/tomasbruckner/dbc> and through winget.

Last updated: 2026-09-29.

## Summary

- dbc has no servers, no accounts, no telemetry, no analytics and no
  crash reporting. The maintainer receives nothing from your copy of dbc.
- Everything dbc stores stays on your computer, in the places listed
  below.
- dbc talks to the network in exactly two ways: to the database servers
  you configure, and to GitHub to check for and download updates (which
  you can turn off).
- AI tools see nothing through the MCP server until you enable
  individual connections for it.

## What is stored locally, and where

The profile folder is `%APPDATA%\dbc` (or the folder in the
`DBC_DATA_DIR` environment variable, if set).

| file | contents |
|---|---|
| `config.toml` | connections: name, engine, host, port, database, user name, TLS and SSH options, folders; UI settings. **No passwords.** |
| `vault.bin` | connection passwords, encrypted (see *Credentials*) |
| `history.sqlite` | query history: full SQL text, connection name, time, duration, row count, error text |
| `dbc.log`, `dbc.log.1` | diagnostic log (see *Logs*) |
| `sessions\*.toml` | open editor tabs, including unsaved SQL text, restored on next start |
| `params.toml` | last values you typed for `:param` placeholders |
| `views.toml` | per-table grid layout (hidden columns, widths, sort) |
| `schema-cache\` | database structure (object names, types, routine definitions), no table data; capped at 128 MB |
| `connection-cache.json` | server version and database names per connection |
| `workspace.toml` | path of your workspace folder, if you use one |

If you set up a **workspace folder**, `config.toml`, `vault.bin`,
`views.toml`, `params.toml` and your SQL scripts live there instead.
Query history and the log never go into the workspace. dbc never runs
git or reads `.git`; if you put the workspace under version control,
what you commit and push is your decision. The generated `.gitignore`
leaves the (encrypted) `vault.bin` included by default and says how to
exclude it.

**Query results** are held in memory. A very large result (over 500,000
rows or 256 MB) is partly written to a temporary folder under `%TEMP%`
and deleted when the result tab is closed.

**Files you create on purpose** go where you choose: CSV/JSON exports,
ER-diagram images, database backups, settings exports (`.dbcx`), SQL
scripts.

## Credentials

- Passwords are stored only in `vault.bin`, encrypted with a key derived
  from your master password (Argon2id key derivation, ChaCha20-Poly1305
  encryption). The master password itself is never written anywhere.
- While the vault is unlocked, the app keeps the key and the passwords
  in memory; they are wiped from memory when the vault is locked or the
  app exits.
- The desktop app does not use the Windows Credential Manager.
- The command line and the MCP server can, if you ask them to, store the
  derived vault key (never the master password) in the **Windows
  Credential Manager**, so they can run without prompting:
  - `dbc login` stores it under the name `dbc-cli`; `dbc logout` removes it.
  - `dbc-mcp setup` stores it under the name `dbc-mcp`;
    `dbc-mcp setup --remove` removes it.
- A settings export (`.dbcx`) contains the configuration, grid layouts
  and the vault **still encrypted**. It does not contain history, logs,
  sessions or caches.
- When dbc runs `pg_dump`, `pg_restore` or `psql` for backups, the
  password is passed in the child process environment, not on the
  command line.

## Query history

History records every statement you run from the app or the command
line, including failed ones, so you can find and re-run them. It is
kept only on this computer, is never exported or synchronised, and has
no automatic expiry. You can delete it at any time:

- in the app: the history tab → **Vymazat…** (everything, or everything
  except starred entries), or **✕** on a single entry; also
  **Vymazat historii dotazů…** in the command palette;
- from the command line: `dbc history clear` (add `--keep-starred` to
  keep starred entries).

Clearing removes the text from the file itself, not just from the list.
The MCP server does not write history.

## Logs

- `dbc.log` records what the app did, so problems can be diagnosed. It
  never contains passwords, connection strings, SQL text or result
  data. It does contain connection ids, database and object names,
  statement kinds, row counts, and error messages from database
  drivers, which can include host names, user names or parts of a
  statement.
- The log is capped at about 4 MB (one 2 MB file plus one older copy);
  older entries are discarded. You can open it from the app menu or
  delete it at any time.
- The log is never sent anywhere. If you share it in a bug report,
  review it first.
- The command line writes no log file.
- The MCP server writes one line per tool call to its standard error
  output: tool, connection name, statement kind (SELECT, …), statement
  length, row count, duration and any error message. It never writes
  the SQL text or result data. Whether that output is saved to disk
  depends on your AI client.

## Data sent to database servers

dbc connects only to the servers you configure and sends them your user
name, password and the SQL you run (plus the metadata queries needed for
autocomplete and the object tree). Encryption in transit depends on your
settings:

- PostgreSQL: `sslmode` `disable`, `prefer` (default), `require` or
  `verify-full`; only `verify-full` verifies the server certificate.
- SQL Server: encryption with certificate validation is on by default.
- SSH tunnels use your system OpenSSH client with key or agent
  authentication.

What a server does with that data is governed by whoever runs the
server. DuckDB can reach the network on its own if your SQL asks it to
(for example reading a remote file or installing an extension).

## MCP server and AI tools

`dbc-mcp` lets an AI tool (an MCP client) query your databases. It runs
only when your MCP client starts it, and communicates only with that
client over standard input/output; it opens no network port.

- **What it can reach:** only the connections on which you turned on
  **Dostupné pro AI (MCP)** in the connection dialog. It is off for every
  connection until you turn it on, including connections saved by
  versions before this switch existed. SQL Server connections and
  connections that use an SSH tunnel are never available. Any other
  connection is indistinguishable, to the AI client, from one that does
  not exist.
- **Read-only:** it has no write tools. Every statement is checked and
  rejected unless it only reads, and the connection itself is opened
  read-only as a second safeguard.
- **Limits:** at most 1,000 rows (200 by default) and 120 seconds per
  query.
- **What leaves your computer:** the connection list (names and engines,
  no hosts or user names), database structure, and the rows your
  queries return are passed to the AI client. If that client uses a
  cloud model, the data is sent to that model's provider under the
  client's own privacy terms, not this policy.
- Access is removed per connection by turning the switch off, or
  entirely by removing dbc-mcp from your AI client and running
  `dbc-mcp setup --remove`.

## Updates

On each start, an installed copy of the app asks GitHub
(`api.github.com`, `github.com`) for the latest release and, if a newer
one exists, downloads it from GitHub. You can turn this off in
**Nastavení → Aktualizace**; the app then makes no request to GitHub at
all. The request carries no user,
device or usage identifiers, only what any HTTPS request reveals (your
IP address and a user-agent). GitHub's handling of those requests is
covered by the [GitHub privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement).
Portable copies that were not installed, the command line and the MCP
server do not check for updates. Installing through winget uses
winget's own download from GitHub.

## Telemetry and third parties

None. dbc does not collect usage statistics, crash reports or any other
data, and shares nothing with third parties. The only external services
involved are the ones listed above: your database servers, GitHub for
updates, and an AI client if you connect one.

## Deleting your data

1. Uninstall dbc (Settings → Apps, or `winget uninstall TomasBruckner.dbc`).
2. Delete `%APPDATA%\dbc`.
3. Run `dbc logout` and `dbc-mcp setup --remove` before uninstalling,
   or delete the `dbc-cli` and `dbc-mcp` entries in Windows Credential
   Manager.
4. Delete any workspace folder, exports and backups you created.

## Contact

Questions about this policy: open an issue at
<https://github.com/tomasbruckner/dbc/issues>. Security issues: see
[SECURITY.md](SECURITY.md). Changes to this policy are tracked in this
file's git history.
