//! „Zapamatovat na tomto počítači" — the desktop app's opt-in way past the
//! master-password prompt.
//!
//! Same shape as `dbc-cli`'s `vault_key.rs`: what is stored is the DERIVED
//! 32-byte key (`Vault::export_key`), never the master password — a stolen
//! key opens this machine's vault, a stolen password opens it everywhere
//! the user reused it.
//!
//! **Its own credential-store entry, deliberately.** Sharing `dbc-cli`'s
//! would mean `dbc logout` silently switching off the app's auto-unlock,
//! and „Zapomenout uložený klíč" in Settings silently logging the CLI out.
//! Three consents (`dbc-ui`, `dbc-cli`, `dbc-mcp`), three revocations.
//!
//! The key is used ONCE per launch (`AppView::start_auto_unlock`), never on
//! demand: „Zamknout trezor" has to mean locked until the next start, and
//! an on-demand unlock would quietly undo every lock.

/// Service/user pair the key is stored under. Constants, so store, read
/// and forget cannot drift apart.
pub const KEYRING_SERVICE: &str = "dbc-ui";
pub const KEYRING_USER: &str = "vault-key";

fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())
}

/// The stored key, if there is one.
///
/// Fails CLOSED and quietly: a missing entry, a credential-store error and
/// a secret of the wrong length are all „no key" — the next move is the
/// same in every case, the ordinary prompt. Never a default key.
pub fn stored_key() -> Option<[u8; 32]> {
    let secret = entry().ok()?.get_secret().ok()?;
    secret.as_slice().try_into().ok()
}

pub fn store_key(key: &[u8; 32]) -> Result<(), String> {
    entry()?.set_secret(key).map_err(|e| e.to_string())
}

/// Remove the stored key. A missing entry is SUCCESS: forgetting is about
/// reaching a state, not about having found something to delete.
pub fn forget_key() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// What a successful password unlock does to the credential store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Write the (possibly new) key. Also on a key that was already
    /// stored: an unlock that re-sealed the vault changed it, and the
    /// stale one would cost a password prompt on every launch.
    Store,
    /// The box was unticked while a key was stored — the user took the
    /// consent back.
    Forget,
    /// Unticked and nothing stored: do not touch the credential store.
    Nothing,
}

pub fn after_unlock(remember: bool, stored: bool) -> KeyAction {
    match (remember, stored) {
        (true, _) => KeyAction::Store,
        (false, true) => KeyAction::Forget,
        (false, false) => KeyAction::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticked_always_stores_so_a_resealed_key_replaces_the_stale_one() {
        assert_eq!(after_unlock(true, false), KeyAction::Store);
        assert_eq!(after_unlock(true, true), KeyAction::Store);
    }

    #[test]
    fn unticking_a_stored_key_forgets_it() {
        assert_eq!(after_unlock(false, true), KeyAction::Forget);
    }

    /// The default path — nobody opted in — must not write, and must not
    /// even call delete, to the user's credential store.
    #[test]
    fn unticked_with_nothing_stored_leaves_the_store_alone() {
        assert_eq!(after_unlock(false, false), KeyAction::Nothing);
    }

    /// `dbc logout` / `dbc-mcp setup --remove` must never revoke the app's
    /// key, nor Settings' „Zapomenout" theirs.
    #[test]
    fn the_credential_identity_is_the_apps_own() {
        assert_eq!(KEYRING_SERVICE, "dbc-ui");
        assert_ne!(KEYRING_SERVICE, "dbc-cli");
        assert_ne!(KEYRING_SERVICE, "dbc-mcp");
        assert!(!KEYRING_USER.is_empty());
    }
}
