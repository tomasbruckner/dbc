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
