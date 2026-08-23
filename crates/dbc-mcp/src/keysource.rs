//! Where the vault's derived 32-byte key comes from.
//!
//! `dbc-mcp`'s only real source is the OS credential store (Windows
//! Credential Manager via the `keyring` crate — see `setup` and MCP-mode
//! startup in `main.rs`). Tests must never touch the real Credential
//! Manager, so the lookup is behind this small enum: tests construct
//! [`KeySource::Direct`] with a key they control, production code
//! constructs [`KeySource::Keyring`]. `keyring` is only ever called from
//! [`KeySource::resolve`]'s `Keyring` arm — no other file in this crate
//! references the `keyring` crate.

/// Service/user pair `setup` stores the derived key under, and MCP-mode
/// startup reads it back from. Kept as free constants (rather than baked
/// into `KeySource::Keyring`'s constructor) so `setup --remove` and the
/// startup path are guaranteed to agree on the exact same identity.
pub const KEYRING_SERVICE: &str = "dbc-mcp";
pub const KEYRING_USER: &str = "vault-key";

pub enum KeySource {
    /// Production path: read the derived vault key from the OS credential
    /// store under `(service, user)`.
    Keyring { service: String, user: String },
    /// Test/injection path: the key is already known to the caller. Only
    /// ever constructed by tests — never touches the real credential store,
    /// which is the whole point (`cfg(test)`-only usage means a normal
    /// `cargo build` sees this variant as unconstructed, hence the
    /// `allow(dead_code)`).
    #[allow(dead_code)]
    Direct([u8; 32]),
}

impl KeySource {
    pub fn default_keyring() -> Self {
        KeySource::Keyring { service: KEYRING_SERVICE.to_string(), user: KEYRING_USER.to_string() }
    }

    /// Resolves the 32-byte vault key. Fails closed: a missing entry, a
    /// credential-store error, or a stored secret of the wrong length all
    /// come back as `Err` — never a fabricated/default key.
    pub fn resolve(&self) -> Result<[u8; 32], String> {
        match self {
            KeySource::Direct(k) => Ok(*k),
            KeySource::Keyring { service, user } => {
                let entry = keyring::Entry::new(service, user).map_err(|e| e.to_string())?;
                let secret = entry.get_secret().map_err(|e| e.to_string())?;
                let arr: [u8; 32] = secret
                    .as_slice()
                    .try_into()
                    .map_err(|_| "stored vault key has the wrong length".to_string())?;
                Ok(arr)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_resolves_to_the_given_key() {
        let key = [7u8; 32];
        let src = KeySource::Direct(key);
        assert_eq!(src.resolve().unwrap(), key);
    }
}
