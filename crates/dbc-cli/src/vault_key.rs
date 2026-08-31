//! Getting into the vault from a terminal.
//!
//! Two ways in, in this order:
//!
//! 1. The DERIVED 32-byte key, if `dbc login` has stored one in the OS
//!    credential store. This is what makes cron and scripts possible.
//! 2. A master-password prompt on the terminal, with no echo.
//!
//! The master password itself is never stored, by either path — `login`
//! stores what Argon2id derives FROM it, which is the same thing
//! `dbc-mcp setup` does and for the same reason: a stolen key unlocks this
//! machine's vault, while a stolen password unlocks it everywhere the user
//! reused it.
//!
//! **Its own credential-store entry, deliberately.** Reusing `dbc-mcp`'s
//! would mean `dbc logout` silently revoking the MCP server's access, and
//! `dbc-mcp setup --remove` silently revoking the CLI's. Two consents, two
//! revocations.

use dbc_state::{StateError, Vault};
use std::path::Path;

/// Service/user pair `login` stores the derived key under. Free constants
/// so `login`, `logout` and the read path cannot drift apart.
pub const KEYRING_SERVICE: &str = "dbc-cli";
pub const KEYRING_USER: &str = "vault-key";

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())
}

/// The stored derived key, if there is one.
///
/// Fails CLOSED and quietly: a missing entry, a credential-store error and
/// a stored secret of the wrong length are all just „no key", because the
/// caller's next move is the same in every case — ask for the password.
/// Never a fabricated or default key.
pub fn stored_key() -> Option<[u8; 32]> {
    let secret = entry().ok()?.get_secret().ok()?;
    secret.as_slice().try_into().ok()
}

pub fn store_key(key: &[u8; 32]) -> Result<(), String> {
    entry()?.set_secret(key).map_err(|e| e.to_string())
}

/// Remove the stored key. A missing entry is SUCCESS: `logout` is about
/// reaching a state, not about having found something to delete, and
/// telling someone their logout failed because they were not logged in is
/// an error message that helps nobody.
pub fn forget_key() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Prompt for the master password on the terminal, no echo.
pub fn prompt_master() -> Result<String, String> {
    rpassword::prompt_password("Master heslo k trezoru: ").map_err(|e| e.to_string())
}

/// Open the vault: stored key first, prompt second.
///
/// A stored key that no longer works (the master password was changed on
/// another machine, say) falls through to the prompt instead of failing —
/// the key is a convenience, and a stale one should cost a password entry,
/// not an error the user has to decode.
pub fn unlock(vault_path: &Path) -> Result<Vault, String> {
    if let Some(key) = stored_key() {
        if let Ok(v) = Vault::unlock_with_key(vault_path, &key) {
            return Ok(v);
        }
    }
    let password = prompt_master()?;
    Vault::unlock(vault_path, &password).map_err(|e: StateError| e.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two halves of the identity must be constants, not literals
    /// repeated per call site — `login` storing under one name and
    /// `logout` deleting another would be a silent, permanent leak of a
    /// key the user believes they revoked.
    #[test]
    fn the_credential_identity_is_shared_and_not_the_mcp_servers() {
        assert_eq!(KEYRING_SERVICE, "dbc-cli");
        assert_ne!(KEYRING_SERVICE, "dbc-mcp");
        assert!(!KEYRING_USER.is_empty());
    }
}
