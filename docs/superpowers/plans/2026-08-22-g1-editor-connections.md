# G1 — Editor & Connections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Multiline SQL editor plus a real connection manager (folders, Argon2id master-password vault, SSH tunnels, read-only flag, timeout, auto-LIMIT), with all connecting moved off the UI thread.

**Architecture:** New `dbc-state` crate owns persistent config (TOML metadata file) and the encrypted vault (Argon2id → ChaCha20-Poly1305 AEAD file). The editor gains a GPUI-free text model (`MultilineBuffer`) unit-tested in isolation, then a GPUI element around it. SSH tunnels spawn the system `ssh.exe` with `-N -L`. Execution-path guards (auto-LIMIT, timeout, read-only) are pure functions in `dbc-core`/`dbc-state` where testable, wired in `dbc-ui`.

**Tech Stack:** Rust, GPUI (pinned rev 907ed09), tokio, argon2 0.5 (stable line, NOT the 0.6 RC), chacha20poly1305 0.11, serde+toml, dirs 6, tempfile (tests).

**Spec:** `docs/superpowers/specs/2026-08-22-gui-target-design.md` (§1 Connections/Editor decisions, §2 row G1, §3 constraints)

## Global Constraints

- `dbc-core` never sees GPUI; `dbc-ui` never sees concrete driver crates; new persistent state lives in `dbc-state` (consumed by dbc-ui only).
- Passwords/secrets NEVER on disk in plaintext and NEVER logged; vault file is the only secret store; master password is never stored.
- Errors are values (`QueryError` or typed state errors); no panics on user-data paths.
- GPUI stays pinned to rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`; if plan code drifts from the rev's API, ground truth is `%USERPROFILE%\.cargo\git\checkouts\zed-*\907ed09\crates\gpui\examples\`.
- Build/test only with `-p <crate>` (never bare `cargo build` — the GPUI tree is huge); long cargo commands run in background with log polling.
- Read-only flag semantics in G1: the app refuses to EXECUTE any statement on a read-only connection whose first keyword is not in the read allowlist (SELECT / WITH / EXPLAIN / SHOW / VALUES / PRAGMA). Deeper enforcement (sandbox, admin) arrives with those features.
- Commit after every task; conventional messages.

---

### Task 1: dbc-state — connection config model + TOML persistence

**Files:**
- Create: `crates/dbc-state/Cargo.toml`, `crates/dbc-state/src/lib.rs`, `crates/dbc-state/src/config.rs`
- Modify: root `Cargo.toml` (workspace members + workspace deps `serde`, `toml`, `dirs`, `tempfile`)

**Interfaces:**
- Consumes: nothing from workspace (serde/toml/dirs only).
- Produces (all `pub use`d from `dbc_state` root):
  - `Engine { Postgres, Mssql, Sqlite }` (serde, lowercase)
  - `SshTunnelConfig { host: String, port: u16, user: String, key_path: Option<String> }`
  - `ConnectionConfig { id: String, name: String, folder: Vec<String>, engine: Engine, host: String, port: Option<u16>, database: String, user: String, read_only: bool, timeout_secs: Option<u64>, auto_limit: Option<u64>, ssh: Option<SshTunnelConfig>, favourite: bool }` — no password field, ever
  - `AppConfig { connections: Vec<ConnectionConfig> }`
  - `AppConfig::load(path: &Path) -> Result<AppConfig, StateError>` (missing file → default empty)
  - `AppConfig::save(&self, path: &Path) -> Result<(), StateError>` (atomic: write temp + rename)
  - `default_config_path() -> PathBuf` (`dirs::config_dir()/dbc/config.toml`)
  - `StateError { message: String }` with Display; `From<io::Error>`, `From<toml::…>`

- [ ] **Step 1: Crate setup**

Root `Cargo.toml` — add `"crates/dbc-state"` to members and to `[workspace.dependencies]`:
```toml
serde = { version = "1", features = ["derive"] }
toml = "1"
dirs = "6"
tempfile = "3"
```
`crates/dbc-state/Cargo.toml`:
```toml
[package]
name = "dbc-state"
version = "0.1.0"
edition.workspace = true

[dependencies]
serde.workspace = true
toml.workspace = true
dirs.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Write failing tests** (bottom of `config.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppConfig {
        AppConfig {
            connections: vec![ConnectionConfig {
                id: "c1".into(),
                name: "demo".into(),
                folder: vec!["work".into(), "prod".into()],
                engine: Engine::Postgres,
                host: "localhost".into(),
                port: Some(5432),
                database: "postgres".into(),
                user: "postgres".into(),
                read_only: true,
                timeout_secs: Some(30),
                auto_limit: Some(1000),
                ssh: Some(SshTunnelConfig {
                    host: "bastion".into(), port: 22, user: "tomas".into(), key_path: None,
                }),
                favourite: false,
            }],
        }
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        sample().save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded, sample());
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = AppConfig::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(loaded, AppConfig::default());
    }

    #[test]
    fn no_password_field_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        sample().save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.to_lowercase().contains("password"));
    }
}
```

- [ ] **Step 3: Run to verify failure** — `cargo test -p dbc-state` → compile error (types undefined).

- [ ] **Step 4: Implement** `config.rs`:

```rust
use std::io::Write;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError { pub message: String }
impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.message) }
}
impl std::error::Error for StateError {}
impl From<std::io::Error> for StateError {
    fn from(e: std::io::Error) -> Self { Self { message: e.to_string() } }
}
impl From<toml::de::Error> for StateError {
    fn from(e: toml::de::Error) -> Self { Self { message: e.to_string() } }
}
impl From<toml::ser::Error> for StateError {
    fn from(e: toml::ser::Error) -> Self { Self { message: e.to_string() } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine { Postgres, Mssql, Sqlite }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnelConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub folder: Vec<String>,
    pub engine: Engine,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub read_only: bool,
    pub timeout_secs: Option<u64>,
    pub auto_limit: Option<u64>,
    pub ssh: Option<SshTunnelConfig>,
    #[serde(default)]
    pub favourite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<AppConfig, StateError> {
        if !path.exists() { return Ok(AppConfig::default()); }
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(toml::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

pub fn default_config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("dbc").join("config.toml")
}
```
`lib.rs`:
```rust
mod config;
pub use config::{
    default_config_path, AppConfig, ConnectionConfig, Engine, SshTunnelConfig, StateError,
};
```

- [ ] **Step 5: Run tests** — `cargo test -p dbc-state` → 3 PASS.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat: dbc-state connection config with TOML persistence"`

---

### Task 2: dbc-state — Argon2id master-password vault

**Files:**
- Create: `crates/dbc-state/src/vault.rs`
- Modify: `crates/dbc-state/Cargo.toml`, `src/lib.rs`

**Interfaces:**
- Consumes: `StateError` from Task 1.
- Produces:
  - `Vault::create(path: &Path, master: &str) -> Result<Vault, StateError>` (new empty vault file)
  - `Vault::unlock(path: &Path, master: &str) -> Result<Vault, StateError>` (wrong password or tampered file → Err whose message contains "unlock")
  - `Vault::exists(path: &Path) -> bool`
  - `Vault::set_secret(&mut self, key: &str, value: &str) -> Result<(), StateError>` (persists immediately)
  - `Vault::get_secret(&self, key: &str) -> Option<String>`
  - `Vault::remove_secret(&mut self, key: &str) -> Result<(), StateError>`
  - `default_vault_path() -> PathBuf` (`dirs::config_dir()/dbc/vault.bin`)
- File format (JSON envelope, binary payload base64): `{ "kdf": "argon2id", "m_cost": 65536, "t_cost": 3, "p_cost": 4, "salt": b64, "nonce": b64, "ciphertext": b64 }`. Plaintext inside AEAD is a JSON map `{key: secret}`. Every save re-generates the nonce. AEAD = ChaCha20-Poly1305; the Poly1305 tag is what makes tampering and wrong passwords fail closed.

- [ ] **Step 1: Deps**

`crates/dbc-state/Cargo.toml` add:
```toml
argon2 = "0.5"
chacha20poly1305 = "0.11"
rand = "0.9"
base64 = "0.22"
serde_json = "1"
```
(If `chacha20poly1305` 0.11 pulls incompatible `rand_core`, align: use the `chacha20poly1305::aead::OsRng` re-export instead of `rand` and drop the `rand` dep — record the adaptation.)

- [ ] **Step 2: Write failing tests** (bottom of `vault.rs`)

```rust
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
```

- [ ] **Step 3: Run to verify failure** — `cargo test -p dbc-state vault` → compile error.

- [ ] **Step 4: Implement** `vault.rs`:

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
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

fn err(m: impl Into<String>) -> StateError { StateError { message: m.into() } }

fn derive_key(master: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<Key, StateError> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| err(e.to_string()))?;
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    a2.hash_password_into(master.as_bytes(), salt, &mut out)
        .map_err(|e| err(e.to_string()))?;
    Ok(*Key::from_slice(&out))
}

impl Vault {
    pub fn exists(path: &Path) -> bool { path.exists() }

    pub fn create(path: &Path, master: &str) -> Result<Vault, StateError> {
        let mut salt = [0u8; 16];
        use chacha20poly1305::aead::rand_core::RngCore;
        OsRng.fill_bytes(&mut salt);
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
        let key = derive_key(master, &salt, env.m_cost, env.t_cost, env.p_cost)?;
        let nonce_bytes = B64.decode(&env.nonce).map_err(|_| err("vault unlock failed: bad nonce"))?;
        let ct = B64.decode(&env.ciphertext).map_err(|_| err("vault unlock failed: bad ciphertext"))?;
        let cipher = ChaCha20Poly1305::new(&key);
        let plain = cipher
            .decrypt(Nonce::from_slice(&nonce_bytes), ct.as_ref())
            .map_err(|_| err("vault unlock failed: wrong master password or tampered file"))?;
        let secrets: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|_| err("vault unlock failed: bad payload"))?;
        Ok(Vault { path: path.to_path_buf(), key, salt, secrets })
    }

    fn persist(&mut self) -> Result<(), StateError> {
        let cipher = ChaCha20Poly1305::new(&self.key);
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plain = serde_json::to_vec(&self.secrets).map_err(|e| err(e.to_string()))?;
        let ct = cipher.encrypt(&nonce, plain.as_ref()).map_err(|e| err(e.to_string()))?;
        let env = Envelope {
            kdf: "argon2id".into(),
            m_cost: M_COST, t_cost: T_COST, p_cost: P_COST,
            salt: B64.encode(self.salt),
            nonce: B64.encode(nonce),
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
```
Add to `lib.rs`: `mod vault; pub use vault::{default_vault_path, Vault};`
API-drift note: exact `argon2`/`chacha20poly1305` 0.5/0.11 item paths may differ slightly (e.g. `Params::new` arg order, `generate_nonce` location) — adapt to the installed versions' docs.rs, record adaptations.

- [ ] **Step 5: Run tests** — `cargo test -p dbc-state` → 7 PASS total (3 + 4). Argon2 at 64 MiB may take ~1 s per derivation — fine.

- [ ] **Step 6: Commit** — `git commit -m "feat: Argon2id master-password vault with AEAD storage"`

---

### Task 3: Multiline text model (GPUI-free)

**Files:**
- Create: `crates/dbc-ui/src/text_model.rs`
- Modify: `crates/dbc-ui/src/main.rs` (add `mod text_model;`)

**Interfaces:**
- Consumes: `unicode-segmentation` (already a dep).
- Produces `pub struct MultilineBuffer` — pure model, no GPUI imports (this is what makes it unit-testable; the GPUI element in Task 4 delegates every mutation here):
  - `new() -> Self`; `from_text(&str) -> Self`
  - `text(&self) -> &str`; `set_text(&mut self, &str)` (resets cursor to end)
  - `cursor(&self) -> usize` (byte offset, always on a char boundary)
  - `selection(&self) -> Option<Range<usize>>` (byte range, ordered)
  - `insert(&mut self, s: &str)` (replaces selection if any)
  - `backspace(&mut self)`, `delete(&mut self)` (selection-aware; grapheme-aware when no selection)
  - `move_left/move_right(&mut self, extend_selection: bool)` (grapheme steps)
  - `move_up/move_down(&mut self, extend_selection: bool)` (remembers goal column across consecutive vertical moves)
  - `move_home/move_end(&mut self, extend_selection: bool)` (line start/end)
  - `select_all(&mut self)`
  - `line_count(&self) -> usize`; `lines(&self) -> impl Iterator<Item = &str>`
  - `cursor_position(&self) -> (usize, usize)` (line index, byte column within line)
  - `offset_at(&self, line: usize, byte_col: usize) -> usize` (clamped; for mouse clicks)

- [ ] **Step 1: Write failing tests** (bottom of `text_model.rs`) — the test suite IS the behavioural contract:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_newlines() {
        let mut b = MultilineBuffer::new();
        b.insert("select 1\nfrom t");
        assert_eq!(b.text(), "select 1\nfrom t");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.cursor(), b.text().len());
    }

    #[test]
    fn vertical_movement_keeps_goal_column() {
        let mut b = MultilineBuffer::from_text("abcdef\nxy\nabcdef");
        // cursor at end of first line (col 6)
        for _ in 0..10 { b.move_right(false); }
        assert_eq!(b.cursor_position(), (0, 6));
        b.move_down(false);              // line "xy" only has 2 cols → clamp
        assert_eq!(b.cursor_position(), (1, 2));
        b.move_down(false);              // back to a long line → goal col 6 restored
        assert_eq!(b.cursor_position(), (2, 6));
    }

    #[test]
    fn selection_replace() {
        let mut b = MultilineBuffer::from_text("hello world");
        b.move_home(false);
        for _ in 0..5 { b.move_right(true); } // select "hello"
        assert_eq!(b.selection(), Some(0..5));
        b.insert("bye");
        assert_eq!(b.text(), "bye world");
        assert_eq!(b.selection(), None);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn grapheme_aware_backspace() {
        let mut b = MultilineBuffer::from_text("ař🙂");
        b.backspace();
        assert_eq!(b.text(), "ař");
        b.backspace();
        assert_eq!(b.text(), "a");
    }

    #[test]
    fn home_end_are_line_scoped() {
        let mut b = MultilineBuffer::from_text("one\ntwo three");
        // put cursor on line 1 middle
        let off = b.offset_at(1, 3);
        assert_eq!(&b.text()[off..off+1], " ");
        b.set_cursor_for_test(off);
        b.move_home(false);
        assert_eq!(b.cursor_position(), (1, 0));
        b.move_end(false);
        assert_eq!(b.cursor_position(), (1, "two three".len()));
    }

    #[test]
    fn click_offset_clamps() {
        let b = MultilineBuffer::from_text("ab\ncd");
        assert_eq!(b.offset_at(0, 99), 2);   // end of line 0
        assert_eq!(b.offset_at(9, 0), 5);    // past last line → end of text
    }

    #[test]
    fn delete_selection_across_lines() {
        let mut b = MultilineBuffer::from_text("aaa\nbbb\nccc");
        b.set_cursor_for_test(1);
        for _ in 0..6 { b.move_right(true); } // selects "aa\nbbb"... byte-wise 1..7
        b.delete();
        assert_eq!(b.text(), "abb\nccc".replace("bb\n", "b\n")); // = "ab\nccc"? — compute: "aaa\nbbb\nccc" minus 1..7 = "a" + "b\nccc" = "ab\nccc"
        assert_eq!(b.text(), "ab\nccc");
    }
}
```
(`set_cursor_for_test` is `#[cfg(test)] pub(crate) fn` — clamps to char boundary.)

- [ ] **Step 2: Run to verify failure** — `cargo test -p dbc-ui text_model` → compile error. Note: `cargo test -p dbc-ui` builds GPUI — run it in background with log polling; iterate on this file via `cargo test -p dbc-ui text_model` only.

- [ ] **Step 3: Implement** — grapheme navigation via `unicode_segmentation::UnicodeSegmentation` (`grapheme_indices`); lines via `split('\n')` with byte-offset bookkeeping; goal column stored as `Option<usize>` cleared by any horizontal move/edit. Keep the file self-contained; no GPUI imports (enforced by review).

- [ ] **Step 4: Run tests** — all 7 PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: multiline text model with grapheme-aware editing"`

---

### Task 4: Multiline editor element (GPUI)

**Files:**
- Modify: `crates/dbc-ui/src/sql_input.rs` (major rework around `MultilineBuffer`)
- Modify: `crates/dbc-ui/src/main.rs` (editor area height: fixed 8 lines with internal vertical scroll)

**Interfaces:**
- Consumes: `MultilineBuffer` (Task 3), existing gpui actions/bindings in `sql_input.rs`.
- Produces: `SqlInput` keeps its public surface — `new(cx)`, `text() -> String` (now returns real multiline text, no `\n` replacement — delete the old replace), `set_text(&mut self, &str, cx)` (new, for history-load later), focus handle. Key behaviour: `Enter` inserts newline; `Ctrl+Enter` still triggers the app-level `RunQuery` action (binding precedence: keep `enter` binding scoped to the input context and `ctrl-enter` app-level, as today); arrows/home/end/backspace/delete/select-variants delegate to the model; click maps y→line, x→`ShapedLine::closest_index_for_x` → `offset_at`; drag selects; Ctrl+A/C/V/X work across lines (clipboard via existing handlers).
- Rendering: one `ShapedLine` per visible line (shape each line separately with the existing TextRun approach); cursor drawn as a 2px quad at (line, x); selection drawn as per-line background quads (first/middle/last line spans); vertical scroll offset in lines, mouse wheel ± and cursor-follow (keep cursor visible on edits/moves).
- The Zed example `input.rs` at the pinned rev remains the reference for element plumbing (`ElementInputHandler`, layout/paint), but state and mutations now live in `MultilineBuffer`. IME/marked-text: keep the existing single-range mechanics operating on the model's byte offsets — degraded IME positioning across wrapped lines is acceptable (no wrapping in G1: long lines scroll horizontally per-line via the shaped line's natural width, clipped).

- [ ] **Step 1: Rework** `sql_input.rs` per the interface block. Delete fields duplicated by the model (`content`, `selected_range`, `selection_reversed` …) and route every action handler through `MultilineBuffer`.
- [ ] **Step 2: Build** — `cargo build -p dbc-ui` in background, poll log; fix drift against the checkout example.
- [ ] **Step 3: Manual verification** — launch against a sqlite file; type a 3-line query with Enter; arrows/home/end move within and across lines; shift-select across lines shows per-line highlight; Ctrl+C/V roundtrips multiline text; Ctrl+Enter runs; the runner receives the full multiline SQL (sqlite accepts `select\n1` — verify row appears). Record what was verified; a human pass stays on the checklist.
- [ ] **Step 4: Commit** — `git commit -m "feat: multiline SQL editor"`

---

### Task 5: SSH tunnel manager

**Files:**
- Create: `crates/dbc-ui/src/tunnel.rs`
- Modify: `crates/dbc-ui/src/main.rs` (`mod tunnel;`)

**Interfaces:**
- Consumes: `dbc_state::SshTunnelConfig`.
- Produces:
  - `Tunnel::open(cfg: &SshTunnelConfig, target_host: &str, target_port: u16) -> Result<Tunnel, String>` — picks a free local port (bind `127.0.0.1:0`, note port, drop listener), spawns system `ssh.exe`: `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes [-i key_path] -p {cfg.port} -L {local}:{target_host}:{target_port} {cfg.user}@{cfg.host}`, then polls `TcpStream::connect(("127.0.0.1", local))` up to 10 s (100 ms steps). On success returns `Tunnel { local_port: u16, child: std::process::Child }`.
  - `Tunnel::local_port(&self) -> u16`
  - `impl Drop for Tunnel` → `child.kill()`.
  - Failure modes as `Err(String)`: ssh.exe not found (probe `where ssh` once, cache), auth failure/timeout (include stderr tail — BatchMode makes password prompts fail fast, which is intended: G1 supports key/agent auth only; say so in the connection dialog's SSH section).
- Design note: spawning the system OpenSSH client (present on Windows 11 and in Git) avoids embedding an SSH stack; keys/known_hosts/agent behave exactly as the user's `ssh` does. A pure-Rust client (russh) is a possible later swap behind this same interface.

- [ ] **Step 1: Write tests** — unit-test what is testable without a server:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn free_port_is_free() {
        let p = pick_free_port().unwrap();
        assert!(std::net::TcpListener::bind(("127.0.0.1", p)).is_ok());
    }
    #[test]
    fn missing_binary_is_a_value_error() {
        let e = spawn_ssh("definitely-not-ssh-binary-xyz", &["-V".into()]).unwrap_err();
        assert!(e.contains("ssh"));
    }
}
```
(`pick_free_port() -> Result<u16, String>` and `spawn_ssh(program: &str, args: &[String]) -> Result<std::process::Child, String>` are the internal seams; `Tunnel::open` composes them.)
- [ ] **Step 2: Run** — fail, implement, pass (`cargo test -p dbc-ui tunnel`).
- [ ] **Step 3: Commit** — `git commit -m "feat: ssh tunnel via system OpenSSH client"`

---

### Task 6: Execution guards — auto-LIMIT + read-only allowlist (dbc-core)

**Files:**
- Create: `crates/dbc-core/src/guards.rs`
- Modify: `crates/dbc-core/src/lib.rs` (`mod guards; pub use guards::{apply_auto_limit, is_read_statement};`)

**Interfaces:**
- Produces:
  - `is_read_statement(sql: &str) -> bool` — first significant keyword (case-insensitive, after stripping leading whitespace and `--`/`/* */` comments) ∈ {SELECT, WITH, EXPLAIN, SHOW, VALUES, PRAGMA}.
  - `apply_auto_limit(sql: &str, limit: u64) -> (String, bool)` — returns possibly rewritten SQL and whether it changed. Heuristic (documented as such): only when the statement starts with SELECT (not WITH — CTEs are left alone), contains no top-level `LIMIT`, `OFFSET`, `FETCH`, or `INTO` token (token scan outside string literals `'…'`, quoted idents `"…"`, and comments), and does not end in an open string/comment. Rewrites by appending ` LIMIT {n}` before any trailing `;`.

- [ ] **Step 1: Write failing tests**
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_allowlist() {
        assert!(is_read_statement("  SELECT 1"));
        assert!(is_read_statement("-- note\nwith x as (select 1) select * from x"));
        assert!(is_read_statement("EXPLAIN ANALYZE select 1"));
        assert!(!is_read_statement("UPDATE t SET a=1"));
        assert!(!is_read_statement("/* c */ delete from t"));
        assert!(!is_read_statement("insert into t values (1)"));
    }

    #[test]
    fn auto_limit_appends() {
        let (sql, changed) = apply_auto_limit("select * from big", 1000);
        assert!(changed);
        assert_eq!(sql, "select * from big LIMIT 1000");
        let (sql2, changed2) = apply_auto_limit("select * from big;", 1000);
        assert!(changed2);
        assert_eq!(sql2, "select * from big LIMIT 1000;");
    }

    #[test]
    fn auto_limit_leaves_limited_and_nonselect_alone() {
        assert!(!apply_auto_limit("select * from t limit 5", 1000).1);
        assert!(!apply_auto_limit("select * from t OFFSET 2", 1000).1);
        assert!(!apply_auto_limit("update t set a=1", 1000).1);
        assert!(!apply_auto_limit("with x as (select 1) select * from x", 1000).1);
        // LIMIT inside a string literal must not count as a LIMIT token:
        let (s, ch) = apply_auto_limit("select 'no limit here' from t", 1000);
        assert!(ch);
        assert_eq!(s, "select 'no limit here' from t LIMIT 1000");
    }
}
```
- [ ] **Step 2: Run** — fail; **Step 3** implement (hand-rolled scanner: iterate chars tracking in_string/in_ident/in_line_comment/in_block_comment; collect top-level uppercase tokens); **Step 4** pass (`cargo test -p dbc-core`).
- [ ] **Step 5: Commit** — `git commit -m "feat: auto-LIMIT and read-only statement guards"`

---

### Task 7: Connection dialog, folders, switcher, master-password prompt

**Files:**
- Create: `crates/dbc-ui/src/connections_ui.rs` (dialog + dropdown + folder tree)
- Modify: `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/Cargo.toml` (add `dbc-state = { path = "../dbc-state" }`)

**Interfaces:**
- Consumes: `AppConfig`/`ConnectionConfig`/`Engine`/`Vault` (dbc-state), `SqlInput::set_text`, existing `connect::open` (extended in Task 8), modal-overlay pattern: a top-level `Option<ModalState>` on `AppView` rendered as a dimmed overlay + centered panel (GPUI: absolutely-positioned div; reuse the palette-less approach — plain conditional child in `render`).
- Produces (behavioural contract for review):
  1. Startup: load `AppConfig` from `default_config_path()`. If a CLI arg is present it still works exactly as today (back-compat path, no vault needed for sqlite paths / URL-with-password).
  2. If any saved connection needs a secret and the vault exists → master-password prompt modal on first use (not at startup — only when a connection is actually opened); wrong password → error inside the modal, retry. No vault yet + saving a password → "create master password" variant (enter twice, match check).
  3. Top bar: current connection name + ▾; click opens dropdown grouped by folder path (`work/prod` renders as nested groups), favourites first (`favourite: true`), then folders alphabetically; entries show engine + host. Click = switch (Task 8 connect flow). Last entry "Nové spojení…".
  4. Dialog (new/edit): fields name, engine (cycle button pg/mssql/sqlite), host, port, database, user, password (masked, single-line input reusing `SqlInput` with a mask flag is NOT required — implement a tiny `MaskedInput` single-line by instantiating a second `MultilineBuffer`-backed input that renders `•`), folder (free text `a/b`), read-only checkbox, timeout secs, auto-limit rows, SSH section (enable checkbox → host/port/user/key path; note "key/agent auth only"). Buttons: Test (runs Task 8 connect+`SELECT 1` off-thread, shows ✓/error inline), Save (persists config; password → vault under key = connection id), Cancel. Editing an existing connection pre-fills; password field empty means "keep existing secret".
  5. `Engine::Mssql` may be selected but Test/connect reports "MSSQL driver not yet available" (driver is a separate roadmap item) — the config round-trips fine.
- [ ] **Step 1: Implement** per contract. **Step 2: Build** (background+poll). **Step 3: Manual verification** — create vault, save a postgres connection with password, restart app, open connection → master prompt → connect works; folder grouping renders; test button reports ✓ and a deliberate wrong-password failure surfaces inline; `config.toml` contains no secrets (grep). **Step 4: Commit** — `git commit -m "feat: connection manager with folders, vault prompt, test button"`

---

### Task 8: Connect flow off the UI thread + guards wiring

**Files:**
- Modify: `crates/dbc-ui/src/connect.rs`, `crates/dbc-ui/src/runner.rs`, `crates/dbc-ui/src/main.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `connect.rs`: `pub fn open_config(cfg: &ConnectionConfig, secret: Option<String>, runtime: &tokio::runtime::Handle) -> Result<OpenConnection, QueryError>` where `OpenConnection { conn: Box<dyn Connection>, _tunnel: Option<tunnel::Tunnel> }` (tunnel lifetime tied to the connection). Builds the URL from fields (pg: `postgres://user:pass@host:port/db`; sqlite: `database` field is the file path; tunnel rewrites host/port to `127.0.0.1:{tunnel.local_port()}`). Existing `open(url, …)` stays for the CLI-arg path.
  - **Off the UI thread:** `AppView::on_run_query` no longer calls connect synchronously. New flow: spawn on runner runtime → (tunnel? + connect + query) inside the async task; UI state machine gains `Connecting` status ("connecting…" in status bar, Esc aborts by cancelling a connect-scoped `CancelToken` checked between steps). The existing `QueryEvent` channel carries the outcome (reuse `Failed` for connect errors).
  - **Guards wiring (order):** (1) read-only: if `cfg.read_only && !is_read_statement(&sql)` → `Failed(QueryError::msg("connection is read-only"))` without connecting; (2) auto-limit: if `cfg.auto_limit = Some(n)` → `apply_auto_limit`; when changed, status bar suffix "· auto-LIMIT n" and a per-run bypass: `Ctrl+Shift+Enter` runs without the guard (new action `RunQueryUnlimited`); (3) timeout: if `cfg.timeout_secs = Some(t)` → runner spawns a watchdog `tokio::time::sleep(t)` racing the stream; on firing it cancels the query's `CancelToken` and the UI shows `error: [timeout] query exceeded {t}s`.
- [ ] **Step 1: Implement.** **Step 2: Build + full workspace test sweep** — `cargo test -p dbc-core -p dbc-buffer -p dbc-state -p dbc-driver-sqlite` green; postgres ignored suite if Docker is up. **Step 3: Manual verification** — saved pg connection: connect happens with UI alive (status "connecting…"); unreachable host: UI stays responsive, Esc aborts, error lands in status bar; read-only connection rejects `update` instantly; bare `select * from big` gets auto-LIMIT with status note; Ctrl+Shift+Enter bypasses; timeout 2 s kills `pg_sleep(30)` with `[timeout]`. **Step 4: Commit** — `git commit -m "feat: async connect with tunnel, read-only, auto-limit, timeout guards"`

---

## Self-Review Notes

- Spec coverage (G1 row + §1 Connections/Editor): multiline editor → T3/T4; dialog+folders → T7; Argon2id vault → T2; SSH tunnel → T5+T8; read-only flag → T6+T8; timeout → T8; auto-LIMIT → T6+T8; connect off UI thread → T8; favourites field exists in config (T1) but favourites UX is G3 — only the dropdown ordering consumes it here, per spec's "favourites first" dropdown note landing fully in G3 (dropdown shows favourites first already, cheap).
- Type consistency pass: `ConnectionConfig` fields used in T7/T8 match T1; `Vault` API in T7 matches T2; `MultilineBuffer` API in T4 matches T3; `Tunnel::open`/`local_port` in T8 matches T5; guard fns in T8 match T6.
- Placeholder scan: T4/T7/T8 are GPUI-heavy and specify behavioural contracts + verification checklists instead of verbatim GPUI code (pre-1.0 API drift makes verbatim GPUI code counterproductive — established pattern from phases 0-2); all testable logic (T1/T2/T3/T5/T6) carries complete code or complete test contracts.
- Known risks: multiline editor is the big one (T3 model-first de-risks it); vault crate API drift (adaptation note in T2); BatchMode-only SSH auth is an accepted G1 limitation surfaced in the dialog UI.
