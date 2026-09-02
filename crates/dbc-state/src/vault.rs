use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::config::StateError;

/// The Argon2id cost a NEW seal uses. Deliberately light (user, 2026-09-02:
/// „nechci tam tyhle advanced algoritmy proti bruteforce protection, chci
/// se lognout hned"): the previous 64 MiB / 3 passes / 4 lanes took 0.16 s
/// in a release build and 3.1 s in the dev build the user actually runs.
/// 8 MiB / 1 pass / 1 lane is ~20× cheaper — an unlock is now a blink —
/// while still being Argon2id, so a stolen `vault.bin` is guessed at
/// memory-hard speed, not SHA speed. Older vaults carry their own cost in
/// the envelope and are re-sealed to this one the first time they open
/// (`Vault::unlock`).
const M_COST: u32 = 8192; // 8 MiB
const T_COST: u32 = 1;
const P_COST: u32 = 1;

/// The KDF cost a key was derived with. Travels WITH the key: `persist`
/// writes these, never the constants above, because a key derived under
/// one cost and an envelope claiming another is a vault nobody can open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Kdf {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

impl Kdf {
    const CURRENT: Kdf = Kdf { m_cost: M_COST, t_cost: T_COST, p_cost: P_COST };
}

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
    key: [u8; 32], // derived once per unlock; lives only in memory; zeroized on drop
    salt: [u8; 16],
    /// The cost `key` was derived with — what `persist` writes back.
    kdf: Kdf,
    secrets: BTreeMap<String, String>, // plaintext secrets; zeroized on drop
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

/// Security follow-up (final-review.md #14 / task-2-review.md #2): the
/// derived Argon2id key and the decrypted secret strings used to sit in
/// freed-but-unzeroed heap/stack memory after `Vault` was dropped, which a
/// coredump or an unrelated memory-disclosure bug could expose. `key` is a
/// plain `[u8; 32]` (rather than `chacha20poly1305::Key`/`GenericArray`)
/// specifically so it implements `Zeroize` directly without pulling in the
/// `generic-array` crate's own zeroize feature; it's converted to `Key` only
/// at the point of use (`persist`). Does NOT cover values already handed out
/// by `get_secret` (those are ordinary owned `String`s the caller controls)
/// or `export_key`'s copy (already `Zeroizing`, see its doc comment).
impl Drop for Vault {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl Vault {
    /// The actual scrub, factored out of `Drop::drop` so tests can invoke it
    /// on a still-LIVE `Vault` and assert the fields directly (`key ==
    /// [0; 32]`, secrets empty of their plaintext) instead of reading memory
    /// after `drop` — reading post-drop memory is technically UB even when
    /// it happens to pass; see `key_and_secrets_are_scrubbed_by_wipe` below.
    fn wipe(&mut self) {
        self.key.zeroize();
        for secret in self.secrets.values_mut() {
            secret.zeroize();
        }
    }
}

fn err(m: impl Into<String>) -> StateError { StateError { message: m.into() } }

/// Does this text have the SHAPE of a sealed vault envelope?
///
/// Not a decryption and not an authentication — it cannot be either, since
/// both need the master password. It answers exactly one narrow question,
/// for [`crate::bundle`]: is the file sitting at the vault's path the
/// ciphertext envelope [`Vault::persist`] writes, or is it something else
/// that merely lives there — a leftover, a hand-edit, a restore of the
/// wrong file?
///
/// The bundle asks BEFORE copying that file anywhere, because the one
/// failure this module must never have is readable text leaving the machine
/// under the name „vault". A wrong `true` here costs nothing (the envelope
/// still will not decrypt); a wrong `false` refuses an export, loudly. Both
/// errors are safe in the direction that matters.
pub(crate) fn text_is_sealed_envelope(text: &str) -> bool {
    let Ok(env) = serde_json::from_str::<Envelope>(text) else { return false };
    env.kdf == "argon2id"
        && !env.salt.is_empty()
        && !env.nonce.is_empty()
        && !env.ciphertext.is_empty()
}

fn fresh_salt() -> [u8; 16] {
    use rand::RngCore as _;
    let mut salt = [0u8; 16];
    rand::rng().fill_bytes(&mut salt);
    salt
}

fn derive_key(master: &str, salt: &[u8], kdf: Kdf) -> Result<[u8; 32], StateError> {
    let params = Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(32))
        .map_err(|e| err(e.to_string()))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    a2.hash_password_into(master.as_bytes(), salt, &mut out)
        .map_err(|e| err(e.to_string()))?;
    Ok(out)
}

impl Vault {
    pub fn exists(path: &Path) -> bool { path.exists() }

    pub fn create(path: &Path, master: &str) -> Result<Vault, StateError> {
        Self::create_with(path, master, Kdf::CURRENT)
    }

    /// `create` with an explicit cost — how the tests below manufacture a
    /// vault sealed under an OLDER cost than `Kdf::CURRENT`.
    fn create_with(path: &Path, master: &str, kdf: Kdf) -> Result<Vault, StateError> {
        let salt = fresh_salt();
        let key = derive_key(master, &salt, kdf)?;
        let mut v = Vault { path: path.to_path_buf(), key, salt, kdf, secrets: BTreeMap::new() };
        v.persist()?;
        Ok(v)
    }

    /// Opens the vault with the master password.
    ///
    /// A vault sealed under a cost other than `Kdf::CURRENT` is re-sealed
    /// under the current one here, silently, with a fresh salt — so the
    /// one slow unlock a lighter cost inherits from an older file happens
    /// exactly once. If the re-seal cannot be written (read-only profile,
    /// full disk) the vault still opens, under its old key; the next unlock
    /// simply tries again. A re-seal changes the derived key, so a copy
    /// stored by `dbc login` / `dbc-mcp setup` stops fitting — both fall
    /// back to asking for the password.
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
        let kdf = Kdf { m_cost: env.m_cost, t_cost: env.t_cost, p_cost: env.p_cost };
        let key = derive_key(master, &salt, kdf)?;
        let nonce_bytes = B64.decode(&env.nonce).map_err(|_| err("vault unlock failed: bad nonce"))?;
        let ct = B64.decode(&env.ciphertext).map_err(|_| err("vault unlock failed: bad ciphertext"))?;
        let nonce_arr: [u8; 12] = nonce_bytes.as_slice().try_into()
            .map_err(|_| err("vault unlock failed: bad nonce"))?;
        let cipher = ChaCha20Poly1305::new(&Key::from(key));
        let nonce = Nonce::from(nonce_arr);
        // M2(a): the decrypted plaintext (the whole secrets map, serialized)
        // is scrubbed the moment it's dropped rather than left as an
        // unzeroed intermediate `Vec<u8>` between decrypt and deserialize.
        let plain: Zeroizing<Vec<u8>> = Zeroizing::new(
            cipher
                .decrypt(&nonce, ct.as_ref())
                .map_err(|_| err("vault unlock failed: wrong master password or tampered file"))?,
        );
        let secrets: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|_| err("vault unlock failed: bad payload"))?;
        let mut v = Vault { path: path.to_path_buf(), key, salt, kdf, secrets };
        if kdf != Kdf::CURRENT {
            v.reseal(master);
        }
        Ok(v)
    }

    /// Re-derive the key under `Kdf::CURRENT` with a fresh salt and write
    /// the vault back. Best effort: on a write failure the old key, salt
    /// and cost are put back, so the open vault and the file on disk keep
    /// agreeing. Whichever key ends up unused is scrubbed.
    fn reseal(&mut self, master: &str) {
        let new_salt = fresh_salt();
        let Ok(new_key) = derive_key(master, &new_salt, Kdf::CURRENT) else { return };
        let mut old_key = std::mem::replace(&mut self.key, new_key);
        let old_salt = std::mem::replace(&mut self.salt, new_salt);
        let old_kdf = std::mem::replace(&mut self.kdf, Kdf::CURRENT);
        if self.persist().is_err() {
            let mut unused = std::mem::replace(&mut self.key, old_key);
            unused.zeroize();
            self.salt = old_salt;
            self.kdf = old_kdf;
            return;
        }
        old_key.zeroize();
    }

    /// Curated unlock path (dbc-mcp): opens the vault with an
    /// already-derived 32-byte key instead of a master password, skipping
    /// the Argon2id derivation step entirely. The key is expected to have
    /// come from a prior [`Vault::export_key`] call (persisted at rest by
    /// the caller, e.g. in the OS credential store) — this function itself
    /// never derives, stores, or otherwise handles a password.
    ///
    /// Additive: does not change `unlock`'s signature or behavior. Fails
    /// closed exactly like `unlock` on a corrupt envelope, bad salt/nonce,
    /// or a key that doesn't decrypt the ciphertext (wrong key or tampered
    /// file) — the two paths share every check except how the key is
    /// obtained.
    pub fn unlock_with_key(path: &Path, key: &[u8; 32]) -> Result<Vault, StateError> {
        let env: Envelope = serde_json::from_str(&std::fs::read_to_string(path)?)
            .map_err(|_| err("vault unlock failed: corrupt envelope"))?;
        let salt: [u8; 16] = B64.decode(&env.salt)
            .ok().and_then(|v| v.try_into().ok())
            .ok_or_else(|| err("vault unlock failed: bad salt"))?;
        let key = *key;
        let nonce_bytes = B64.decode(&env.nonce).map_err(|_| err("vault unlock failed: bad nonce"))?;
        let ct = B64.decode(&env.ciphertext).map_err(|_| err("vault unlock failed: bad ciphertext"))?;
        let nonce_arr: [u8; 12] = nonce_bytes.as_slice().try_into()
            .map_err(|_| err("vault unlock failed: bad nonce"))?;
        let cipher = ChaCha20Poly1305::new(&Key::from(key));
        let nonce = Nonce::from(nonce_arr);
        // M2(a): see `unlock`'s identical comment.
        let plain: Zeroizing<Vec<u8>> = Zeroizing::new(
            cipher
                .decrypt(&nonce, ct.as_ref())
                .map_err(|_| err("vault unlock failed: wrong key, tampered file, or a vault re-sealed since the key was stored"))?,
        );
        let secrets: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|_| err("vault unlock failed: bad payload"))?;
        let kdf = Kdf { m_cost: env.m_cost, t_cost: env.t_cost, p_cost: env.p_cost };
        Ok(Vault { path: path.to_path_buf(), key, salt, kdf, secrets })
    }

    /// Exports the raw 32-byte vault key derived at unlock time, for a
    /// caller (dbc-mcp's `setup` subcommand) to persist somewhere it can
    /// later feed back into [`Vault::unlock_with_key`] — e.g. the Windows
    /// Credential Manager via the `keyring` crate. The master password
    /// itself is never exposed by this or any other `Vault` method.
    ///
    /// The returned copy is wrapped in [`Zeroizing`] so it's overwritten
    /// with zeros the moment the caller drops it (review round 1 finding
    /// #3: the previous wording here — "zeroize after storing, where
    /// practical" — overclaimed; nothing actually zeroized anything before
    /// this). `Vault`'s own internal `key` field is ALSO zeroized on
    /// `Vault::drop` (security follow-up, final-review.md #14 /
    /// task-2-review.md #2) — the two coverages are independent, so the
    /// exported copy and the vault's own copy are each scrubbed the moment
    /// their respective owner drops them.
    ///
    /// SECURITY (review round 1 finding #4): this method is intentionally
    /// `pub` so `dbc-mcp setup` can call it from outside this crate, which
    /// necessarily widens the exposure surface of a security primitive
    /// (the vault's derived key) beyond `Vault` itself. Callers must never
    /// persist the returned key anywhere except an OS-backed credential
    /// store (Windows Credential Manager / macOS Keychain / Secret
    /// Service) — never to a plain file, a log line, or a config file.
    pub fn export_key(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.key)
    }

    fn persist(&mut self) -> Result<(), StateError> {
        let cipher = ChaCha20Poly1305::new(&Key::from(self.key));
        let mut nonce_arr = [0u8; 12];
        use rand::RngCore as _;
        let mut rng = rand::rng();
        rng.fill_bytes(&mut nonce_arr);
        let nonce = Nonce::from(nonce_arr);
        // M2(b): same reasoning as `unlock`'s decrypted `plain` — the
        // serialized secrets map is plaintext until `encrypt` below, so it's
        // scrubbed on drop instead of left as an unzeroed `Vec<u8>`.
        let plain: Zeroizing<Vec<u8>> =
            Zeroizing::new(serde_json::to_vec(&self.secrets).map_err(|e| err(e.to_string()))?);
        let ct = cipher.encrypt(&nonce, plain.as_slice()).map_err(|e| err(e.to_string()))?;
        let env = Envelope {
            kdf: "argon2id".into(),
            // The cost THIS key came from — not the constants. See `Kdf`.
            m_cost: self.kdf.m_cost, t_cost: self.kdf.t_cost, p_cost: self.kdf.p_cost,
            salt: B64.encode(self.salt),
            nonce: B64.encode(nonce_arr),
            ciphertext: B64.encode(ct),
        };
        if let Some(dir) = self.path.parent() { std::fs::create_dir_all(dir)?; }
        let tmp = crate::fsutil::tmp_path_for(&self.path);
        std::fs::write(&tmp, serde_json::to_string(&env).map_err(|e| err(e.to_string()))?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn set_secret(&mut self, key: &str, value: &str) -> Result<(), StateError> {
        // M2(c): `BTreeMap::insert` returns the REPLACED value (the old
        // plaintext secret, when overwriting an existing key) — previously
        // dropped unzeroed as an anonymous temporary. Scrub it before it
        // drops.
        if let Some(mut old) = self.secrets.insert(key.into(), value.into()) {
            old.zeroize();
        }
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
    crate::workspace::profile_dir().join("vault.bin")
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

    /// Security follow-up (final-review.md #14 / task-2-review.md #2), fixed
    /// per MAJOR M1 of the td-security fix round: the original version of
    /// this test read memory THROUGH raw pointers AFTER `drop` — reading
    /// freed memory is UB regardless of whether it happens to read back
    /// zeros in practice; it is NOT the technique the `zeroize` crate's own
    /// tests use (they assert in place on live values). This version calls
    /// the scrub (`wipe`, the same private method `Drop::drop` calls) on a
    /// still-LIVE `Vault` and asserts the fields directly — no unsafe, and
    /// no dependence on drop/dealloc ordering or allocator behaviour.
    #[test]
    fn key_and_secrets_are_scrubbed_by_wipe() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create(&p, "correct horse").unwrap();
        v.set_secret("c1", "super-secret-plaintext").unwrap();
        assert_ne!(v.key, [0u8; 32], "test setup bug: key was already all-zero");
        assert_eq!(v.secrets.get("c1").map(String::as_str), Some("super-secret-plaintext"));

        v.wipe();

        assert_eq!(v.key, [0u8; 32], "vault key was not scrubbed by wipe()");
        assert_eq!(
            v.secrets.get("c1").map(String::as_str),
            Some(""),
            "secret was not scrubbed by wipe()"
        );
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
    fn unlock_with_key_roundtrips_after_password_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create(&p, "correct horse").unwrap();
        v.set_secret("c1", "tajne-heslo").unwrap();
        let key = v.export_key();
        drop(v);

        // Curated path: open with the exported key, no password involved.
        let v2 = Vault::unlock_with_key(&p, &key).unwrap();
        assert_eq!(v2.get_secret("c1").as_deref(), Some("tajne-heslo"));

        // Locking (dropping) and reopening with the same key still works.
        drop(v2);
        let v3 = Vault::unlock_with_key(&p, &key).unwrap();
        assert_eq!(v3.get_secret("c1").as_deref(), Some("tajne-heslo"));
    }

    #[test]
    fn unlock_with_key_wrong_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let v = Vault::create(&p, "correct horse").unwrap();
        let mut wrong_key = v.export_key();
        wrong_key[0] ^= 0xFF; // flip a bit: definitely the wrong key
        drop(v);

        let err = Vault::unlock_with_key(&p, &wrong_key).unwrap_err();
        assert!(err.message.contains("unlock"), "got: {}", err.message);
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

    /// The cost every vault was sealed with before 2026-09-02.
    const OLD: Kdf = Kdf { m_cost: 65536, t_cost: 3, p_cost: 4 };

    fn cost_on_disk(p: &Path) -> Kdf {
        let env: Envelope = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        Kdf { m_cost: env.m_cost, t_cost: env.t_cost, p_cost: env.p_cost }
    }

    fn salt_on_disk(p: &Path) -> String {
        let env: Envelope = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        env.salt
    }

    /// The latent bug the lighter cost would have set off: `persist` used
    /// to write the CONSTANTS while the key had been derived from the cost
    /// in the file. Saving a password into an old vault would then have
    /// produced an envelope claiming one cost and a ciphertext under
    /// another — a vault nobody can open.
    #[test]
    fn persist_writes_the_cost_the_key_was_derived_with() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create_with(&p, "pw", OLD).unwrap();
        v.set_secret("c1", "x").unwrap();
        assert_eq!(cost_on_disk(&p), OLD, "persist must not claim a cost the key does not have");
        drop(v);
        assert_eq!(Vault::unlock(&p, "pw").unwrap().get_secret("c1").as_deref(), Some("x"));
    }

    #[test]
    fn an_old_vault_is_resealed_under_the_current_cost_on_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let mut v = Vault::create_with(&p, "pw", OLD).unwrap();
        v.set_secret("c1", "tajne").unwrap();
        drop(v);
        let salt_before = salt_on_disk(&p);

        let v = Vault::unlock(&p, "pw").unwrap();
        assert_eq!(v.kdf, Kdf::CURRENT, "the open vault must carry the cost it was re-sealed under");
        assert_eq!(cost_on_disk(&p), Kdf::CURRENT, "the file must be re-sealed at once");
        assert_ne!(salt_on_disk(&p), salt_before, "a re-seal takes a fresh salt");
        assert_eq!(v.get_secret("c1").as_deref(), Some("tajne"));
        drop(v);

        // Second unlock: nothing left to migrate, secrets intact.
        let v2 = Vault::unlock(&p, "pw").unwrap();
        assert_eq!(v2.get_secret("c1").as_deref(), Some("tajne"));
        assert_eq!(cost_on_disk(&p), Kdf::CURRENT);
    }

    #[test]
    fn a_vault_already_at_the_current_cost_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        drop(Vault::create(&p, "pw").unwrap());
        let salt_before = salt_on_disk(&p);
        let bytes_before = std::fs::read(&p).unwrap();
        drop(Vault::unlock(&p, "pw").unwrap());
        assert_eq!(salt_on_disk(&p), salt_before);
        assert_eq!(std::fs::read(&p).unwrap(), bytes_before, "no rewrite without a reason");
    }

    /// A key exported under the old cost cannot open the re-sealed file —
    /// and says so — while a key exported AFTER the re-seal can.
    #[test]
    fn a_stored_key_stops_fitting_once_the_vault_is_resealed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        let v = Vault::create_with(&p, "pw", OLD).unwrap();
        let old_key = v.export_key();
        drop(v);

        let v = Vault::unlock(&p, "pw").unwrap(); // re-seals
        let new_key = v.export_key();
        drop(v);

        let e = Vault::unlock_with_key(&p, &old_key).unwrap_err();
        assert!(e.message.contains("re-sealed"), "got: {}", e.message);
        assert!(Vault::unlock_with_key(&p, &new_key).is_ok());
    }

    /// The whole point of the lighter cost.
    #[test]
    fn a_current_unlock_is_quick() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.bin");
        drop(Vault::create(&p, "pw").unwrap());
        let t = std::time::Instant::now();
        drop(Vault::unlock(&p, "pw").unwrap());
        // Loose on purpose: an unoptimised CI box, not a benchmark. The
        // old cost took 3 s here.
        assert!(t.elapsed() < std::time::Duration::from_millis(1500), "took {:?}", t.elapsed());
    }
}
