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
    Help,
    /// Design §W7: the pointer file names a workspace this build cannot
    /// use, AND the command actually needs a path it would have supplied.
    /// One stderr line, non-zero exit — dbc-mcp must not silently serve
    /// profile-mode connections any more than the GUI may silently show
    /// them (§W4).
    Fail(String),
}

struct Args {
    config: PathBuf,
    vault: PathBuf,
    command: Command,
}

/// The stderr message for a broken pointer. Names the folder (or says the
/// pointer itself is unreadable), names the reason, and points at the
/// escape hatch — nothing else: no config contents, no vault bytes, no
/// connection names (`stdout is sacred`, and stderr is a log too).
fn workspace_broken_message(root: &Option<PathBuf>, reason: &str) -> String {
    let where_ = match root {
        Some(r) => r.display().to_string(),
        None => "ukazatel na pracovní prostor je nečitelný".to_string(),
    };
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
                    help = true;
                }
            }
            "--vault" => {
                i += 1;
                if let Some(v) = raw.get(i) {
                    vault = v.into();
                    vault_explicit = true;
                } else {
                    eprintln!("dbc-mcp: --vault requires a path");
                    help = true;
                }
            }
            other => {
                eprintln!("dbc-mcp: unrecognized argument '{other}'");
                help = true;
            }
        }
        i += 1;
    }

    let command =
        if help { Command::Help } else if is_setup { Command::Setup { remove } } else { Command::Serve };
    // §W7: fatal only when a default the broken pointer would have supplied
    // is actually needed. `--help` needs nothing; `setup --remove` only
    // touches the credential store; `setup` needs the vault; `serve` needs
    // both. Overriding exactly what you need keeps working.
    let needs_config = matches!(command, Command::Serve) && !config_explicit;
    let needs_vault =
        matches!(command, Command::Serve | Command::Setup { remove: false }) && !vault_explicit;
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
    dbc-mcp --help                                       Show this message

    Cesty se ve výchozím stavu řídí pracovním prostorem nastaveným v aplikaci dbc
    (ukazatel %APPDATA%\\dbc\\workspace.toml). --config/--vault mají vždy přednost.

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
            eprintln!("dbc-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
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

    #[test]
    fn setup_remove_needs_no_paths_at_all() {
        // Revocation deletes a keyring entry; it never opens the vault.
        let a = parse_args_from(&raw(&["setup", "--remove"]), broken());
        assert!(matches!(a.command, Command::Setup { remove: true }));
    }

    #[test]
    fn an_unrecognized_argument_still_prints_usage() {
        let a = parse_args_from(&raw(&["--nonsense"]), profile());
        assert!(matches!(a.command, Command::Help));
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
}
