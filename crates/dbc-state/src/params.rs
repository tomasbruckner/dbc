use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::StateError;

/// A single remembered `:name` parameter value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ParamValue {
    pub text: String,
    pub is_null: bool,
}

/// Stores and retrieves last-used `:name` parameter values by connection + param name.
pub struct ParamValuesStore {
    path: PathBuf,
    values: HashMap<String, ParamValue>,
}

impl ParamValuesStore {
    /// Load param values from a TOML file.
    /// Missing file → empty store; corrupt file → error.
    pub fn load(path: &Path) -> Result<ParamValuesStore, StateError> {
        let values = if path.exists() {
            let content = fs::read_to_string(path)?;
            toml::from_str(&content)?
        } else {
            HashMap::new()
        };
        Ok(ParamValuesStore {
            path: path.to_path_buf(),
            values,
        })
    }

    /// Get the last-used value for a param name on a connection.
    pub fn get(&self, connection_id: &str, name: &str) -> Option<&ParamValue> {
        let key = encode_key(connection_id, name);
        self.values.get(&key)
    }

    /// Upsert a param value and save atomically to disk.
    pub fn set(&mut self, connection_id: &str, name: &str, value: ParamValue) -> Result<(), StateError> {
        let key = encode_key(connection_id, name);
        self.values.insert(key, value);
        self.save()
    }

    fn save(&self) -> Result<(), StateError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = crate::fsutil::tmp_path_for(&self.path);
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(toml::to_string_pretty(&self.values)?.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Encode connection_id and param name into a TOML key.
/// Uses the unit separator (U+001F) to avoid collisions with dots/colons in names.
fn encode_key(connection_id: &str, name: &str) -> String {
    format!("{}\u{1F}{}", connection_id, name)
}

/// `dbc/params.toml` alongside `dbc/views.toml`.
pub fn default_param_values_path() -> PathBuf {
    crate::workspace::profile_dir().join("params.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_set_save_load_get() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");

        let mut store = ParamValuesStore::load(&p).unwrap();
        let value = ParamValue { text: "42".to_string(), is_null: false };
        store.set("conn1", "id", value.clone()).unwrap();

        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn1", "id"), Some(&value));
    }

    #[test]
    fn missing_file_creates_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope.toml");
        let store = ParamValuesStore::load(&p).unwrap();
        assert_eq!(store.get("any", "name"), None);
    }

    #[test]
    fn scope_key_isolates_databases_and_preserves_legacy_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");
        let mut store = ParamValuesStore::load(&p).unwrap();
        let legacy = ParamValue { text: "1".into(), is_null: false };
        let scoped_v = ParamValue { text: "2".into(), is_null: false };
        store.set("conn-1", "id", legacy.clone()).unwrap();
        let scoped = crate::connection_scope_key("conn-1", Some("inventory"));
        store.set(&scoped, "id", scoped_v.clone()).unwrap();
        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn-1", "id"), Some(&legacy));
        assert_eq!(loaded.get(&scoped, "id"), Some(&scoped_v));
    }

    #[test]
    fn key_collision_safety() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");
        let mut store = ParamValuesStore::load(&p).unwrap();

        // "conn" + "a:b" vs "conn:a" + "b" must not collide via naive
        // concatenation (the unit-separator encode_key prevents this,
        // same guard as view_prefs.rs's own key_collision_safety test).
        let v1 = ParamValue { text: "one".to_string(), is_null: false };
        let v2 = ParamValue { text: "two".to_string(), is_null: false };
        store.set("conn", "a:b", v1.clone()).unwrap();
        store.set("conn:a", "b", v2.clone()).unwrap();

        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn", "a:b"), Some(&v1));
        assert_eq!(loaded.get("conn:a", "b"), Some(&v2));
    }

    #[test]
    fn null_flag_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("params.toml");
        let mut store = ParamValuesStore::load(&p).unwrap();
        let value = ParamValue { text: String::new(), is_null: true };
        store.set("conn1", "note", value.clone()).unwrap();

        let loaded = ParamValuesStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn1", "note"), Some(&value));
    }
}
