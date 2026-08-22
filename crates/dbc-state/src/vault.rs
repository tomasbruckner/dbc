use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};

use crate::config::StateError;

const M_COST: u32 = 65536; // 64 MiB
const T_COST: u32 = 3;
const P_COST: u32 = 4;

#[derive(Serialize, Deserialize)]
struct Envelope {
    kdf: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

pub struct Vault {
    path: PathBuf,
    key: Key, // derived once per unlock; lives only in memory
    salt: [u8; 16],
    secrets: BTreeMap<String, String>,
}

// Hand-written: the derived impl would print the raw key and every secret.
impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("path", &self.path)
            .field("secrets_len", &self.secrets.len())
            .finish_non_exhaustive()
    }
}

fn err(m: impl Into<String>) -> StateError { StateError { message: m.into() } }

fn derive_key(master: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Key, StateError> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| err(e.to_string()))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    a2.hash_password_into(master.as_bytes(), salt, &mut out)
        .map_err(|e| err(e.to_string()))?;
    Ok(Key::from(out))
}

impl Vault {
    pub fn exists(path: &Path) -> bool { path.exists() }

    pub fn create(path: &Path, master: &str) -> Result<Vault, StateError> {
        let mut salt = [0u8; 16];
        use rand::RngCore as _;
        let mut rng = rand::rng();
        rng.fill_bytes(&mut salt);
        let key = derive_key(master, &salt, M_COST, T_COST, P_COST)?;
        let mut v = Vault { path: path.to_path_buf(), key, salt, secrets: BTreeMap::new() };
        v.persist()?;
        Ok(v)
    }

    pub fn unlock(path: &Path, master: &str) -> Result<Vault, StateError> {
        let env: Envelope = serde_json::from_str(&std::fs::read_to_string(path)?)
            .map_err(|_| err("vault unlock failed: corrupt envelope"))?;
        let salt: [u8; 16] = B64.decode(&env.salt)
            .ok().and_then(|v| v.try_into().ok())
            .ok_or_else(|| err("vault unlock failed: bad salt"))?;
        // On-disk KDF params keep old vaults working after param bumps, but the
        // envelope is unauthenticated until decrypt — cap them so a corrupted
        // file can't force a multi-GiB allocation (m_cost is in KiB).
        if env.m_cost > 2 * 1024 * 1024 || env.t_cost > 64 || env.p_cost > 64 {
            return Err(err("vault unlock failed: implausible KDF params"));
        }
        let key = derive_key(master, &salt, env.m_cost, env.t_cost, env.p_cost)?;
        let nonce_bytes = B64.decode(&env.nonce).map_err(|_| err("vault unlock failed: bad nonce"))?;
        let ct = B64.decode(&env.ciphertext).map_err(|_| err("vault unlock failed: bad ciphertext"))?;
        let nonce_arr: [u8; 12] = nonce_bytes.as_slice().try_into()
            .map_err(|_| err("vault unlock failed: bad nonce"))?;
        let cipher = ChaCha20Poly1305::new(&key);
        let nonce = Nonce::from(nonce_arr);
        let plain = cipher
            .decrypt(&nonce, ct.as_ref())
            .map_err(|_| err("vault unlock failed: wrong master password or tampered file"))?;
        let secrets: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|_| err("vault unlock failed: bad payload"))?;
        Ok(Vault { path: path.to_path_buf(), key, salt, secrets })
    }

    fn persist(&mut self) -> Result<(), StateError> {
        let cipher = ChaCha20Poly1305::new(&self.key);
        let mut nonce_arr = [0u8; 12];
        use rand::RngCore as _;
        let mut rng = rand::rng();
        rng.fill_bytes(&mut nonce_arr);
        let nonce = Nonce::from(nonce_arr);
        let plain = serde_json::to_vec(&self.secrets).map_err(|e| err(e.to_string()))?;
        let ct = cipher.encrypt(&nonce, plain.as_ref()).map_err(|e| err(e.to_string()))?;
        let env = Envelope {
            kdf: "argon2id".into(),
            m_cost: M_COST, t_cost: T_COST, p_cost: P_COST,
            salt: B64.encode(self.salt),
            nonce: B64.encode(nonce_arr),
            ciphertext: B64.encode(ct),
        };
        if let Some(dir) = self.path.parent() { std::fs::create_dir_all(dir)?; }
        let tmp = self.path.with_extension("bin.tmp");
        std::fs::write(&tmp, serde_json::to_string(&env).map_err(|e| err(e.to_string()))?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn set_secret(&mut self, key: &str, value: &str) -> Result<(), StateError> {
        self.secrets.insert(key.into(), value.into());
        self.persist()
    }

    pub fn get_secret(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
    }

    pub fn remove_secret(&mut self, key: &str) -> Result<(), StateError> {
        self.secrets.remove(key);
        self.persist()
    }
}

pub fn default_vault_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("dbc").join("vault.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_secret() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create(&p, "correct horse").unwrap();
        v.set_secret("c1", "tajne-heslo").unwrap();
        drop(v);
        let v2 = Vault::unlock(&p, "correct horse").unwrap();
        assert_eq!(v2.get_secret("c1").as_deref(), Some("tajne-heslo"));
        assert_eq!(v2.get_secret("missing"), None);
    }

    #[test]
    fn wrong_password_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        Vault::create(&p, "right").unwrap();
        let err = Vault::unlock(&p, "wrong").unwrap_err();
        assert!(err.message.contains("unlock"), "got: {}", err.message);
    }

    #[test]
    fn tampered_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create(&p, "pw").unwrap();
        v.set_secret("k", "v").unwrap();
        drop(v);
        // flip one byte of ciphertext
        let mut env: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let ct = env["ciphertext"].as_str().unwrap().to_string();
        let mut bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, ct).unwrap();
        bytes[0] ^= 0xFF;
        env["ciphertext"] = serde_json::Value::String(
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes));
        std::fs::write(&p, serde_json::to_string(&env).unwrap()).unwrap();
        assert!(Vault::unlock(&p, "pw").is_err());
    }

    #[test]
    fn plaintext_never_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create(&p, "pw").unwrap();
        v.set_secret("c1", "SUPERTAJNE123").unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert!(!raw.windows(13).any(|w| w == b"SUPERTAJNE123"));
    }
}
