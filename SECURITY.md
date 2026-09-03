# Security

dbc stores database passwords, so it is worth reporting a hole quietly
and giving it a fix before it is public.

## Reporting

Use GitHub's private vulnerability reporting:
<https://github.com/tomasbruckner/dbc/security/advisories/new>.
The report reaches only the maintainer. Please do not open a public issue
for anything that could expose credentials or data.

Expect an acknowledgement within a week. Fixes ship as a normal release;
the advisory is published once the release is out.

## What is in scope

- `vault.bin`: the encrypted password store (Argon2id key derivation,
  ChaCha20-Poly1305). A way to read it without the master password, or
  to weaken the key, is the most serious thing you can find here.
- Credentials leaking into `config.toml`, `dbc.log`, `history.sqlite`,
  the settings bundle (`.dbcx`), the clipboard, or the window title.
- SQL the app writes on your behalf (autocomplete, context-menu actions,
  admin panel, backups): anything that lets a crafted schema or result
  inject statements you did not ask for.
- The MCP server (`dbc-mcp`): anything that lets a model reach a
  connection, database, or statement the configuration did not allow.

## Out of scope

- Bugs that need the attacker to already have your Windows account.
- The security of the database servers you connect to.
- Missing code signing on the shipped exe (known; see README).

## Supported versions

Only the latest release gets fixes. There is no LTS line.
