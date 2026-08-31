//! Command line parsing — pure, so the whole surface is testable.
//!
//! Hand-rolled rather than `clap`, for the same reason `dbc-mcp` hand-rolls
//! its three flags: the parse is the part a user meets first, and a parse
//! that can be unit-tested end to end is worth more here than the flags a
//! derive macro would give for free. Nothing in this file does I/O.

use std::path::PathBuf;

/// How results are written to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Aligned columns for a person reading the terminal.
    Table,
    /// One JSON object, for a script.
    Json,
    /// RFC-4180 CSV, for a spreadsheet or a pipe.
    Csv,
}

impl Format {
    fn parse(s: &str) -> Option<Format> {
        match s {
            "table" => Some(Format::Table),
            "json" => Some(Format::Json),
            "csv" => Some(Format::Csv),
            _ => None,
        }
    }
}

/// Where a `query`'s SQL text comes from. Resolved to actual text later —
/// this stage only records which one was asked for, so „two sources at
/// once" is a parse error rather than a silent precedence rule nobody can
/// remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlSource {
    Text(String),
    File(PathBuf),
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Every saved connection: name, engine, host, database, read-only.
    Connections,
    /// The databases on one connection.
    Databases { conn: String },
    /// The tables in one database.
    Tables { conn: String, schema: Option<String> },
    /// Run SQL and print the result.
    Query { conn: String, sql: SqlSource, write: bool },
    /// Store the derived vault key so scripts can run without a prompt.
    Login,
    /// Remove it again.
    Logout,
    /// Usage was ASKED for: goes to stdout, exit 0.
    Help,
    /// Version was asked for.
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub command: Command,
    /// Overrides the database the connection has saved. Ignored by the
    /// commands that have no database to pick.
    pub database: Option<String>,
    pub format: Format,
    pub row_limit: usize,
    pub timeout_secs: u64,
    pub config: Option<PathBuf>,
    pub vault: Option<PathBuf>,
}

/// The default cap on rows brought back into memory.
///
/// A CLI is a pipe: `dbc query … | head` looks harmless and `SELECT * FROM
/// events` against a real table is not. The cap is applied at result
/// consumption, not by rewriting the SQL, so it holds even for statements
/// no auto-limit could safely touch — and hitting it is REPORTED rather
/// than silently truncating.
pub const DEFAULT_ROW_LIMIT: usize = 1000;

/// The default per-statement timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

pub const USAGE: &str = "\
dbc — příkazová řádka nad připojeními uloženými v aplikaci

POUŽITÍ
    dbc <příkaz> [volby]

PŘÍKAZY
    connections              vypíše uložená připojení
    databases <conn>         vypíše databáze na připojení
    tables <conn>            vypíše tabulky v databázi
    query <conn> --sql <s>   spustí SQL a vypíše výsledek
    query <conn> --file <f>  spustí SQL ze souboru
    query <conn> -           spustí SQL ze standardního vstupu
    login                    uloží odvozený klíč trezoru pro neinteraktivní běh
    logout                   uložený klíč smaže

    <conn> je jméno připojení, jak ho vidíš v aplikaci (nebo jeho id).

VOLBY
    --db <jméno>             databáze místo té uložené u připojení
    --schema <jméno>         u `tables`: jen tohle schéma
    --write                  povolí i zapisující příkazy (viz níže)
    --format table|json|csv  podoba výstupu (výchozí table)
    --limit <n>              strop řádků (výchozí 1000, 0 = bez stropu)
    --timeout <s>            časový limit na příkaz (výchozí 60)
    --config <cesta>         jiný config.toml
    --vault <cesta>          jiný soubor trezoru
    -h, --help               tato nápověda
    -V, --version            verze

ZÁPIS
    Bez --write se spustí jen to, co jen čte; cokoli jiného se odmítne a
    nic se neprovede. --write je v příkazové řádce obdobou potvrzovacího
    dialogu v aplikaci: musíš ho napsat ty, pro tenhle jeden běh.
    Připojení označené jen pro čtení --write NEPŘEBIJE.

HESLO
    Hesla k připojením jsou v trezoru. Bez uloženého klíče se dbc zeptá na
    master heslo v terminálu; `dbc login` klíč uloží do úložiště pověření
    operačního systému, aby šlo dbc volat ze skriptu. Samotné heslo se
    neukládá nikam.

PŘÍKLADY
    dbc connections
    dbc query prodej --sql \"select * from objednavky where stav = 'nova'\"
    dbc query prodej --file report.sql --format csv > report.csv
    dbc tables prodej --db sklad --schema dbo
    dbc query prodej --file migrace.sql --write
";

/// A parse failure. `message` is already a finished sentence for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

fn err(message: impl Into<String>) -> ParseError {
    ParseError { message: message.into() }
}

/// Pulls the value that follows a flag, naming the flag if it is missing.
fn value(flag: &str, it: &mut std::vec::IntoIter<String>) -> Result<String, ParseError> {
    it.next().ok_or_else(|| err(format!("volbě {flag} chybí hodnota")))
}

/// Parse `argv` WITHOUT the program name.
///
/// The command word comes first and is required; every flag may appear in
/// any order after it. Flags that make no sense for the command are a hard
/// error rather than a silent no-op — `dbc connections --write` says so
/// instead of pretending it did something.
pub fn parse(argv: Vec<String>) -> Result<Args, ParseError> {
    let mut it = argv.into_iter();
    let Some(first) = it.next() else {
        return Err(err("chybí příkaz — zkus `dbc --help`"));
    };

    let mut database = None;
    let mut schema = None;
    let mut format = Format::Table;
    let mut row_limit = DEFAULT_ROW_LIMIT;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut config = None;
    let mut vault = None;
    let mut write = false;
    let mut sql: Option<SqlSource> = None;

    let simple = |command| Args {
        command,
        database: None,
        format: Format::Table,
        row_limit: DEFAULT_ROW_LIMIT,
        timeout_secs: DEFAULT_TIMEOUT_SECS,
        config: None,
        vault: None,
    };
    match first.as_str() {
        "-h" | "--help" | "help" => return Ok(simple(Command::Help)),
        "-V" | "--version" | "version" => return Ok(simple(Command::Version)),
        _ => {}
    }

    // The connection argument, for the commands that take one. Taken here
    // so a missing one is reported as a missing ARGUMENT rather than
    // surfacing later as an unknown flag.
    let needs_conn = matches!(first.as_str(), "databases" | "tables" | "query");
    let mut conn = String::new();
    if needs_conn {
        conn = match it.next() {
            Some(c) if !c.starts_with('-') => c,
            Some(other) => {
                return Err(err(format!("{first} chce nejdřív jméno připojení, ne {other}")))
            }
            None => return Err(err(format!("{first} chce jméno připojení"))),
        };
    }

    let set_sql = |src: SqlSource, sql: &mut Option<SqlSource>| -> Result<(), ParseError> {
        if sql.is_some() {
            return Err(err("SQL je zadané dvakrát — vyber jeden zdroj (--sql, --file, nebo -)"));
        }
        *sql = Some(src);
        Ok(())
    };

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sql" => set_sql(SqlSource::Text(value("--sql", &mut it)?), &mut sql)?,
            "--file" => {
                set_sql(SqlSource::File(PathBuf::from(value("--file", &mut it)?)), &mut sql)?
            }
            "-" => set_sql(SqlSource::Stdin, &mut sql)?,
            "--write" => write = true,
            "--db" => database = Some(value("--db", &mut it)?),
            "--schema" => schema = Some(value("--schema", &mut it)?),
            "--config" => config = Some(PathBuf::from(value("--config", &mut it)?)),
            "--vault" => vault = Some(PathBuf::from(value("--vault", &mut it)?)),
            "--format" => {
                let raw = value("--format", &mut it)?;
                format = Format::parse(&raw)
                    .ok_or_else(|| err(format!("neznámý formát {raw} — čekám table, json nebo csv")))?;
            }
            "--limit" => {
                let raw = value("--limit", &mut it)?;
                row_limit = raw
                    .parse()
                    .map_err(|_| err(format!("--limit chce nezáporné číslo, ne {raw}")))?;
            }
            "--timeout" => {
                let raw = value("--timeout", &mut it)?;
                timeout_secs = raw
                    .parse()
                    .map_err(|_| err(format!("--timeout chce nezáporné číslo, ne {raw}")))?;
                if timeout_secs == 0 {
                    return Err(err("--timeout 0 by znamenalo okamžité vypršení"));
                }
            }
            "-h" | "--help" => return Ok(simple(Command::Help)),
            "-V" | "--version" => return Ok(simple(Command::Version)),
            other => return Err(err(format!("neznámá volba {other}"))),
        }
    }

    let command = match first.as_str() {
        "connections" => Command::Connections,
        "databases" => Command::Databases { conn },
        "tables" => Command::Tables { conn, schema: schema.clone() },
        "query" => {
            let Some(sql) = sql else {
                return Err(err("query chce SQL — použij --sql, --file, nebo -"));
            };
            Command::Query { conn, sql, write }
        }
        "login" => Command::Login,
        "logout" => Command::Logout,
        other => return Err(err(format!("neznámý příkaz {other} — zkus `dbc --help`"))),
    };

    // Flags that would have been silently ignored. A CLI that accepts a
    // flag and does nothing with it teaches the wrong thing about every
    // other flag.
    if write && !matches!(command, Command::Query { .. }) {
        return Err(err("--write dává smysl jen u query"));
    }
    if schema.is_some() && !matches!(command, Command::Tables { .. }) {
        return Err(err("--schema dává smysl jen u tables"));
    }

    Ok(Args { command, database, format, row_limit, timeout_secs, config, vault })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, ParseError> {
        parse(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn no_arguments_is_an_error_that_points_at_help() {
        let e = p(&[]).unwrap_err();
        assert!(e.message.contains("--help"), "{}", e.message);
    }

    #[test]
    fn help_and_version_are_commands_not_errors() {
        for flag in ["-h", "--help", "help"] {
            assert_eq!(p(&[flag]).unwrap().command, Command::Help);
        }
        for flag in ["-V", "--version", "version"] {
            assert_eq!(p(&[flag]).unwrap().command, Command::Version);
        }
    }

    /// `--help` must work AFTER a command too — that is where a hand
    /// reaches for it when the command is the confusing part.
    #[test]
    fn help_after_a_command_still_prints_help() {
        assert_eq!(p(&["query", "prod", "--help"]).unwrap().command, Command::Help);
    }

    #[test]
    fn connections_needs_nothing_else() {
        let a = p(&["connections"]).unwrap();
        assert_eq!(a.command, Command::Connections);
        assert_eq!(a.format, Format::Table);
        assert_eq!(a.row_limit, DEFAULT_ROW_LIMIT);
    }

    #[test]
    fn a_command_that_needs_a_connection_says_so_when_it_is_missing() {
        let e = p(&["databases"]).unwrap_err();
        assert!(e.message.contains("připojení"), "{}", e.message);
        let e = p(&["query", "--sql", "select 1"]).unwrap_err();
        assert!(e.message.contains("připojení"), "{}", e.message);
    }

    #[test]
    fn query_takes_its_sql_from_any_of_the_three_sources() {
        let Command::Query { conn, sql, write } = p(&["query", "prod", "--sql", "select 1"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert_eq!(conn, "prod");
        assert_eq!(sql, SqlSource::Text("select 1".into()));
        assert!(!write);

        let Command::Query { sql, .. } = p(&["query", "prod", "--file", "a.sql"]).unwrap().command
        else {
            panic!()
        };
        assert_eq!(sql, SqlSource::File(PathBuf::from("a.sql")));

        let Command::Query { sql, .. } = p(&["query", "prod", "-"]).unwrap().command else {
            panic!()
        };
        assert_eq!(sql, SqlSource::Stdin);
    }

    /// Two sources is a mistake, not a precedence question. Silently
    /// picking one would mean a script that runs the wrong file.
    #[test]
    fn two_sql_sources_at_once_is_refused() {
        let e = p(&["query", "prod", "--sql", "select 1", "--file", "a.sql"]).unwrap_err();
        assert!(e.message.contains("dvakrát"), "{}", e.message);
        let e = p(&["query", "prod", "--file", "a.sql", "-"]).unwrap_err();
        assert!(e.message.contains("dvakrát"), "{}", e.message);
    }

    #[test]
    fn query_without_any_sql_is_refused() {
        let e = p(&["query", "prod"]).unwrap_err();
        assert!(e.message.contains("--sql"), "{}", e.message);
    }

    #[test]
    fn write_is_off_unless_it_is_asked_for() {
        let Command::Query { write, .. } =
            p(&["query", "prod", "--sql", "delete from t", "--write"]).unwrap().command
        else {
            panic!()
        };
        assert!(write);
    }

    /// A flag the command cannot use is an error, never a no-op.
    #[test]
    fn flags_that_belong_to_another_command_are_refused() {
        assert!(p(&["connections", "--write"]).unwrap_err().message.contains("query"));
        assert!(p(&["connections", "--schema", "dbo"]).unwrap_err().message.contains("tables"));
    }

    #[test]
    fn formats_parse_and_a_bad_one_names_the_alternatives() {
        assert_eq!(p(&["connections", "--format", "json"]).unwrap().format, Format::Json);
        assert_eq!(p(&["connections", "--format", "csv"]).unwrap().format, Format::Csv);
        let e = p(&["connections", "--format", "xml"]).unwrap_err();
        assert!(e.message.contains("csv"), "{}", e.message);
    }

    #[test]
    fn numeric_flags_reject_junk_and_a_zero_timeout() {
        assert!(p(&["connections", "--limit", "x"]).unwrap_err().message.contains("--limit"));
        assert!(p(&["connections", "--timeout", "x"]).unwrap_err().message.contains("--timeout"));
        assert!(p(&["connections", "--timeout", "0"]).unwrap_err().message.contains("vypršení"));
        assert_eq!(p(&["connections", "--limit", "0"]).unwrap().row_limit, 0);
    }

    #[test]
    fn a_flag_without_its_value_names_the_flag() {
        let e = p(&["query", "prod", "--sql"]).unwrap_err();
        assert!(e.message.contains("--sql"), "{}", e.message);
    }

    #[test]
    fn an_unknown_flag_or_command_is_named_back() {
        assert!(p(&["connections", "--nope"]).unwrap_err().message.contains("--nope"));
        assert!(p(&["frobnicate"]).unwrap_err().message.contains("frobnicate"));
    }

    /// A connection name is a positional, so a flag in its place is a
    /// mistake worth naming rather than a connection called „--sql".
    #[test]
    fn a_flag_where_the_connection_should_be_is_refused() {
        let e = p(&["query", "--sql", "select 1"]).unwrap_err();
        assert!(e.message.contains("připojení"), "{}", e.message);
    }

    #[test]
    fn paths_and_the_database_override_come_through() {
        let a = p(&[
            "tables", "prod", "--db", "sales", "--schema", "dbo", "--config", "c.toml", "--vault",
            "v.bin",
        ])
        .unwrap();
        assert_eq!(a.database.as_deref(), Some("sales"));
        assert_eq!(a.config, Some(PathBuf::from("c.toml")));
        assert_eq!(a.vault, Some(PathBuf::from("v.bin")));
        assert_eq!(a.command, Command::Tables { conn: "prod".into(), schema: Some("dbo".into()) });
    }

    #[test]
    fn login_and_logout_take_no_connection() {
        assert_eq!(p(&["login"]).unwrap().command, Command::Login);
        assert_eq!(p(&["logout"]).unwrap().command, Command::Logout);
    }

    /// The example lines under PŘÍKLADY, as they appear in the usage text.
    fn usage_examples() -> Vec<String> {
        USAGE
            .lines()
            .skip_while(|l| !l.starts_with("PŘÍKLADY"))
            .map(|l| l.trim())
            .filter(|l| l.starts_with("dbc "))
            .map(|l| l.to_string())
            .collect()
    }

    /// Split an example the way a shell would, well enough for these:
    /// double quotes group, and a `>` ends the command (what follows is a
    /// redirection, not an argument).
    fn shell_split(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        for c in line.chars() {
            match c {
                '"' => quoted = !quoted,
                '>' if !quoted => break,
                c if c.is_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// Every example in the help must actually run through the parser. A
    /// help text showing a command line the binary rejects is worse than
    /// no help at all — and examples are exactly the part that rots when a
    /// flag is renamed.
    #[test]
    fn every_example_in_the_usage_text_parses() {
        let examples = usage_examples();
        assert!(examples.len() >= 3, "the examples section vanished: {examples:?}");
        for ex in examples {
            let argv = shell_split(&ex);
            assert_eq!(argv.first().map(String::as_str), Some("dbc"), "{ex}");
            let parsed = parse(argv[1..].to_vec());
            assert!(parsed.is_ok(), "example does not parse: {ex}
{:?}", parsed.unwrap_err());
        }
    }

    /// The splitter above has to be right, or the test it feeds passes for
    /// the wrong reason.
    #[test]
    fn the_example_splitter_handles_quotes_and_redirection() {
        assert_eq!(shell_split("dbc query p --sql \"a b\""), ["dbc", "query", "p", "--sql", "a b"]);
        assert_eq!(shell_split("dbc connections > out.csv"), ["dbc", "connections"]);
    }

    /// The usage text is the only documentation this binary ships with, so
    /// every command it accepts has to appear in it. Cheap, and it catches
    /// the commit that adds a command and forgets the help.
    #[test]
    fn every_command_appears_in_the_usage_text() {
        for word in
            ["connections", "databases", "tables", "query", "login", "logout", "--write", "--format"]
        {
            assert!(USAGE.contains(word), "usage never mentions {word}");
        }
    }
}
