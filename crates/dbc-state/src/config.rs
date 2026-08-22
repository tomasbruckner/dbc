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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavouriteObject {
    pub connection_id: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub favourite_objects: Vec<FavouriteObject>,
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

    pub fn is_favourite(&self, f: &FavouriteObject) -> bool {
        self.favourite_objects.contains(f)
    }

    pub fn toggle_favourite(&mut self, f: FavouriteObject) -> bool {
        if let Some(pos) = self.favourite_objects.iter().position(|x| x == &f) {
            self.favourite_objects.remove(pos);
            false
        } else {
            self.favourite_objects.push(f);
            true
        }
    }
}

pub fn default_config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("dbc").join("config.toml")
}

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
            favourite_objects: vec![],
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
    fn corrupt_file_is_load_error_not_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "this is not valid toml {{{").unwrap();
        let result = AppConfig::load(&p);
        assert!(result.is_err(), "a parse error must surface as Err, not silently become default");
    }

    #[test]
    fn no_password_field_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        sample().save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.to_lowercase().contains("password"));
    }

    #[test]
    fn favourite_objects_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");

        let mut config = sample();
        let fav = FavouriteObject {
            connection_id: "c1".into(),
            schema: Some("public".into()),
            name: "users".into(),
            kind: "table".into(),
        };

        // Toggle on
        let state = config.toggle_favourite(fav.clone());
        assert_eq!(state, true);
        assert_eq!(config.is_favourite(&fav), true);

        // Save and load
        config.save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded.is_favourite(&fav), true);

        // Toggle off
        let mut config2 = loaded;
        let state = config2.toggle_favourite(fav.clone());
        assert_eq!(state, false);
        assert_eq!(config2.is_favourite(&fav), false);

        // Save and load again
        config2.save(&p).unwrap();
        let loaded2 = AppConfig::load(&p).unwrap();
        assert_eq!(loaded2.is_favourite(&fav), false);
    }

    #[test]
    fn old_config_without_favourites_loads() {
        let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.favourite_objects, vec![]);
    }
}
