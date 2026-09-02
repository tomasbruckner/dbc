//! `dbc-mcp` — MCP server exposing dbc's saved connections to LLM tools,
//! read-only (design doc: docs/superpowers/specs/drafts/mcp-server-design.md).
//!
//! ## Setup (run once, manually, from a terminal)
//!
//! ```text
//! dbc-mcp setup
//! ```
//!
//! Prompts for the vault's master password (no echo), verifies it by
//! unlocking the vault, then stores the Argon2id-DERIVED key (never the
//! password) in the Windows Credential Manager. `dbc-mcp setup --remove`
//! deletes it again (revocation).
//!
//! ## Registering with an MCP client
//!
//! Claude Code (no secrets in the command — the key lives in the OS
//! credential store, not in any client config file):
//!
//! ```text
//! claude mcp add dbc -- dbc-mcp
//! ```
//!
//! Claude Desktop (`claude_desktop_config.json`):
//!
//! ```json
//! { "dbc": { "command": "dbc-mcp" } }
//! ```
//!
//! ## stdout is sacred
//!
//! Nothing but the MCP JSON-RPC stream may ever write to stdout — all
//! logging goes to stderr. A stray `println!` anywhere in this crate or a
//! transitive dependency corrupts the protocol stream.

mod connect;
mod keysource;
mod serialize;
mod tools;

use std::path::PathBuf;
use std::process::ExitCode;

use dbc_state::{AppConfig, Vault};
use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

use keysource::{KeySource, KEYRING_SERVICE, KEYRING_USER};
use tools::McpServer;

enum Command {
    Serve,
    Setup { remove: bool },
    /// The operator ASKED for usage (`--help` / `-h`) — stderr, exit 0.
    Help,
    /// The operator got usage because their argv was wrong (unknown flag,
    /// or a flag whose value is missing). The offending argument has
    /// already been named on stderr by the parse loop; this variant exists
    /// so the exit code says "you did not get what you asked for".
    ///
    /// Review MINOR-2: this used to be `Help`, so a typo'd flag exited 0
    /// AND — once Task 6 made `Help` exempt from the broken-pointer check
    /// — suppressed the workspace diagnosis entirely. `dbc-mcp --confg
    /// C:\ws\config.toml` against a broken pointer reported nothing worth
    /// acting on and exited 0.
    Usage,
    /// Design §W7: the pointer file names a workspace this build cannot
    /// use, AND the command actually needs a path it would have supplied.
    /// One stderr MESSAGE — two lines, the diagnosis and the escape hatch
    /// (`workspace_broken_message` guarantees exactly that shape, see
    /// review MINOR-3) — and a non-zero exit. dbc-mcp must not silently
    /// serve profile-mode connections any more than the GUI may silently
    /// show them (§W4).
    Fail(String),
}

struct Args {
    config: PathBuf,
    vault: PathBuf,
    command: Command,
}

/// Shown when the POINTER itself could not be read, so there is no folder
/// path to name.
///
/// **CROSS-CRATE TWIN of `dbc_ui::connections_ui::WORKSPACE_MISSING_NO_PATH`**
/// (byte-pinned there by `workspace_missing_text_tests`, and carrying the
/// reciprocal pointer back to this const). The blocking GUI modal (§W4)
/// and this headless server describe the SAME condition; they must not
/// drift into two different Czech sentences. A copy sweep that rewords one
/// MUST reword the other — both are byte-pinned, so both tests fail
/// together and neither side can be changed quietly.
///
/// Review MINOR-1: this was an unpinned inline literal, and the only test
/// that touched the `None` branch was satisfied entirely by the `reason`
/// argument — `None => String::new()` left the whole suite green.
const WORKSPACE_MISSING_NO_PATH: &str = "ukazatel na pracovní prostor je nečitelný";

/// The stderr message for a broken pointer. Names the folder (or says the
/// pointer itself is unreadable), names the reason, and points at the
/// escape hatch — nothing else: no config contents, no vault bytes, no
/// connection names (`stdout is sacred`, and stderr is a log too).
///
/// Exactly TWO lines, always: the diagnosis and the escape hatch. That is
/// a tested property, not an aspiration — see
/// [`dbc_state::workspace::one_line_reason`], which T10 moved out of this
/// file so the blocking GUI modal gets the same treatment (carry-forward
/// 5). Note it is applied to BOTH halves: the pointer's `path` field is
/// arbitrary TOML text and a hand-edited `\n` in it would otherwise buy a
/// third line of attacker-chosen output (carry-forward 3).
///
/// **NOTE FOR THE TASK 10 SWEEP — the composed text stutters, and it is
/// PHASE-WIDE, not an MCP quirk.** With `root: None` the message reads
/// „…: ukazatel na pracovní prostor je nečitelný (ukazatel na pracovní
/// prostor je poškozený: …)", because `WORKSPACE_MISSING_NO_PATH` (the
/// subject) and `dbc-state`'s `read_pointer` reason (the predicate) both
/// name the pointer. The GUI modal composes the same two halves and
/// stutters identically (`connections_ui.rs`, `render_workspace_missing`'s
/// `path_line` + `reason`). T10 looked and left it: both halves are
/// verbatim from binding sources pinned by two different tasks, and
/// quietly harmonising copy across a seam is the trap this phase has
/// already recorded twice in the §W3.1 as-built addenda. Reword BOTH
/// crates together, or neither.
fn workspace_broken_message(root: &Option<PathBuf>, reason: &str) -> String {
    use dbc_state::workspace::one_line_reason;
    let where_ = match root {
        Some(r) => one_line_reason(&r.display().to_string()),
        None => WORKSPACE_MISSING_NO_PATH.to_string(),
    };
    let reason = one_line_reason(reason);
    format!(
        "dbc-mcp: pracovní prostor není použitelný: {where_} ({reason})\n\
         Otevřete aplikaci dbc a prostor obnovte, nebo spusťte dbc-mcp s explicitními cestami: --config <path> --vault <path>"
    )
}

/// Pure core of [`parse_args`] — takes the raw arguments and the ALREADY
/// resolved workspace state, so the whole precedence rule (explicit flags >
/// workspace defaults > profile defaults; broken ⇒ fail when the default
/// would be used) is unit-testable without env vars or a filesystem.
fn parse_args_from(raw: &[String], res: dbc_state::workspace::Resolution) -> Args {
    use dbc_state::workspace::Resolution;
    let (mut config, mut vault, broken) = match res {
        Resolution::Profile(p) => (p.config, p.vault, None),
        Resolution::Workspace { paths, .. } => (paths.config, paths.vault, None),
        // Deliberately NOT profile paths: if a `Fail` ever leaked through,
        // the paths it carries must not open the profile's real files.
        Resolution::Broken { root, reason } => {
            (PathBuf::new(), PathBuf::new(), Some(workspace_broken_message(&root, &reason)))
        }
    };
    let mut config_explicit = false;
    let mut vault_explicit = false;
    let mut is_setup = false;
    let mut remove = false;
    let mut help = false;
    let mut argv_error = false;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "setup" => is_setup = true,
            "--remove" => remove = true,
            "--help" | "-h" => help = true,
            "--config" => {
                i += 1;
                if let Some(v) = raw.get(i) {
                    config = v.into();
                    config_explicit = true;
                } else {
                    eprintln!("dbc-mcp: --config requires a path");
                    argv_error = true;
                }
            }
            "--vault" => {
                i += 1;
                if let Some(v) = raw.get(i) {
                    vault = v.into();
                    vault_explicit = true;
                } else {
                    eprintln!("dbc-mcp: --vault requires a path");
                    argv_error = true;
                }
            }
            other => {
                eprintln!("dbc-mcp: unrecognized argument '{other}'");
                argv_error = true;
            }
        }
        i += 1;
    }

    // An argv error OUTRANKS an explicit `--help` (`dbc-mcp --help
    // --confg x` must not exit 0): the operator's argv was wrong either
    // way, and the usage text is printed for both.
    let command = if argv_error {
        Command::Usage
    } else if help {
        Command::Help
    } else if is_setup {
        Command::Setup { remove }
    } else {
        Command::Serve
    };
    // §W7: fatal only when a default the broken pointer would have supplied
    // is actually needed. `--help` needs nothing; `setup --remove` only
    // touches the credential store; `setup` needs the vault; `serve` needs
    // both. Overriding exactly what you need keeps working.
    //
    // `Usage` is listed under BOTH needs on purpose (review MINOR-2). An
    // argv we could not parse is not an argv whose path needs we get to
    // narrow: the reviewer's case is `--confg <path>`, where the typo
    // means config is NOT explicit and the broken pointer's default is
    // exactly what would have been used. Assuming the widest need is the
    // choice that never withholds the diagnosis — and it still honours
    // §W7's scope rule, because `--config X --vault Y --bogus` overrides
    // both, needs neither default, and stays a plain `Usage`.
    let needs_config = matches!(command, Command::Serve | Command::Usage) && !config_explicit;
    let needs_vault = matches!(
        command,
        Command::Serve | Command::Setup { remove: false } | Command::Usage
    ) && !vault_explicit;
    // A broken pointer OUTRANKS a `Usage`, deliberately. Nothing is lost:
    // the parse loop above has ALREADY named the offending argument on
    // stderr, unconditionally, so the operator sees both problems. The
    // ranking matters because only one of the two survives fixing the
    // other — the typo is gone the moment they retype it, the broken
    // workspace is still there — and `Fail` is the arm that carries the
    // non-zero exit and the "never a silent fallback" rail (§W4/§W7).
    let command = match broken {
        Some(msg) if needs_config || needs_vault => Command::Fail(msg),
        _ => command,
    };
    Args { config, vault, command }
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // Design §W0 fact 1's SECOND path-resolution call site — the same
    // `dbc_state::workspace::resolve()` the GUI uses. There is exactly one
    // resolution rule in this repo; a second one here would be the
    // divergence §W2 exists to prevent.
    parse_args_from(&raw, dbc_state::workspace::resolve())
}

fn print_usage() {
    eprintln!(
        "dbc-mcp — MCP server exposing dbc's saved connections to LLM tools (read-only)

USAGE:
    dbc-mcp [--config <path>] [--vault <path>]          Run the server (stdio transport)
    dbc-mcp setup [--vault <path>]                       Store the vault's derived key in the OS credential store
    dbc-mcp setup --remove                               Remove the stored key (revocation)
    dbc-mcp --help | -h                                  Show this message
"
    );
    // The pointer path is COMPUTED, not spelled. `DBC_DATA_DIR` relocates the
    // profile dir, and a usage text that always said `%APPDATA%\dbc` would
    // then name a file the user does not have and cannot fix.
    eprintln!(
        "    Cesty se ve výchozím stavu řídí pracovním prostorem nastaveným v aplikaci dbc
    (ukazatel {}). --config/--vault mají vždy přednost.",
        dbc_state::workspace::pointer_path().display()
    );
    eprintln!(
        "
SETUP (run once, manually, from a terminal — a TTY exists there, unlike the
MCP launch path):
    dbc-mcp setup
        Prompts for the vault master password (no echo), verifies it by
        unlocking the vault, then stores the Argon2id-DERIVED key — never
        the password itself — in the Windows Credential Manager.

REGISTERING WITH AN MCP CLIENT (no secrets in the command; the key lives in
the OS credential store, never in an MCP client's own config file):

    Claude Code:
        claude mcp add dbc -- dbc-mcp

    Claude Desktop (claude_desktop_config.json):
        {{ \"dbc\": {{ \"command\": \"dbc-mcp\" }} }}
"
    );
}

fn run_setup(vault_path: &std::path::Path, remove: bool) -> ExitCode {
    if remove {
        let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("dbc-mcp: failed to access the credential store: {e}");
                return ExitCode::FAILURE;
            }
        };
        return match entry.delete_credential() {
            Ok(()) => {
                eprintln!("dbc-mcp: removed the stored vault key.");
                ExitCode::SUCCESS
            }
            Err(keyring::Error::NoEntry) => {
                eprintln!("dbc-mcp: no stored vault key to remove.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("dbc-mcp: failed to remove the stored vault key: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if !Vault::exists(vault_path) {
        eprintln!(
            "dbc-mcp: no vault found at {} — run the GUI app first to create one.",
            vault_path.display()
        );
        return ExitCode::FAILURE;
    }

    // Review round 1 finding #3: wrap the master password in `Zeroizing`
    // too — it's the setup-side local this finding specifically calls out,
    // same rationale as `Vault::export_key`'s own `Zeroizing` wrap below.
    let password: zeroize::Zeroizing<String> =
        match rpassword::prompt_password("dbc-mcp setup — master password: ") {
            Ok(p) => zeroize::Zeroizing::new(p),
            Err(e) => {
                eprintln!("dbc-mcp: failed to read the password: {e}");
                return ExitCode::FAILURE;
            }
        };

    let vault = match Vault::unlock(vault_path, &password) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("dbc-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Already Zeroizing<[u8; 32]> — see Vault::export_key's doc comment for
    // exactly what this does and doesn't cover.
    let key = vault.export_key();

    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("dbc-mcp: failed to access the credential store: {e}");
            return ExitCode::FAILURE;
        }
    };
    match entry.set_secret(key.as_slice()) {
        Ok(()) => {
            eprintln!(
                "dbc-mcp: vault key stored. Register the server with no secrets in its config, e.g.:\n  claude mcp add dbc -- dbc-mcp"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("dbc-mcp: failed to store the vault key: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_serve(config_path: &std::path::Path, vault_path: &std::path::Path) -> ExitCode {
    // All logging to stderr — stdout is the JSON-RPC stream (see the
    // crate-level doc comment).
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    // Fail closed (design doc §3): missing/wrong key or a corrupt vault all
    // produce a one-line stderr message and a non-zero exit BEFORE the
    // stdio server loop starts.
    let key = match KeySource::default_keyring().resolve() {
        Ok(k) => k,
        Err(e) => {
            eprintln!(
                "dbc-mcp: failed to read the vault key from the credential store: {e}\nRun `dbc-mcp setup` first."
            );
            return ExitCode::FAILURE;
        }
    };
    let vault = match Vault::unlock_with_key(vault_path, &key) {
        Ok(v) => v,
        Err(e) => {
            // The stored key stops fitting when the master password changes
            // or the app re-seals the vault under a new KDF cost; either
            // way the cure is the same.
            eprintln!("dbc-mcp: {e}\nRun `dbc-mcp setup` again to store the current key.");
            return ExitCode::FAILURE;
        }
    };
    // ORDER IS LOAD-BEARING — do not move this above the vault unlock
    // (review NIT-1). `Command::Fail` carries empty `PathBuf`s rather than
    // profile paths precisely so a leaked `Fail` could open nothing; that
    // guard holds here only because `Vault::unlock_with_key("")` fails
    // loudly first. `AppConfig::load` on a missing path returns
    // `Ok(AppConfig::default())` — a SILENT success — so a reordering that
    // put the config load first would start building a server from an
    // empty config before anything complained.
    let config = match AppConfig::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dbc-mcp: failed to load config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let server = McpServer::new(config, vault);
    let service = match server.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dbc-mcp: failed to start MCP server: {e}");
            return ExitCode::FAILURE;
        }
    };
    match service.waiting().await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dbc-mcp: server error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let args = parse_args();
    match args.command {
        Command::Help => {
            print_usage();
            ExitCode::SUCCESS
        }
        // Same text, opposite verdict: usage the operator did NOT ask for
        // means their argv was wrong, and an MCP client that reads exit
        // codes must be told so (review MINOR-2).
        Command::Usage => {
            print_usage();
            ExitCode::FAILURE
        }
        Command::Setup { remove } => run_setup(&args.vault, remove),
        Command::Serve => run_serve(&args.config, &args.vault).await,
        // Design §W7 / §W4: loud, non-zero, and on stderr only — stdout is
        // the JSON-RPC stream (crate doc comment).
        Command::Fail(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod parse_args_tests {
    use super::*;
    use dbc_state::workspace::{profile_paths, workspace_paths, Resolution};

    fn raw(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    fn profile() -> Resolution {
        Resolution::Profile(profile_paths())
    }

    fn workspace() -> Resolution {
        let root = PathBuf::from("D:\\ws");
        Resolution::Workspace { root: root.clone(), paths: workspace_paths(&root) }
    }

    fn broken() -> Resolution {
        Resolution::Broken {
            root: Some(PathBuf::from("D:\\ws-gone")),
            reason: "složka neexistuje".to_string(),
        }
    }

    #[test]
    fn no_pointer_keeps_todays_defaults_exactly() {
        let a = parse_args_from(&raw(&[]), profile());
        assert_eq!(a.config, dbc_state::default_config_path());
        assert_eq!(a.vault, dbc_state::default_vault_path());
        assert!(matches!(a.command, Command::Serve));
    }

    #[test]
    fn a_valid_pointer_moves_both_defaults_into_the_workspace() {
        let a = parse_args_from(&raw(&[]), workspace());
        assert_eq!(a.config, PathBuf::from("D:\\ws").join("config.toml"));
        assert_eq!(a.vault, PathBuf::from("D:\\ws").join("vault.bin"));
    }

    #[test]
    fn explicit_flags_still_win_over_the_workspace() {
        let a =
            parse_args_from(&raw(&["--config", "C:\\x.toml", "--vault", "C:\\x.bin"]), workspace());
        assert_eq!(a.config, PathBuf::from("C:\\x.toml"));
        assert_eq!(a.vault, PathBuf::from("C:\\x.bin"));
        assert!(matches!(a.command, Command::Serve));
    }

    #[test]
    fn a_broken_pointer_fails_loudly_instead_of_serving_the_profile() {
        // The §W4/§W7 rail on the MCP side: no silent profile fallback.
        let a = parse_args_from(&raw(&[]), broken());
        let Command::Fail(msg) = a.command else { panic!("expected Fail") };
        assert!(msg.contains("D:\\ws-gone"), "names the folder: {msg}");
        assert!(msg.contains("složka neexistuje"), "names the reason: {msg}");
        assert_ne!(a.config, dbc_state::default_config_path(), "must not fall back");
    }

    #[test]
    fn a_broken_pointer_is_survivable_by_overriding_exactly_what_is_needed() {
        // serve needs both …
        let a =
            parse_args_from(&raw(&["--config", "C:\\x.toml", "--vault", "C:\\x.bin"]), broken());
        assert!(matches!(a.command, Command::Serve));
        // … and only one is not enough.
        let a = parse_args_from(&raw(&["--config", "C:\\x.toml"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
        // setup needs only the vault …
        let a = parse_args_from(&raw(&["setup", "--vault", "C:\\x.bin"]), broken());
        assert!(matches!(a.command, Command::Setup { remove: false }));
        // … and without it, it fails.
        let a = parse_args_from(&raw(&["setup"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
        // help needs neither.
        let a = parse_args_from(&raw(&["--help"]), broken());
        assert!(matches!(a.command, Command::Help));
    }

    /// FINAL-REVIEW NIT-4. `-h` has been accepted since the first draft
    /// and appeared in no usage line, so the only way to learn it existed
    /// was to read the parser. Both halves are pinned: it still behaves
    /// exactly like `--help`, and it is now written down.
    #[test]
    fn the_short_help_flag_works_and_is_documented() {
        let a = parse_args_from(&raw(&["-h"]), broken());
        assert!(matches!(a.command, Command::Help), "-h must be the same exemption as --help");
        // …and, like `--help`, it stops exempting once the argv is wrong.
        let a = parse_args_from(&raw(&["-h", "--confg", "x"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));

        let src = include_str!("main.rs");
        let usage = src.split("fn print_usage()").nth(1).expect("print_usage exists");
        let usage = &usage[..usage.find("\nfn ").unwrap_or(usage.len())];
        assert!(usage.contains("USAGE:"), "the sliced body is not the real usage text");
        // Every OTHER flag the parser accepts was already documented; the
        // loop is what keeps the next one from not being.
        for flag in ["setup", "--remove", "--config", "--vault", "--help | -h"] {
            assert!(usage.contains(flag), "`{flag}` is accepted by the parser but undocumented");
        }
    }

    #[test]
    fn setup_remove_needs_no_paths_at_all() {
        // Revocation deletes a keyring entry; it never opens the vault.
        let a = parse_args_from(&raw(&["setup", "--remove"]), broken());
        assert!(matches!(a.command, Command::Setup { remove: true }));
    }

    #[test]
    fn an_unrecognized_argument_still_prints_usage() {
        // Review MINOR-2: still usage, but `Usage` rather than `Help` —
        // `main` maps the two to opposite exit codes.
        let a = parse_args_from(&raw(&["--nonsense"]), profile());
        assert!(matches!(a.command, Command::Usage));
    }

    #[test]
    fn the_broken_message_names_the_pointer_and_the_fix_without_leaking_anything() {
        let m = workspace_broken_message(
            &Some(PathBuf::from("D:\\ws-gone")),
            "chybí dbc-workspace.toml",
        );
        assert!(m.contains("D:\\ws-gone"));
        assert!(m.contains("chybí dbc-workspace.toml"));
        assert!(m.contains("--config"), "tells the operator the override exists");
        let m = workspace_broken_message(&None, "ukazatel je poškozený");
        assert!(m.contains("ukazatel je poškozený"));
    }

    /// Review MINOR-1. The byte pin plus the assertion the old test was
    /// missing: the `None` branch's OWN subject must reach the composed
    /// message, so `None => String::new()` can no longer pass.
    #[test]
    fn the_unreadable_pointer_subject_is_byte_pinned_and_actually_composed_in() {
        assert_eq!(WORKSPACE_MISSING_NO_PATH, "ukazatel na pracovní prostor je nečitelný");
        let m = workspace_broken_message(&None, "ukazatel je poškozený");
        assert!(
            m.contains(WORKSPACE_MISSING_NO_PATH),
            "the None branch must name the pointer itself, not just echo the reason: {m}"
        );
        // …and it is the SUBJECT, not something the reason happened to
        // supply: a reason that says nothing still leaves it standing.
        let m = workspace_broken_message(&None, "");
        assert!(m.contains(WORKSPACE_MISSING_NO_PATH), "{m}");
    }

    /// Review MINOR-3: `toml::de::Error`'s multi-line `Display` used to
    /// render as eight stderr lines ending in an orphaned `)`.
    #[test]
    fn a_multi_line_reason_keeps_its_position_and_explanation_and_drops_the_art() {
        // Verbatim shape of a real `toml` parse error.
        let reason = "TOML parse error at line 1, column 12\n  \
                      |\n1 | path = \"D:\\ws-gone\"\n  |            ^\n\
                      missing escaped value, expected `b`";
        let m = workspace_broken_message(&None, reason);
        assert_eq!(m.lines().count(), 2, "diagnosis + escape hatch, nothing else: {m}");
        assert!(m.contains("TOML parse error at line 1, column 12"), "keeps WHERE: {m}");
        assert!(m.contains("missing escaped value, expected `b`"), "keeps WHAT: {m}");
        assert!(!m.contains("path = "), "drops toml's source echo: {m}");
        assert!(!m.contains('^'), "drops the ascii art: {m}");
    }

    #[test]
    fn a_single_line_reason_is_passed_through_untouched() {
        let m = workspace_broken_message(&Some(PathBuf::from("D:\\ws-gone")), "složka neexistuje");
        assert_eq!(m.lines().count(), 2);
        assert!(m.contains("(složka neexistuje)"), "{m}");
    }

    /// T10 carry-forward 3. The "exactly two lines" property was only ever
    /// pinned on the REASON; `where_` was unbounded. The pointer's `path`
    /// is arbitrary TOML text that nothing validates as a real path, so a
    /// hand-edited `path = "D:\\ws\ntext"` put attacker-chosen text on its
    /// own stderr line — three lines, not two, and the third one looks
    /// like the tool talking. Both halves go through the collapse now.
    #[test]
    fn a_newline_in_the_pointers_path_cannot_buy_a_third_stderr_line() {
        let root = PathBuf::from("D:\\ws-gone\ndbc-mcp: připojeno k prod, heslo přijato");
        let m = workspace_broken_message(&root.clone().into(), "složka neexistuje");
        assert_eq!(m.lines().count(), 2, "diagnosis + escape hatch, nothing else: {m}");
        // The text is not censored, merely denied its own line — the
        // operator still sees what the pointer actually says.
        assert!(m.contains("D:\\ws-gone"), "{m}");
        // …and the escape hatch is still the LAST line, not buried.
        assert!(m.lines().last().unwrap().contains("--config"), "{m}");
    }

    /// T10 carry-forward 2. `needs_config` lists `Command::Usage` on
    /// purpose (review MINOR-2), but nothing pinned it: narrowing it back
    /// to `matches!(command, Command::Serve)` survived the whole suite,
    /// because every other broken-pointer case in it also needs the VAULT
    /// default and `needs_vault` alone carried the verdict. This is the
    /// case that separates them — the vault IS explicit, so only the
    /// config default is at stake, and out of process the mutation would
    /// silently withhold the broken-workspace diagnosis while reporting
    /// nothing but a typo.
    #[test]
    fn an_explicit_vault_does_not_excuse_the_config_default_of_a_broken_pointer() {
        let a = parse_args_from(&raw(&["--vault", "C:\\x.bin", "--bogus"]), broken());
        let Command::Fail(msg) = a.command else {
            panic!("`--bogus` still needs the pointer's CONFIG default — that must be diagnosed")
        };
        assert!(msg.contains("D:\\ws-gone"), "{msg}");
        // The mirror image, so the assertion above is about `needs_config`
        // and not merely about `Usage` being fatal: with CONFIG explicit
        // and the vault defaulted it is `needs_vault` that fires.
        let a = parse_args_from(&raw(&["--config", "C:\\x.toml", "--bogus"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
    }

    /// Review MINOR-2, the reviewer's exact scenario: a typo'd `--config`
    /// leaves config NOT explicit, so the broken pointer's default is
    /// precisely what would have been used. The operator must hear about
    /// the workspace, not only about the typo.
    #[test]
    fn a_typod_flag_cannot_mask_a_broken_pointer() {
        let a = parse_args_from(&raw(&["--confg", "C:\\ws\\config.toml"]), broken());
        let Command::Fail(msg) = a.command else { panic!("a typo must not swallow the diagnosis") };
        assert!(msg.contains("D:\\ws-gone"), "{msg}");
        // Explicit `--help` is the ONE thing that still exempts…
        let a = parse_args_from(&raw(&["--help"]), broken());
        assert!(matches!(a.command, Command::Help));
        // …and it stops exempting the moment the argv is also wrong.
        let a = parse_args_from(&raw(&["--help", "--confg", "x"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
        // A bad argv with a healthy pointer is a plain Usage (non-zero).
        let a = parse_args_from(&raw(&["--help", "--nonsense"]), profile());
        assert!(matches!(a.command, Command::Usage));
    }

    /// §W7's scope rule survives the MINOR-2 widening: overriding both
    /// paths means no default is needed, so a bad argv stays a `Usage` and
    /// does not get upgraded into a workspace failure it does not have.
    #[test]
    fn a_bad_argv_that_overrides_everything_is_still_only_a_usage_error() {
        let a = parse_args_from(
            &raw(&["--config", "C:\\x.toml", "--vault", "C:\\x.bin", "--bogus"]),
            broken(),
        );
        assert!(matches!(a.command, Command::Usage));
        assert_eq!(a.config, PathBuf::from("C:\\x.toml"));
        assert_eq!(a.vault, PathBuf::from("C:\\x.bin"));
    }

    /// A missing flag VALUE is an argv error too — it used to share the
    /// `Help` arm and therefore the exit-0 / exemption bug.
    #[test]
    fn a_flag_without_its_value_is_an_argv_error_not_a_help_request() {
        let a = parse_args_from(&raw(&["--config"]), profile());
        assert!(matches!(a.command, Command::Usage));
        let a = parse_args_from(&raw(&["--vault"]), broken());
        assert!(matches!(a.command, Command::Fail(_)));
    }
}
