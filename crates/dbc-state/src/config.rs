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
pub enum Engine { Postgres, Mssql, Sqlite, Duckdb }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnelConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<String>,
}

fn default_true() -> bool { true }

/// G15 T3: MSSQL-only connection options, saved alongside `ConnectionConfig`.
/// Carries NO secret — the password stays vault-only, never in this struct
/// or in `config.toml` (see `no_password_field_serialized`). `None` on
/// `ConnectionConfig::mssql` means "all defaults" — the secure-by-default
/// Driver 18 posture (`encrypt: true`, `trust_server_certificate: false`,
/// `driver: None` ⇒ "ODBC Driver 18 for SQL Server") — so old config files
/// with no `[connections.mssql]` table load unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MssqlOptions {
    #[serde(default = "default_true")]
    pub encrypt: bool,
    #[serde(default)]
    pub trust_server_certificate: bool,
    #[serde(default)]
    pub driver: Option<String>,
}

impl Default for MssqlOptions {
    fn default() -> Self {
        Self { encrypt: true, trust_server_certificate: false, driver: None }
    }
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
    /// G15 T3: MSSQL-only options (encrypt/trust cert/driver override).
    /// `None` for every non-MSSQL connection and for old config files —
    /// see `MssqlOptions`'s doc comment.
    #[serde(default)]
    pub mssql: Option<MssqlOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavouriteObject {
    pub connection_id: String,
    pub schema: Option<String>,
    pub name: String,
    pub kind: String,
    /// Sidebar rework (design §5 row 9): the database this favourite lives
    /// in. `None` = the connection's DEFAULT database (whatever
    /// `ConnectionConfig::database` says at read time), so every existing
    /// config.toml entry keeps meaning exactly what it meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolPaths {
    #[serde(default)]
    pub pg_dump: Option<String>,
    #[serde(default)]
    pub pg_restore: Option<String>,
    /// Design CURATION item 1: was missing from the design's own §1 sketch
    /// of `ToolPaths` even though §3's plain-SQL restore pipes through
    /// `psql` — added here with identical shape/detection/override to
    /// `pg_dump`/`pg_restore`.
    #[serde(default)]
    pub psql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub favourite_objects: Vec<FavouriteObject>,
    #[serde(default)]
    pub theme: ThemeMode,
    /// Global, not per-connection (an installed tool is a machine property,
    /// not a connection property) — design §1.
    #[serde(default)]
    pub tool_paths: ToolPaths,
    /// Scripts library (Bruno model, design §2): absolute path of the
    /// user-chosen folder of plain `.sql` files. `None` = feature dormant
    /// (the sidebar section points at Settings). A path, not a secret.
    /// Git integration for this folder is deliberately EXTERNAL (user
    /// decision 2026-08-25) — the app never reads or writes anything
    /// git-related about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts_dir: Option<String>,
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
                mssql: None,
            }],
            favourite_objects: vec![],
            theme: ThemeMode::Dark,
            tool_paths: ToolPaths::default(),
            scripts_dir: None,
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
            database: None,
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

    #[test]
    fn favourite_without_database_field_loads_and_roundtrips_byte_identically() {
        // Sidebar rework: `database` is additive with serde(default) +
        // skip_serializing_if — an old config.toml must load AND save back
        // without gaining the field (same posture as G16's variant pin).
        let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"

[[favourite_objects]]
connection_id = "c1"
name = "orders"
kind = "table"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.favourite_objects[0].database, None);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        config.save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("database = ") || raw.matches("database = ").count() == 1,
            "favourite must not serialize a database key when None (only the connection's own): {raw}");
        let reloaded = AppConfig::load(&p).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn favourite_with_database_roundtrips() {
        let mut config = AppConfig::default();
        config.favourite_objects.push(FavouriteObject {
            connection_id: "c1".into(),
            schema: Some("public".into()),
            name: "orders".into(),
            kind: "table".into(),
            database: Some("inventory".into()),
        });
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        config.save(&p).unwrap();
        let reloaded = AppConfig::load(&p).unwrap();
        assert_eq!(reloaded.favourite_objects[0].database.as_deref(), Some("inventory"));
    }

    #[test]
    fn toggle_favourite_distinguishes_databases() {
        // Full-struct equality in toggle_favourite means the same table in
        // two databases is two distinct favourites — pin it.
        let mut config = AppConfig::default();
        let f_default = FavouriteObject {
            connection_id: "c1".into(), schema: None, name: "t".into(),
            kind: "table".into(), database: None,
        };
        let f_other = FavouriteObject { database: Some("inventory".into()), ..f_default.clone() };
        assert!(config.toggle_favourite(f_default.clone()));
        assert!(config.toggle_favourite(f_other.clone()));
        assert_eq!(config.favourite_objects.len(), 2);
        assert!(!config.toggle_favourite(f_default)); // removes only the default-db one
        assert_eq!(config.favourite_objects.len(), 1);
    }

    #[test]
    fn old_config_without_theme_loads() {
        // Same forward-compat posture as old_config_without_favourites_loads:
        // a pre-G14 config.toml with no `theme` key defaults to Dark.
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
        assert_eq!(config.theme, ThemeMode::Dark);
    }

    #[test]
    fn theme_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut config = sample();
        config.theme = ThemeMode::Light;
        config.save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded.theme, ThemeMode::Light);
    }

    fn sample_with_tools() -> AppConfig {
        let mut c = sample();
        c.tool_paths = ToolPaths {
            pg_dump: Some(r"C:\Program Files\PostgreSQL\16\bin\pg_dump.exe".into()),
            pg_restore: Some(r"C:\Program Files\PostgreSQL\16\bin\pg_restore.exe".into()),
            psql: Some(r"C:\Program Files\PostgreSQL\16\bin\psql.exe".into()),
        };
        c
    }

    #[test]
    fn tool_paths_roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        sample_with_tools().save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded, sample_with_tools());
    }

    #[test]
    fn tool_paths_defaults_to_none_when_absent_from_old_config() {
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
        assert_eq!(config.tool_paths, ToolPaths::default());
        assert_eq!(config.tool_paths.psql, None);
    }

    #[test]
    fn old_config_without_scripts_dir_loads_and_roundtrips_byte_identically() {
        // Scripts library (design §2): additive Option field with
        // serde(default) + skip_serializing_if — an old config.toml must
        // load AND save back without gaining the field.
        let toml_str = r#"[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.scripts_dir, None);
        let back = toml::to_string_pretty(&config).unwrap();
        assert!(!back.contains("scripts_dir"));
    }

    #[test]
    fn scripts_dir_set_roundtrips() {
        let mut config = AppConfig::default();
        config.scripts_dir = Some("D:\\sql\\library".to_string());
        let s = toml::to_string_pretty(&config).unwrap();
        let back: AppConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.scripts_dir.as_deref(), Some("D:\\sql\\library"));
    }

    #[test]
    fn old_config_without_mssql_options_loads() {
        // G15 T3: back-compat — a pre-G15 config.toml has no
        // `[connections.mssql]` table at all, and must load with `mssql ==
        // None` (not a default-filled `Some(MssqlOptions::default())`).
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
        assert_eq!(config.connections[0].mssql, None);
    }

    #[test]
    fn pre_g16_config_without_duckdb_loads_unchanged() {
        // §1 REQUIRED (a): adding the variant must not change how existing
        // postgres/mssql/sqlite configs load — purely additive.
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
        assert_eq!(config.connections[0].engine, Engine::Postgres);
        assert_eq!(config.connections[0].mssql, None);
    }

    #[test]
    fn duckdb_connection_roundtrip_save_load() {
        // §1 REQUIRED (b): a duckdb connection (database = file path,
        // read_only) survives save/load byte-exact.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut config = sample();
        config.connections[0].engine = Engine::Duckdb;
        config.connections[0].database = r"D:\data\analytics.duckdb".into();
        config.connections[0].read_only = true;
        config.connections[0].ssh = None;
        config.save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn duckdb_serde_string_form_is_pinned() {
        // §1 REQUIRED (c): the exact `engine = "duckdb"` spelling is a
        // saved-config contract — a future enum rename must not silently
        // break existing config.toml files.
        let toml_str = r#"
[[connections]]
id = "d1"
name = "analytics"
engine = "duckdb"
host = ""
database = "D:\\data\\analytics.duckdb"
user = ""
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.connections[0].engine, Engine::Duckdb);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        config.save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains(r#"engine = "duckdb""#), "raw: {raw}");
    }

    #[test]
    fn mssql_options_roundtrip_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let mut config = sample();
        config.connections[0].engine = Engine::Mssql;
        config.connections[0].mssql = Some(MssqlOptions {
            encrypt: false,
            trust_server_certificate: true,
            driver: Some("ODBC Driver 17 for SQL Server".into()),
        });
        config.save(&p).unwrap();
        let loaded = AppConfig::load(&p).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn mssql_options_partial_table_applies_serde_defaults() {
        let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "mssql"
host = "localhost"
database = "master"
user = "sa"

[connections.mssql]
trust_server_certificate = true
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let opts = config.connections[0].mssql.clone().unwrap();
        assert_eq!(opts.trust_server_certificate, true);
        assert_eq!(opts.encrypt, true);
        assert_eq!(opts.driver, None);
    }

    #[test]
    fn non_mssql_config_serializes_no_mssql_table() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        sample().save(&p).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("mssql"));
    }
}
