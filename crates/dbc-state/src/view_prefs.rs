use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::StateError;

/// Per-table view preferences: column visibility, widths, sort order, expanded FK joins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TableViewPrefs {
    #[serde(default)]
    pub hidden_columns: Vec<String>,      // by column name
    #[serde(default)]
    pub col_widths: Vec<(String, f32)>,   // name → px
    #[serde(default)]
    pub sort: Option<(String, bool)>,     // (column, ascending)
    #[serde(default)]
    pub fk_joins: Vec<String>,            // FK column names expanded
}

/// Stores and retrieves table view preferences by connection + schema + table.
pub struct ViewPrefsStore {
    path: PathBuf,
    prefs: HashMap<String, TableViewPrefs>,
}

impl ViewPrefsStore {
    /// Load view preferences from a TOML file.
    /// Missing file → empty store; corrupt file → error.
    pub fn load(path: &Path) -> Result<ViewPrefsStore, StateError> {
        let prefs = if path.exists() {
            let content = fs::read_to_string(path)?;
            toml::from_str(&content)?
        } else {
            HashMap::new()
        };
        Ok(ViewPrefsStore {
            path: path.to_path_buf(),
            prefs,
        })
    }

    /// Get view preferences for a specific table.
    pub fn get(&self, connection_id: &str, schema: Option<&str>, table: &str) -> Option<&TableViewPrefs> {
        let key = encode_key(connection_id, schema, table);
        self.prefs.get(&key)
    }

    /// Upsert view preferences and save atomically to disk.
    pub fn set(
        &mut self,
        connection_id: &str,
        schema: Option<&str>,
        table: &str,
        prefs: TableViewPrefs,
    ) -> Result<(), StateError> {
        let key = encode_key(connection_id, schema, table);
        self.prefs.insert(key, prefs);
        self.save()
    }

    fn save(&self) -> Result<(), StateError> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(toml::to_string_pretty(&self.prefs)?.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Encode connection_id, schema, and table into a TOML key.
/// Uses the unit separator (U+001F) to avoid collisions with dots in names.
fn encode_key(connection_id: &str, schema: Option<&str>, table: &str) -> String {
    let schema_part = schema.unwrap_or("-");
    format!("{}\u{1F}{}\u{1F}{}", connection_id, schema_part, table)
}

pub fn default_view_prefs_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dbc")
        .join("views.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_set_save_load_get() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("views.toml");

        let mut store = ViewPrefsStore::load(&p).unwrap();
        let prefs = TableViewPrefs {
            hidden_columns: vec!["id".to_string()],
            col_widths: vec![("name".to_string(), 150.0)],
            sort: Some(("created_at".to_string(), false)),
            fk_joins: vec!["author_id".to_string()],
        };

        store
            .set("conn1", Some("public"), "users", prefs.clone())
            .unwrap();

        // Reload from disk
        let loaded = ViewPrefsStore::load(&p).unwrap();
        let retrieved = loaded.get("conn1", Some("public"), "users").unwrap();
        assert_eq!(retrieved, &prefs);
    }

    #[test]
    fn missing_file_creates_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope.toml");
        let store = ViewPrefsStore::load(&p).unwrap();
        assert_eq!(store.prefs.len(), 0);
        assert_eq!(store.get("any", None, "table"), None);
    }

    #[test]
    fn key_collision_safety() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("views.toml");

        let mut store = ViewPrefsStore::load(&p).unwrap();

        let prefs1 = TableViewPrefs {
            hidden_columns: vec!["col1".to_string()],
            col_widths: vec![],
            sort: None,
            fk_joins: vec![],
        };
        let prefs2 = TableViewPrefs {
            hidden_columns: vec!["col2".to_string()],
            col_widths: vec![],
            sort: None,
            fk_joins: vec![],
        };

        // Set with table "a.b" and schema None
        store.set("conn", None, "a.b", prefs1.clone()).unwrap();

        // Set with table "b" and schema "a" — should be different key
        store.set("conn", Some("a"), "b", prefs2.clone()).unwrap();

        // Reload and verify both are stored separately
        let loaded = ViewPrefsStore::load(&p).unwrap();
        assert_eq!(loaded.get("conn", None, "a.b"), Some(&prefs1));
        assert_eq!(loaded.get("conn", Some("a"), "b"), Some(&prefs2));
    }
}
