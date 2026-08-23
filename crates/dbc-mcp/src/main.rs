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
}

struct Args {
    config: PathBuf,
    vault: PathBuf,
    command: Command,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut config = dbc_state::default_config_path();
    let mut vault = dbc_state::default_vault_path();
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
                } else {
                    eprintln!("dbc-mcp: --config requires a path");
                    help = true;
                }
            }
            "--vault" => {
                i += 1;
                if let Some(v) = raw.get(i) {
                    vault = v.into();
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
    Args { config, vault, command }
}

fn print_usage() {
    eprintln!(
        "dbc-mcp — MCP server exposing dbc's saved connections to LLM tools (read-only)

USAGE:
    dbc-mcp [--config <path>] [--vault <path>]          Run the server (stdio transport)
    dbc-mcp setup [--vault <path>]                       Store the vault's derived key in the OS credential store
    dbc-mcp setup --remove                               Remove the stored key (revocation)
    dbc-mcp --help                                       Show this message

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

    let password = match rpassword::prompt_password("dbc-mcp setup — master password: ") {
        Ok(p) => p,
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
    let key = vault.export_key();

    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("dbc-mcp: failed to access the credential store: {e}");
            return ExitCode::FAILURE;
        }
    };
    match entry.set_secret(&key) {
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
    }
}
